//! Phase 7a gate: the ambient thread, and surfacing a scheduled automation's run.
//!
//! This drives the **real** runtime, not a mock of our own beliefs about it. It
//! spawns `ironclaw-reborn serve`, has the agent create a real trigger through
//! `builtin__trigger_create`, waits for the gateway's own poller to fire it, and
//! then runs the widget's own `ambient::automations::AutomationWatch` — the code
//! that ships — over the result. If upstream changes where a trigger-fired run
//! lands, this fails.
//!
//! Four facts it pins, each of which the Phase 7a plan got wrong or left open:
//!
//! 1. **The trigger poller is off unless we ask for it.**
//!    `IRONCLAW_TRIGGER_POLLER_ENABLED` is what makes a schedule fire at all; the
//!    first test asserts the negative, because "the automation never ran" is a
//!    silent failure everywhere else.
//! 2. **A fire lands in a new thread of its own**, which `GET /threads` lists and
//!    whose timeline holds the answer.
//! 3. **Nothing correlates the automation row to that thread** — the watcher pairs
//!    them by timing, and this proves the pairing on a real fire.
//! 4. **`builtin__trigger_create` runs with no approval prompt**, despite being
//!    declared `PermissionMode::Ask` (the Phase 4 finding, live in this lane). That
//!    is why ambient mode gates the poller.
#![cfg(feature = "webui-v2-beta")]

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use ic_integration_tests::{MockReply, RebornServer};
use ic_widget::ambient::automations::AutomationWatch;
use ic_widget::ambient::log::SurfacingLog;
use ic_widget::ambient::{AmbientConfig, AmbientService, Suggestion, ensure_thread};
use ic_widget::gateway_client::GatewayClient;
use ic_widget::settings::AmbientSettings;

/// The prompt that makes the mock agent arm a schedule.
const ARM: &str = "AMBIENT-GATE-ARM";
/// The prompt the fired trigger runs. Its presence in a thread is proof that thread
/// is the trigger's.
const FIRED: &str = "AMBIENT-GATE-FIRED";
/// What the agent answers a fired run with.
const ANSWER: &str = "the digest is ready";

/// A mock that arms a once-a-minute trigger when it sees [`ARM`], and otherwise
/// answers with text. Content-conditioned rather than a positional script: the chat
/// thread, the ambient thread, and the fired run are all in flight against it.
fn responder() -> ic_integration_tests::MockResponder {
    Arc::new(|body: &str| {
        if body.contains("\"role\":\"tool\"") {
            return MockReply::Text("armed".to_string());
        }
        if body.contains(ARM) {
            return MockReply::ToolCall {
                name: "builtin__trigger_create".to_string(),
                arguments: serde_json::json!({
                    "name": "Nightly digest",
                    "prompt": format!("{FIRED} — summarize the day"),
                    "cron": "* * * * *",
                }),
            };
        }
        MockReply::Text(ANSWER.to_string())
    })
}

/// The ambient service under test, wired to a real gateway, with the guardrails
/// wide open so the *watcher* is what is being tested here and not the cap (which
/// has its own unit tests).
fn service(
    client: GatewayClient,
    dir: &tempfile::TempDir,
) -> (Arc<AmbientService>, Arc<Mutex<Vec<Suggestion>>>) {
    let log = SurfacingLog::open(dir.path().join("ambient-log.jsonl")).expect("open the log");
    let shown: Arc<Mutex<Vec<Suggestion>>> = Arc::new(Mutex::new(Vec::new()));
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

fn client_for(server: &RebornServer) -> GatewayClient {
    GatewayClient::new(server.base_url.clone(), server.token.clone()).expect("gateway client")
}

/// The ambient thread is a real thread, it takes a turn, and it is *not* the chat
/// thread. This is the plumbing every later sub-phase (reflection, skill review)
/// sends its turns down.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_ambient_thread_takes_a_turn_and_is_not_the_chat_thread() {
    let server = RebornServer::start().await;
    let client = client_for(&server);
    let chat = server.create_thread().await;

    let ambient = ensure_thread(&client, None)
        .await
        .expect("the ambient thread should open");
    assert_ne!(
        ambient.as_str(),
        chat,
        "the app's own conversation must not be the user's"
    );

    // A turn on it round-trips: send, run to terminal, read the reply off the
    // timeline (the reply text is never on the event stream).
    let dir = tempfile::tempdir().expect("tempdir");
    let (service, _shown) = service(client.clone(), &dir);
    let reply = tokio::time::timeout(
        Duration::from_secs(90),
        service.ask(&ambient, "What is the ambient thread for?"),
    )
    .await
    .expect("the ambient turn should not hang")
    .expect("the ambient turn should produce a reply");
    assert!(
        reply.contains(&server.answer),
        "expected the mock's answer in {reply:?}\n--- serve stderr ---\n{}",
        server.stderr_snapshot()
    );

    // A saved id is reused rather than re-created — the ambient conversation has to
    // survive a restart, or every launch starts a new one.
    let again = ensure_thread(&client, Some(ambient.as_str()))
        .await
        .expect("the saved thread should resolve");
    assert_eq!(again.as_str(), ambient.as_str());

    // And a thread id that no longer exists (a wiped store) yields a fresh one
    // rather than a permanently broken ambient mode.
    let recovered = ensure_thread(&client, Some("00000000-0000-0000-0000-000000000000"))
        .await
        .expect("a lost thread should be replaced, not fatal");
    assert_ne!(recovered.as_str(), ambient.as_str());
}

