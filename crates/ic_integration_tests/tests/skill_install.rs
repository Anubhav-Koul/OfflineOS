//! Phase 7b verify item: `builtin__skill_install`, end to end, against a real
//! `serve` — before any reflection or consent UI is built on top of it.
//!
//! The Phase 4 lesson, applied up front: a capability can be *listed* in the
//! tool surface and still be unreachable at dispatch (discovery passed, egress
//! refused). `local_dev_capability_policy.toml` says skill_install is granted
//! the effects it declares; this test makes that a fact about the running
//! gateway rather than a belief about its config. It pins the three things 7b
//! rests on:
//!
//! 1. **The agent installs a skill from inline content, with no gate.**
//!    `builtin.skill_install` is declared `PermissionMode::Ask` and, like every
//!    `Ask` builtin in this lane, runs unprompted — the consent gate is ours to
//!    build (Phase 4 finding, third confirmation).
//! 2. **The install is plain files under the reborn home** —
//!    `<IRONCLAW_REBORN_HOME>/local-dev/skills/<name>/SKILL.md` — so it
//!    survives a gateway restart exactly as long as the home does. Not the
//!    libSQL store.
//! 3. **In the next session the skill is listed by name, activates, and its
//!    full body reaches the model's context.** The local-dev skill selector is
//!    explicit-only — installing puts nothing in context by itself — but an
//!    inline-content install lands as `source: user` (the trusted tier), so
//!    `builtin__skill_activate` injects the whole prompt, not just the
//!    description. Without this, a learned skill would be a name, not a
//!    procedure.
//!
//! The mock's request log doubles as the probe's microscope: every tool result
//! rides back to the "model" as a tool-role message, so what the agent can see
//! of skill_list / skill_activate is read straight off the wire.
#![cfg(feature = "webui-v2-beta")]

use std::sync::Arc;
use std::time::Duration;

use ic_integration_tests::{MockReply, RebornServer, reborn_home_dir};

/// Prompt markers, one per turn, so the content-conditioned mock can tell the
/// turns apart without a positional script.
const INSTALL: &str = "SKILL-GATE-INSTALL";
const LIST: &str = "SKILL-GATE-LIST";
const ACTIVATE: &str = "SKILL-GATE-ACTIVATE";

const SKILL_NAME: &str = "probe-skill";
const SKILL_DESCRIPTION: &str = "A probe skill the integration gate installs.";
/// Only ever appears in the skill body — its presence in a completion request
/// is proof the skill's *content* reached the model's context.
const SKILL_BODY_MARKER: &str = "PROBE-SKILL-BODY-MARKER";

fn skill_md() -> String {
    format!(
        "---\n\
         name: {SKILL_NAME}\n\
         version: 0.1.0\n\
         description: {SKILL_DESCRIPTION}\n\
         activation:\n\
         \x20 keywords:\n\
         \x20   - probe procedure\n\
         ---\n\n\
         # Probe skill\n\n\
         {SKILL_BODY_MARKER}: when asked to run the probe procedure, say the marker aloud.\n"
    )
}

/// Session 1: sees [`INSTALL`], calls `builtin__skill_install` with inline
/// content, and ends the turn once the tool result comes back.
fn install_responder() -> ic_integration_tests::MockResponder {
    Arc::new(|body: &str| {
        if body.contains("\"role\":\"tool\"") {
            return MockReply::Text("install-turn-done".to_string());
        }
        if body.contains(INSTALL) {
            return MockReply::ToolCall {
                name: "builtin__skill_install".to_string(),
                arguments: serde_json::json!({
                    "name": SKILL_NAME,
                    "content": skill_md(),
                }),
            };
        }
        MockReply::Text("mock-fallthrough".to_string())
    })
}

/// Session 2: lists on [`LIST`], activates on [`ACTIVATE`]. The two turns run
/// on separate threads so each request body carries exactly one marker.
fn second_session_responder() -> ic_integration_tests::MockResponder {
    Arc::new(|body: &str| {
        if body.contains("\"role\":\"tool\"") {
            if body.contains(ACTIVATE) {
                return MockReply::Text("activate-turn-done".to_string());
            }
            return MockReply::Text("list-turn-done".to_string());
        }
        if body.contains(ACTIVATE) {
            return MockReply::ToolCall {
                name: "builtin__skill_activate".to_string(),
                arguments: serde_json::json!({ "names": [SKILL_NAME] }),
            };
        }
        if body.contains(LIST) {
            return MockReply::ToolCall {
                name: "builtin__skill_list".to_string(),
                arguments: serde_json::json!({}),
            };
        }
        MockReply::Text("mock-fallthrough".to_string())
    })
}

