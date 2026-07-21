//! Listing and removing the user's installed skills (Phase 8c).
//!
//! ## Why this is buildable when the "skills list" panel was marked unavailable
//!
//! `docs/desktop/dashboard-gaps.md` listed a skills browser as impossible,
//! reasoning that skills are "an in-agent tool, not an HTTP route". The 8c
//! VERIFY corrected that: user skills are **plain files on disk** at
//! `<reborn-home>/local-dev/skills/<name>/SKILL.md` — the exact directory this
//! widget already *writes* when it installs a reflection draft (7b) or a folder
//! import (7c), and which the `skill_install` gate proved the runtime reads back
//! after a restart. Reading a directory the widget co-owns is not the
//! "couple to the gateway's private libSQL DB" that memory/audit would require
//! (those stay unavailable — see the 8c notes); it needs no HTTP route, no core
//! change, and no LLM turn.
//!
//! ## What this does and does not list
//!
//! Only **user-installed** skills — the ones under `local-dev/skills`. The
//! runtime's own *bundled* skills (code-review, …) live in a separate,
//! runtime-managed `local-dev/system/skills` tree with an install lock and
//! stale-removal; that directory is the gateway's to own, so we leave it alone.
//! The panel is honest about this: it shows what the user (or the agent, with
//! the user's consent) added, which is exactly the set the user can prune.
//!
//! Symmetry with [`crate::skill_import`]: install writes a directory here, so
//! [`remove`] deletes one — the same root, the same ownership. A skill is not
//! LLM data (it is user-authored procedure/config), so removing one the user
//! chose to remove does not touch the never-delete invariant.

use std::path::Path;

use serde::Serialize;

use crate::ambient::reflection;

/// One installed skill, as shown in the Skills panel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstalledSkill {
    /// The directory name — the identity the runtime activates by, and the
    /// ground truth even when a hand-placed `SKILL.md` disagrees with it.
    pub name: String,
    /// The frontmatter `description:`, or empty when the `SKILL.md` could not be
    /// parsed (`valid` is then false).
    pub description: String,
    /// Whether `SKILL.md` parsed with valid frontmatter. A malformed skill is
    /// still listed — it is on disk and the user may want to remove it — but
    /// flagged rather than shown with a blank description that looks like a bug.
    pub valid: bool,
    /// Number of files in the skill directory (including `SKILL.md`).
    pub files: usize,
    /// Total size of the skill directory on disk, in bytes.
    pub bytes: u64,
}

/// List the user's installed skills, sorted by name.
///
/// A missing skills root is not an error — it means nothing has been installed
/// yet, so the answer is an empty list. Only directories that actually contain
/// a `SKILL.md` are skills; anything else under the root is ignored.
pub fn list(skills_root: &Path) -> Result<Vec<InstalledSkill>, String> {
    let entries = match std::fs::read_dir(skills_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(format!(
                "could not read the skills directory {}: {error}",
                skills_root.display()
            ));
        }
    };

    let mut skills = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("could not read a skills entry: {error}"))?;
        let file_type = entry
            .file_type()
            .map_err(|error| format!("could not inspect {}: {error}", entry.path().display()))?;
        if !file_type.is_dir() {
            continue;
        }
        let dir = entry.path();
        let skill_md = dir.join("SKILL.md");
        if !skill_md.is_file() {
            continue; // a directory here without a SKILL.md is not a skill
        }
        // The directory name is the identity; a non-UTF-8 name cannot be one the
        // widget or the runtime installed, so skip it rather than guess.
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };

        let (description, valid) = match std::fs::read_to_string(&skill_md) {
            Ok(text) => match reflection::parse_skill_md(&text.replace("\r\n", "\n")) {
                Some(draft) => (draft.description, true),
                None => (String::new(), false),
            },
            Err(_) => (String::new(), false),
        };
        let (files, bytes) = dir_stats(&dir);

        skills.push(InstalledSkill {
            name,
            description,
            valid,
            files,
            bytes,
        });
    }

    skills.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(skills)
}

/// Remove one installed skill by name.
///
/// `name` must be a single plain path component naming a directory directly
/// under the skills root — never a traversal (`..`), an absolute path, or
/// anything with a separator. This is the one write here, and it deletes only a
/// direct child directory of the root the widget already owns.
pub fn remove(skills_root: &Path, name: &str) -> Result<(), String> {
    let plain = plain_component(name)?;
    let dest = skills_root.join(&plain);
    // Belt and suspenders: after joining, the parent must be exactly the root.
    // A `plain` that slipped a separator past the component check would land
    // elsewhere and this catches it before any deletion.
    if dest.parent() != Some(skills_root) {
        return Err(format!("{name} is not a skill in this directory"));
    }
    if !dest.is_dir() {
        return Err(format!(
            "there is no installed skill named \u{201c}{name}\u{201d}"
        ));
    }
    std::fs::remove_dir_all(&dest)
        .map_err(|error| format!("could not remove the skill \u{201c}{name}\u{201d}: {error}"))?;
    tracing::info!(skill = %plain, "removed an installed skill at the user's request");
    Ok(())
}

