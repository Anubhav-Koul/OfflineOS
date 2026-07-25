//! Self-learning skills (Phase 7b): the reflection turn, its draft, and the
//! consent-gated install.
//!
//! After a *user-initiated* run completes, one reflection turn runs on the
//! ambient thread: "did this teach a reusable procedure?" A yes becomes a draft
//! SKILL.md surfaced through the same [`super::AmbientService::propose`] path as
//! every other suggestion — rate-capped, quiet-hour-bounded, shown once ever
//! (the guardrail's exact-key memory is what "a rejected draft is never
//! re-proposed" is made of; `skill:<name>` has no per-run component on purpose).
//!
//! Three facts, all pinned by the `skill_install` gate against a running
//! gateway, shape this module:
//!
//! - **Install is plain files.** A skill under
//!   `<reborn home>/local-dev/skills/<name>/SKILL.md` is listed, activatable,
//!   and — because a user-placed skill is the trusted tier — fully injected on
//!   activation. So an approved draft is installed by a validated file write:
//!   deterministic, no LLM in the loop for an action the user already approved.
//! - **The runtime never prompts before a tool runs** (`PermissionMode::Ask` is
//!   enforced by nothing — the Phase 4 finding, confirmed for `skill_install`).
//!   The consent gate is therefore *ours*: a draft is never installed by code
//!   until the user answers the bubble, and the bubble defaults to No.
//! - **The reflection turn itself runs with the agent's full tool surface**, so
//!   a model could disobey "do NOT install anything" and call
//!   `builtin__skill_install` mid-reflection — and no gate would stop it. That
//!   cannot be prevented from here; it *is* detected: [`reflect`] snapshots the
//!   skills root around the turn and logs loudly when the agent self-installs.
//!
//! Everything here fails closed: an unparseable reply, an unreadable skills
//! root, a name that is already taken — each declines to propose rather than
//! guessing.

use std::collections::HashSet;
use std::path::Path;

use uuid::Uuid;

use crate::gateway_client::{MessageKind, RunPhase, ThreadId, Timeline};

use super::guardrail::Suppression;
use super::{AmbientService, Suggestion, SuggestionKind};

/// The default cap on skills learned this way. A companion that has taught
/// itself hundreds of procedures is a context-budget problem wearing a
/// self-improvement costume.
pub const DEFAULT_MAX_LEARNED: usize = 50;

/// The most transcript the reflection prompt will carry, in characters. The
/// tail is kept — the task that just finished is at the end.
const MAX_TRANSCRIPT_CHARS: usize = 6_000;

/// The most a draft SKILL.md may weigh. Far under the runtime's own 2 MiB
/// per-file bundle cap; a skill near this size is not a procedure, it is a dump.
const MAX_DRAFT_BYTES: usize = 64 * 1024;

/// How many timeline messages the transcript is read from.
const TRANSCRIPT_MESSAGES: u32 = 12;

// ---------------------------------------------------------------- run watch

/// Which chat runs have earned a reflection turn.
///
/// The projection stream repeats a run's status on every poll, and a snapshot
/// replays it on every (re)connect — so "completed" is a *level*, not an edge.
/// This watch fires only on a run it first saw **in flight**: a run that is
/// already terminal when first observed (an old run replayed by a snapshot, a
/// thread switched into after the fact) is history, not news. The same shape as
/// the automations watcher's priming rule.
#[derive(Debug, Default)]
pub struct RunWatch {
    /// Runs seen in a non-terminal phase, awaiting their edge.
    in_flight: HashSet<String>,
    /// Runs already fired on, or terminal at first sight — never fire again.
    done: HashSet<String>,
}

impl RunWatch {
    /// A fresh watch that has seen nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// Observe one run-status item. Returns `true` exactly once per run: when a
    /// run this watch saw in flight first reaches [`RunPhase::Completed`].
    /// Failed, cancelled, and killed runs never fire — a task that did not
    /// finish taught nothing worth keeping.
    pub fn observe(&mut self, run_id: &str, phase: &RunPhase) -> bool {
        if self.done.contains(run_id) {
            return false;
        }
        if !phase.is_terminal() {
            // Bound the in-flight set: forgetting a live run only costs a
            // possible reflection, never a duplicate one.
            if self.in_flight.len() >= 512 {
                self.in_flight.clear();
            }
            self.in_flight.insert(run_id.to_string());
            return false;
        }
        let was_in_flight = self.in_flight.remove(run_id);
        self.done.insert(run_id.to_string());
        was_in_flight && *phase == RunPhase::Completed
    }
}

