//! Phase 1 acceptance: an agent round-trip on a real GGUF, fully offline.
//!
//! Everything in `ic_llama` is unit- and integration-tested against fakes. This
//! is the one test that proves the *whole* claim of Phase 1: that a local model
//! can drive IronClaw's agent loop with no changes to any core crate, no network,
//! and no API key.
//!
//! ```text
//! ic_llama::LocalLlm  ──spawns──▶  llama-server (llama.cpp, real weights)
//!         │                                  ▲
//!         │ LLM_BACKEND=openai_compatible    │ POST /v1/chat/completions
//!         │ LLM_BASE_URL=http://127.0.0.1:…  │
//!         ▼                                  │
//! ironclaw-reborn serve  ──agent loop──────▶─┘
//!         ▲
//!         │ WebChat v2: create thread, send message, stream SSE
//!      this test
//! ```
//!
//! It is `#[ignore]`d because it needs multi-gigabyte weights on disk. Run it
//! after pulling a model:
//!
//! ```bash
//! cargo run -p ic_llama --example probe -- --root <root> \
//!     --pull Qwen/Qwen3-4B-GGUF Qwen3-4B-Q4_K_M.gguf
//!
//! cargo build -p ironclaw_reborn_cli --bin ironclaw-reborn --features webui-v2-beta
//!
//! IC_LLAMA_ROOT=<root> IC_LLAMA_MODEL=Qwen3-4B-Q4_K_M \
//!   cargo test -p ic_integration_tests --features webui-v2-beta \
//!   -- --ignored local_model_drives_the_agent_loop --nocapture
//! ```
#![cfg(feature = "webui-v2-beta")]

use std::time::{Duration, Instant};

use ic_integration_tests::RebornServer;
use ic_llama::{LocalLlm, LocalLlmOptions, ModelId};

/// Big enough for IronClaw's system prompt plus its built-in tool schemas, which
/// together overflow `llama-server`'s 4096-token default.
const CTX_SIZE: u32 = 16_384;

/// A local 4B model on a warm cache answers in seconds, but a cold page cache
/// and a partial offload can make the first token take a while.
const REPLY_TIMEOUT: Duration = Duration::from_secs(300);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a real GGUF on disk; see the module docs"]
async fn local_model_drives_the_agent_loop() {
    let root = std::env::var("IC_LLAMA_ROOT")
        .expect("set IC_LLAMA_ROOT to the directory holding models/ and runtimes/");
    let model = ModelId::new(
        std::env::var("IC_LLAMA_MODEL")
            .expect("set IC_LLAMA_MODEL to the model id, e.g. Qwen3-4B-Q4_K_M"),
    )
    .expect("IC_LLAMA_MODEL must be a valid model id");

    // 1. Start llama.cpp on the real weights, offloading as much as fits.
    let started = Instant::now();
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

    let placement = llm.placement();
    println!(
        "llama-server ready in {:?}: {} backend, -ngl {} ({:?}), ~{} MiB VRAM",
        started.elapsed(),
        llm.backend().as_str(),
        placement.n_gpu_layers,
        placement.verdict,
        placement.estimated_vram_bytes >> 20
    );
    for warning in &placement.warnings {
        println!("  warning: {warning}");
    }

    // 2. Point `ironclaw-reborn serve` at it. This is the entire integration:
    //    four environment variables, zero core-crate changes.
    let llm_env: Vec<(String, String)> = llm
        .env()
        .vars()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
    assert_eq!(
        llm_env
            .iter()
            .find(|(name, _)| name == "LLM_BACKEND")
            .map(|(_, value)| value.as_str()),
        Some("openai_compatible")
    );

    let server = RebornServer::start_with_llm(llm_env).await;

    // 3. Drive the same chat contract the Phase 0 gate drives, but against real
    //    weights. The question is chosen so the answer is a token that cannot
    //    appear in the prompt, the system prompt, or the tool schemas.
    let thread_id = server.create_thread().await;
    server
        .send_message(
            &thread_id,
            "What is 21 multiplied by 2? Reply with only the number.",
        )
        .await;

    let (completed, stream) = server
        .stream_until(&thread_id, "\"status\":\"completed\"", REPLY_TIMEOUT)
        .await;
    assert!(
        completed,
        "the run never reached `completed`.\n--- SSE ---\n{stream}\n--- serve stderr ---\n{}\n\
         --- llama-server output ---\n{}",
        server.stderr_snapshot(),
        llm.sidecar().output_tail()
    );

    let (found, timeline) = server
        .wait_for_timeline_text(&thread_id, "42", Duration::from_secs(30))
        .await;
    assert!(
        found,
        "the model's answer never reached the timeline.\n--- timeline ---\n{timeline:#}\n\
         --- llama-server output ---\n{}",
        llm.sidecar().output_tail()
    );

    // 4. The sidecar survived the round-trip rather than crashing into a restart.
    assert!(
        llm.sidecar().state().is_ready(),
        "llama-server is {:?} after the turn",
        llm.sidecar().state()
    );

    println!("round-trip completed in {:?}", started.elapsed());
    llm.stop().await;
}
