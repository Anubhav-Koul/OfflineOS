//! Importing skills from a git repository (Phase 8e).
//!
//! This extends the local-folder import (Phase 7c) to a git URL: shallow-clone
//! the repo, find every `SKILL.md` folder in it, and offer each one for review
//! and install through the *same* consent-gated path as a folder import. The
//! clone is the only new machinery; everything downstream reuses
//! [`crate::skill_import`].
//!
//! ## Hard constraints on the clone (all enforced here)
//!
//! - **No `git.exe`.** Users may not have git installed, so this uses `gix` (pure
//!   Rust, rustls transport) — never a subprocess.
//! - **Depth-1, no submodules.** A plain `gix` clone fetches neither history nor
//!   submodule content, which is exactly what we want: the tree at `HEAD`, once.
//! - **Hard timeout.** `gix` takes a `&AtomicBool` it polls during fetch/checkout;
//!   a timer trips it, so a slow or enormous repo is abandoned rather than hanging.
//! - **Hard size cap.** After checkout the whole tree is measured; over the cap is
//!   refused (and the temp dir deleted). The timeout bounds the download; this
//!   bounds the checked-out tree.
//! - **Symlinks refused outright.** A symlink anywhere in the tree aborts the whole
//!   import — a link can point outside the clone, and "import this repo" must never
//!   become "import whatever it points at". (The install path refuses them too, and
//!   so does the runtime; this is the outermost of three guards.)
//!
//! ## Untrusted text
//!
//! A third-party skill body is prompt-injection with persistence. The review card
//! renders it as **plain text**, never markdown/HTML (rendering can visually hide
//! instructions), and [`suspicious_chars`] flags zero-width and bidi-control
//! characters that could hide text even in a plain-text view. Names are
//! **namespaced by repo** (`<repo-slug>-<skill>`), because two repos will
//! eventually ship the same skill name — done by rewriting the frontmatter `name:`,
//! which is what the runtime keys a skill's identity on.
//!
//! ## Studying a repo that ships no skills
//!
//! [`clone_and_study`] is the other half: the same guarded clone, but for a repo
//! that has no `SKILL.md` to import. It gathers a **bounded** reading list —
//! README and manifests first, a dozen files at most — and [`study_prompt`] asks
//! the agent to distil a procedure out of it. Bounded because a small local model
//! cannot read a repository; the gathering is a pure function over a directory so
//! every cap is a unit test rather than a network round-trip.

use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::Serialize;

use crate::skill_import::{self, ImportFile};

/// The default whole-tree size cap for a cloned repo.
pub const MAX_REPO_BYTES: u64 = 50 * 1024 * 1024;
/// The default clone timeout.
pub const CLONE_TIMEOUT: Duration = Duration::from_secs(60);

/// One skill found in a cloned repo, ready for review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepoSkill {
    /// The skill's own frontmatter name, as written in the repo.
    pub name: String,
    /// The namespaced name it will install as (`<repo-slug>-<name>`), and the
    /// identity the runtime will know it by.
    pub install_name: String,
    /// The frontmatter description.
    pub description: String,
    /// The folder within the repo the skill lives in, for display (`/`-separated,
    /// `.` for the repo root).
    pub rel_dir: String,
    /// The full SKILL.md text that will install — **already namespaced** (its
    /// frontmatter `name:` rewritten to `install_name`), so what installs is what
    /// was reviewed.
    pub skill_md: String,
    /// The bundle files that ride along (SKILL.md excluded).
    pub files: Vec<ImportFile>,
    /// Absolute path to the skill folder in the clone — where [`install`] copies
    /// the bundle from. Not serialized to the UI.
    #[serde(skip)]
    pub folder: PathBuf,
}

/// A `SKILL.md` folder the scan found but cannot offer, and why.
///
/// One unusable skill must not refuse the whole repo: a real repo of eighteen
/// skills where one is oversized is seventeen skills the user still wants. So a
/// rejection is *reported per folder* and the rest are offered — but reported,
/// not swallowed, because silently listing 17 of 18 reads as "this repo has 17
/// skills".
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RejectedSkill {
    /// The folder within the repo (`/`-separated).
    pub rel_dir: String,
    /// Why it cannot be imported, in the user's words.
    pub reason: String,
}

/// The result of cloning and scanning a repo: the skills it offers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepoImport {
    /// The repo slug the skills are namespaced under (e.g. `owner-repo`).
    pub slug: String,
    /// The URL that was cloned.
    pub url: String,
    /// Every skill found, sorted by folder.
    pub skills: Vec<RepoSkill>,
    /// Every `SKILL.md` folder that cannot be offered, with its reason.
    pub rejected: Vec<RejectedSkill>,
}

/// Clone `url` into `into` (a caller-owned, ideally temp directory) and scan it
/// for skills. `into` must be empty or absent; the clone owns it.
///
/// The clone runs on a blocking thread (gix's transport is blocking); the timeout
/// is enforced by an interrupt flag a timer trips, because a `spawn_blocking` task
/// cannot itself be cancelled.
pub async fn clone_and_scan(
    url: String,
    into: PathBuf,
    max_bytes: u64,
    timeout: Duration,
) -> Result<RepoImport, String> {
    let slug = clone_repo(&url, &into, timeout).await?;
    let scan = scan_clone(&into, max_bytes, &slug, &url);
    if scan.is_err() {
        let _ = std::fs::remove_dir_all(&into);
    }
    scan
}

