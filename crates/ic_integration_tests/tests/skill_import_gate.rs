//! Phase 7c gate: a third-party skill folder, imported by the widget's own
//! shipping code (`skill_import::preview` + `install`), lands where a running
//! gateway actually reads — and the agent can see it by name immediately, no
//! restart needed, because the skills root is re-read on every `skill_list`.
//!
//! The trust chain this closes: `preview` is what the user reviewed, `install`
//! writes that reviewed text verbatim, and the `skill_install` gate separately
//! proves a skill in this root is activatable with its full body injected.
#![cfg(feature = "webui-v2-beta")]

use std::sync::Arc;
use std::time::Duration;

use ic_integration_tests::{MockReply, RebornServer, reborn_home_dir};
use ic_widget::skill_import;

const LIST: &str = "IMPORT-GATE-LIST";
const IMPORTED: &str = "imported-probe";

const SKILL: &str = "---\nname: imported-probe\ndescription: A third-party skill imported after review.\n---\n\n# Imported probe\n\nWhen asked, say the import worked.\n";

fn responder() -> ic_integration_tests::MockResponder {
    Arc::new(|body: &str| {
        if body.contains("\"role\":\"tool\"") {
            return MockReply::Text("list-done".to_string());
        }
        if body.contains(LIST) {
            return MockReply::ToolCall {
                name: "builtin__skill_list".to_string(),
                arguments: serde_json::json!({}),
            };
        }
        MockReply::Text("unexpected".to_string())
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_imported_folder_is_visible_to_the_running_agent() {
    // The third-party folder being imported, with a bundle file riding along.
    let source = tempfile::tempdir().expect("source");
    std::fs::write(source.path().join("SKILL.md"), SKILL).expect("write SKILL.md");
    std::fs::create_dir_all(source.path().join("reference")).expect("mkdir");
    std::fs::write(
        source.path().join("reference").join("style.md"),
        "short sentences\n",
    )
    .expect("write bundle file");

    let home = tempfile::tempdir().expect("home");
    let server = RebornServer::start_scripted_in_home(
        responder(),
        "unused".to_string(),
        Vec::new(),
        home.path(),
    )
    .await;

    // Review, then install the reviewed text — the same pair `main.rs` runs
    // after the bubble's yes — into the root the RUNNING gateway reads.
    let preview = skill_import::preview(source.path()).expect("preview");
    assert_eq!(preview.name, IMPORTED);
    assert_eq!(preview.files.len(), 1, "the bundle file is in the review");
    let skills_root = reborn_home_dir(home.path())
        .join("local-dev")
        .join("skills");
    let name =
        skill_import::install(source.path(), &preview.skill_md, &skills_root).expect("install");
    assert_eq!(name, IMPORTED);
    assert!(
        skills_root
            .join(IMPORTED)
            .join("reference")
            .join("style.md")
            .exists(),
        "the bundle rides along"
    );

    // No restart: the skills root is read lazily, so the agent's very next
    // skill_list sees the import.
    let thread = server.create_thread().await;
    server
        .send_message(&thread, &format!("{LIST} — what skills do you have?"))
        .await;
    let (done, timeline) = server
        .wait_for_timeline_text(&thread, "list-done", Duration::from_secs(90))
        .await;
    assert!(
        done,
        "the list turn should complete: {timeline}\n--- stderr ---\n{}",
        server.stderr_snapshot()
    );
    let listed = server
        .chat_requests()
        .iter()
        .any(|request| request.contains("\"role\":\"tool\"") && request.contains(IMPORTED));
    assert!(
        listed,
        "the running agent's skill_list should name the imported skill"
    );
}
