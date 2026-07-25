//! Phase 8g ⚠️ VERIFY: the memory-seed contract, and how a subagent shows up.
//!
//! 8g asks for two things and puts a VERIFY in front of each:
//!
//! 1. **Memory seeding** — verify "the exact seed/import capability contract
//!    in-agent before building UI". 8c already corrected this once:
//!    `memory_import` / `memory_seed` are not agent tools; the real write is
//!    `builtin.memory_write`. This drives that tool through a real gateway and
//!    reads the result back, because a schema is not a running runtime.
//! 2. **Subagent visibility** — verify "how it appears in the event stream —
//!    likely `capability_progress`". The `CapabilityProgress` variant exists in
//!    `WebChatV2Event`, but so does `Gate`, and 8d found that one has no
//!    producer under our profile. This captures the whole stream of a run that
//!    spawns a subagent and prints exactly what arrives.
#![cfg(feature = "webui-v2-beta")]

use std::sync::Arc;
use std::time::Duration;

use ic_integration_tests::{MockReply, MockResponder, RebornServer};

const SEED: &str = "MEMORY-SEED-CANARY";
const SPAWN: &str = "SUBAGENT-CANARY";
const DONE: &str = "verify-done";

/// Write a memory, then read it back on a second turn.
fn memory_responder() -> MockResponder {
    Arc::new(|body: &str| {
        // Second turn: read the seeded document straight back.
        if body.contains("READ-BACK") && !body.contains("\"role\":\"tool\"") {
            return MockReply::ToolCall {
                name: "builtin__memory_read".to_string(),
                arguments: serde_json::json!({ "path": "MEMORY.md" }),
            };
        }
        // Third turn: the semantic path, which is a separate question.
        if body.contains("SEARCH-BACK") && !body.contains("\"role\":\"tool\"") {
            return MockReply::ToolCall {
                name: "builtin__memory_search".to_string(),
                arguments: serde_json::json!({ "query": SEED }),
            };
        }
        if body.contains("\"role\":\"tool\"") {
            return MockReply::Text(DONE.to_string());
        }
        if body.contains(SEED) {
            return MockReply::ToolCall {
                name: "builtin__memory_write".to_string(),
                arguments: serde_json::json!({
                    "content": format!("The user's name is Wren. {SEED}"),
                    "target": "memory",
                    "append": true,
                }),
            };
        }
        MockReply::Text("fallthrough".to_string())
    })
}

/// What `builtin.memory_write` actually does under `local-dev serve`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn what_a_memory_seed_costs_and_whether_it_persists() {
    let server =
        RebornServer::start_scripted(memory_responder(), "unused".to_string(), Vec::new()).await;
    let thread = server.create_thread().await;
    server
        .send_message(&thread, &format!("{SEED} — remember this about me"))
        .await;
    let (done, stream) = server
        .stream_until(&thread, "\"status\":\"completed\"", Duration::from_secs(90))
        .await;
    println!("--- write run completed: {done} ---");
    if !done {
        println!(
            "stream:\n{stream}\n--- stderr ---\n{}",
            server.stderr_snapshot()
        );
    }
    let timeline = server.timeline(&thread).await;
    println!(
        "--- write timeline ---\n{}",
        serde_json::to_string_pretty(&timeline).unwrap_or_default()
    );

    // A second thread proves persistence *across conversations*, which is what
    // a seed has to survive — not just the turn that wrote it.
    let second = server.create_thread().await;
    server
        .send_message(&second, "READ-BACK — what do you know about me?")
        .await;
    let (found, _) = server
        .stream_until(&second, "\"status\":\"completed\"", Duration::from_secs(90))
        .await;
    println!("--- read-back run completed: {found} ---");
    let back = server.timeline(&second).await;
    println!(
        "--- read-back timeline ---\n{}",
        serde_json::to_string_pretty(&back).unwrap_or_default()
    );

    // And the semantic path, separately: a seed UI must not promise search if
    // search cannot run on this machine.
    let third = server.create_thread().await;
    server
        .send_message(&third, "SEARCH-BACK — search your memory")
        .await;
    let (searched, _) = server
        .stream_until(&third, "\"status\":\"completed\"", Duration::from_secs(90))
        .await;
    println!("--- search run completed: {searched} ---");
    let searched_timeline = server.timeline(&third).await;
    println!(
        "--- search timeline ---\n{}",
        serde_json::to_string_pretty(&searched_timeline).unwrap_or_default()
    );
}

/// Three distinct turns reach the mock and each needs its own answer: the
/// parent's opener (spawn), the child's own turn (it carries the task text), and
/// the parent's continuation (it carries the child's result summary). Matching on
/// the trigger word alone would make the child spawn a grandchild, because the
/// child's first request carries the parent's context.
fn subagent_responder() -> MockResponder {
    Arc::new(|body: &str| {
        if body.contains("Subagent completed") {
            return MockReply::Text(DONE.to_string());
        }
        if body.contains("Count to three") {
            return MockReply::Text("one, two, three".to_string());
        }
        MockReply::ToolCall {
            name: "builtin__spawn_subagent".to_string(),
            arguments: serde_json::json!({
                "flavor_id": "general",
                "task": "Count to three and report back.",
            }),
        }
    })
}

