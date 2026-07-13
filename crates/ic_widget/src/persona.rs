//! Teaching the agent its own name, and the user's.
//!
//! The names the setup wizard collects are not labels the UI paints on — the
//! agent actually knows them. Reborn's system prompt is a **file on disk**,
//! `<reborn-home>/local-dev/system/prompts/default-system.md`, and
//! `DefaultSystemPromptIdentitySource::prompt_content` re-reads it on *every run*
//! (not once at boot), so a rewrite takes effect on the next turn with no gateway
//! restart. This module owns that file's persona section.
//!
//! ## Why a marker block and not a rewrite
//!
//! The file is not ours. The runtime seeds it from an embedded default that
//! carries real instructions (response style, tool-continuation behavior), and the
//! user may edit it. So we do not overwrite it — we maintain one delimited block
//! at the top and leave every other byte alone:
//!
//! ```text
//! <!-- ic:persona:start -->
//! You are Nova, ...
//! <!-- ic:persona:end -->
//! ```
//!
//! Writing is therefore idempotent: applying twice replaces the block rather than
//! stacking a second copy, which is what a naive prepend would do on every launch
//! until the 64 KiB ceiling killed the prompt.

use std::path::{Path, PathBuf};

use crate::settings::Settings;

/// Opens the block we own. Everything between this and [`END`] is ours to
/// replace; everything outside it is the runtime's or the user's.
const START: &str = "<!-- ic:persona:start -->";
/// Closes the block we own.
const END: &str = "<!-- ic:persona:end -->";

/// The runtime rejects a system prompt above this, so a persona that would push
/// the file past it must not be written — a too-large file makes the identity
/// source unavailable and the agent loses its *whole* system prompt, not just the
/// persona.
const MAX_PROMPT_BYTES: usize = 64 * 1024;

/// The system-prompt file for a given reborn home.
pub fn prompt_path(reborn_home: &Path) -> PathBuf {
    reborn_home
        .join("local-dev")
        .join("system")
        .join("prompts")
        .join("default-system.md")
}

/// The persona text for these settings, or `None` when there is nothing to say.
///
/// Both names are optional and independent: a user who names the assistant but
/// not themselves still gets a persona, and vice versa.
fn persona_text(settings: &Settings) -> Option<String> {
    let assistant = settings.assistant_name.trim();
    let user = settings.user_name.trim();
    if assistant.is_empty() && user.is_empty() {
        return None;
    }

    let mut lines = Vec::new();
    if !assistant.is_empty() {
        lines.push(format!(
            "Your name is {assistant}. The user chose it for you. Answer to it, and refer to \
             yourself as {assistant} when you need to name yourself."
        ));
    }
    if !user.is_empty() {
        lines.push(format!(
            "The user's name is {user}. Address them as {user} when it is natural to do so; do \
             not force it into every message."
        ));
    }
    // The character is a desktop companion, not a terminal. Reply length matters
    // more here than in a chat window, because a reply may be *spoken* aloud or
    // shown in a small speech bubble beside the character.
    lines.push(
        "You are a companion character standing on the user's desktop. Your replies may be read \
         aloud or shown in a small speech bubble, so keep them short and conversational unless \
         the user asks for detail."
            .to_string(),
    );
    Some(lines.join("\n\n"))
}

/// Replace (or insert, or remove) our persona block in `content`.
///
/// Pure, so the awkward cases are testable without a filesystem: no block yet, a
/// block already there, a start marker with no end (a half-written file from a
/// crash — we refuse to guess and leave the file alone), and a persona that has
/// been cleared.
fn apply_block(content: &str, persona: Option<&str>) -> Result<String, String> {
    let existing = match (content.find(START), content.find(END)) {
        (Some(start), Some(end)) if end > start => Some((start, end + END.len())),
        (Some(_), Some(_)) => {
            return Err("the persona markers in the system prompt are out of order".to_string());
        }
        (Some(_), None) | (None, Some(_)) => {
            return Err(
                "the system prompt has an unpaired persona marker; leaving it alone".to_string(),
            );
        }
        (None, None) => None,
    };

    let block = persona.map(|persona| format!("{START}\n{persona}\n{END}"));

    let updated = match (existing, block) {
        // Replace what is there.
        (Some((start, end)), Some(block)) => {
            format!("{}{block}{}", &content[..start], &content[end..])
        }
        // Nothing to say any more: drop the block and the blank line after it.
        (Some((start, end)), None) => {
            let rest = content[end..].trim_start_matches(['\n', '\r']);
            format!("{}{rest}", &content[..start])
        }
        // First time: the persona goes *first*, ahead of the runtime's default, so
        // the model reads who it is before how to behave.
        (None, Some(block)) => format!("{block}\n\n{content}"),
        (None, None) => content.to_string(),
    };

    if updated.len() > MAX_PROMPT_BYTES {
        return Err(format!(
            "the system prompt would be {} bytes, over the {MAX_PROMPT_BYTES}-byte limit",
            updated.len()
        ));
    }
    Ok(updated)
}