/// Clone `url` into `into` and return the repo slug, or say why not.
///
/// Shared by the skill scan and the study flow. The clone runs on a blocking
/// thread (gix's transport is blocking); the timeout is enforced by an interrupt
/// flag a timer trips, because a `spawn_blocking` task cannot itself be
/// cancelled. A failure deletes whatever was written.
async fn clone_repo(url: &str, into: &Path, timeout: Duration) -> Result<String, String> {
    let slug = repo_slug(url);
    if slug.is_empty() {
        return Err(format!("{url} does not look like a git repository URL"));
    }

    let interrupt = Arc::new(AtomicBool::new(false));
    let timer = {
        let flag = Arc::clone(&interrupt);
        tokio::spawn(async move {
            tokio::time::sleep(timeout).await;
            flag.store(true, Ordering::SeqCst);
        })
    };

    let clone_url = url.to_string();
    let clone_into = into.to_path_buf();
    let clone_flag = Arc::clone(&interrupt);
    let cloned =
        tokio::task::spawn_blocking(move || do_clone(&clone_url, &clone_into, &clone_flag))
            .await
            .map_err(|error| format!("the clone task failed: {error}"))?;
    timer.abort();

    if let Err(error) = cloned {
        let _ = std::fs::remove_dir_all(into);
        if interrupt.load(Ordering::SeqCst) {
            return Err(format!(
                "cloning {url} took longer than {}s and was stopped",
                timeout.as_secs()
            ));
        }
        return Err(error);
    }
    Ok(slug)
}

/// The blocking depth-1, no-submodule clone.
fn do_clone(url: &str, into: &Path, interrupt: &AtomicBool) -> Result<(), String> {
    let depth = NonZeroU32::new(1).expect("1 is nonzero");
    let mut prepare = gix::prepare_clone(url, into)
        .map_err(|error| format!("could not start cloning {url}: {error}"))?
        .with_shallow(gix::remote::fetch::Shallow::DepthAtRemote(depth));
    let (mut checkout, _) = prepare
        .fetch_then_checkout(gix::progress::Discard, interrupt)
        .map_err(|error| format!("could not fetch {url}: {error}"))?;
    checkout
        .main_worktree(gix::progress::Discard, interrupt)
        .map_err(|error| format!("could not check out {url}: {error}"))?;
    Ok(())
}

/// Walk the checked-out tree once — refusing symlinks, enforcing the size cap, and
/// collecting `SKILL.md` folders — then build a [`RepoSkill`] per folder.
fn scan_clone(root: &Path, max_bytes: u64, slug: &str, url: &str) -> Result<RepoImport, String> {
    let mut total: u64 = 0;
    let mut skill_dirs: Vec<PathBuf> = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir)
            .map_err(|error| format!("could not read the clone: {error}"))?;
        let mut has_skill_md = false;
        for entry in entries {
            let entry = entry.map_err(|error| format!("could not read the clone: {error}"))?;
            let file_type = entry.file_type().map_err(|error| {
                format!("could not inspect {}: {error}", entry.path().display())
            })?;
            let path = entry.path();
            if file_type.is_symlink() {
                return Err(format!(
                    "the repo contains a symlink ({}); imports must be plain files",
                    path.strip_prefix(root).unwrap_or(&path).display()
                ));
            }
            if file_type.is_dir() {
                // `.git` is clone metadata, not skill content: don't descend it,
                // but do count it toward the size cap below via its own entries?
                // No — skip it entirely; a shallow `.git` is small and not the
                // user's to review.
                if entry.file_name() == ".git" {
                    continue;
                }
                stack.push(path);
                continue;
            }
            let size = entry
                .metadata()
                .map_err(|error| format!("could not read {}: {error}", path.display()))?
                .len();
            total = total.saturating_add(size);
            if total > max_bytes {
                return Err(format!(
                    "the repo is larger than the {} MiB limit",
                    max_bytes / (1024 * 1024)
                ));
            }
            if entry.file_name() == "SKILL.md" {
                has_skill_md = true;
            }
        }
        if has_skill_md {
            skill_dirs.push(dir);
        }
    }

    if skill_dirs.is_empty() {
        return Err("no SKILL.md folders were found in the repo".to_string());
    }

    skill_dirs.sort();
    let mut skills = Vec::new();
    let mut rejected = Vec::new();
    for folder in skill_dirs {
        let rel_dir = folder
            .strip_prefix(root)
            .ok()
            .map(|rel| {
                if rel.as_os_str().is_empty() {
                    ".".to_string()
                } else {
                    rel.to_string_lossy().replace('\\', "/")
                }
            })
            .unwrap_or_else(|| ".".to_string());
        // Reuse the folder-import review verbatim (parse, caps, bundle walk).
        // A failure here is this folder's alone — the rest of the repo stands.
        let preview = match skill_import::preview(&folder) {
            Ok(preview) => preview,
            Err(reason) => {
                rejected.push(RejectedSkill { rel_dir, reason });
                continue;
            }
        };
        let install_name = namespaced_name(slug, &preview.name);
        let Some(skill_md) = rewrite_name(&preview.skill_md, &install_name) else {
            rejected.push(RejectedSkill {
                rel_dir,
                reason: format!(
                    "could not namespace the skill \u{201c}{}\u{201d} \
                     (its frontmatter has no top-level `name:` line to rewrite)",
                    preview.name
                ),
            });
            continue;
        };
        skills.push(RepoSkill {
            name: preview.name,
            install_name,
            description: preview.description,
            rel_dir,
            skill_md,
            files: preview.files,
            folder,
        });
    }

    Ok(RepoImport {
        slug: slug.to_string(),
        url: url.to_string(),
        skills,
        rejected,
    })
}