// ---------------------------------------------------------------- the draft

/// A draft skill parsed out of the agent's reflection reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    /// The frontmatter `name:`, validated kebab-case.
    pub name: String,
    /// The frontmatter `description:`.
    pub description: String,
    /// The full SKILL.md text, exactly what an approval installs.
    pub content: String,
}

/// Pull a draft SKILL.md out of a reflection reply, or decline.
///
/// Accepts the draft inside a fenced code block (the prompt asks for one) or as
/// the whole reply; anything else — "NO", prose, a block without frontmatter, a
/// name that could not be a directory — is `None`. **Fail closed:** an
/// unparseable reply means nothing is proposed, never a guess.
pub fn parse_draft(reply: &str) -> Option<Draft> {
    // The whole reply is always the last candidate: a bare SKILL.md whose *body*
    // contains fenced code blocks would otherwise be shredded into those inner
    // blocks (none of which is a skill) and never tried whole.
    let mut candidates = fenced_blocks(reply);
    candidates.push(reply.trim().to_string());
    candidates
        .into_iter()
        .find_map(|candidate| candidate_draft(&candidate))
}

/// Validate one complete SKILL.md text — no fence extraction, the file *is* the
/// candidate.
///
/// **Not what the import path uses.** These rules are for a *model's reply*, and
/// their narrowness is the fail-closed posture 7b wants. A third-party file gets
/// [`crate::skill_import::parse_skill_md`] instead, which applies the runtime's
/// own YAML rules — 8e found that this scanner refuses real skills (a
/// `description: |-` block scalar reads as absent to it).
pub fn parse_skill_md(content: &str) -> Option<Draft> {
    candidate_draft(content.trim())
}

/// The one candidate → draft rule shared by both entry points.
fn candidate_draft(candidate: &str) -> Option<Draft> {
    let candidate = candidate.trim();
    if candidate.len() > MAX_DRAFT_BYTES {
        return None;
    }
    let (front, body) = split_frontmatter(candidate)?;
    if body.trim().is_empty() {
        return None; // a skill with no body is a name, not a procedure
    }
    let name = frontmatter_value(front, "name")?;
    let description = frontmatter_value(front, "description")?;
    if !valid_skill_name(&name) || description.is_empty() || description.len() > 1_000 {
        return None;
    }
    Some(Draft {
        name,
        description,
        content: candidate.to_string(),
    })
}

/// The contents of every ``` fenced block in `text`, in order.
fn fenced_blocks(text: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            match current.take() {
                Some(block) => blocks.push(block),
                None => current = Some(String::new()),
            }
            continue;
        }
        if let Some(block) = current.as_mut() {
            block.push_str(line);
            block.push('\n');
        }
    }
    blocks
}

/// Split `--- … ---` frontmatter from the body. `None` when there is none.
fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix("---")?;
    let rest = rest
        .strip_prefix("\r\n")
        .or_else(|| rest.strip_prefix('\n'))?;
    // The closing fence is a `---` on its own line.
    for index in rest.match_indices('\n').map(|(index, _)| index) {
        let before = &rest[..index];
        if before.lines().last().map(str::trim) == Some("---") {
            let close = before.rfind("---").expect("just matched");
            return Some((&rest[..close], &rest[index + 1..]));
        }
    }
    if rest.lines().last().map(str::trim) == Some("---") {
        let close = rest.rfind("---").expect("just matched");
        return Some((&rest[..close], ""));
    }
    None
}

/// A top-level `key: value` scalar out of frontmatter. Quotes are stripped.
fn frontmatter_value(front: &str, key: &str) -> Option<String> {
    front.lines().find_map(|line| {
        // Top-level keys only: an indented `name:` under `activation:` is not
        // the skill's name.
        if line.starts_with([' ', '\t']) {
            return None;
        }
        let (found, value) = line.split_once(':')?;
        if found.trim() != key {
            return None;
        }
        let value = value.trim().trim_matches('"').trim_matches('\'').trim();
        (!value.is_empty()).then(|| value.to_string())
    })
}

