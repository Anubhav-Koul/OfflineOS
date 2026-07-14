//! Phase 8a: the chat-management contract, driven against a real `serve`.
//!
//! This file began as the ⚠️ VERIFY probe for 8a's two unknowns — *is there a
//! thread-delete route?* and *what does cancelling a run actually stop?* — and
//! stayed on as the contract gate for every field the Chats panel reads. On the
//! next upstream merge these assertions are the alarm.
//!
//! Contract verified against the pinned upstream commit `a492857`
//! (`reborn-integration`).
#![cfg(feature = "webui-v2-beta")]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ic_integration_tests::{API_PREFIX, RebornServer};

/// A message the mock answers slowly, so a cancel can land mid-generation.
const SLOW: &str = "CHAT-CONTROL-SLOW";
const ANSWER: &str = "the slow answer";

// ------------------------------------------------------------------ probe 1
// Is there any route that deletes or archives a thread?

/// Every plausible spelling of "get rid of this thread", against the real
/// router. `gateway-api-notes.md` §3 lists none — this proves it rather than
/// trusting the table, because the whole design of "Hide" rests on it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_serve_api_exposes_no_way_to_delete_or_archive_a_thread() {
    let server = RebornServer::start().await;
    let thread = server.create_thread().await;
    let client = reqwest::Client::new();
    let base = format!("{}{API_PREFIX}", server.base_url);

    // (method, path) — every shape the router could plausibly mount.
    let candidates = [
        ("DELETE", format!("/threads/{thread}")),
        ("POST", format!("/threads/{thread}/delete")),
        ("POST", format!("/threads/{thread}/archive")),
        ("POST", format!("/threads/{thread}/hide")),
        ("DELETE", format!("/threads/{thread}/messages")),
    ];

    let mut observed = Vec::new();
    for (method, path) in candidates {
        let method = reqwest::Method::from_bytes(method.as_bytes()).expect("a valid method");
        let status = client
            .request(method.clone(), format!("{base}{path}"))
            .bearer_auth(&server.token)
            .json(&serde_json::json!({ "client_action_id": uuid::Uuid::new_v4().to_string() }))
            .send()
            .await
            .expect("the gateway should answer")
            .status();
        observed.push((method.to_string(), path.clone(), status));

        // 404 = no such route; 405 = the path exists but not for this method.
        // Anything else would mean a delete surface we did not know about, and
        // the honest-Hide design would be the wrong call.
        assert!(
            status == reqwest::StatusCode::NOT_FOUND
                || status == reqwest::StatusCode::METHOD_NOT_ALLOWED,
            "{method} {path} answered {status} — a thread-removal route may exist after all; \
             re-read gateway-api-notes §3 before shipping \u{201c}Hide\u{201d}"
        );
    }
    eprintln!("probe: thread-removal candidates → {observed:?}");

    // And the thread is still listed, which is the point: nothing removes it.
    assert!(
        server.thread_ids().await.contains(&thread),
        "the thread should survive every removal attempt"
    );
}

// ------------------------------------------------------------------ probe 2
// What does cancelling a run actually stop?

/// A mock LLM that holds the chat-completions request open, and reports whether
/// the *client* (the gateway) hung up before it answered.
///
/// That is the question the Stop button turns on: if the gateway aborts its
/// in-flight HTTP request to the provider, the `ic_llama` proxy's own upstream
/// request dies with it and `llama-server` stops generating (llama.cpp aborts a
/// completion when its client disconnects). If the gateway instead waits for the
/// provider to finish, the GPU keeps burning tokens nobody will ever read.
struct SlowLlm {
    port: u16,
    /// Set when a chat-completions request was aborted by the client mid-answer.
    aborted: Arc<Mutex<bool>>,
    /// Set when a chat-completions request was answered in full.
    answered: Arc<Mutex<bool>>,
    _handle: tokio::task::JoinHandle<()>,
}

