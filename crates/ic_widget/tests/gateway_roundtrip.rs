//! `gateway_client` against a real `ironclaw-reborn serve`.
//!
//! The unit tests in `src/gateway_client/` pin the decoding of frames we hand
//! them. This test pins the decoding of frames the *gateway* hands us — which is
//! the only thing that catches upstream protocol drift. It reuses the Phase 0
//! harness (`ic_integration_tests::RebornServer`), so the LLM is the hermetic
//! mock and the whole test is offline.
//!
//! Build the binary first, then run:
//!
//! ```bash
//! cargo build -p ironclaw_reborn_cli --bin ironclaw-reborn --features webui-v2-beta
//! cargo test  -p ic_widget --features webui-v2-beta
//! ```
#![cfg(feature = "webui-v2-beta")]

use std::time::Duration;

use ic_integration_tests::RebornServer;
use ic_widget::error::Error;
use ic_widget::gateway_client::{
    ClientActionId, GatewayClient, GatewayEvent, MessageKind, RunId, RunPhase, SubmitOutcome,
};

/// The mock LLM answers immediately, but the projection stream polls once a
/// second, so a terminal status can take a few seconds to surface.
const RUN_TIMEOUT: Duration = Duration::from_secs(60);

fn client(server: &RebornServer) -> GatewayClient {
    GatewayClient::new(&server.base_url, &server.token).expect("client")
}

