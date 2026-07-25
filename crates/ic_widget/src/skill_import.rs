//! Importing a third-party skill from a local folder (Phase 7c).
//!
//! The 7b probe settled what this module is up against: an installed skill is
//! the **trusted tier** — its full body reaches the model on activation — and
//! the runtime scans skill text for **nothing** on install (`gating.rs` checks
//! environment requirements; the context-build scan is structural, not
//! semantic). So the review step here is not politeness, it is the only gate a
//! third-party skill ever passes through. That shapes both halves:
//!
//! - [`preview`] is a pure read that returns everything the user needs to
//!   *actually* review: the full SKILL.md text, and the bundle's file list with
//!   sizes. It enforces the runtime's own install-bundle limits
//!   (`MAX_INSTALL_BUNDLE_*` in `ironclaw_skills/src/management/install_bundle.rs`,
//!   mirrored here) so an import can never be bigger than the capability path
//!   would have allowed. It also names the two things a reviewer cannot see by
//!   reading the text: bundle content this app has no lane for and will never run
//!   ([`inert_lanes`]), and what the body costs the model's context every time it
//!   activates ([`context_cost`]).
//! - [`install`] writes **the reviewed text**, passed back in, not a re-read of
//!   the folder — the file could have changed between the review and the yes,
//!   and what installs must be exactly what was consented to. Bundle data files
//!   are copied fresh (they are not part of the reviewed text; their names and
//!   sizes were) with every cap re-checked, and a failed install removes the
//!   half-written directory rather than leaving a skill that half-exists.
//!
//! Symlinks are refused outright: a link inside the folder can point anywhere
//! on the machine, and "import this folder" must never quietly become "import
//! whatever this folder points at".

use std::path::Path;

use serde::Serialize;

use crate::ambient::reflection::Draft;

/// The runtime's own install-bundle limits, mirrored from
/// `ironclaw_skills/src/management/install_bundle.rs` — an import must never
/// admit more than `builtin__skill_install` would have.
pub const MAX_BUNDLE_FILES: usize = 256;
/// Per-file byte cap (2 MiB upstream).
pub const MAX_BUNDLE_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// Whole-bundle byte cap (20 MiB upstream).
pub const MAX_BUNDLE_TOTAL_BYTES: u64 = 20 * 1024 * 1024;
/// The runtime's cap on a SKILL.md itself (`MAX_PROMPT_FILE_SIZE` in
/// `ironclaw_skills/src/types.rs`), enforced by `skill_install` and by the
/// bounded read on discovery. A bigger file cannot install, so an import refuses
/// it here with a message that says which limit it hit.
pub const MAX_SKILL_MD_BYTES: u64 = 64 * 1024;

/// One bundle file, as shown in the review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImportFile {
    /// Folder-relative path, `/`-separated.
    pub path: String,
    /// Size on disk at preview time.
    pub bytes: u64,
}

/// Everything the review step shows. Pure data; nothing here has side effects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImportPreview {
    /// The skill's frontmatter name — and the directory it would install as.
    pub name: String,
    /// The frontmatter description.
    pub description: String,
    /// The full, normalized SKILL.md text the user is consenting to.
    pub skill_md: String,
    /// The bundle files that would ride along (SKILL.md itself excluded).
    pub files: Vec<ImportFile>,
    /// Bundle content this app has no lane for, and so will never run.
    pub inert: Vec<InertLane>,
    /// What activating this skill costs the model's context.
    pub cost: ContextCost,
}

/// Read a skill folder and produce its review, or say exactly why not.
///
/// Line endings are normalized to `\n` (the one normalization the runtime's own
/// install path performs that matters to us — the returned `skill_md` is what
/// [`install`] later writes verbatim).
pub fn preview(folder: &Path) -> Result<ImportPreview, String> {
    if !folder.is_dir() {
        return Err(format!("{} is not a folder", folder.display()));
    }
    let skill_md_path = folder.join("SKILL.md");
    let metadata =
        std::fs::metadata(&skill_md_path).map_err(|_| "the folder has no SKILL.md".to_string())?;
    if metadata.len() > MAX_BUNDLE_FILE_BYTES {
        return Err("SKILL.md is larger than the 2 MiB per-file limit".to_string());
    }
    let raw = std::fs::read_to_string(&skill_md_path)
        .map_err(|error| format!("could not read SKILL.md: {error}"))?;
    let normalized = raw.replace("\r\n", "\n");
    let draft = parse(&normalized)?;

    let files = bundle_files(folder)?;
    Ok(ImportPreview {
        inert: inert_lanes(&files),
        cost: context_cost(&draft.content),
        name: draft.name,
        description: draft.description,
        skill_md: draft.content,
        files,
    })
}

