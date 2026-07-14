//! Phase 8b: how many tools can the local model actually hold?
//!
//! The Connectors panel warns the user when the active toolset gets large, and a
//! warning threshold is a number somebody has to *choose*. This test is where the
//! number comes from, because the alternative is a constant with a comment
//! claiming a measurement that never happened.
//!
//! The experiment: ask a 4B local model (Qwen3-4B-Q4_K_M, the model this fork
//! ships as its default) questions that need **no tool at all**, first with the
//! runtime's built-in toolset and then with a registry connector's 34 tools added
//! on top. A model that is drowning does not fail loudly — it starts reaching for
//! tools it does not need, or stops answering the question in front of it. Both
//! are visible here: the answer text, whether the turn terminated, and how long it
//! took.
//!
//! # What it found first was not a slope. It was a cliff.
//!
//! With GitHub installed, the model answered **nothing**: 0/3, every turn running
//! the full 240 s timeout without terminating. The cause was not the model. GitHub's
//! `owner`/`repo` properties carry `pattern: "[^\\s/?#]+"`; llama.cpp transcribes a
//! JSON-Schema `pattern` into GBNF verbatim; GBNF has no `\s`. So llama.cpp answered
//! `400 failed to parse grammar` — to **every** request, because all of a turn's
//! tools compile into one grammar — and the agent loop retried forever.
//!
//! That is fixed in `ic_llama`'s SchemaProxy (the CP-3 lane), which now drops a
//! `pattern` it cannot prove GBNF can express, exactly as it already drops an
//! oversized repetition bound. The same question then answers correctly in 9.2 s.
//! **This test is the reason we know that**, and it stays as the regression: if the
//! repair is ever weakened, the flooded half goes back to `STUCK` and the assertion
//! at the bottom fires.
//!
//! It is `#[ignore]`d: it needs multi-gigabyte weights and takes minutes.
//!
//! ```bash
//! cargo build -p ironclaw_reborn_cli --bin ironclaw-reborn --features webui-v2-beta
//!
//! IC_LLAMA_ROOT="$LOCALAPPDATA/IronClaw Desktop/llama" IC_LLAMA_MODEL=Qwen3-4B-Q4_K_M \
//!   cargo test -p ic_integration_tests --features webui-v2-beta --test tool_flood \
//!   -- --ignored --nocapture
//! ```
#![cfg(feature = "webui-v2-beta")]

use std::time::{Duration, Instant};

use ic_integration_tests::RebornServer;
use ic_llama::{LocalLlm, LocalLlmOptions, ModelId};

/// The registry connector with the biggest toolset we can install — 34 tools, and
/// the one a user is most likely to add first.
const CONNECTOR: &str = "github";
const API_PREFIX: &str = "/api/webchat/v2";

/// GitHub's 34 schemas are large. 16 KiB (what the Phase 1 round-trip uses) is not
/// enough to hold the system prompt *and* 62 tool schemas, and a context overflow
/// would be measuring the wrong thing — we want to see the model choose badly, not
/// see it truncated. This is deliberately generous.
const CTX_SIZE: u32 = 32_768;

/// A 4B model handed 62 schemas spends real time on prompt evaluation before it
/// says anything.
const REPLY_TIMEOUT: Duration = Duration::from_secs(240);

/// Questions with an unambiguous right answer that **no tool can help with**. If a
/// tool call shows up in one of these turns, the model reached for a tool it did
/// not need — which is the failure mode the warning exists to predict.
const QUESTIONS: &[(&str, &str)] = &[
    (
        "What is 21 multiplied by 2? Reply with only the number.",
        "42",
    ),
    (
        "What is the capital of France? Reply with one word.",
        "Paris",
    ),
    (
        "Reply with exactly one word and nothing else: pineapple",
        "pineapple",
    ),
    (
        "Name the largest planet in our solar system. One word.",
        "Jupiter",
    ),
];

struct Round {
    question: &'static str,
    expected: &'static str,
    answer: String,
    elapsed: Duration,
    completed: bool,
}