/// Without the poller switch, a schedule is listed and **never fires**. The whole
/// ambient design rests on this, and it fails silently — hence a test.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_schedule_never_fires_while_the_trigger_poller_is_off() {
    // No IRONCLAW_TRIGGER_POLLER_ENABLED: the runtime's own default.
    let server = RebornServer::start_scripted(responder(), "unused".to_string(), Vec::new()).await;
    let chat = server.create_thread().await;
    server.send_message(&chat, ARM).await;
    server
        .stream_until(&chat, "\"status\":\"completed\"", Duration::from_secs(60))
        .await;

    let automations = server.automations().await;
    assert_eq!(
        automations["automations"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0),
        1,
        "the trigger should exist: {automations}"
    );

    // Well past the once-a-minute cadence.
    tokio::time::sleep(Duration::from_secs(75)).await;
    let after = server.automations().await;
    assert!(
        after["automations"][0]["last_run_at"].is_null(),
        "with the poller off, a schedule must never run: {after}"
    );
}

/// The whole path: arm a schedule, let the gateway fire it, and let the widget's own
/// watcher find it. What the user ends up seeing is one suggestion, carrying the
/// agent's actual answer, pointing at the thread the run landed in.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fired_automation_surfaces_once_with_its_reply() {
    let server = RebornServer::start_scripted(
        responder(),
        "unused".to_string(),
        vec![
            ("IRONCLAW_TRIGGER_POLLER_ENABLED".into(), "true".into()),
            // The floor is a 1 s poll; the *fire* cadence is still the cron's
            // one-minute minimum, which is what the timeouts below are sized for.
            ("IRONCLAW_TRIGGER_POLLER_INTERVAL_SECS".into(), "1".into()),
        ],
    )
    .await;
    let client = client_for(&server);
    let dir = tempfile::tempdir().expect("tempdir");
    let (service, shown) = service(client.clone(), &dir);

    // The agent arms the schedule — no approval prompt appears, though
    // `builtin.trigger_create` is declared `Ask`. The run simply completes.
    let chat = server.create_thread().await;
    server.send_message(&chat, ARM).await;
    let (completed, stream) = server
        .stream_until(&chat, "\"status\":\"completed\"", Duration::from_secs(90))
        .await;
    assert!(
        completed,
        "arming the trigger should complete.\n--- SSE ---\n{stream}\n--- stderr ---\n{}",
        server.stderr_snapshot()
    );
    assert!(
        !stream.contains("blocked_approval"),
        "the runtime does not gate `Ask` tools — if it now does, ambient mode can \
         stop carrying that weight itself.\n{stream}"
    );

    // Prime: what already exists is not news.
    let mut watch = AutomationWatch::new();
    watch.tick(&service).await.expect("the priming tick");
    assert!(
        shown.lock().expect("lock").is_empty(),
        "priming must surface nothing"
    );

    // The gateway's own poller fires it. The cron minimum is 60 s, so the first fire
    // lands at the next minute boundary.
    let fired = server
        .wait_for_new_thread(std::slice::from_ref(&chat), Duration::from_secs(150))
        .await
        .unwrap_or_else(|| {
            panic!(
                "the trigger never fired.\n--- serve stderr ---\n{}",
                server.stderr_snapshot()
            )
        });

    // The fired run is an ordinary thread: it lists, and its timeline holds the
    // trigger's prompt and the agent's answer.
    let (landed, timeline) = server
        .wait_for_timeline_text(&fired, ANSWER, Duration::from_secs(90))
        .await;
    assert!(landed, "the fired run should answer: {timeline}");
    assert!(
        timeline.to_string().contains(FIRED),
        "the fired thread should carry the trigger's own prompt: {timeline}"
    );

    // Now the widget's watcher sees it. One tick, one suggestion.
    watch.tick(&service).await.expect("the surfacing tick");
    let surfaced = shown.lock().expect("lock").clone();
    let [suggestion] = surfaced.as_slice() else {
        panic!("expected exactly one suggestion, got {surfaced:?}");
    };
    assert!(
        suggestion.headline.contains("Nightly digest"),
        "{suggestion:?}"
    );
    assert!(
        suggestion.body.contains(ANSWER),
        "the suggestion should carry what the agent actually said: {suggestion:?}"
    );
    assert_eq!(
        suggestion.thread_id.as_deref(),
        Some(fired.as_str()),
        "Accept has to open the thread the run landed in"
    );
    assert_eq!(
        suggestion.source,
        format!(
            "automation:{}",
            server.automations().await["automations"][0]["automation_id"]
                .as_str()
                .expect("automation id")
        ),
        "a 'Not now' has to quiet the automation it came from"
    );

    // The same run is not news twice, however often the watcher ticks.
    watch.tick(&service).await.expect("a repeat tick");
    assert_eq!(
        shown.lock().expect("lock").len(),
        1,
        "a completed run must surface exactly once"
    );
}