/// Whether `name` is a safe skill directory name: kebab-case, bounded, and not
/// a Windows reserved device name (`con`, `lpt1`, …— `create_dir` on those
/// fails in ways that read like success).
fn valid_skill_name(name: &str) -> bool {
    let kebab = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    if !kebab {
        return false;
    }
    const RESERVED: &[&str] = &["con", "prn", "aux", "nul"];
    if RESERVED.contains(&name) {
        return false;
    }
    for prefix in ["com", "lpt"] {
        if let Some(rest) = name.strip_prefix(prefix)
            && rest.len() == 1
            && rest.chars().all(|c| c.is_ascii_digit())
        {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------- the skills root

/// The names of skills installed in the user skills root — directories holding
/// a `SKILL.md`. An unreadable root reads as empty: every caller treats "is it
/// installed?" as a reason to *decline*, so the safe answer to "cannot tell" is
/// the one that proposes and installs nothing extra downstream.
pub fn installed_skills(skills_root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(skills_root) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().to_str()?.to_string();
            entry.path().join("SKILL.md").exists().then_some(name)
        })
        .collect();
    names.sort();
    names
}

/// How many self-learned skills are installed right now: the accepted
/// `reflection:<name>` sources that still have a directory on disk. Removing a
/// skill frees its slot; the log alone would count ghosts forever.
pub fn learned_count(accepted_reflection_sources: &HashSet<String>, installed: &[String]) -> usize {
    accepted_reflection_sources
        .iter()
        .filter_map(|source| source.strip_prefix("reflection:"))
        .filter(|name| installed.iter().any(|installed| installed == name))
        .count()
}

// ---------------------------------------------------------------- the turn

/// How a reflection turn ended. Every arm short of `Proposed` is a deliberate
/// silence, each for its own reason — worth telling apart in logs and gates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reflection {
    /// A draft was surfaced to the user.
    Proposed {
        /// The drafted skill's name.
        name: String,
    },
    /// The agent said the task taught nothing reusable (or its reply did not
    /// parse as a draft, which fails closed to the same place).
    NothingToLearn,
    /// The reflection turn itself produced no reply.
    NoReply,
    /// The self-learned cap is spent.
    AtCap {
        /// The cap that is spent.
        max: usize,
    },
    /// The drafted name is already installed.
    AlreadyInstalled {
        /// The name that collided.
        name: String,
    },
    /// The guardrail said no (rate cap, quiet hours, shown before, …).
    Suppressed(Suppression),
    /// The chat transcript could not be read.
    TranscriptUnavailable,
}

/// One full reflection turn: cap → transcript → ask → parse → dedupe → propose.
///
/// `chat_thread` is the conversation the completed task lives in; the turn
/// itself runs on `ambient_thread`, so a question the user never asked stays
/// out of their transcript. The caller has already checked the toggles — this
/// function assumes it should try.
pub async fn reflect(
    service: &AmbientService,
    ambient_thread: &ThreadId,
    chat_thread: &ThreadId,
    skills_root: &Path,
    max_learned: usize,
) -> Reflection {
    // The cap first: it needs no LLM turn to answer.
    let accepted = service.accepted_sources_with_prefix("reflection:").await;
    let installed = installed_skills(skills_root);
    if learned_count(&accepted, &installed) >= max_learned {
        return Reflection::AtCap { max: max_learned };
    }

    let transcript = match service
        .client()
        .timeline(chat_thread, Some(TRANSCRIPT_MESSAGES))
        .await
    {
        Ok(timeline) => render_transcript(&timeline, MAX_TRANSCRIPT_CHARS),
        Err(error) => {
            tracing::warn!(%error, "reflection could not read the chat transcript");
            return Reflection::TranscriptUnavailable;
        }
    };
    if transcript.trim().is_empty() {
        return Reflection::TranscriptUnavailable;
    }

    let prompt = reflection_prompt(&transcript, &installed);
    let before: HashSet<String> = installed.iter().cloned().collect();
    let reply = service.ask(ambient_thread, &prompt).await;

    // The turn ran with the agent's full tool surface and no runtime gate
    // (Phase 4). Detect the disobedient case — an install that skipped consent.
    let after = installed_skills(skills_root);
    for name in after.iter().filter(|name| !before.contains(*name)) {
        tracing::error!(
            skill = %name,
            "the agent installed a skill during reflection, bypassing consent — \
             the runtime enforces no tool gate; leaving it in place but flagging it"
        );
    }

    let Some(reply) = reply else {
        return Reflection::NoReply;
    };
    let Some(draft) = parse_draft(&reply) else {
        return Reflection::NothingToLearn;
    };
    if after.contains(&draft.name) {
        return Reflection::AlreadyInstalled { name: draft.name };
    }

    let name = draft.name.clone();
    let suggestion = Suggestion {
        id: Uuid::new_v4().to_string(),
        kind: SuggestionKind::SkillDraft,
        // No per-run component: the guardrail's exact-key memory makes a draft
        // by this name a once-ever offer, however it was answered.
        key: format!("skill:{name}"),
        source: format!("reflection:{name}"),
        headline: format!("Want me to remember \u{201c}{name}\u{201d} as a skill?"),
        body: draft.content,
        thread_id: None,
    };
    match service.propose(suggestion).await {
        Ok(()) => Reflection::Proposed { name },
        Err(suppression) => Reflection::Suppressed(suppression),
    }
}

