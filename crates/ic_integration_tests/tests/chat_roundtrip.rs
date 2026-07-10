//! Phase 0 upstream-merge gate.
//!
//! Spawns `ironclaw-reborn serve` against the libSQL `local-dev` profile with a
//! hermetic mock LLM and drives the minimal chat contract `ic_widget` builds
//! against: auth is enforced, a thread is created, a message starts a turn, and
//! the assistant's reply streams back over SSE. If an upstream sync breaks the
//! serve API shape, the storage substrate, or the agent loop, this test fails.
//!
//! Compiled only under the `webui-v2-beta` feature (which implies the `serve`
//! binary was built). See the crate `Cargo.toml` for the CI build recipe.
#![cfg(feature = "webui-v2-beta")]

use std::time::Duration;

use ic_integration_tests::{API_PREFIX, RebornServer};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_roundtrip_streams_assistant_reply() {
    let server = RebornServer::start().await;

    // 1. Auth is enforced: no bearer -> 401.
    let unauth = reqwest::Client::new()
        .get(format!("{}{API_PREFIX}/threads", server.base_url))
        .send()
        .await
        .expect("unauthenticated request");
    assert_eq!(
        unauth.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "unauthenticated /threads must be 401"
    );

    // 2. Create a thread.
    let thread_id = server.create_thread().await;
    assert!(!thread_id.is_empty(), "thread_id should be non-empty");

    // 3. Send a message -> a turn is submitted with a run_id.
    let run_id = server
        .send_message(&thread_id, "Reply with the marker, nothing else.")
        .await;
    assert!(!run_id.is_empty(), "run_id should be non-empty");

    // 4. The turn streams to a terminal state over SSE. The projection stream
    //    surfaces `run_status` transitions (queued -> running -> completed);
    //    reaching `completed` proves the full path executed end-to-end:
    //    prompt -> agent loop -> mock LLM -> runtime -> SSE projection.
    let (completed, stream) = server
        .stream_until(
            &thread_id,
            "\"status\":\"completed\"",
            Duration::from_secs(60),
        )
        .await;
    assert!(
        completed,
        "run {run_id} never reached `completed` over SSE.\n\
         --- accumulated SSE ---\n{stream}\n\
         --- serve stderr ---\n{}",
        server.stderr_snapshot()
    );

    // 5. The assistant reply (the mock LLM's unique canned answer) is persisted
    //    and read back from the timeline. This confirms the message content
    //    round-tripped through the mock provider, not just that a run finished.
    let marker = server.answer.clone();
    let (found, timeline) = server
        .wait_for_timeline_text(&thread_id, &marker, Duration::from_secs(15))
        .await;
    assert!(
        found,
        "assistant reply marker {marker:?} not found in timeline.\n\
         --- timeline ---\n{timeline:#}\n\
         --- serve stderr ---\n{}",
        server.stderr_snapshot()
    );
}