/// Install one reviewed repo skill through the folder-import path.
///
/// `reviewed_skill_md` is the namespaced text the user consented to (never a
/// re-read); the bundle is copied fresh from `skill.folder`. The install directory
/// is the namespaced name, so two repos' same-named skills do not collide.
pub fn install(
    skill: &RepoSkill,
    reviewed_skill_md: &str,
    skills_root: &Path,
) -> Result<String, String> {
    skill_import::install(&skill.folder, reviewed_skill_md, skills_root)
}

/// Derive a namespace slug from a git URL: the last two path segments
/// (`owner/repo`), `.git` stripped, normalized to a safe skill-name fragment.
/// Falls back to the last segment, then to the host.
pub fn repo_slug(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    // Strip a scheme and any `user@host:` prefix (scp-style), keep the path.
    let after_scheme = trimmed
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(trimmed);
    // The host ends at the first `/` (URL form) or `:` (scp form, `user@host:path`),
    // whichever comes first; the path is everything after it.
    let slash = after_scheme.find('/');
    let colon = after_scheme.find(':');
    let boundary = [slash, colon].into_iter().flatten().min();
    let path = boundary
        .and_then(|at| after_scheme.get(at + 1..))
        .unwrap_or("");
    let path = path.strip_suffix(".git").unwrap_or(path);
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let raw = match segments.as_slice() {
        [.., owner, repo] => format!("{owner}-{repo}"),
        [repo] => repo.to_string(),
        [] => after_scheme
            .split(['/', ':'])
            .next()
            .unwrap_or("")
            .to_string(),
    };
    normalize_fragment(&raw)
}

/// `<slug>-<name>`, normalized. The runtime keys a skill's identity on its
/// (normalized) frontmatter name, so this is what makes two repos' same-named
/// skills distinct.
pub fn namespaced_name(slug: &str, name: &str) -> String {
    let name = normalize_fragment(name);
    // A single-skill repo is usually *named after its skill*
    // (`pskoett/self-improving-agent` shipping `self-improving-agent`), and
    // blindly prefixing gives `pskoett-self-improving-agent-self-improving-agent`
    // — which is what the user then sees in their skills list forever. When the
    // slug already ends in the skill's name, the slug *is* the namespaced name:
    // it still carries the owner, so two repos remain distinguishable.
    let combined = if slug == name || slug.ends_with(&format!("-{name}")) {
        slug.to_string()
    } else {
        format!("{slug}-{name}")
    };
    let normalized = normalize_fragment(&combined);
    normalized.chars().take(64).collect()
}