/// The reflection prompt. The task transcript and the installed list ride
/// along because the ambient thread knows nothing about the chat thread —
/// separate conversations, by design.
pub fn reflection_prompt(transcript: &str, installed: &[String]) -> String {
    let existing = if installed.is_empty() {
        "(none)".to_string()
    } else {
        installed.join(", ")
    };
    format!(
        "You are reflecting on a task you just completed for the user. Here is the \
         transcript of that conversation:\n\n{transcript}\n\nDid this task teach a \
         reusable procedure worth keeping as a skill? Skills that already exist \
         (do not propose these again): {existing}.\n\nIf yes, reply with ONLY a \
         draft SKILL.md inside a ```markdown fenced code block, with YAML \
         frontmatter containing `name` (kebab-case), `description`, and \
         `activation:` keywords, followed by the procedure itself. If it taught \
         nothing reusable, reply with exactly NO. Do NOT install anything and do \
         NOT call any tools — output the draft or NO, nothing else."
    )
}

/// Render a timeline as a `User:`/`Assistant:` transcript, keeping the tail.
pub fn render_transcript(timeline: &Timeline, max_chars: usize) -> String {
    let mut lines: Vec<String> = timeline
        .messages
        .iter()
        .filter_map(|message| {
            let content = message.content.as_deref()?.trim();
            if content.is_empty() {
                return None;
            }
            let who = match message.kind {
                MessageKind::User => "User",
                MessageKind::Assistant => "Assistant",
                _ => return None,
            };
            Some(format!("{who}: {content}"))
        })
        .collect();

    // Keep whole recent messages within the budget — the task that just
    // finished is at the end, and half a message is worse than none.
    let mut total = 0usize;
    let mut kept = Vec::new();
    while let Some(line) = lines.pop() {
        let cost = line.chars().count() + 1;
        if total + cost > max_chars && !kept.is_empty() {
            break;
        }
        total += cost;
        kept.push(line);
        if total > max_chars {
            break;
        }
    }
    kept.reverse();
    kept.join("\n")
}

// ---------------------------------------------------------------- install