/// Write the persona for `settings` into the gateway's system prompt.
///
/// Best-effort and non-fatal: a missing prompt file simply means the gateway has
/// not seeded it yet (it does so at boot), and an agent with no persona still
/// works. Never fails the launch.
///
/// Call this **after** the gateway is up — the file must exist first, or we would
/// create it ourselves and the runtime's `seed` (which only writes when the file is
/// missing) would never lay down its default instructions.
pub fn apply(reborn_home: &Path, settings: &Settings) {
    let path = prompt_path(reborn_home);
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "no system prompt file yet; the agent has no persona this launch");
            return;
        }
    };

    let persona = persona_text(settings);
    let updated = match apply_block(&content, persona.as_deref()) {
        Ok(updated) => updated,
        Err(reason) => {
            tracing::warn!(%reason, "leaving the system prompt unchanged");
            return;
        }
    };

    if updated == content {
        return;
    }

    // Atomic: a crash mid-write must not leave the agent with half a system
    // prompt. Same discipline as `SettingsStore`.
    let temp = path.with_extension("md.tmp");
    if let Err(error) = std::fs::write(&temp, &updated) {
        tracing::warn!(%error, "could not stage the system prompt");
        return;
    }
    if let Err(error) = std::fs::rename(&temp, &path) {
        tracing::warn!(%error, "could not install the system prompt");
        let _ = std::fs::remove_file(&temp);
        return;
    }
    tracing::info!(
        assistant = %settings.assistant_name,
        user = %settings.user_name,
        "the agent's persona is live"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(assistant: &str, user: &str) -> Settings {
        Settings {
            assistant_name: assistant.to_string(),
            user_name: user.to_string(),
            ..Settings::default()
        }
    }

    #[test]
    fn a_fresh_prompt_gets_the_persona_ahead_of_the_runtime_default() {
        let base = "You are IronClaw Agent, a secure autonomous assistant.\n";
        let persona = persona_text(&settings("Nova", "Anubhav")).expect("a persona");

        let updated = apply_block(base, Some(&persona)).expect("applies");

        assert!(updated.starts_with(START));
        assert!(updated.contains("Your name is Nova"));
        assert!(updated.contains("The user's name is Anubhav"));
        // The runtime's own instructions survive.
        assert!(updated.contains("secure autonomous assistant"));
    }

    /// The regression a naive prepend would cause: a second copy of the persona on
    /// every launch, growing until it blows the 64 KiB ceiling and the agent loses
    /// its entire system prompt.
    #[test]
    fn applying_twice_replaces_the_block_rather_than_stacking_it() {
        let base = "runtime default\n";
        let first = apply_block(
            base,
            Some(&persona_text(&settings("Nova", "Anubhav")).unwrap()),
        )
        .unwrap();
        let second = apply_block(
            &first,
            Some(&persona_text(&settings("Aria", "Anubhav")).unwrap()),
        )
        .unwrap();

        assert_eq!(second.matches(START).count(), 1, "{second}");
        assert!(second.contains("Your name is Aria"));
        assert!(!second.contains("Your name is Nova"), "the old name lingers");
        assert!(second.contains("runtime default"));
    }

    #[test]
    fn clearing_the_names_removes_the_block_and_leaves_the_rest() {
        let base = "runtime default\n";
        let with = apply_block(
            base,
            Some(&persona_text(&settings("Nova", "Anubhav")).unwrap()),
        )
        .unwrap();

        let without = apply_block(&with, None).expect("applies");

        assert!(!without.contains(START));
        assert_eq!(without, base);
    }

    /// A half-written file (crash between the two markers) is ambiguous. Guessing
    /// where the block ends could delete the runtime's instructions, so we refuse.
    #[test]
    fn an_unpaired_marker_is_refused_rather_than_guessed_at() {
        let broken = format!("{START}\nhalf a persona\nruntime default\n");
        let error = apply_block(&broken, Some("anything")).expect_err("must refuse");
        assert!(error.contains("unpaired"), "{error}");
    }

    #[test]
    fn no_names_means_no_persona() {
        assert!(persona_text(&settings("", "")).is_none());
    }

    /// Each name stands alone — naming only the assistant still teaches it its name.
    #[test]
    fn one_name_is_enough_to_produce_a_persona() {
        let only_assistant = persona_text(&settings("Nova", "")).expect("a persona");
        assert!(only_assistant.contains("Your name is Nova"));
        assert!(!only_assistant.contains("The user's name is"));

        let only_user = persona_text(&settings("", "Anubhav")).expect("a persona");
        assert!(only_user.contains("The user's name is Anubhav"));
        assert!(!only_user.contains("Your name is"));
    }

    #[test]
    fn a_persona_that_would_overflow_the_prompt_is_refused() {
        let huge = "x".repeat(MAX_PROMPT_BYTES);
        let error = apply_block("base\n", Some(&huge)).expect_err("must refuse");
        assert!(error.contains("over the"), "{error}");
    }

    #[test]
    fn the_prompt_path_is_the_file_the_runtime_reads() {
        let path = prompt_path(Path::new("C:\\data\\reborn"));
        assert!(path.ends_with(Path::new("local-dev/system/prompts/default-system.md")));
    }
}