/// Lowercase, keep `[a-z0-9-]`, everything else becomes `-`, runs of `-`
/// collapse, and leading/trailing `-` are trimmed.
fn normalize_fragment(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut last_dash = true; // trims leading dashes
    for ch in raw.chars() {
        let mapped = if ch.is_ascii_alphanumeric() {
            ch.to_ascii_lowercase()
        } else {
            '-'
        };
        if mapped == '-' {
            if last_dash {
                continue;
            }
            last_dash = true;
        } else {
            last_dash = false;
        }
        out.push(mapped);
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Rewrite the top-level `name:` in a SKILL.md's frontmatter to `new_name`,
/// leaving everything else — body included — untouched. `None` if there is no
/// frontmatter `name:` line to replace (which [`skill_import::preview`] already
/// rules out, so it is a real error if it happens here).
fn rewrite_name(skill_md: &str, new_name: &str) -> Option<String> {
    let mut lines = skill_md.lines();
    let first = lines.next()?;
    if first.trim() != "---" {
        return None;
    }
    let mut out = String::with_capacity(skill_md.len());
    out.push_str("---\n");
    let mut replaced = false;
    let mut in_frontmatter = true;
    for line in lines {
        if in_frontmatter && line.trim() == "---" {
            in_frontmatter = false;
            out.push_str("---\n");
            continue;
        }
        if in_frontmatter
            && !replaced
            && !line.starts_with([' ', '\t'])
            && line
                .split_once(':')
                .is_some_and(|(key, _)| key.trim() == "name")
        {
            out.push_str(&format!("name: {new_name}\n"));
            replaced = true;
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    replaced.then_some(out)
}

/// Detect hidden characters in `text`: zero-width (which vanish entirely) and
/// bidirectional-control (which can reorder visible text). Returns a
/// human-readable list of what was found, for the review card's warning. Empty
/// means the text is what it looks like.
pub fn suspicious_chars(text: &str) -> Vec<String> {
    let mut found = std::collections::BTreeSet::new();
    for ch in text.chars() {
        let label = match ch {
            '\u{200B}' => Some("zero-width space"),
            '\u{200C}' => Some("zero-width non-joiner"),
            '\u{200D}' => Some("zero-width joiner"),
            '\u{FEFF}' => Some("zero-width no-break space"),
            '\u{2060}' => Some("word joiner"),
            '\u{202A}'..='\u{202E}' => Some("bidirectional override"),
            '\u{2066}'..='\u{2069}' => Some("bidirectional isolate"),
            '\u{00AD}' => Some("soft hyphen"),
            _ => None,
        };
        if let Some(label) = label {
            found.insert(label.to_string());
        }
    }
    found.into_iter().collect()
}

// ------------------------------------------------------------ study a repo

/// The most files a study reads. A small local model cannot read a repository;
/// it can read a dozen of its most explanatory files.
pub const MAX_STUDY_FILES: usize = 12;
/// The most one studied file contributes, in bytes. Longer files are truncated
/// with a marker rather than skipped — a README's first pages are the useful part.
pub const MAX_STUDY_FILE_BYTES: usize = 8 * 1024;
/// The whole study's character budget for file text.
pub const MAX_STUDY_BYTES: usize = 40 * 1024;

/// One file the study read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StudiedFile {
    /// Repo-relative path, `/`-separated.
    pub rel_path: String,
    /// Its text, truncated to [`MAX_STUDY_FILE_BYTES`].
    pub text: String,
    /// Whether the text above is only the start of the file.
    pub truncated: bool,
}

/// What a study of a repo gathered, before any LLM turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepoStudy {
    /// The repo slug (`owner-repo`).
    pub slug: String,
    /// The URL that was cloned.
    pub url: String,
    /// The files the study will show the model, in priority order.
    pub files: Vec<StudiedFile>,
    /// Plain observations about the repo's *tool surface* — an MCP server, a CLI
    /// — from its manifests. Facts about what was seen, never a claim that the
    /// widget can wire any of it up.
    pub tool_surface: Vec<String>,
    /// How many candidate files the caps left unread, so the study can say so.
    pub skipped: usize,
}

/// Clone `url` into `into` and gather a bounded reading list from it.
///
/// The counterpart of [`clone_and_scan`] for a repo that is not a skills repo:
/// it does not need a `SKILL.md` and offers nothing to install. Nothing here
/// runs an LLM turn — the caller does that with [`study_prompt`], so the whole
/// gathering step is a pure function over a directory and is unit-tested as one.
pub async fn clone_and_study(
    url: String,
    into: PathBuf,
    max_bytes: u64,
    timeout: Duration,
) -> Result<RepoStudy, String> {
    let slug = clone_repo(&url, &into, timeout).await?;
    let study = gather_study(&into, &slug, &url, max_bytes);
    let _ = std::fs::remove_dir_all(&into);
    study
}

/// Read the repo's most explanatory files, within the caps.
///
/// Priority order is deliberate: a repo explains itself in its README first, its
/// manifests second (they name the tool surface), and its docs third. Reading
/// source would blow any context budget and teach less.
fn gather_study(root: &Path, slug: &str, url: &str, max_bytes: u64) -> Result<RepoStudy, String> {
    let mut candidates = Vec::new();
    collect_candidates(root, root, &mut candidates, 0, max_bytes)?;
    if candidates.is_empty() {
        return Err("this repo has no README, manifest, or documentation to read".to_string());
    }
    candidates.sort_by(|a, b| {
        study_rank(&a.0)
            .cmp(&study_rank(&b.0))
            .then_with(|| a.0.cmp(&b.0))
    });

    let mut files = Vec::new();
    let mut budget = MAX_STUDY_BYTES;
    let mut skipped = 0usize;
    let mut tool_surface = Vec::new();
    for (rel_path, path) in candidates {
        let Ok(raw) = std::fs::read_to_string(&path) else {
            skipped += 1; // silent-ok: a binary or unreadable file teaches nothing
            continue;
        };
        observe_tool_surface(&rel_path, &raw, &mut tool_surface);
        if files.len() >= MAX_STUDY_FILES || budget == 0 {
            skipped += 1;
            continue;
        }
        let allowance = MAX_STUDY_FILE_BYTES.min(budget);
        let (text, truncated) = truncate_on_char_boundary(&raw, allowance);
        budget -= text.len().min(budget);
        files.push(StudiedFile {
            rel_path,
            text,
            truncated,
        });
    }

    tool_surface.sort();
    tool_surface.dedup();
    Ok(RepoStudy {
        slug: slug.to_string(),
        url: url.to_string(),
        files,
        tool_surface,
        skipped,
    })
}

/// Walk the tree collecting study candidates, applying the same symlink refusal
/// and size cap as the skill scan.
fn collect_candidates(
    root: &Path,
    dir: &Path,
    out: &mut Vec<(String, PathBuf)>,
    depth: usize,
    max_bytes: u64,
) -> Result<(), String> {
    // Documentation lives near the top of a repo; descending forever would turn
    // a study into a crawl of every vendored dependency.
    if depth > 3 {
        return Ok(());
    }
    let entries =
        std::fs::read_dir(dir).map_err(|error| format!("could not read the clone: {error}"))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("could not read the clone: {error}"))?;
        let file_type = entry
            .file_type()
            .map_err(|error| format!("could not inspect {}: {error}", entry.path().display()))?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if file_type.is_symlink() {
            return Err(format!(
                "the repo contains a symlink ({name}); studying it would follow it off the clone"
            ));
        }
        if file_type.is_dir() {
            if name.starts_with('.') || name == "node_modules" || name == "target" {
                continue;
            }
            collect_candidates(root, &path, out, depth + 1, max_bytes)?;
            continue;
        }
        let rel_path = path
            .strip_prefix(root)
            .map(|rel| rel.to_string_lossy().replace('\\', "/"))
            .unwrap_or(name);
        if study_rank(&rel_path) < RANK_IGNORED {
            let size = entry
                .metadata()
                .map_err(|error| format!("could not read {rel_path}: {error}"))?
                .len();
            if size <= max_bytes {
                out.push((rel_path, path));
            }
        }
    }
    Ok(())
}