/// Install a reviewed skill: write `reviewed_skill_md` (exactly what the user
/// consented to — never a re-read), copy the bundle, clean up on failure.
///
/// Bundle files are re-walked and re-capped at install time: their *names and
/// sizes* were reviewed, so a swap of contents is the folder owner's own
/// business, but a bundle that grew past the caps since the review is refused.
pub fn install(
    folder: &Path,
    reviewed_skill_md: &str,
    skills_root: &Path,
) -> Result<String, String> {
    let draft = parse(reviewed_skill_md)?;
    let dest = skills_root.join(&draft.name);
    if dest.exists() {
        return Err(format!(
            "a skill named \u{201c}{}\u{201d} already exists",
            draft.name
        ));
    }

    let files = bundle_files(folder)?;
    std::fs::create_dir_all(&dest)
        .map_err(|error| format!("could not create the skill directory: {error}"))?;

    let result = write_bundle(folder, &draft, &files, &dest);
    if result.is_err() {
        // Half a skill is worse than none: the directory was created by this
        // call seconds ago and holds nothing but this failed copy.
        let _ = std::fs::remove_dir_all(&dest);
    }
    result.map(|()| {
        tracing::info!(skill = %draft.name, from = %folder.display(), "imported a skill with the user's consent");
        draft.name
    })
}

fn write_bundle(
    folder: &Path,
    draft: &Draft,
    files: &[ImportFile],
    dest: &Path,
) -> Result<(), String> {
    std::fs::write(dest.join("SKILL.md"), draft.content.as_bytes())
        .map_err(|error| format!("could not write SKILL.md: {error}"))?;
    for file in files {
        let source = folder.join(file.path.replace('/', std::path::MAIN_SEPARATOR_STR));
        let target = dest.join(file.path.replace('/', std::path::MAIN_SEPARATOR_STR));
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
        }
        // Re-check the cap on the actual bytes moved, not the preview's memory.
        let size = std::fs::metadata(&source)
            .map_err(|error| format!("could not read {}: {error}", file.path))?
            .len();
        if size > MAX_BUNDLE_FILE_BYTES {
            return Err(format!("{} grew past the 2 MiB per-file limit", file.path));
        }
        std::fs::copy(&source, &target)
            .map_err(|error| format!("could not copy {}: {error}", file.path))?;
    }
    Ok(())
}

/// The frontmatter keys an import needs. Everything else in the YAML is the
/// skill's own business (the runtime has its own `SkillManifest` for it) and is
/// preserved untouched in the text we write.
#[derive(serde::Deserialize)]
struct Frontmatter {
    name: String,
    /// Optional, exactly as the runtime has it (`SkillManifest` defaults it).
    /// A skill with no description is thin, not invalid — and refusing one the
    /// runtime installs is a wall the user cannot get past.
    #[serde(default)]
    description: String,
}