/// Validate that `name` is a single, plain path component and return it.
fn plain_component(name: &str) -> Result<String, String> {
    if name.is_empty() {
        return Err("a skill name is required".to_string());
    }
    let mut components = Path::new(name).components();
    let (Some(std::path::Component::Normal(only)), None) = (components.next(), components.next())
    else {
        return Err(format!("{name} is not a plain skill name"));
    };
    let only = only
        .to_str()
        .ok_or_else(|| format!("{name} is not a valid skill name"))?;
    if only != name || name.chars().any(|c| c.is_control()) {
        return Err(format!("{name} is not a plain skill name"));
    }
    Ok(only.to_string())
}

/// Count files and sum bytes under `dir`, recursively. Best-effort: an entry we
/// cannot stat contributes nothing rather than failing the whole listing — a
/// size is a nicety, and a skill with an unreadable ride-along file is still a
/// skill the user should see and be able to remove.
fn dir_stats(dir: &Path) -> (usize, u64) {
    let mut files = 0usize;
    let mut bytes = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                stack.push(entry.path());
            } else if file_type.is_file() {
                files += 1;
                if let Ok(metadata) = entry.metadata() {
                    bytes += metadata.len();
                }
            }
            // Symlinks are neither followed nor counted: an installed skill
            // holds plain files (the import path refuses symlinks), so one here
            // is not part of the skill's real footprint.
        }
    }
    (files, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, name: &str, description: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n\n# {name}\n\nDo it.\n"),
        )
        .expect("write SKILL.md");
    }

    #[test]
    fn an_absent_root_is_an_empty_list_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("does-not-exist");
        assert_eq!(list(&root).expect("empty"), Vec::new());
    }

    #[test]
    fn skills_are_listed_sorted_with_their_descriptions() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_skill(dir.path(), "zebra-skill", "The last one.");
        write_skill(dir.path(), "alpha-skill", "The first one.");

        let skills = list(dir.path()).expect("list");
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0].name, "alpha-skill");
        assert_eq!(skills[0].description, "The first one.");
        assert!(skills[0].valid);
        assert!(skills[0].files >= 1);
        assert!(skills[0].bytes > 0);
        assert_eq!(skills[1].name, "zebra-skill");
    }

    #[test]
    fn a_directory_without_a_skill_md_is_not_a_skill() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("just-a-folder")).expect("mkdir");
        write_skill(dir.path(), "real-skill", "Real.");
        let skills = list(dir.path()).expect("list");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "real-skill");
    }

    #[test]
    fn a_malformed_skill_md_is_listed_but_flagged_invalid() {
        let dir = tempfile::tempdir().expect("tempdir");
        let broken = dir.path().join("broken-skill");
        std::fs::create_dir_all(&broken).expect("mkdir");
        std::fs::write(broken.join("SKILL.md"), "not a skill, just prose").expect("write");

        let skills = list(dir.path()).expect("list");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "broken-skill");
        assert!(!skills[0].valid);
        assert_eq!(skills[0].description, "");
    }

    #[test]
    fn bundle_files_count_toward_the_footprint() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_skill(dir.path(), "with-data", "Has a data file.");
        std::fs::create_dir_all(dir.path().join("with-data").join("data")).expect("mkdir");
        std::fs::write(
            dir.path().join("with-data").join("data").join("t.csv"),
            "a,b\n",
        )
        .expect("write");

        let skills = list(dir.path()).expect("list");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].files, 2); // SKILL.md + data/t.csv
    }

    #[test]
    fn remove_deletes_the_named_skill_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_skill(dir.path(), "keep-me", "Keep.");
        write_skill(dir.path(), "remove-me", "Remove.");

        remove(dir.path(), "remove-me").expect("remove");
        assert!(!dir.path().join("remove-me").exists());
        assert!(dir.path().join("keep-me").exists());
        assert_eq!(list(dir.path()).expect("list").len(), 1);
    }

    #[test]
    fn remove_refuses_a_missing_skill() {
        let dir = tempfile::tempdir().expect("tempdir");
        let error = remove(dir.path(), "never-installed").expect_err("missing");
        assert!(error.contains("no installed skill"), "{error}");
    }

    #[test]
    fn remove_refuses_a_traversal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("skills");
        std::fs::create_dir_all(&root).expect("mkdir");
        // A sibling of the root that a traversal would reach.
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).expect("mkdir");

        for evil in ["..", "../outside", "..\\outside", "/etc", "a/b"] {
            let error = remove(&root, evil).expect_err(evil);
            assert!(error.contains("plain skill name"), "{evil}: {error}");
        }
        assert!(
            outside.exists(),
            "a traversal must not delete outside the root"
        );
    }
}