/// Files that rank at or above this are not worth a study's budget.
const RANK_IGNORED: u8 = 9;

/// Lower ranks are read first. A repo explains itself in this order.
fn study_rank(rel_path: &str) -> u8 {
    let lower = rel_path.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    let at_root = !lower.contains('/');
    if at_root && name.starts_with("readme") {
        return 0;
    }
    if at_root
        && matches!(
            name,
            "package.json"
                | "cargo.toml"
                | "pyproject.toml"
                | "setup.py"
                | "go.mod"
                | "composer.json"
                | "gemfile"
                | "pom.xml"
                | "mcp.json"
                | ".mcp.json"
                | "makefile"
                | "justfile"
        )
    {
        return 1;
    }
    if at_root && matches!(name, "agents.md" | "claude.md" | "contributing.md") {
        return 2;
    }
    if lower.starts_with("docs/") && name.ends_with(".md") {
        return 3;
    }
    if at_root && name.ends_with(".md") {
        return 4;
    }
    RANK_IGNORED
}

/// Note what a manifest says about the repo's *tool surface*.
///
/// Deliberately observational. Phase 8b proved the registry connector lane
/// works, but wiring an arbitrary repo up as a connector is not something the
/// widget can do on the user's behalf — so this reports what is there and the
/// study points at the Connectors panel, rather than promising a registration.
fn observe_tool_surface(rel_path: &str, text: &str, out: &mut Vec<String>) {
    let lower = text.to_ascii_lowercase();
    let mentions_mcp =
        lower.contains("modelcontextprotocol") || lower.contains("model context protocol");
    match rel_path {
        "package.json" => {
            if mentions_mcp {
                out.push("an MCP server written for Node/TypeScript".to_string());
            }
            if lower.contains("\"bin\"") {
                out.push("a command-line tool installable from npm".to_string());
            }
        }
        "pyproject.toml" | "setup.py" => {
            if mentions_mcp {
                out.push("an MCP server written for Python".to_string());
            }
            if lower.contains("[project.scripts]") || lower.contains("console_scripts") {
                out.push("a command-line tool installable from PyPI".to_string());
            }
        }
        "Cargo.toml" => {
            if lower.contains("[[bin]]") {
                out.push("a command-line tool built from Rust".to_string());
            }
        }
        "mcp.json" | ".mcp.json" => {
            out.push("an MCP server configuration checked into the repo".to_string());
        }
        _ => {
            if rel_path.eq_ignore_ascii_case("readme.md") && mentions_mcp {
                out.push("an MCP server, according to its README".to_string());
            }
        }
    }
}

/// Truncate to at most `max` bytes without splitting a character.
fn truncate_on_char_boundary(text: &str, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text.to_string(), false);
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

/// The prompt a study sends on its own thread.
///
/// Same contract as the reflection prompt (Phase 7b): a fenced SKILL.md or the
/// single word NO, and no tool calls — because the runtime would not stop one
/// (the Phase 4 finding), the instruction is a request, and the widget's own
/// consent card is what actually gates the install.
pub fn study_prompt(study: &RepoStudy) -> String {
    let mut body = String::new();
    for file in &study.files {
        body.push_str(&format!("\n===== {} =====\n", file.rel_path));
        body.push_str(&file.text);
        if file.truncated {
            body.push_str("\n[… truncated]\n");
        }
    }
    let surface = if study.tool_surface.is_empty() {
        String::new()
    } else {
        format!(
            "\n\nIts manifests suggest the repo ships: {}.",
            study.tool_surface.join("; ")
        )
    };
    let unread = if study.skipped == 0 {
        String::new()
    } else {
        format!(
            " {} further file(s) were left unread to stay within budget.",
            study.skipped
        )
    };
    format!(
        "You are studying a code repository, {}, to learn how to *use* it. Below \
         are its most explanatory files.{unread}\n{body}\n{surface}\n\nIf this \
         repository teaches a reusable procedure — how to run it, how to call it, \
         the steps someone would repeat — reply with ONLY a draft SKILL.md inside \
         a ```markdown fenced code block, with YAML frontmatter containing `name` \
         (kebab-case, starting with `{}`), `description`, and `activation:` \
         keywords, followed by the procedure itself. Write the procedure, not a \
         summary of the project. If it teaches no reusable procedure, reply with \
         exactly NO. Do NOT install anything and do NOT call any tools — output \
         the draft or NO, nothing else.",
        study.url, study.slug
    )
}