/// Parse a complete SKILL.md the way the **runtime** does, with an error message
/// a person can act on.
///
/// This deliberately does *not* reuse [`reflection::parse_skill_md`]. That parser
/// reads a model's reply — a hand-rolled line scanner is right there, and its
/// narrowness is the fail-closed posture 7b wants. A third-party file is the
/// opposite problem: it is real YAML written by someone else, and refusing what
/// the runtime would accept is a bug the user cannot fix. Anthropic's own skills
/// repo is the proof — `claude-api` writes `description: |-` as a block scalar,
/// which a line scanner reads as empty.
///
/// So the rules here are the runtime's (`ironclaw_skills/src/parser.rs` +
/// `validation.rs` + `MAX_PROMPT_FILE_SIZE`), plus one guard the runtime lacks
/// and Windows needs: a name that is a reserved device name cannot be a
/// directory. Pinned by `skill_parser_agreement.rs`.
pub fn parse_skill_md(content: &str) -> Result<Draft, String> {
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    let normalized = normalized
        .strip_prefix('\u{feff}')
        .unwrap_or(&normalized)
        .to_string();
    if normalized.len() as u64 > MAX_SKILL_MD_BYTES {
        return Err(format!(
            "SKILL.md is larger than the {} KiB limit the runtime puts on a skill",
            MAX_SKILL_MD_BYTES / 1024
        ));
    }

    let trimmed = normalized.trim_start_matches('\n');
    let after_first = trimmed
        .strip_prefix("---")
        .and_then(|rest| rest.split_once('\n').map(|(_, rest)| rest))
        .ok_or("SKILL.md does not start with `---` frontmatter")?;
    let (front, body) = split_at_closing_fence(after_first)
        .ok_or("SKILL.md's `---` frontmatter is never closed by another `---` line")?;

    let front: Frontmatter = serde_yml::from_str(front)
        .map_err(|error| format!("SKILL.md's frontmatter is not valid YAML: {error}"))?;
    let name = front.name.trim().to_string();
    let description = front.description.trim().to_string();
    if !valid_import_name(&name) {
        return Err(format!(
            "\u{201c}{name}\u{201d} is not a usable skill name: it must start with a \
             letter or digit, use only letters, digits, `.`, `-` and `_`, be at most \
             64 characters, and not be a reserved Windows device name"
        ));
    }
    if body.trim().is_empty() {
        return Err("SKILL.md has no body: a skill with no body is a name, not a procedure".into());
    }

    Ok(Draft {
        name,
        description,
        content: normalized,
    })
}

/// Split frontmatter from body at the first `---` on its own line.
fn split_at_closing_fence(after_first_line: &str) -> Option<(&str, &str)> {
    let mut offset = 0usize;
    for line in after_first_line.split_inclusive('\n') {
        if line.trim_end_matches('\n').trim() == "---" {
            return Some((
                &after_first_line[..offset],
                &after_first_line[offset + line.len()..],
            ));
        }
        offset += line.len();
    }
    None
}

/// The runtime's own name grammar (`[a-zA-Z0-9][a-zA-Z0-9._-]{0,63}`), plus the
/// Windows device-name guard it lacks — the name becomes a directory here.
fn valid_import_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_alphanumeric()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
        && !is_reserved_device_name(name)
}

/// Parse a complete SKILL.md, with an error message a person can act on.
fn parse(content: &str) -> Result<Draft, String> {
    parse_skill_md(content)
}

/// Every bundle file under `folder` (SKILL.md at the root excluded), validated
/// against the caps and the path rules. Fails on the first problem — an import
/// with one bad file is an import to fix, not to trim silently.
fn bundle_files(folder: &Path) -> Result<Vec<ImportFile>, String> {
    let mut files = Vec::new();
    let mut total: u64 = 0;
    let mut stack = vec![folder.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir)
            .map_err(|error| format!("could not read {}: {error}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|error| format!("could not read the folder: {error}"))?;
            let file_type = entry.file_type().map_err(|error| {
                format!("could not inspect {}: {error}", entry.path().display())
            })?;
            let path = entry.path();
            let relative = relative_path(folder, &path)?;
            if file_type.is_symlink() {
                // A link can point anywhere on the machine; importing it would
                // quietly import something the user never looked at.
                return Err(format!(
                    "{relative} is a symlink; imports must be plain files"
                ));
            }
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if relative == "SKILL.md" {
                continue; // the reviewed text itself, written separately
            }
            let size = entry
                .metadata()
                .map_err(|error| format!("could not read {relative}: {error}"))?
                .len();
            if size > MAX_BUNDLE_FILE_BYTES {
                return Err(format!(
                    "{relative} is larger than the 2 MiB per-file limit"
                ));
            }
            total += size;
            if total > MAX_BUNDLE_TOTAL_BYTES {
                return Err("the folder is larger than the 20 MiB bundle limit".to_string());
            }
            files.push(ImportFile {
                path: relative,
                bytes: size,
            });
            if files.len() > MAX_BUNDLE_FILES {
                return Err(format!("the folder has more than {MAX_BUNDLE_FILES} files"));
            }
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

/// `path` relative to `root`, `/`-separated, every component checked: no
/// control characters, no `:`, no Windows reserved device names (`con`,
/// `lpt1`, … — writing those "succeeds" in ways that are not files).
fn relative_path(root: &Path, path: &Path) -> Result<String, String> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| format!("{} escaped the folder being imported", path.display()))?;
    let mut parts = Vec::new();
    for component in relative.components() {
        let std::path::Component::Normal(part) = component else {
            return Err(format!(
                "{} has a path component that is not a plain name",
                relative.display()
            ));
        };
        let part = part
            .to_str()
            .ok_or_else(|| format!("{} has a non-UTF-8 file name", relative.display()))?;
        if part.chars().any(|c| c.is_control() || c == ':') {
            return Err(format!(
                "{part} has characters a skill file name cannot carry"
            ));
        }
        if is_reserved_device_name(part) {
            return Err(format!("{part} is a reserved Windows device name"));
        }
        parts.push(part);
    }
    Ok(parts.join("/"))
}

fn is_reserved_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).to_ascii_lowercase();
    if ["con", "prn", "aux", "nul"].contains(&stem.as_str()) {
        return true;
    }
    for prefix in ["com", "lpt"] {
        if let Some(rest) = stem.strip_prefix(prefix)
            && rest.len() == 1
            && rest.chars().all(|c| c.is_ascii_digit())
        {
            return true;
        }
    }
    false
}