impl SlowLlm {
    async fn start(hold: Duration) -> SlowLlm {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the slow mock");
        let port = listener.local_addr().expect("addr").port();
        let aborted = Arc::new(Mutex::new(false));
        let answered = Arc::new(Mutex::new(false));
        let aborted_sink = Arc::clone(&aborted);
        let answered_sink = Arc::clone(&answered);

        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let aborted = Arc::clone(&aborted_sink);
                let answered = Arc::clone(&answered_sink);
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut chunk = [0u8; 4096];
                    // Read the head; that is enough to know which route it is.
                    loop {
                        let Ok(read) = socket.read(&mut chunk).await else {
                            return;
                        };
                        if read == 0 {
                            return;
                        }
                        request.extend_from_slice(&chunk[..read]);
                        if request.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let head = String::from_utf8_lossy(&request).to_string();
                    let is_chat = head.contains("chat/completions");

                    let body = if is_chat {
                        // Hold the answer. If the gateway hangs up while we wait,
                        // the socket reads EOF — that is the abort signal.
                        let hung_up = tokio::select! {
                            read = socket.read(&mut chunk) => matches!(read, Ok(0)),
                            () = tokio::time::sleep(hold) => false,
                        };
                        if hung_up {
                            *aborted.lock().expect("lock") = true;
                            return; // nothing to answer; the client is gone
                        }
                        *answered.lock().expect("lock") = true;
                        serde_json::json!({
                            "id": "chatcmpl-slow",
                            "object": "chat.completion",
                            "created": 0,
                            "model": "mock-model",
                            "choices": [{
                                "index": 0,
                                "message": {"role": "assistant", "content": ANSWER},
                                "finish_reason": "stop"
                            }],
                            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                        })
                        .to_string()
                    } else {
                        serde_json::json!({
                            "object": "list",
                            "data": [{"id": "mock-model", "object": "model", "owned_by": "mock"}]
                        })
                        .to_string()
                    };

                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.flush().await;
                });
            }
        });

        SlowLlm {
            port,
            aborted,
            answered,
            _handle: handle,
        }
    }

    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    fn aborted(&self) -> bool {
        *self.aborted.lock().expect("lock")
    }

    fn answered(&self) -> bool {
        *self.answered.lock().expect("lock")
    }
}

/// Cancel a run that is genuinely in flight, and observe **everything** it does:
/// the HTTP answer, the projection stream, and — the question that matters for a
/// local GPU — whether the provider's in-flight request is abandoned.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_an_in_flight_run_reports_terminal_and_what_it_does_to_the_provider() {
    // Hold each completion for 30 s: far longer than the cancel round-trip, so
    // the run is unambiguously mid-generation when Stop is pressed.
    let llm = SlowLlm::start(Duration::from_secs(30)).await;
    let server = RebornServer::start_with_llm(vec![
        ("LLM_BACKEND".into(), "openai_compatible".into()),
        ("LLM_BASE_URL".into(), llm.base_url()),
        ("LLM_API_KEY".into(), "test-key".into()),
        ("LLM_MODEL".into(), "mock-model".into()),
        ("LLM_MAX_RETRIES".into(), "0".into()),
    ])
    .await;

    let thread = server.create_thread().await;
    let run_id = server.send_message(&thread, SLOW).await;

    // Let it get properly under way before stopping it.
    let (running, stream) = server
        .stream_until(&thread, "\"status\":\"running\"", Duration::from_secs(30))
        .await;
    assert!(running, "the run should reach `running`:\n{stream}");

    let cancel = server.cancel_run(&thread, &run_id).await;
    eprintln!("probe: cancel response = {cancel}");
    assert_eq!(
        cancel["already_terminal"], false,
        "a run cancelled mid-flight is not already terminal"
    );
    // Every field the UI reads must be present and typed as expected.
    assert_eq!(cancel["run_id"], run_id.as_str());
    assert!(cancel["status"].is_string(), "cancel carries a run status");

    // The projection stream must show it terminal, or the widget would sit on
    // "thinking" forever.
    let (terminal, stream) = server
        .stream_until(&thread, "\"status\":\"cancelled\"", Duration::from_secs(60))
        .await;
    assert!(
        terminal,
        "the run must go terminal on the stream after a cancel:\n{stream}\n--- stderr ---\n{}",
        server.stderr_snapshot()
    );

    // THE question: did the gateway abandon its in-flight provider request?
    // Give it a moment to propagate, but far less than the 30 s the mock holds.
    tokio::time::sleep(Duration::from_secs(3)).await;
    eprintln!(
        "probe: after cancel — provider request aborted = {}, answered = {}",
        llm.aborted(),
        llm.answered()
    );
    assert!(
        !llm.answered(),
        "the mock cannot have answered yet — it holds for 30 s and we waited 3"
    );
    // Recorded, not asserted: this is the observation the progress note reports.
    // Either way the UI behaves the same (a "stopping…" state, then terminal);
    // what changes is whether a local GPU keeps generating into the void.
    if llm.aborted() {
        eprintln!(
            "probe: VERDICT — cancel ABORTS the provider request; llama-server \
             would stop generating with it"
        );
    } else {
        eprintln!(
            "probe: VERDICT — cancel leaves the provider request in flight; the \
             GPU keeps generating an answer nobody will read"
        );
    }
}

