//! Phase 7b gate: the reflection turn, driven end to end against a real `serve`
//! with the widget's own shipping code — `ambient::reflection::reflect` and
//! `install`, the same functions `main.rs` calls.
//!
//! What it pins, in one pass:
//!
//! 1. A completed chat turn's transcript reaches the **ambient** thread as a
//!    reflection prompt, and the agent's draft comes back off the timeline.
//! 2. The draft surfaces as a `SkillDraft` suggestion through the same
//!    guardrailed `propose` path as every other surfacing.
//! 3. An Accept installs the draft into the **real skills root the gateway
//!    reads** — `<IRONCLAW_REBORN_HOME>/local-dev/skills/` — which the
//!    `skill_install` gate separately proves is listed, activatable, and fully
//!    injected across a restart. Together the two gates are the 7b
//!    definition-of-done chain.
//! 4. The same skill is never proposed twice: once installed (or once answered),
//!    a later reflection declines rather than re-asking.
#![cfg(feature = "webui-v2-beta")]

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use ic_integration_tests::{MockReply, RebornServer, reborn_home_dir};
use ic_widget::ambient::reflection::{self, Reflection};
use ic_widget::ambient::{
    AmbientConfig, AmbientService, Suggestion, SuggestionKind, ensure_thread,
};
use ic_widget::gateway_client::{GatewayClient, ThreadId};
use ic_widget::settings::AmbientSettings;

/// The name the mock's draft carries.
const LEARNED: &str = "release-note-drafting";

/// What the mock answers the reflection prompt with: prose around a fenced
/// draft, the shape a real model produces.
fn draft_reply() -> String {
    format!(
        "This taught a reusable procedure. Here is the draft:\n\n```markdown\n---\nname: {LEARNED}\ndescription: Draft release notes from a set of commits.\nactivation:\n  keywords:\n    - release notes\n---\n\n# Release note drafting\n\nGroup the commits by theme, lead with user-facing changes.\n```\n"
    )
}

/// Content-conditioned: the reflection prompt carries a fixed phrase, and the
/// chat turn carries none of it. The reflection request body *also* contains
/// the chat transcript, so the reflection check must come first.
fn responder() -> ic_integration_tests::MockResponder {
    Arc::new(|body: &str| {
        if body.contains("reusable procedure worth keeping") {
            return MockReply::Text(draft_reply());
        }
        MockReply::Text("The task is done.".to_string())
    })
}

fn service_for(
    client: GatewayClient,
    dir: &tempfile::TempDir,
) -> (Arc<AmbientService>, Arc<StdMutex<Vec<Suggestion>>>) {
    let log = ic_widget::ambient::log::SurfacingLog::open(dir.path().join("log.jsonl"))
        .expect("open the log");
    let shown: Arc<StdMutex<Vec<Suggestion>>> = Arc::new(StdMutex::new(Vec::new()));
    let sink_shown = Arc::clone(&shown);
    let service = AmbientService::new(
        client,
        Arc::new(|| AmbientConfig {
            enabled: true,
            settings: AmbientSettings {
                max_per_hour: 10,
                quiet_hours: None,
                thread_id: None,
            },
        }),
        Arc::new(move |suggestion| {
            sink_shown
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(suggestion)
        }),
        log,
    );
    (Arc::new(service), shown)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reflection_turn_drafts_a_skill_and_consent_installs_it() {
    let home = tempfile::tempdir().expect("home");
    let server = RebornServer::start_scripted_in_home(
        responder(),
        "unused".to_string(),
        Vec::new(),
        home.path(),
    )
    .await;
    let client = GatewayClient::new(server.base_url.clone(), server.token.clone()).expect("client");
    let dir = tempfile::tempdir().expect("tempdir");
    let (service, shown) = service_for(client.clone(), &dir);

    // The user's task completes on the chat thread.
    let chat = server.create_thread().await;
    server
        .send_message(&chat, "Write the release notes for me.")
        .await;
    let (done, timeline) = server
        .wait_for_timeline_text(&chat, "The task is done.", Duration::from_secs(90))
        .await;
    assert!(done, "the chat turn should complete: {timeline}");
    let chat_thread = ThreadId::new(&chat).expect("chat thread id");

    // The reflection turn runs on the ambient thread, against the REAL skills
    // root the gateway reads — that is what makes an approved install "active
    // in the next session" (the skill_install gate proves that half).
    let ambient = ensure_thread(&client, None)
        .await
        .expect("the ambient thread should open");
    let skills_root = reborn_home_dir(home.path())
        .join("local-dev")
        .join("skills");

    let outcome = tokio::time::timeout(
        Duration::from_secs(120),
        reflection::reflect(&service, &ambient, &chat_thread, &skills_root, 50),
    )
    .await
    .expect("reflection should not hang");
    assert_eq!(
        outcome,
        Reflection::Proposed {
            name: LEARNED.to_string()
        },
        "--- serve stderr ---\n{}",
        server.stderr_snapshot()
    );

    // The draft surfaced as a consent prompt, not a notification.
    let suggestion = {
        let surfaced = shown.lock().expect("lock").clone();
        let [suggestion] = surfaced.as_slice() else {
            panic!("expected exactly one suggestion, got {surfaced:?}");
        };
        suggestion.clone()
    };
    assert_eq!(suggestion.kind, SuggestionKind::SkillDraft);
    assert_eq!(suggestion.key, format!("skill:{LEARNED}"));
    assert!(
        suggestion.body.contains("Group the commits by theme"),
        "the card must show the full text the user is consenting to: {suggestion:?}"
    );
    assert!(
        !skills_root.join(LEARNED).exists(),
        "nothing may be installed before the user answers"
    );

    // The user says yes — the same respond → install pair `main.rs` runs.
    service
        .respond(&suggestion.id, true)
        .await
        .expect("the suggestion should be pending");
    let installed =
        reflection::install(&skills_root, &suggestion.body).expect("the approved draft installs");
    assert_eq!(installed, LEARNED);
    assert!(
        skills_root.join(LEARNED).join("SKILL.md").exists(),
        "the skill must land in the root the gateway reads"
    );

    // A later completed task drafting the same skill does not ask again — the
    // disk dedupe answers before the guardrail even has to.
    let again = reflection::reflect(&service, &ambient, &chat_thread, &skills_root, 50).await;
    assert_eq!(
        again,
        Reflection::AlreadyInstalled {
            name: LEARNED.to_string()
        }
    );
    assert_eq!(
        shown.lock().expect("lock").len(),
        1,
        "the same skill must not be offered twice"
    );

    // And the cap is real: with the ceiling at what is already learned, the
    // next reflection declines before spending an LLM turn.
    let capped = reflection::reflect(&service, &ambient, &chat_thread, &skills_root, 1).await;
    assert_eq!(capped, Reflection::AtCap { max: 1 });
}