// ------------------------------------------- what the card must say out loud

/// A lane of bundle content this app has no way to run.
///
/// A skill bundle can ship more than instructions. Skills written for other
/// hosts ship a `hooks/` folder — event handlers the *host* is supposed to
/// dispatch. `ironclaw_skills` has no hook concept at all: the crate does not
/// contain the word (verified against the pinned upstream `a492857`), and the
/// only thing the runtime does with a bundle beyond `SKILL.md` is put its path
/// in the prompt. So those files install, sit on disk, and never fire.
///
/// A user must never learn that by wondering why nothing happened, so the review
/// card says it before the yes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InertLane {
    /// What the lane is called, in the sentence ("hooks").
    pub lane: String,
    /// The bundle paths that fall in it.
    pub files: Vec<String>,
    /// The plain sentence the card shows.
    pub note: String,
}

/// One kind of automatic content, and the bundle path that marks it.
///
/// Adding a lane is one row: name the marker and what to call it in the
/// sentence. Matching, wording, and rendering are shared — which is the point,
/// because the next lane (agents? commands? MCP server manifests?) will be found
/// the same way this one was: by importing a real skill written for a host that
/// has one and noticing this app does not.
struct LaneRule {
    /// A top-level bundle directory, or a root file whose stem is this.
    marker: &'static str,
    /// What the lane is called in the sentence shown to the user.
    lane: &'static str,
}

/// Every lane known to install here and never run.
const INERT_LANES: &[LaneRule] = &[LaneRule {
    marker: "hooks",
    lane: "hooks",
}];

/// The bundle content that will not run here, each with the sentence to show.
/// Empty means everything in the bundle has somewhere to go.
pub fn inert_lanes(files: &[ImportFile]) -> Vec<InertLane> {
    let mut lanes = Vec::new();
    for rule in INERT_LANES {
        let matched: Vec<String> = files
            .iter()
            .filter(|file| in_lane(&file.path, rule.marker))
            .map(|file| file.path.clone())
            .collect();
        if matched.is_empty() {
            continue;
        }
        lanes.push(InertLane {
            lane: rule.lane.to_string(),
            files: matched,
            note: format!(
                "This skill's automatic parts ({}) will not run in this app; \
                 only the instructional parts will.",
                rule.lane
            ),
        });
    }
    lanes
}

/// Whether `path` (folder-relative, `/`-separated) belongs to `marker`'s lane:
/// anything under a top-level `<marker>/`, or a root file called `<marker>.*`.
fn in_lane(path: &str, marker: &str) -> bool {
    let first = path.split('/').next().unwrap_or(path);
    let stem = first.split('.').next().unwrap_or(first);
    stem.eq_ignore_ascii_case(marker)
}

/// The runtime's own rough token estimate: ~0.25 tokens per byte (~4 bytes per
/// token of English prose). Not a number of ours — it is the arithmetic in
/// `ironclaw_skills::selector::skill_token_cost`, which is what actually decides
/// whether a skill fits a turn.
pub const SKILL_TOKENS_PER_BYTE: f64 = 0.25;

