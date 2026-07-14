//! Phase 7d gate: a watcher rule's firing, run end to end by the widget's own
//! shipping code (`ambient::watch::run_rule_fire`) against a real `serve`.
//!
//! What it pins:
//!
//! 1. A firing opens a **fresh thread** (like the gateway's own trigger fires —
//!    the ambient thread stays the app's private conversation), materializes
//!    the user's prompt verbatim, and surfaces the agent's actual answer as a
//!    guardrailed suggestion whose Accept opens that thread.
//! 2. The guardrail's pre-check makes a suppressed watcher *cheap*: with the
//!    hourly cap already spent, a firing runs **no turn at all** — the mock
//!    sees no request, so quiet hours cannot be bought with LLM cycles.
#![cfg(feature = "webui-v2-beta")]

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use ic_integration_tests::{MockReply, RebornServer};
use ic_widget::ambient::watch::{Firing, run_rule_fire};
use ic_widget::ambient::{AmbientConfig, AmbientService, Suggestion, SuggestionKind};
use ic_widget::gateway_client::GatewayClient;
use ic_widget::settings::AmbientSettings;

/// The rule's prompt — its presence in a thread proves the thread is the fire's.
const PROMPT: &str = "WATCH-GATE — summarize what changed in the drop folder";
const ANSWER: &str = "Two new PDFs arrived.";

fn responder() -> ic_integration_tests::MockResponder {
    Arc::new(|body: &str| {
        if body.contains("WATCH-GATE") {
            return MockReply::Text(ANSWER.to_string());
        }
        MockReply::Text("unrelated".to_string())
    })
}

fn service_with_cap(
    client: GatewayClient,
    dir: &tempfile::TempDir,
    max_per_hour: u32,
) -> (Arc<AmbientService>, Arc<StdMutex<Vec<Suggestion>>>) {
    let log = ic_widget::ambient::log::SurfacingLog::open(dir.path().join("log.jsonl"))
        .expect("open the log");
    let shown: Arc<StdMutex<Vec<Suggestion>>> = Arc::new(StdMutex::new(Vec::new()));
    let sink_shown = Arc::clone(&shown);
    let service = AmbientService::new(
        client,
        Arc::new(move || AmbientConfig {
            enabled: true,
            settings: AmbientSettings {
                max_per_hour,
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

fn firing(id: &str) -> Firing {
    Firing {
        rule_id: id.to_string(),
        key: format!("watch:{id}:2026-07-14T12:00:00Z"),
        source: format!("watch:{id}"),
        prompt: PROMPT.to_string(),
        headline: "Noticed C:\\drop changed".to_string(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_watcher_firing_surfaces_the_agents_answer_in_its_own_thread() {
    let server = RebornServer::start_scripted(responder(), "unused".to_string(), Vec::new()).await;
    let client = GatewayClient::new(server.base_url.clone(), server.token.clone()).expect("client");
    let dir = tempfile::tempdir().expect("tempdir");
    let (service, shown) = service_with_cap(client, &dir, 10);

    tokio::time::timeout(
        Duration::from_secs(120),
        run_rule_fire(&service, &firing("r1")),
    )
    .await
    .expect("a firing should not hang");

    let suggestion = {
        let surfaced = shown.lock().expect("lock").clone();
        let [suggestion] = surfaced.as_slice() else {
            panic!(
                "expected exactly one suggestion, got {surfaced:?}\n--- stderr ---\n{}",
                server.stderr_snapshot()
            );
        };
        suggestion.clone()
    };
    assert_eq!(suggestion.kind, SuggestionKind::Watcher);
    assert!(suggestion.body.contains(ANSWER), "{suggestion:?}");
    assert_eq!(suggestion.source, "watch:r1");

    // Accept opens a real thread that holds exactly this rule's question and
    // the answer — and it is a thread of its own, not the user's.
    let thread_id = suggestion.thread_id.expect("a firing's thread");
    let (found, timeline) = server
        .wait_for_timeline_text(&thread_id, ANSWER, Duration::from_secs(30))
        .await;
    assert!(found, "the thread should hold the answer: {timeline}");
    assert!(
        timeline.to_string().contains("WATCH-GATE"),
        "and the rule's own prompt: {timeline}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_suppressed_firing_spends_no_llm_turn() {
    let server = RebornServer::start_scripted(responder(), "unused".to_string(), Vec::new()).await;
    let client = GatewayClient::new(server.base_url.clone(), server.token.clone()).expect("client");
    let dir = tempfile::tempdir().expect("tempdir");
    // Cap of one, already spent by the first firing.
    let (service, shown) = service_with_cap(client, &dir, 1);

    tokio::time::timeout(
        Duration::from_secs(120),
        run_rule_fire(&service, &firing("r1")),
    )
    .await
    .expect("the first firing should not hang");
    assert_eq!(shown.lock().expect("lock").len(), 1, "the cap's one slot");
    let requests_after_first = server.chat_requests().len();
    assert!(requests_after_first > 0, "the first firing ran a real turn");

    tokio::time::timeout(
        Duration::from_secs(30),
        run_rule_fire(&service, &firing("r2")),
    )
    .await
    .expect("a suppressed firing returns at once");
    assert_eq!(
        shown.lock().expect("lock").len(),
        1,
        "the second firing must be suppressed by the cap"
    );
    assert_eq!(
        server.chat_requests().len(),
        requests_after_first,
        "and it must not have spent a turn finding that out — the pre-check \
         is what keeps quiet hours from being bought with LLM cycles"
    );
}