/// Everything the wire carries while a run spawns a subagent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn what_the_stream_says_when_a_subagent_runs() {
    let server =
        RebornServer::start_scripted(subagent_responder(), "unused".to_string(), Vec::new()).await;
    let thread = server.create_thread().await;
    server
        .send_message(&thread, &format!("{SPAWN} — delegate this"))
        .await;
    let (done, stream) = server
        .stream_until(
            &thread,
            "\"status\":\"completed\"",
            Duration::from_secs(120),
        )
        .await;
    println!("--- subagent run completed: {done} ---");
    for line in stream.lines() {
        if let Some(at) = line.find("\"failure_summary\"") {
            println!("FAILURE: {}", &line[at..line.len().min(at + 200)]);
        }
    }
    let requests = server.chat_requests();
    println!("--- {} chat requests ---", requests.len());
    for (index, request) in requests.iter().enumerate() {
        let tail: String = request
            .chars()
            .rev()
            .take(300)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        println!("[{index}] …{tail}");
    }
    for marker in [
        "capability_progress",
        "capability_activity",
        "subagent",
        "spawn_subagent",
        "child",
    ] {
        println!(
            "stream mentions {marker}: {}",
            stream.to_ascii_lowercase().contains(marker)
        );
    }
    let timeline = server.timeline(&thread).await;
    println!(
        "--- subagent timeline ---\n{}",
        serde_json::to_string_pretty(&timeline).unwrap_or_default()
    );
    let text = serde_json::to_string(&timeline).unwrap_or_default();
    for marker in ["spawn_subagent", "subagent", "flavor"] {
        println!(
            "timeline mentions {marker}: {}",
            text.to_ascii_lowercase().contains(marker)
        );
    }
    if !done {
        println!("--- stderr ---\n{}", server.stderr_snapshot());
    }
}

/// The smoke-run question the mock cannot answer: **does the model this product
/// actually ships with call the tool?**
///
/// The seed panel's whole design rests on it — `SeedOutcome::NotAttempted`
/// exists because a small local model will often reply "I'll remember that" and
/// call nothing. The mock-driven canary above proves the *contract*; this proves
/// the prompt survives contact with a 4B.
///
/// Ignored: it needs a running `llama-server` behind the `ic_llama` schema proxy.
/// Point `IC_LOCAL_LLM_BASE` at it (the widget logs the proxy port on startup)
/// and run:
///
/// ```text
/// IC_LOCAL_LLM_BASE=http://127.0.0.1:54533/v1 \
///   cargo test -p ic_integration_tests --features webui-v2-beta \
///   --test memory_and_subagent_verify -- --ignored a_real_local_model
/// ```
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a running local model; see the doc comment"]
async fn a_real_local_model_actually_calls_memory_write() {
    let base = std::env::var("IC_LOCAL_LLM_BASE")
        .expect("set IC_LOCAL_LLM_BASE to the running schema proxy");
    let server = RebornServer::start_with_llm(vec![
        ("LLM_BACKEND".to_string(), "openai_compatible".to_string()),
        ("LLM_BASE_URL".to_string(), base),
        ("LLM_API_KEY".to_string(), "not-used-locally".to_string()),
        ("LLM_MODEL".to_string(), "local".to_string()),
    ])
    .await;

    let thread = server.create_thread().await;
    // The exact prompt the widget sends, so this tests the shipped wording.
    let prompt = ic_widget::memory_seed::seed_prompt("My name is Wren and I prefer short answers.");
    server.send_message(&thread, &prompt).await;
    let (done, stream) = server
        .stream_until(
            &thread,
            "\"status\":\"completed\"",
            Duration::from_secs(300),
        )
        .await;
    let timeline = server.timeline(&thread).await;
    let text = serde_json::to_string(&timeline).unwrap_or_default();
    println!(
        "run completed: {done}\nstream tail: {}",
        &stream[stream.len().saturating_sub(400)..]
    );
    println!("timeline: {text}");
    // Distinguish the two failures, because they mean opposite things. A turn
    // that never finished says nothing about the prompt — on the first run of
    // this the sidecar was shared with a live widget and the message sat at
    // `submitted` for the whole 300s, which is an environment problem, not a
    // model verdict. Only a *completed* turn can convict the wording.
    assert!(
        done,
        "the turn never completed, so this run says nothing about the prompt — \
         give the model an uncontended sidecar and a longer timeout before \
         reading anything into it."
    );
    assert!(
        text.contains("builtin.memory_write"),
        "a completed turn on a real local model did NOT call memory_write — the \
         panel's NotAttempted path is then the common case, and the shipped \
         prompt needs work:\n{text}"
    );
}