/// The whole-turn budget every active skill competes for, under the profile we
/// run: `LOCAL_DEV_MAX_SKILL_CONTEXT_TOKENS` in `ironclaw_reborn_composition`.
/// Up to three skills share it, and one that does not fit what is left is
/// dropped — not truncated.
pub const SKILL_BUDGET_TOKENS: usize = 6000;

/// Where the review card starts warning: 8 KiB of injected body.
///
/// Not a round number picked for looking sensible. At the runtime's own 0.25
/// tokens/byte, 8 KiB of body is 2,048 tokens — the 2,000 of
/// `default_max_context_tokens()`, which is the cost the selector *assumes* a
/// skill has when it declares none. Past that point three things become true at
/// once: the skill costs more than it says, it takes a third of the 6,000-token
/// turn budget it shares with two others, and on the 16k window `ic_llama` gives
/// a small local model (`MIN_AGENT_CTX`) it is a visible slice of the context
/// before the user's own message is even added. Below it the cost is worth
/// showing and not worth a warning.
pub const CONTEXT_COST_WARN_BYTES: u64 = 8 * 1024;

/// What activating a skill costs, in the terms the runtime actually charges.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContextCost {
    /// The whole SKILL.md on disk.
    pub file_bytes: u64,
    /// The part that is actually injected: the body after the frontmatter, which
    /// is what the runtime keeps as `prompt_content` and what it charges for.
    pub body_bytes: u64,
    /// The runtime's own estimate of what that body costs in tokens.
    pub approx_tokens: usize,
    /// The per-turn budget it competes for.
    pub budget_tokens: usize,
    /// That cost as a percentage of the budget.
    pub budget_percent: u32,
    /// The sentence the card shows.
    pub summary: String,
    /// Set when the body is large enough to be worth a second thought.
    pub warning: Option<String>,
}

/// Price the skill the way the runtime will, for the card.
///
/// An imported skill is the **trusted tier**: its body is injected verbatim into
/// the system prompt on every activation (`format_skills` in the orchestrator
/// puts `content` between `<skill>` tags and nothing trims it). That is a
/// recurring cost paid on the user's context, and the moment to see it is the
/// moment of consent — not afterwards, when replies have quietly got worse.
pub fn context_cost(skill_md: &str) -> ContextCost {
    let body = injected_body(skill_md);
    let body_bytes = body.len() as u64;
    let approx_tokens = (body.len() as f64 * SKILL_TOKENS_PER_BYTE) as usize;
    let budget_percent =
        ((approx_tokens as u64 * 100) / SKILL_BUDGET_TOKENS as u64).min(u32::MAX as u64) as u32;
    let summary = format!(
        "Trusted tier: the full {} body is injected into the model's context on \
         activation — about {} tokens, {budget_percent}% of the {} the runtime \
         allows all active skills in one turn.",
        format_bytes(body_bytes),
        thousands(approx_tokens),
        thousands(SKILL_BUDGET_TOKENS),
    );
    let warning = (body_bytes > CONTEXT_COST_WARN_BYTES).then(|| {
        format!(
            "That is a large skill. It shares the turn's {} budget with up to two \
             others, so a skill this size can crowd them out — and on the 16k \
             window a small local model runs at, it is a sizeable slice of the \
             context before your own message is added.",
            thousands(SKILL_BUDGET_TOKENS)
        )
    });
    ContextCost {
        file_bytes: skill_md.len() as u64,
        body_bytes,
        approx_tokens,
        budget_tokens: SKILL_BUDGET_TOKENS,
        budget_percent,
        summary,
        warning,
    }
}

/// The part of a SKILL.md the runtime injects: everything after the closing
/// frontmatter fence. Falls back to the whole text when there is no frontmatter
/// — [`parse_skill_md`] has already refused that, so this is belt and braces.
fn injected_body(skill_md: &str) -> &str {
    skill_md
        .trim_start_matches('\n')
        .strip_prefix("---")
        .and_then(|rest| rest.split_once('\n').map(|(_, rest)| rest))
        .and_then(split_at_closing_fence)
        .map(|(_, body)| body)
        .unwrap_or(skill_md)
}

/// `1.2 KB`-style sizes, matching the dashboard's own `formatBytes`.
fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let kb = bytes as f64 / 1024.0;
    if kb < 1024.0 {
        return format!("{kb:.1} KB");
    }
    format!("{:.1} MB", kb / 1024.0)
}

