//! Phase 8c gate: the Skills panel sees a real gateway-installed skill, and can
//! remove it — driven through the code that ships, not a re-implementation.
//!
//! The panel reads the widget-owned skills directory on disk rather than any
//! gateway route (there is none — `docs/desktop/dashboard-gaps.md`). The risk a
//! unit test cannot cover is that the on-disk *layout* the panel assumes and the
//! layout the runtime actually writes could drift. So this gate has the agent
//! install a skill through `builtin__skill_install` against a real `serve`
//! (exactly as `skill_install.rs` does), then calls the shipping
//! `ic_widget::skills::list` / `::remove` over the very directory the gateway
//! wrote. If the runtime ever moves user skills, or changes the SKILL.md shape
//! the panel parses, this fails — which is the whole point (the Phase 4 lesson:
//! drive the shipping code against the real runtime, not your beliefs about it).
#![cfg(feature = "webui-v2-beta")]

use std::sync::Arc;
use std::time::Duration;

use ic_integration_tests::{MockReply, RebornServer, reborn_home_dir};

const INSTALL: &str = "PANEL-GATE-INSTALL";
const SKILL_NAME: &str = "panel-probe-skill";
const SKILL_DESCRIPTION: &str = "A skill the 8c panel gate installs and then lists.";

fn skill_md() -> String {
    format!(
        "---\n\
         name: {SKILL_NAME}\n\
         description: {SKILL_DESCRIPTION}\n\
         ---\n\n\
         # Panel probe\n\n\
         When asked to run the panel probe, say so.\n"
    )
}

/// Installs one skill from inline content, then ends the turn.
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_panel_lists_and_removes_a_skill_the_agent_installed() {
    let home = tempfile::tempdir().expect("home");
    // The exact directory the widget's `skills_root()` resolves to, computed the
    // same way the running gateway does.
    let skills_root = reborn_home_dir(home.path())
        .join("local-dev")
        .join("skills");

    let server = RebornServer::start_scripted_in_home(
        install_responder(),
        "unused".to_string(),
        Vec::new(),
        home.path(),
    )
    .await;

    let chat = server.create_thread().await;
    server
        .send_message(&chat, &format!("{INSTALL} — learn this procedure"))
        .await;
    let (done, timeline) = server
        .wait_for_timeline_text(&chat, "install-turn-done", Duration::from_secs(90))
        .await;
    assert!(
        done,
        "the install turn should complete: {timeline}\n--- stderr ---\n{}",
        server.stderr_snapshot()
    );

    // The panel's own listing code, over the directory the gateway just wrote.
    let listed = ic_widget::skills::list(&skills_root).expect("list installed skills");
    let found = listed
        .iter()
        .find(|skill| skill.name == SKILL_NAME)
        .unwrap_or_else(|| {
            panic!(
                "the Skills panel should list the installed skill; saw {listed:?}\n--- stderr ---\n{}",
                server.stderr_snapshot()
            )
        });
    assert!(
        found.valid,
        "a well-formed installed skill should parse as valid: {found:?}"
    );
    assert_eq!(
        found.description, SKILL_DESCRIPTION,
        "the panel should read the description from the on-disk SKILL.md"
    );
    assert!(found.files >= 1 && found.bytes > 0, "footprint: {found:?}");

    // And the panel's remove deletes exactly that skill from the same root.
    ic_widget::skills::remove(&skills_root, SKILL_NAME).expect("remove the skill");
    let after = ic_widget::skills::list(&skills_root).expect("list after removal");
    assert!(
        !after.iter().any(|skill| skill.name == SKILL_NAME),
        "the removed skill should no longer be listed: {after:?}"
    );
    assert!(
        !skills_root.join(SKILL_NAME).exists(),
        "the skill directory should be gone from disk"
    );
}