/// Install an approved draft: a validated file write to the user skills root.
///
/// Deterministic on purpose — the user approved *this exact text*, so no LLM
/// sits between the approval and the install. A user-placed skill is the
/// trusted tier (verified by the `skill_install` gate), identical in effect to
/// `builtin__skill_install` with inline content. The draft is re-parsed here
/// rather than trusted: the bubble round-trip is not a validation.
pub fn install(skills_root: &Path, draft_content: &str) -> std::result::Result<String, String> {
    let draft = parse_draft(draft_content)
        .ok_or_else(|| "the draft is not a valid SKILL.md".to_string())?;
    let dir = skills_root.join(&draft.name);
    if dir.exists() {
        return Err(format!(
            "a skill named \u{201c}{}\u{201d} already exists",
            draft.name
        ));
    }
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("could not create the skill directory: {error}"))?;
    std::fs::write(dir.join("SKILL.md"), draft.content.as_bytes())
        .map_err(|error| format!("could not write SKILL.md: {error}"))?;
    tracing::info!(skill = %draft.name, "installed a self-learned skill with the user's consent");
    Ok(draft.name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway_client::Message;

    const DRAFT: &str = "---\nname: release-notes\ndescription: Draft release notes from a diff.\nactivation:\n  keywords:\n    - release notes\n---\n\n# Release notes\n\nRead the diff, group by theme.\n";

    fn fenced(inner: &str) -> String {
        format!("```markdown\n{inner}```\n")
    }

    // ---------------------------------------------------------- parse_draft

    #[test]
    fn a_fenced_draft_parses() {
        let draft = parse_draft(&fenced(DRAFT)).expect("valid draft");
        assert_eq!(draft.name, "release-notes");
        assert_eq!(draft.description, "Draft release notes from a diff.");
        assert!(draft.content.contains("# Release notes"));
    }

    #[test]
    fn a_bare_draft_parses_too() {
        assert!(parse_draft(DRAFT).is_some());
    }

    #[test]
    fn a_bare_skill_with_fenced_blocks_in_its_body_still_parses() {
        // Real skills carry code examples. The inner fences are not skills, and
        // they must not stop the whole file from being tried as one.
        let skill = format!("{DRAFT}\nRun this:\n\n```bash\ncargo test\n```\n\nThen stop.\n");
        let parsed = parse_draft(&skill).expect("the whole file is the skill");
        assert_eq!(parsed.name, "release-notes");
        assert!(parse_skill_md(&skill).is_some());
    }

    #[test]
    fn parse_skill_md_never_extracts_an_inner_fence() {
        // For an imported *file*, the file is the candidate — a prose file with
        // a fenced skill inside is not itself a skill.
        let prose = format!("Some notes.\n\n{}", fenced(DRAFT));
        assert!(parse_skill_md(&prose).is_none());
        assert!(
            parse_draft(&prose).is_some(),
            "the reply parser still finds it"
        );
    }

    #[test]
    fn a_no_reply_is_nothing() {
        assert_eq!(parse_draft("NO"), None);
        assert_eq!(parse_draft("Nothing reusable here, sorry."), None);
    }

    #[test]
    fn prose_around_a_fenced_draft_still_parses() {
        let reply = format!("Here is what I learned:\n\n{}\nShall I?", fenced(DRAFT));
        assert!(parse_draft(&reply).is_some());
    }

    #[test]
    fn a_draft_without_a_name_is_rejected() {
        let draft = "---\ndescription: no name\n---\n\nBody.\n";
        assert_eq!(parse_draft(draft), None);
    }

    #[test]
    fn an_indented_name_under_activation_is_not_the_skill_name() {
        let draft = "---\ndescription: d\nactivation:\n  name: sneaky\n---\n\nBody.\n";
        assert_eq!(parse_draft(draft), None);
    }

    #[test]
    fn hostile_names_are_rejected() {
        for name in [
            "../escape",
            "UPPER",
            "has space",
            "trailing-",
            "-leading",
            "con",
            "lpt1",
            "a/b",
        ] {
            let draft = format!("---\nname: {name}\ndescription: d\n---\n\nBody.\n");
            assert_eq!(parse_draft(&draft), None, "{name} must not parse");
        }
    }

    #[test]
    fn a_draft_with_no_body_is_rejected() {
        let draft = "---\nname: empty\ndescription: d\n---\n";
        assert_eq!(parse_draft(draft), None);
    }

    #[test]
    fn an_oversized_draft_is_rejected() {
        let draft = format!(
            "---\nname: big\ndescription: d\n---\n\n{}",
            "x".repeat(MAX_DRAFT_BYTES)
        );
        assert_eq!(parse_draft(&draft), None);
    }

    #[test]
    fn quoted_frontmatter_values_are_unquoted() {
        let draft = "---\nname: \"quoted-name\"\ndescription: 'single'\n---\n\nBody.\n";
        let parsed = parse_draft(draft).expect("valid");
        assert_eq!(parsed.name, "quoted-name");
        assert_eq!(parsed.description, "single");
    }

    // ------------------------------------------------------------ RunWatch

    #[test]
    fn a_run_seen_in_flight_fires_once_on_completion() {
        let mut watch = RunWatch::new();
        assert!(!watch.observe("r1", &RunPhase::Running));
        assert!(!watch.observe("r1", &RunPhase::Running), "polls repeat");
        assert!(watch.observe("r1", &RunPhase::Completed));
        assert!(
            !watch.observe("r1", &RunPhase::Completed),
            "the stream repeats terminal status too"
        );
    }

    #[test]
    fn a_run_that_is_already_terminal_at_first_sight_never_fires() {
        let mut watch = RunWatch::new();
        assert!(
            !watch.observe("old", &RunPhase::Completed),
            "a snapshot replaying history is not news"
        );
        assert!(!watch.observe("old", &RunPhase::Completed));
    }

    #[test]
    fn a_failed_run_never_fires() {
        let mut watch = RunWatch::new();
        assert!(!watch.observe("r1", &RunPhase::Running));
        assert!(!watch.observe("r1", &RunPhase::Failed));
        assert!(
            !watch.observe("r1", &RunPhase::Completed),
            "terminal is terminal; no second chance"
        );
    }

    // ------------------------------------------------- skills root helpers

    #[test]
    fn installed_skills_reads_only_directories_with_a_skill_md() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("real")).expect("mkdir");
        std::fs::write(dir.path().join("real").join("SKILL.md"), "x").expect("write");
        std::fs::create_dir_all(dir.path().join("empty")).expect("mkdir");
        std::fs::write(dir.path().join("loose-file"), "x").expect("write");

        assert_eq!(installed_skills(dir.path()), vec!["real".to_string()]);
        assert!(installed_skills(&dir.path().join("missing")).is_empty());
    }

    #[test]
    fn the_cap_counts_only_learned_skills_still_on_disk() {
        let accepted: HashSet<String> = ["reflection:kept", "reflection:removed"]
            .into_iter()
            .map(String::from)
            .collect();
        let installed = vec!["kept".to_string(), "imported-by-hand".to_string()];
        assert_eq!(learned_count(&accepted, &installed), 1);
    }

    // ------------------------------------------------------------- install

    #[test]
    fn an_approved_draft_installs_as_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let name = install(dir.path(), DRAFT).expect("install");
        assert_eq!(name, "release-notes");
        let written = std::fs::read_to_string(dir.path().join("release-notes").join("SKILL.md"))
            .expect("read back");
        assert_eq!(written, DRAFT.trim());
    }

    #[test]
    fn an_existing_skill_is_never_overwritten() {
        let dir = tempfile::tempdir().expect("tempdir");
        install(dir.path(), DRAFT).expect("first install");
        let error = install(dir.path(), DRAFT).expect_err("second install must refuse");
        assert!(error.contains("already exists"), "{error}");
    }

    #[test]
    fn garbage_never_installs() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(install(dir.path(), "not a skill at all").is_err());
        assert!(
            std::fs::read_dir(dir.path())
                .expect("read dir")
                .next()
                .is_none(),
            "nothing may be written for an invalid draft"
        );
    }

    // ---------------------------------------------------------- transcript

    fn message(kind: MessageKind, content: Option<&str>) -> Message {
        Message {
            sequence: 0,
            kind,
            status: "finalized".into(),
            content: content.map(String::from),
        }
    }

    #[test]
    fn the_transcript_renders_user_and_assistant_turns() {
        let timeline = Timeline {
            next_cursor: None,
            messages: vec![
                message(MessageKind::User, Some("rename the release")),
                message(MessageKind::Assistant, None),
                message(MessageKind::Assistant, Some("Done.")),
            ],
        };
        assert_eq!(
            render_transcript(&timeline, 1_000),
            "User: rename the release\nAssistant: Done."
        );
    }

    #[test]
    fn the_transcript_keeps_the_tail_when_over_budget() {
        let timeline = Timeline {
            next_cursor: None,
            messages: vec![
                message(MessageKind::User, Some(&"old ".repeat(100))),
                message(MessageKind::User, Some("the recent question")),
            ],
        };
        let rendered = render_transcript(&timeline, 40);
        assert!(rendered.contains("the recent question"));
        assert!(!rendered.contains("old old"), "the head is dropped first");
    }
}