/// `5,376` — a token count a person can read at a glance.
fn thousands(value: usize) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SKILL: &str = "---\nname: imported-skill\ndescription: A skill from elsewhere.\n---\n\n# Imported\n\nDo the thing.\n";

    fn folder_with_skill(dir: &Path) {
        std::fs::write(dir.join("SKILL.md"), SKILL).expect("write SKILL.md");
    }

    #[test]
    fn a_plain_folder_previews_with_its_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        folder_with_skill(dir.path());
        std::fs::create_dir_all(dir.path().join("data")).expect("mkdir");
        std::fs::write(dir.path().join("data").join("table.csv"), "a,b\n").expect("write");

        let preview = preview(dir.path()).expect("preview");
        assert_eq!(preview.name, "imported-skill");
        assert_eq!(preview.description, "A skill from elsewhere.");
        assert!(preview.skill_md.contains("Do the thing."));
        assert_eq!(preview.files.len(), 1);
        assert_eq!(preview.files[0].path, "data/table.csv");
    }

    #[test]
    fn crlf_is_normalized_before_review() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("SKILL.md"), SKILL.replace('\n', "\r\n")).expect("write");
        let preview = preview(dir.path()).expect("preview");
        assert!(!preview.skill_md.contains('\r'));
        assert_eq!(preview.name, "imported-skill");
    }

    #[test]
    fn a_folder_without_a_skill_md_says_so() {
        let dir = tempfile::tempdir().expect("tempdir");
        let error = preview(dir.path()).expect_err("no SKILL.md");
        assert!(error.contains("no SKILL.md"), "{error}");
    }

    #[test]
    fn an_invalid_skill_md_says_what_is_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("SKILL.md"), "just prose").expect("write");
        let error = preview(dir.path()).expect_err("invalid");
        assert!(error.contains("frontmatter"), "{error}");
    }

    /// The bug a real repo found: Anthropic's own `claude-api` skill writes its
    /// description as a YAML block scalar. A line scanner reads that as empty and
    /// refuses a skill the runtime installs happily.
    #[test]
    fn a_block_scalar_description_is_read_as_a_description() {
        let md = "---\nname: claude-api\ndescription: |-\n  First line of the description.\n  Second line, still the description.\nlicense: Complete terms in LICENSE.txt\n---\n\n# Body\n\nDo the thing.\n";
        let draft = parse_skill_md(md).expect("a block scalar is valid YAML");
        assert_eq!(draft.name, "claude-api");
        assert!(draft.description.starts_with("First line"), "{draft:?}");
        assert!(draft.description.contains("Second line"), "{draft:?}");
        // The text that installs is the file, untouched but for line endings.
        assert!(draft.content.contains("license: Complete terms"));
    }

    #[test]
    fn folded_and_quoted_descriptions_parse_too() {
        let folded = "---\nname: folded\ndescription: >-\n  one\n  two\n---\n\nBody.\n";
        assert_eq!(
            parse_skill_md(folded).expect("folded").description,
            "one two"
        );
        let quoted = "---\nname: quoted\ndescription: \"has: a colon\"\n---\n\nBody.\n";
        assert_eq!(
            parse_skill_md(quoted).expect("quoted").description,
            "has: a colon"
        );
    }

    /// The runtime's grammar is `[a-zA-Z0-9][a-zA-Z0-9._-]{0,63}` — wider than
    /// kebab-case. Refusing what the runtime accepts is a bug the user can't fix.
    #[test]
    fn the_name_grammar_is_the_runtimes_plus_a_windows_guard() {
        for name in ["Skill_Name", "skill.v2", "a", "A1-b_c.d"] {
            let md = format!("---\nname: {name}\ndescription: d\n---\n\nBody.\n");
            assert!(parse_skill_md(&md).is_ok(), "{name} should be accepted");
        }
        for name in ["-leading", "has space", "has/slash", "con", "lpt1", ""] {
            let md = format!("---\nname: \"{name}\"\ndescription: d\n---\n\nBody.\n");
            assert!(parse_skill_md(&md).is_err(), "{name} should be refused");
        }
        let long = "a".repeat(65);
        let md = format!("---\nname: {long}\ndescription: d\n---\n\nBody.\n");
        assert!(parse_skill_md(&md).is_err(), "65 characters is too long");
    }

    #[test]
    fn a_skill_md_over_the_runtimes_own_limit_says_which_limit() {
        let md = format!(
            "---\nname: huge\ndescription: d\n---\n\n{}",
            "x".repeat(MAX_SKILL_MD_BYTES as usize)
        );
        let error = parse_skill_md(&md).expect_err("over the cap");
        assert!(error.contains("64 KiB"), "{error}");
    }

    #[test]
    fn a_frontmatter_that_is_never_closed_says_so() {
        let error =
            parse_skill_md("---\nname: x\ndescription: d\n\nBody.\n").expect_err("unclosed");
        assert!(error.contains("never closed"), "{error}");
    }

    #[test]
    fn an_oversized_bundle_file_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        folder_with_skill(dir.path());
        std::fs::write(
            dir.path().join("big.bin"),
            vec![0u8; (MAX_BUNDLE_FILE_BYTES + 1) as usize],
        )
        .expect("write");
        let error = preview(dir.path()).expect_err("too big");
        assert!(error.contains("2 MiB"), "{error}");
    }

    #[test]
    fn a_reserved_device_name_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        folder_with_skill(dir.path());
        // `con.txt` cannot be created on Windows itself, so exercise the rule
        // directly: the validator is what the walk consults.
        assert!(is_reserved_device_name("con.txt"));
        assert!(is_reserved_device_name("LPT1"));
        assert!(!is_reserved_device_name("config.toml"));
        assert!(!is_reserved_device_name("console.md"));
    }

    /// The finding that earned this: a real third-party skill shipped a `hooks/`
    /// bundle, and `ironclaw_skills` has no hook concept — so the files install
    /// and never fire. The card has to say so before the yes.
    #[test]
    fn a_hooks_bundle_is_reported_as_inert() {
        let dir = tempfile::tempdir().expect("tempdir");
        folder_with_skill(dir.path());
        std::fs::create_dir_all(dir.path().join("hooks")).expect("mkdir");
        std::fs::write(dir.path().join("hooks").join("on-stop.py"), "print(1)").expect("write");
        std::fs::write(dir.path().join("hooks.json"), "{}").expect("write");
        std::fs::create_dir_all(dir.path().join("reference")).expect("mkdir");
        std::fs::write(dir.path().join("reference").join("notes.md"), "n").expect("write");

        let preview = preview(dir.path()).expect("preview");
        assert_eq!(preview.inert.len(), 1, "{:?}", preview.inert);
        let lane = &preview.inert[0];
        assert_eq!(lane.lane, "hooks");
        assert!(
            lane.note.contains("will not run in this app"),
            "{}",
            lane.note
        );
        // Both the folder and the root manifest are named; the reference file,
        // which is only ever read as instruction, is not.
        assert!(lane.files.contains(&"hooks/on-stop.py".to_string()));
        assert!(lane.files.contains(&"hooks.json".to_string()));
        assert!(!lane.files.iter().any(|file| file.contains("reference")));
    }

    #[test]
    fn a_bundle_the_runtime_can_use_reports_nothing_inert() {
        let dir = tempfile::tempdir().expect("tempdir");
        folder_with_skill(dir.path());
        std::fs::create_dir_all(dir.path().join("scripts")).expect("mkdir");
        std::fs::write(dir.path().join("scripts").join("run.py"), "x").expect("write");
        // A nested `hooks` is not a top-level lane — the runtime treats it as
        // ordinary bundle data, and so must the warning.
        std::fs::create_dir_all(dir.path().join("docs").join("hooks")).expect("mkdir");
        std::fs::write(dir.path().join("docs").join("hooks").join("why.md"), "d").expect("write");

        assert!(preview(dir.path()).expect("preview").inert.is_empty());
    }

    /// The cost is charged on the *body*, because that is what the runtime keeps
    /// as `prompt_content` and injects — the frontmatter is parsed, not sent.
    #[test]
    fn the_context_cost_prices_the_body_the_way_the_runtime_does() {
        let body = "x".repeat(4000);
        let md = format!("---\nname: sized\ndescription: d\n---\n\n{body}");
        let cost = context_cost(&md);
        assert!(cost.file_bytes > cost.body_bytes, "{cost:?}");
        // The runtime's own arithmetic: 0.25 tokens per byte of the body.
        assert_eq!(
            cost.approx_tokens,
            (cost.body_bytes as f64 * SKILL_TOKENS_PER_BYTE) as usize
        );
        assert_eq!(cost.budget_tokens, SKILL_BUDGET_TOKENS);
        assert!(cost.summary.contains("injected into the model's context"));
        assert!(cost.summary.contains("KB"), "{}", cost.summary);
        // 4 KB of body is a sixth of the budget: worth showing, not worth a warning.
        assert!(cost.warning.is_none(), "{cost:?}");
        assert!(
            cost.budget_percent > 0 && cost.budget_percent < 50,
            "{cost:?}"
        );
    }

    #[test]
    fn a_body_over_the_threshold_warns_about_the_budget_it_eats() {
        let md = format!(
            "---\nname: big\ndescription: d\n---\n\n{}",
            "x".repeat(CONTEXT_COST_WARN_BYTES as usize + 1)
        );
        let cost = context_cost(&md);
        let warning = cost.warning.expect("a body over the threshold warns");
        assert!(warning.contains("6,000"), "{warning}");
        assert!(warning.contains("crowd them out"), "{warning}");
        // The threshold is the runtime's default declaration, not a number of
        // ours: 8 KiB of body is 2,048 tokens at the runtime's own rate — the
        // 2,000 `default_max_context_tokens()` assumes when a skill declares
        // nothing, which is the point past which a skill costs more than it says.
        assert_eq!(
            (CONTEXT_COST_WARN_BYTES as f64 * SKILL_TOKENS_PER_BYTE) as usize,
            2048
        );
    }

    #[test]
    fn sizes_and_counts_read_like_a_person_wrote_them() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(21 * 1024), "21.0 KB");
        assert_eq!(format_bytes(3 * 1024 * 1024), "3.0 MB");
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(5376), "5,376");
        assert_eq!(thousands(1234567), "1,234,567");
    }

    #[test]
    fn install_writes_the_reviewed_text_not_the_folder() {
        let dir = tempfile::tempdir().expect("tempdir");
        folder_with_skill(dir.path());
        let root = tempfile::tempdir().expect("root");

        // The folder changes after review; the reviewed text is what installs.
        let reviewed = preview(dir.path()).expect("preview").skill_md;
        std::fs::write(
            dir.path().join("SKILL.md"),
            "---\nname: swapped\ndescription: x\n---\n\nSwapped.\n",
        )
        .expect("swap");

        let name = install(dir.path(), &reviewed, root.path()).expect("install");
        assert_eq!(name, "imported-skill");
        let written = std::fs::read_to_string(root.path().join("imported-skill").join("SKILL.md"))
            .expect("read back");
        assert!(written.contains("Do the thing."));
        assert!(!written.contains("Swapped."));
    }

    #[test]
    fn install_copies_the_bundle_preserving_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        folder_with_skill(dir.path());
        std::fs::create_dir_all(dir.path().join("ref").join("deep")).expect("mkdir");
        std::fs::write(dir.path().join("ref").join("deep").join("notes.md"), "n").expect("write");
        let root = tempfile::tempdir().expect("root");

        let reviewed = preview(dir.path()).expect("preview").skill_md;
        install(dir.path(), &reviewed, root.path()).expect("install");
        assert!(
            root.path()
                .join("imported-skill")
                .join("ref")
                .join("deep")
                .join("notes.md")
                .exists()
        );
    }

    #[test]
    fn an_existing_skill_is_never_overwritten_and_a_failure_leaves_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        folder_with_skill(dir.path());
        let root = tempfile::tempdir().expect("root");
        let reviewed = preview(dir.path()).expect("preview").skill_md;

        install(dir.path(), &reviewed, root.path()).expect("first install");
        let error = install(dir.path(), &reviewed, root.path()).expect_err("refuse");
        assert!(error.contains("already exists"), "{error}");

        // A bundle that breaks mid-copy removes the half-written directory.
        std::fs::write(
            dir.path().join("grown.bin"),
            vec![0u8; (MAX_BUNDLE_FILE_BYTES + 1) as usize],
        )
        .expect("write");
        let reviewed_two = reviewed.replace("imported-skill", "second-skill");
        assert!(install(dir.path(), &reviewed_two, root.path()).is_err());
        assert!(
            !root.path().join("second-skill").exists(),
            "half a skill is worse than none"
        );
    }
}