/// Every tool-role message content across the mock's request log — i.e. every
/// tool result exactly as the model saw it.
fn tool_results(requests: &[String]) -> Vec<String> {
    let mut results = Vec::new();
    for raw in requests {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
            continue;
        };
        let Some(messages) = value["messages"].as_array() else {
            continue;
        };
        for message in messages {
            if message["role"] == "tool"
                && let Some(content) = message["content"].as_str()
                && !results.iter().any(|seen| seen == content)
            {
                results.push(content.to_string());
            }
        }
    }
    results
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_skill_the_agent_installs_survives_a_restart_and_activates() {
    let home = tempfile::tempdir().expect("home");
    let skill_file = reborn_home_dir(home.path())
        .join("local-dev")
        .join("skills")
        .join(SKILL_NAME)
        .join("SKILL.md");

    // ---- Session 1: install. ----
    {
        let server = RebornServer::start_scripted_in_home(
            install_responder(),
            "unused".to_string(),
            Vec::new(),
            home.path(),
        )
        .await;
        let chat = server.create_thread().await;
        server
            .send_message(&chat, &format!("{INSTALL} — remember this procedure"))
            .await;
        let (done, timeline) = server
            .wait_for_timeline_text(&chat, "install-turn-done", Duration::from_secs(90))
            .await;
        assert!(
            done,
            "the install turn should complete without a gate: {timeline}\n--- stderr ---\n{}",
            server.stderr_snapshot()
        );

        let results = tool_results(&server.chat_requests());
        eprintln!("probe: skill_install tool result(s) = {results:?}");
        assert!(
            !results.is_empty(),
            "the install tool result should ride back to the model"
        );

        // Ground truth is the disk, not the tool result's phrasing: the skill
        // is plain files under the reborn home.
        assert!(
            skill_file.exists(),
            "the installed skill should be on disk at {}\ntool results: {results:?}\n--- stderr ---\n{}",
            skill_file.display(),
            server.stderr_snapshot()
        );
        let installed = std::fs::read_to_string(&skill_file).expect("read installed SKILL.md");
        assert!(
            installed.contains(SKILL_BODY_MARKER),
            "the installed body should be ours: {installed}"
        );
    } // <- the first server drops here: child killed, libSQL lock released.

    // ---- Session 2: same home, fresh gateway. ----
    let server = RebornServer::start_scripted_in_home(
        second_session_responder(),
        "unused".to_string(),
        Vec::new(),
        home.path(),
    )
    .await;

    // The skill is listed by name after the restart.
    let list_thread = server.create_thread().await;
    server
        .send_message(&list_thread, &format!("{LIST} — what skills do you have?"))
        .await;
    let (done, timeline) = server
        .wait_for_timeline_text(&list_thread, "list-turn-done", Duration::from_secs(90))
        .await;
    assert!(
        done,
        "the list turn should complete: {timeline}\n--- stderr ---\n{}",
        server.stderr_snapshot()
    );
    let results = tool_results(&server.chat_requests());
    eprintln!("probe: skill_list tool result(s) after restart = {results:?}");
    assert!(
        results.iter().any(|result| result.contains(SKILL_NAME)),
        "skill_list after a restart should name the installed skill — if the model \
         cannot see names here, 7b's dedupe cannot work.\ntool results: {results:?}"
    );

    // And it activates by name. `builtin.skill_activate` answers
    // {"activated":[...],"count":N} filtered to the requested names, so the
    // name's presence in the result is what distinguishes "activated ours"
    // from "succeeded activating nothing".
    let activate_thread = server.create_thread().await;
    server
        .send_message(
            &activate_thread,
            &format!("{ACTIVATE} — use the probe skill"),
        )
        .await;
    let (done, timeline) = server
        .wait_for_timeline_text(
            &activate_thread,
            "activate-turn-done",
            Duration::from_secs(90),
        )
        .await;
    assert!(
        done,
        "the activate turn should complete: {timeline}\n--- stderr ---\n{}",
        server.stderr_snapshot()
    );
    let requests = server.chat_requests();
    let results = tool_results(&requests);
    eprintln!("probe: skill_activate tool result(s) = {results:?}");
    assert!(
        results
            .iter()
            .any(|result| result.contains("activated") && result.contains(SKILL_NAME)),
        "skill_activate should report activating the skill by name: {results:?}"
    );

    // The payoff everything above exists for: activation puts the skill's
    // *content* in front of the model. An inline-content install lands as
    // `source: user` — the trusted tier, full-prompt injection — unlike
    // URL-provenance installs, whose `Installed` trust would surface the
    // description only. If upstream changes that trust assignment, 7b's
    // learned skills stop working and this is the line that says so.
    let after_activate: Vec<&String> = requests
        .iter()
        .filter(|request| request.contains(ACTIVATE) && request.contains("\"role\":\"tool\""))
        .collect();
    assert!(
        after_activate
            .iter()
            .any(|request| request.contains(SKILL_BODY_MARKER)),
        "the activated skill's body should reach the model's context — description-only \
         injection means the skill's trust tier changed"
    );
    assert!(
        after_activate
            .iter()
            .any(|request| request.contains(SKILL_DESCRIPTION)),
        "the activated skill's description should reach the model's context"
    );
}