/// The race the Stop button lives in: the answer lands while the click is in the
/// air. This must be a *success* with `already_terminal: true`, never an error
/// the user sees — `chat.ts` refreshes the reply on it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_a_run_that_already_finished_is_success_not_an_error() {
    let server = RebornServer::start().await;
    let thread = server.create_thread().await;
    let run_id = server.send_message(&thread, "hello").await;

    // Let it finish completely.
    let (done, _) = server
        .stream_until(&thread, "\"status\":\"completed\"", Duration::from_secs(60))
        .await;
    assert!(done, "the run should complete");

    let (status, body) = server.cancel_run_raw(&thread, &run_id).await;
    eprintln!("probe: cancel-after-terminal → {status}: {body}");
    assert!(
        status.is_success(),
        "cancelling a finished run must not be an error the user sees — it is the \
         common race when the reply lands as Stop is clicked (got {status}: {body})"
    );
    assert_eq!(
        body["already_terminal"], true,
        "and it must say so, so the UI can collect the reply instead of complaining"
    );
}

/// A run id that never existed. The UI must treat this as "refresh", not as a
/// dialog — a stale id survives a gateway restart in a way the run does not.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_an_unknown_run_is_a_clean_404() {
    let server = RebornServer::start().await;
    let thread = server.create_thread().await;

    let (status, body) = server
        .cancel_run_raw(&thread, "00000000-0000-0000-0000-000000000000")
        .await;
    eprintln!("probe: cancel-unknown-run → {status}: {body}");
    assert_eq!(
        status,
        reqwest::StatusCode::NOT_FOUND,
        "an unknown run should be a 404 the UI can silently refresh on"
    );
}

// ------------------------------------------------------------------ contract
// Every field the Chats panel reads, from the real gateway.