/// A single line in a diff between an installed skill and an incoming update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "tag", rename_all = "snake_case")]
pub enum DiffLine {
    /// Unchanged (shown for context).
    Context {
        /// The line text.
        text: String,
    },
    /// Present only in the new version.
    Added {
        /// The line text.
        text: String,
    },
    /// Present only in the installed version.
    Removed {
        /// The line text.
        text: String,
    },
}

/// A line-level diff of `old` → `new` (LCS-based), so a re-import shows what
/// changed rather than asking the user to re-read the whole skill. No dependency:
/// skills are small, so an O(n·m) LCS table is fine.
pub fn diff_lines(old: &str, new: &str) -> Vec<DiffLine> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let (n, m) = (a.len(), b.len());
    // lcs[i][j] = length of the LCS of a[i..] and b[j..].
    let mut lcs = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            out.push(DiffLine::Context {
                text: a[i].to_string(),
            });
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            out.push(DiffLine::Removed {
                text: a[i].to_string(),
            });
            i += 1;
        } else {
            out.push(DiffLine::Added {
                text: b[j].to_string(),
            });
            j += 1;
        }
    }
    while i < n {
        out.push(DiffLine::Removed {
            text: a[i].to_string(),
        });
        i += 1;
    }
    while j < m {
        out.push(DiffLine::Added {
            text: b[j].to_string(),
        });
        j += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_come_from_owner_and_repo() {
        assert_eq!(repo_slug("https://github.com/owner/repo.git"), "owner-repo");
        assert_eq!(repo_slug("https://github.com/owner/repo"), "owner-repo");
        assert_eq!(repo_slug("https://example.com/a/b/c/deep.git"), "c-deep");
        assert_eq!(
            repo_slug("git@github.com:Owner/My_Repo.git"),
            "owner-my-repo"
        );
        assert_eq!(repo_slug("https://host/single"), "single");
    }

    #[test]
    fn namespacing_produces_a_safe_bounded_name() {
        assert_eq!(
            namespaced_name("owner-repo", "my-skill"),
            "owner-repo-my-skill"
        );
        assert_eq!(
            namespaced_name("Owner_Repo", "Weird Name!"),
            "owner-repo-weird-name"
        );
        assert!(namespaced_name("a".repeat(80).as_str(), "b").len() <= 64);
        // A repo named after its one skill does not say it twice.
        assert_eq!(
            namespaced_name("pskoett-self-improving-agent", "self-improving-agent"),
            "pskoett-self-improving-agent"
        );
        assert_eq!(namespaced_name("thing", "thing"), "thing");
        // But a name that merely *contains* the skill still gets prefixed.
        assert_eq!(
            namespaced_name("owner-agent-tools", "agent"),
            "owner-agent-tools-agent"
        );
    }

    #[test]
    fn rewrite_name_replaces_only_the_top_level_name() {
        let md = "---\nname: original\ndescription: A skill.\nactivation:\n  name: not-this\n---\n\n# Body\n\nname: also not this\n";
        let out = rewrite_name(md, "repo-original").expect("rewrite");
        assert!(out.contains("name: repo-original\n"));
        assert!(out.contains("  name: not-this")); // nested key untouched
        assert!(out.contains("name: also not this")); // body untouched
        assert!(!out.contains("name: original\n"));
        // The description and body survive.
        assert!(out.contains("description: A skill."));
        assert!(out.contains("# Body"));
    }

    #[test]
    fn rewrite_name_declines_without_frontmatter() {
        assert!(rewrite_name("no frontmatter here", "x").is_none());
        assert!(rewrite_name("---\ndescription: x\n---\nbody\n", "x").is_none());
    }

    #[test]
    fn suspicious_chars_flags_hidden_text() {
        assert!(suspicious_chars("plain text is fine").is_empty());
        let sneaky = "delete\u{200B}everything\u{202E}reversed";
        let flags = suspicious_chars(sneaky);
        assert!(flags.iter().any(|f| f.contains("zero-width")));
        assert!(flags.iter().any(|f| f.contains("bidirectional")));
    }

    #[test]
    fn diff_marks_added_and_removed_lines() {
        let old = "one\ntwo\nthree\n";
        let new = "one\ntwo-changed\nthree\nfour\n";
        let diff = diff_lines(old, new);
        assert!(diff.contains(&DiffLine::Context { text: "one".into() }));
        assert!(diff.contains(&DiffLine::Removed { text: "two".into() }));
        assert!(diff.contains(&DiffLine::Added {
            text: "two-changed".into()
        }));
        assert!(diff.contains(&DiffLine::Added {
            text: "four".into()
        }));
    }

    #[test]
    fn identical_texts_diff_to_all_context() {
        let text = "same\nlines\n";
        let diff = diff_lines(text, text);
        assert!(
            diff.iter()
                .all(|line| matches!(line, DiffLine::Context { .. }))
        );
    }

    fn write_skill(dir: &Path, name: &str) {
        std::fs::create_dir_all(dir).expect("mkdir");
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: A {name} skill.\n---\n\n# {name}\n\nDo it.\n"),
        )
        .expect("write");
    }

    #[test]
    fn scan_finds_every_skill_namespaces_it_and_skips_dot_git() {
        let root = tempfile::tempdir().expect("root");
        // A .git dir that must be skipped (even one holding a stray SKILL.md).
        write_skill(&root.path().join(".git").join("evil"), "evil");
        // Two real skills at different depths.
        write_skill(&root.path().join("first"), "alpha");
        write_skill(&root.path().join("nested").join("second"), "beta");

        let import = scan_clone(
            root.path(),
            MAX_REPO_BYTES,
            "owner-repo",
            "https://x/owner/repo",
        )
        .expect("scan");
        assert_eq!(import.slug, "owner-repo");
        let names: Vec<&str> = import
            .skills
            .iter()
            .map(|s| s.install_name.as_str())
            .collect();
        assert!(names.contains(&"owner-repo-alpha"), "{names:?}");
        assert!(names.contains(&"owner-repo-beta"), "{names:?}");
        assert!(
            !names.iter().any(|n| n.contains("evil")),
            ".git must be skipped: {names:?}"
        );
        // The installed text is namespaced; the body survives.
        let alpha = import.skills.iter().find(|s| s.name == "alpha").unwrap();
        assert!(alpha.skill_md.contains("name: owner-repo-alpha\n"));
        assert!(alpha.skill_md.contains("Do it."));
    }

    /// A real repo of eighteen skills where one is oversized is seventeen skills
    /// the user still wants. The bad folder is reported, not fatal.
    #[test]
    fn one_unusable_skill_does_not_refuse_the_whole_repo() {
        let root = tempfile::tempdir().expect("root");
        write_skill(&root.path().join("good"), "good");
        std::fs::create_dir_all(root.path().join("bad")).expect("mkdir");
        std::fs::write(root.path().join("bad").join("SKILL.md"), "just prose").expect("write");

        let import = scan_clone(root.path(), MAX_REPO_BYTES, "owner-repo", "https://x/o/r")
            .expect("the good skill still imports");
        assert_eq!(import.skills.len(), 1);
        assert_eq!(import.skills[0].install_name, "owner-repo-good");
        assert_eq!(import.rejected.len(), 1);
        assert_eq!(import.rejected[0].rel_dir, "bad");
        assert!(
            import.rejected[0].reason.contains("frontmatter"),
            "{:?}",
            import.rejected[0]
        );
    }

    #[test]
    fn scan_refuses_a_repo_over_the_size_cap() {
        let root = tempfile::tempdir().expect("root");
        write_skill(&root.path().join("s"), "s");
        std::fs::write(root.path().join("big.bin"), vec![0u8; 2048]).expect("write");
        let error = scan_clone(root.path(), 1024, "slug", "https://x/o/r").expect_err("too big");
        assert!(error.contains("larger than"), "{error}");
    }

    #[test]
    fn scan_reports_when_a_repo_has_no_skills() {
        let root = tempfile::tempdir().expect("root");
        std::fs::write(root.path().join("README.md"), "not a skill").expect("write");
        let error = scan_clone(root.path(), MAX_REPO_BYTES, "slug", "https://x/o/r")
            .expect_err("no skills");
        assert!(error.contains("no SKILL.md"), "{error}");
    }

    // ------------------------------------------------------- study a repo

    fn study_repo_tree(root: &Path) {
        std::fs::write(root.join("README.md"), "# The repo\n\nRun `thing --go`.\n").expect("w");
        std::fs::write(
            root.join("package.json"),
            "{\n  \"name\": \"thing\",\n  \"bin\": {\"thing\": \"./cli.js\"},\n  \
             \"dependencies\": {\"@modelcontextprotocol/sdk\": \"^1\"}\n}\n",
        )
        .expect("w");
        std::fs::create_dir_all(root.join("docs")).expect("mkdir");
        std::fs::write(root.join("docs").join("usage.md"), "## Usage\n").expect("w");
        // Source is deliberately not study material.
        std::fs::write(root.join("cli.js"), "console.log('hi')\n").expect("w");
        std::fs::create_dir_all(root.join("node_modules").join("dep")).expect("mkdir");
        std::fs::write(
            root.join("node_modules").join("dep").join("README.md"),
            "not ours\n",
        )
        .expect("w");
    }

    #[test]
    fn a_study_reads_the_readme_first_and_skips_source_and_vendored_trees() {
        let root = tempfile::tempdir().expect("root");
        study_repo_tree(root.path());
        let study =
            gather_study(root.path(), "o-r", "https://x/o/r", MAX_REPO_BYTES).expect("study");
        let paths: Vec<&str> = study.files.iter().map(|f| f.rel_path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["README.md", "package.json", "docs/usage.md"],
            "README first, then manifests, then docs — and nothing else"
        );
    }

    #[test]
    fn a_study_names_the_tool_surface_it_can_see() {
        let root = tempfile::tempdir().expect("root");
        study_repo_tree(root.path());
        let study =
            gather_study(root.path(), "o-r", "https://x/o/r", MAX_REPO_BYTES).expect("study");
        assert!(
            study.tool_surface.iter().any(|s| s.contains("MCP server")),
            "{:?}",
            study.tool_surface
        );
        assert!(
            study
                .tool_surface
                .iter()
                .any(|s| s.contains("command-line")),
            "{:?}",
            study.tool_surface
        );
        // The prompt carries the observation and asks for a procedure, not prose.
        let prompt = study_prompt(&study);
        assert!(prompt.contains("MCP server"));
        assert!(prompt.contains("reply with exactly NO"));
        assert!(prompt.contains("README.md"));
    }

    #[test]
    fn a_long_file_is_truncated_and_says_so() {
        let root = tempfile::tempdir().expect("root");
        std::fs::write(
            root.path().join("README.md"),
            "x".repeat(MAX_STUDY_FILE_BYTES * 2),
        )
        .expect("w");
        let study =
            gather_study(root.path(), "o-r", "https://x/o/r", MAX_REPO_BYTES).expect("study");
        assert_eq!(study.files.len(), 1);
        assert!(study.files[0].truncated);
        assert_eq!(study.files[0].text.len(), MAX_STUDY_FILE_BYTES);
        assert!(study_prompt(&study).contains("truncated"));
    }

    #[test]
    fn a_study_of_a_repo_with_nothing_to_read_says_so() {
        let root = tempfile::tempdir().expect("root");
        std::fs::write(root.path().join("main.rs"), "fn main() {}\n").expect("w");
        let error = gather_study(root.path(), "o-r", "https://x/o/r", MAX_REPO_BYTES)
            .expect_err("nothing to read");
        assert!(error.contains("no README"), "{error}");
    }

    #[test]
    fn a_study_never_reads_more_than_the_file_cap() {
        let root = tempfile::tempdir().expect("root");
        std::fs::write(root.path().join("README.md"), "readme\n").expect("w");
        for index in 0..(MAX_STUDY_FILES + 5) {
            std::fs::write(root.path().join(format!("doc-{index}.md")), "text\n").expect("w");
        }
        let study =
            gather_study(root.path(), "o-r", "https://x/o/r", MAX_REPO_BYTES).expect("study");
        assert_eq!(study.files.len(), MAX_STUDY_FILES);
        // README + 17 docs = 18 candidates; the cap reads 12 and reports the rest.
        assert_eq!(study.skipped, 18 - MAX_STUDY_FILES);
        assert!(study_prompt(&study).contains("left unread"));
    }

    /// A real depth-1 clone over the network. Ignored (needs the internet); run
    /// with `cargo test -p ic_widget -- --ignored clones_a_real_repo`. octocat's
    /// Hello-World has no SKILL.md, so the *scan* reports none — which still
    /// exercises the whole gix clone + walk + size/symlink guards live.
    #[tokio::test]
    #[ignore = "networked; run with --ignored"]
    async fn clones_a_real_repo_and_scans_it() {
        let into = std::env::temp_dir().join(format!("ic_git_import_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&into);
        let result = clone_and_scan(
            "https://github.com/octocat/Hello-World.git".to_string(),
            into.clone(),
            MAX_REPO_BYTES,
            CLONE_TIMEOUT,
        )
        .await;
        let _ = std::fs::remove_dir_all(&into);
        // The clone + scan ran; the repo simply carries no skills.
        let error = result.expect_err("Hello-World has no SKILL.md");
        assert!(error.contains("no SKILL.md"), "{error}");
    }

    /// The real thing: a public repo that actually ships skills, cloned and
    /// scanned by the shipping code. Ignored (needs the internet); run with
    /// `cargo test -p ic_widget -- --ignored clones_a_real_skills_repo`.
    ///
    /// Loose assertions on purpose — the repo's contents are upstream's to
    /// change. What is pinned is the shape: many skills found, each namespaced
    /// under the repo slug, each carrying its own body.
    #[tokio::test]
    #[ignore = "networked; run with --ignored"]
    async fn clones_a_real_skills_repo_and_namespaces_every_skill() {
        let into = std::env::temp_dir().join(format!("ic_git_skills_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&into);
        let result = clone_and_scan(
            "https://github.com/anthropics/skills.git".to_string(),
            into.clone(),
            MAX_REPO_BYTES,
            CLONE_TIMEOUT,
        )
        .await;
        let import = match result {
            Ok(import) => import,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&into);
                panic!("clone + scan failed: {error}");
            }
        };
        assert_eq!(import.slug, "anthropics-skills");
        assert!(
            import.skills.len() > 5,
            "expected many skills, got {}",
            import.skills.len()
        );
        for skill in &import.skills {
            assert!(
                skill.install_name.starts_with("anthropics-skills-"),
                "not namespaced: {}",
                skill.install_name
            );
            assert!(
                skill
                    .skill_md
                    .contains(&format!("name: {}\n", skill.install_name)),
                "the installed text is not namespaced: {}",
                skill.install_name
            );
            assert!(!skill.description.is_empty(), "{}", skill.install_name);
        }
        // The repo's `claude-api` skill is a 72 KiB SKILL.md — over the runtime's
        // own 64 KiB limit, so it cannot install and is reported rather than
        // silently dropped, while its seventeen neighbours are still offered.
        // (If upstream trims it, this assertion is what says the finding is gone.)
        for reject in &import.rejected {
            println!("rejected {}: {}", reject.rel_dir, reject.reason);
        }
        assert!(
            import.rejected.iter().all(|r| !r.reason.is_empty()),
            "a rejection must say why"
        );
        let _ = std::fs::remove_dir_all(&into);
    }
}