/// Drive the event stream until `run_id` reports a terminal phase.
async fn await_run_completion(
    client: &GatewayClient,
    thread_id: &ic_widget::ThreadId,
    run_id: &RunId,
) -> RunPhase {
    let mut stream = client.events(thread_id.clone());
    let deadline = tokio::time::Instant::now() + RUN_TIMEOUT;

    loop {
        let next = tokio::time::timeout_at(deadline, stream.next())
            .await
            .unwrap_or_else(|_| panic!("run {run_id} never reached a terminal phase"));
        let Some(event) = next else {
            panic!("the event stream ended before run {run_id} finished");
        };
        match event.expect("a well-formed event") {
            GatewayEvent::ProjectionSnapshot(state) | GatewayEvent::ProjectionUpdate(state) => {
                if let Some(status) = state.run_phase(run_id)
                    && status.status.is_terminal()
                {
                    return status.status.clone();
                }
            }
            GatewayEvent::Error(error) => panic!("the stream failed: {error:?}"),
            // keep_alive and capability_* are expected; nothing to do.
            _ => {}
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_runs_to_completion_and_the_reply_is_read_from_the_timeline() {
    let server = RebornServer::start().await;
    let client = client(&server);

    client.health().await.expect("the gateway is up");

    let thread_id = client.create_thread().await.expect("create thread");

    let outcome = client
        .send_message(&thread_id, "hello", &ClientActionId::new())
        .await
        .expect("send");
    let SubmitOutcome::Submitted { run_id } = outcome else {
        panic!("expected the turn to be admitted, got {outcome:?}");
    };

    // The run's terminal state is only observable through `run_status` items on
    // the projection stream — there is no `final_reply` event.
    let phase = await_run_completion(&client, &thread_id, &run_id).await;
    assert_eq!(
        phase,
        RunPhase::Completed,
        "serve stderr:\n{}",
        server.stderr_snapshot()
    );

    // And the answer itself is only in the timeline.
    let timeline = client.timeline(&thread_id, None).await.expect("timeline");
    let reply = timeline
        .latest_assistant_reply()
        .expect("an assistant reply")
        .content
        .clone()
        .expect("reply text");
    assert_eq!(reply, server.answer);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_event_stream_never_carries_the_assistant_text() {
    // This is the finding that shapes the whole chat UI. If a future upstream
    // release starts emitting the reply on the stream, this test fails and we
    // get to simplify.
    let server = RebornServer::start().await;
    let client = client(&server);
    let thread_id = client.create_thread().await.expect("create thread");

    let mut stream = client.events(thread_id.clone());
    let outcome = client
        .send_message(&thread_id, "hello", &ClientActionId::new())
        .await
        .expect("send");
    let run_id = outcome.run_id().clone();

    let deadline = tokio::time::Instant::now() + RUN_TIMEOUT;
    let mut saw_terminal = false;
    let mut text_items = 0;

    while !saw_terminal {
        let event = tokio::time::timeout_at(deadline, stream.next())
            .await
            .expect("the run should finish")
            .expect("the stream should not end")
            .expect("a well-formed event");

        if let GatewayEvent::ProjectionSnapshot(state) | GatewayEvent::ProjectionUpdate(state) =
            &event
        {
            text_items += state
                .items
                .iter()
                .filter(|item| {
                    matches!(item, ic_widget::gateway_client::ProjectionItem::Text { .. })
                })
                .count();
            if let Some(status) = state.run_phase(&run_id) {
                saw_terminal = status.status.is_terminal();
            }
        }
    }

    assert_eq!(
        text_items, 0,
        "the gateway now emits projection `text` items; the widget can render \
         the reply from the stream and no longer needs to poll the timeline"
    );
    // The reply exists — it is simply somewhere else.
    let timeline = client.timeline(&thread_id, None).await.expect("timeline");
    assert!(timeline.latest_assistant_reply().is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replaying_a_client_action_id_does_not_run_the_turn_twice() {
    let server = RebornServer::start().await;
    let client = client(&server);
    let thread_id = client.create_thread().await.expect("create thread");

    // The idempotency key is what makes a retry after a dropped connection safe.
    let action = ClientActionId::new();
    let first = client
        .send_message(&thread_id, "hello", &action)
        .await
        .expect("first send");
    let SubmitOutcome::Submitted { run_id } = first else {
        panic!("expected the first send to be admitted, got {first:?}");
    };

    // The replay's *answer* depends on a race the caller cannot control: while
    // the original message is still `Submitted` the gateway returns
    // `already_submitted` (reborn_services.rs:711), but once the turn reaches a
    // terminal state the identical replay is refused with 409
    // (reborn_services.rs:741). Both outcomes mean the same thing, and the
    // widget must not show the user an error for either.
    match client.send_message(&thread_id, "hello", &action).await {
        Ok(SubmitOutcome::AlreadySubmitted {
            run_id: replayed_run,
        }) => assert_eq!(replayed_run, run_id, "the replay named a different run"),
        Err(error) => assert!(
            error.is_duplicate_action(),
            "a replay must be a duplicate-action conflict, got {error:?}"
        ),
        Ok(other) => panic!("a replay must not start a second turn, got {other:?}"),
    }

    // Whichever branch ran, the invariant is the same: the message was accepted
    // exactly once.
    let timeline = client.timeline(&thread_id, None).await.expect("timeline");
    let user_messages = timeline
        .messages
        .iter()
        .filter(|message| message.kind == MessageKind::User)
        .count();
    assert_eq!(user_messages, 1, "the replay duplicated the user message");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_a_finished_run_reports_it_was_already_terminal() {
    let server = RebornServer::start().await;
    let client = client(&server);
    let thread_id = client.create_thread().await.expect("create thread");

    let outcome = client
        .send_message(&thread_id, "hello", &ClientActionId::new())
        .await
        .expect("send");
    let run_id = outcome.run_id().clone();
    await_run_completion(&client, &thread_id, &run_id).await;

    // The Stop button races the answer. The gateway must say so rather than
    // erroring, or the UI would show a failure for a turn that succeeded.
    let cancelled = client
        .cancel_run(&thread_id, &run_id)
        .await
        .expect("cancel a finished run");
    assert_eq!(cancelled.run_id, run_id);
    assert!(cancelled.already_terminal);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bad_token_is_a_401_with_the_gateways_own_error_taxonomy() {
    let server = RebornServer::start().await;
    let client = GatewayClient::new(&server.base_url, "not-the-token").expect("client");

    let error = client.health().await.expect_err("a wrong token must fail");
    let Error::Gateway {
        status,
        code,
        retryable,
        ..
    } = &error
    else {
        panic!("expected a gateway error, got {error:?}");
    };
    assert_eq!(*status, 401);
    assert!(error.is_unauthorized());
    assert!(!retryable, "a wrong token will not fix itself");
    assert!(!error.is_retryable());
    // The auth middleware answers with a bare text body, not the JSON error
    // shape every other route uses (`webui_serve.rs:737`), so the client falls
    // back to the status. If upstream ever aligns it, this assertion tells us.
    assert_eq!(
        code, "unknown",
        "the 401 body is now structured; the client can surface its code"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_thread_that_does_not_exist_is_a_typed_error_not_a_panic() {
    let server = RebornServer::start().await;
    let client = client(&server);

    let absent = ic_widget::ThreadId::new("00000000-0000-0000-0000-000000000000").expect("valid");
    let error = client
        .timeline(&absent, None)
        .await
        .expect_err("an unknown thread must fail");
    let Error::Gateway { status, .. } = error else {
        panic!("expected a gateway error, got {error:?}");
    };
    assert!(
        matches!(status, 403 | 404),
        "expected the gateway to deny or not-find the thread, got {status}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listing_threads_returns_the_one_we_created() {
    let server = RebornServer::start().await;
    let client = client(&server);

    let thread_id = client.create_thread().await.expect("create thread");
    let threads = client.list_threads(Some(50)).await.expect("list");
    assert!(
        threads.iter().any(|thread| thread.thread_id == thread_id),
        "created thread {thread_id} not in {threads:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_event_stream_resumes_from_its_cursor_after_a_reconnect() {
    let server = RebornServer::start().await;
    let client = client(&server);
    let thread_id = client.create_thread().await.expect("create thread");

    let outcome = client
        .send_message(&thread_id, "hello", &ClientActionId::new())
        .await
        .expect("send");
    let run_id = outcome.run_id().clone();

    // Read until we have a cursor, then drop the stream mid-run.
    let mut stream = client.events(thread_id.clone());
    let deadline = tokio::time::Instant::now() + RUN_TIMEOUT;
    while stream.cursor().is_none() {
        tokio::time::timeout_at(deadline, stream.next())
            .await
            .expect("an event should arrive")
            .expect("the stream should not end")
            .expect("a well-formed event");
    }
    let cursor = stream.cursor().expect("a cursor").to_string();
    drop(stream);

    // A fresh stream resumed from that cursor still observes the run finishing.
    let mut resumed = client.events(thread_id.clone()).resume_from(cursor);
    let mut saw_terminal = false;
    while !saw_terminal {
        let event = tokio::time::timeout_at(deadline, resumed.next())
            .await
            .expect("the resumed stream should see the run finish")
            .expect("the stream should not end")
            .expect("a well-formed event");
        if let GatewayEvent::ProjectionSnapshot(state) | GatewayEvent::ProjectionUpdate(state) =
            &event
            && let Some(status) = state.run_phase(&run_id)
        {
            saw_terminal = status.status.is_terminal();
        }
    }
    assert!(saw_terminal, "serve stderr:\n{}", server.stderr_snapshot());
}
