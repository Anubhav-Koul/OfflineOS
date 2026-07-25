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
//!   would have allowed.
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