impl Round {
    /// The model is graded on the answer it gave, not on how it got there — a
    /// correct answer after a needless tool call is still a correct answer, and the
    /// timing shows the cost.
    fn correct(&self) -> bool {
        self.answer
            .to_lowercase()
            .contains(&self.expected.to_lowercase())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a real GGUF on disk and several minutes; see the module docs"]
async fn a_local_model_is_measured_with_and_without_a_connectors_tools() {
    let root = std::env::var("IC_LLAMA_ROOT")
        .expect("set IC_LLAMA_ROOT to the directory holding models/ and runtimes/");
    let model = ModelId::new(
        std::env::var("IC_LLAMA_MODEL")
            .expect("set IC_LLAMA_MODEL to the model id, e.g. Qwen3-4B-Q4_K_M"),
    )
    .expect("IC_LLAMA_MODEL must be a valid model id");

    let llm = LocalLlm::launch(
        root.as_ref(),
        &model,
        LocalLlmOptions {
            ctx_size: Some(CTX_SIZE),
            ..Default::default()
        },
    )
    .await
    .expect("llama-server should start");
    let llm_env: Vec<(String, String)> = llm
        .env()
        .vars()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
    eprintln!(
        "llama-server: {} backend, -ngl {}, ctx {CTX_SIZE}",
        llm.backend().as_str(),
        llm.placement().n_gpu_layers
    );

    // One home across both halves of the experiment: the connector is installed
    // into it between them, so the *only* difference the model sees is the toolset.
    let home = tempfile::tempdir().expect("home");
    let http = reqwest::Client::new();

    // ---- Half 1: the built-in toolset --------------------------------------
    let server = RebornServer::start_with_llm_in_home(llm_env.clone(), home.path()).await;
    let before = ask_all(&server).await;
    drop(server); // it holds the libSQL write lock; the next one needs it.

    // ---- Install the connector, then measure again --------------------------
    let server = RebornServer::start_with_llm_in_home(llm_env.clone(), home.path()).await;
    let base = format!("{}{API_PREFIX}", server.base_url);
    install_connector(&http, &base, &server.token).await;
    let capabilities = capability_count(&http, &base, &server.token).await;
    eprintln!("{CONNECTOR} is active with {capabilities} capabilities");
    assert!(
        capabilities >= 30,
        "the connector did not activate with its tools ({capabilities}); \
         the measurement below would be meaningless"
    );
    let after = ask_all(&server).await;

    // A turn that never terminates is not "the model chose badly" — it is a
    // different failure, and the warning we write depends on which. So when the
    // flooded half gets stuck, say where: the model's own output tail shows whether
    // llama.cpp is still compiling a grammar, evaluating an enormous prompt, or
    // looping tool calls.
    if after.iter().any(|round| !round.completed) {
        eprintln!(
            "\n--- llama-server tail (a flooded turn did not terminate) ---\n{}",
            llm.sidecar().output_tail()
        );
        eprintln!(
            "\n--- serve stderr tail ---\n{}",
            truncate(&server.stderr_snapshot(), 3000)
        );
    }
    drop(server);
    llm.stop().await;

    // ---- The finding --------------------------------------------------------
    report("built-in tools only", &before);
    report(&format!("built-in tools + {CONNECTOR}"), &after);

    let before_ok = before.iter().filter(|round| round.correct()).count();
    let after_ok = after.iter().filter(|round| round.correct()).count();
    eprintln!(
        "\ncorrect answers: {before_ok}/{} before, {after_ok}/{} after",
        before.len(),
        after.len()
    );

    // The bar this test holds: the *baseline* must be sound, or the comparison
    // says nothing. A 4B model that cannot answer "what is 21 × 2" with 28 tools
    // in front of it is broken for reasons that have nothing to do with flooding.
    assert_eq!(
        before_ok,
        QUESTIONS.len(),
        "the model failed questions it should answer with the built-in toolset alone — \
         the flood comparison is not measuring what it claims to"
    );

    // The regression the cliff left behind: with the connector's tools in the
    // prompt, the model must still *terminate*. A stuck turn here means the
    // SchemaProxy stopped repairing something llama.cpp cannot compile, and the
    // symptom the user sees is not "worse answers" — it is an agent that never
    // replies again for as long as the connector is installed.
    assert!(
        after.iter().all(|round| round.completed),
        "a flooded turn did not terminate: llama.cpp is refusing the tool grammar \
         again (see the llama-server tail above, and `ic_llama::proxy`)"
    );

    // Deliberately NOT asserted: that `after_ok < before_ok`. This test's job is to
    // *produce* the number in the warning, not to demand that the model degrade.
    // Read the printout, then set `TOOL_FLOOD_WARNING` in `ui/src/dashboard.tsx`
    // from what it says — and if the model held up fine, raise the threshold rather
    // than keep a warning that cries wolf.
}

async fn ask_all(server: &RebornServer) -> Vec<Round> {
    let mut rounds = Vec::new();
    for (question, expected) in QUESTIONS {
        let started = Instant::now();
        let thread = server.create_thread().await;
        server.send_message(&thread, question).await;
        let (completed, _) = server
            .stream_until(&thread, "\"status\":\"completed\"", REPLY_TIMEOUT)
            .await;
        let answer = last_assistant_text(&server.timeline(&thread).await);
        rounds.push(Round {
            question,
            expected,
            answer,
            elapsed: started.elapsed(),
            completed,
        });
    }
    rounds
}

fn report(label: &str, rounds: &[Round]) {
    eprintln!("\n=== {label} ===");
    for round in rounds {
        eprintln!(
            "  [{}] {:>6.1}s  q: {}\n           a: {}",
            if round.correct() {
                "ok  "
            } else if round.completed {
                "WRONG"
            } else {
                "STUCK"
            },
            round.elapsed.as_secs_f32(),
            round.question,
            truncate(round.answer.trim(), 160),
        );
    }
}

fn last_assistant_text(timeline: &serde_json::Value) -> String {
    timeline["messages"]
        .as_array()
        .map(|messages| {
            messages
                .iter()
                .rev()
                .filter(|message| message["kind"] == "assistant")
                .filter_map(|message| message["content"].as_str())
                .find(|content| !content.trim().is_empty())
                .unwrap_or_default()
                .to_string()
        })
        .unwrap_or_default()
}

/// Install + credential + activate, exactly as `connector_verify.rs` proved the
/// lane works. The token is deliberately junk: these questions never touch GitHub,
/// and what is being measured is the *presence of the schemas* in the prompt.
async fn install_connector(http: &reqwest::Client, base: &str, token: &str) {
    let install = http
        .post(format!("{base}/extensions/install"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "package_ref": { "kind": "extension", "id": CONNECTOR },
        }))
        .send()
        .await
        .expect("install should answer");
    assert!(
        install.status().is_success(),
        "install: {}",
        install.status()
    );

    let setup: serde_json::Value = http
        .post(format!(
            "{}/api/reborn/product-auth/manual-token/setup",
            base.trim_end_matches(API_PREFIX)
        ))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "provider": CONNECTOR,
            "account_label": "ironclaw-desktop",
        }))
        .send()
        .await
        .expect("manual-token setup")
        .json()
        .await
        .expect("setup json");
    let submit = http
        .post(format!(
            "{}/api/reborn/product-auth/manual-token/secret-submit",
            base.trim_end_matches(API_PREFIX)
        ))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "interaction_id": setup["interaction_id"],
            "invocation_id": setup["invocation_id"],
            "token": "ghp_measurement_only_never_used",
        }))
        .send()
        .await
        .expect("secret-submit");
    assert!(
        submit.status().is_success(),
        "secret-submit: {}",
        submit.status()
    );

    let activate = http
        .post(format!("{base}/extensions/{CONNECTOR}/activate"))
        .bearer_auth(token)
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("activate should answer");
    let status = activate.status();
    let body: serde_json::Value = activate.json().await.unwrap_or(serde_json::Value::Null);
    eprintln!("activate → {status}: {body}");
    assert!(status.is_success(), "activate: {status}");
}

/// The capability ids live under `summary.visible_capability_ids` — the same shape
/// `connector_verify.rs` pinned, and the same place an earlier parser looked for in
/// vain while reporting "0 capabilities" for an extension that had 34.
async fn capability_count(http: &reqwest::Client, base: &str, token: &str) -> usize {
    let installed: serde_json::Value = http
        .get(format!("{base}/extensions"))
        .bearer_auth(token)
        .send()
        .await
        .expect("list extensions")
        .json()
        .await
        .unwrap_or(serde_json::Value::Null);
    let entry = installed["extensions"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|entry| entry["package_ref"]["id"] == CONNECTOR)
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    eprintln!(
        "GET /extensions entry keys → {:?}",
        entry.as_object().map(|map| map.keys().collect::<Vec<_>>())
    );
    entry["tools"].as_array().map(|ids| ids.len()).unwrap_or(0)
}

fn truncate(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}