/// `GET /threads` — the list the Chats panel renders, and its paging.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_threads_list_carries_what_the_chats_panel_renders() {
    let server = RebornServer::start().await;
    let first = server.create_thread().await;
    let second = server.create_thread().await;
    server.send_message(&second, "hello").await;

    let body = server.threads_raw(None, None).await;
    let threads = body["threads"].as_array().expect("threads is an array");
    assert!(threads.len() >= 2, "both threads should list: {body}");

    // The one field the panel cannot do without.
    for thread in threads {
        assert!(
            thread["thread_id"].is_string(),
            "every row needs a thread_id: {thread}"
        );
    }
    let ids: Vec<&str> = threads
        .iter()
        .filter_map(|thread| thread["thread_id"].as_str())
        .collect();
    assert!(ids.contains(&first.as_str()) && ids.contains(&second.as_str()));

    // Paging: `limit` is honoured and `next_cursor` is present-or-null (the
    // panel treats null as "end of list").
    let page = server.threads_raw(Some(1), None).await;
    assert_eq!(
        page["threads"].as_array().map(Vec::len),
        Some(1),
        "limit=1 must return one row: {page}"
    );
    assert!(
        page.get("next_cursor").is_some(),
        "next_cursor must exist (null is fine): {page}"
    );

    // And a second page through the cursor, when the gateway offers one.
    if let Some(cursor) = page["next_cursor"].as_str() {
        let next = server.threads_raw(Some(1), Some(cursor)).await;
        let next_ids: Vec<&str> = next["threads"]
            .as_array()
            .expect("array")
            .iter()
            .filter_map(|thread| thread["thread_id"].as_str())
            .collect();
        assert!(
            !next_ids.is_empty(),
            "a cursor the gateway handed us must page: {next}"
        );
        eprintln!("probe: threads paged via cursor → {next_ids:?}");
    } else {
        eprintln!("probe: GET /threads returned next_cursor = null with 2 threads and limit=1");
    }
}

/// `GET /threads/{id}/timeline` — the history the Chats panel replays.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_timeline_carries_what_the_history_view_renders() {
    let server = RebornServer::start().await;
    let thread = server.create_thread().await;
    server.send_message(&thread, "hello").await;
    let (found, _) = server
        .wait_for_timeline_text(&thread, &server.answer, Duration::from_secs(60))
        .await;
    assert!(found, "the reply should land");

    let body = server.timeline(&thread).await;
    let messages = body["messages"].as_array().expect("messages is an array");
    assert!(messages.len() >= 2, "user + assistant: {body}");

    // The three fields the renderer reads. `content` is nullable — a tool-result
    // message has none, and the panel must render it as a neutral row rather
    // than crash on the null.
    let mut kinds = Vec::new();
    for message in messages {
        assert!(message["kind"].is_string(), "every message has a kind: {message}");
        assert!(
            message["sequence"].is_number(),
            "every message has a sequence: {message}"
        );
        assert!(
            message.get("content").is_some(),
            "content must be present, even when null: {message}"
        );
        kinds.push(message["kind"].as_str().unwrap_or("?").to_string());
    }
    eprintln!("probe: timeline message kinds = {kinds:?}");
    assert!(kinds.iter().any(|kind| kind == "user"));
    assert!(kinds.iter().any(|kind| kind == "assistant"));

    // Paging: `next_cursor` is **omitted entirely** when there is no next page —
    // it is not serialized as `null`. A client that reads it as a required field
    // (or as "null means end") breaks on the common case, so the panel treats
    // absent and null identically: end of history.
    eprintln!(
        "probe: timeline next_cursor present = {}",
        body.get("next_cursor").is_some()
    );
    assert!(
        body["next_cursor"].as_str().is_none(),
        "a short history has no next page: {body}"
    );

    // And when a page *is* cut short, the cursor appears and pages.
    let page = server.timeline_raw(&thread, Some(1), None).await;
    assert_eq!(
        page["messages"].as_array().map(Vec::len),
        Some(1),
        "limit=1 must return one message: {page}"
    );
    if let Some(cursor) = page["next_cursor"].as_str() {
        let next = server.timeline_raw(&thread, Some(1), Some(cursor)).await;
        assert!(
            next["messages"].as_array().is_some_and(|m| !m.is_empty()),
            "a cursor the gateway handed us must page: {next}"
        );
        eprintln!("probe: timeline paged via cursor");
    } else {
        eprintln!("probe: timeline limit=1 returned no next_cursor (paging may be one-shot)");
    }
}
