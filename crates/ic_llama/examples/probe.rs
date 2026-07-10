//! Phase 1 smoke tool: what does this machine look like, and can we run on it?
//!
//! ```text
//! # Hardware only — no network, no downloads.
//! cargo run -p ic_llama --example probe -- --root ./scratch
//!
//! # Also download and unpack the pinned llama.cpp build.
//! cargo run -p ic_llama --example probe -- --root ./scratch --install
//!
//! # Also start a model from the store and check it answers.
//! cargo run -p ic_llama --example probe -- --root ./scratch --model Qwen3-4B-Q4_K_M
//! ```
//!
//! `--model` expects the GGUF to already be in `<root>/models`. Fetch one with
//! `--pull <repo> <file>`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use ic_llama::download::Downloader;
use ic_llama::hardware::{probe_gpus, system_memory};
use ic_llama::models::{Digest, HubModel, ModelStore};
use ic_llama::placement::{PlacementPolicy, PlacementRequest, plan};
use ic_llama::{Backend, LLAMA_CPP_TAG, LlamaRuntime, LocalLlm, LocalLlmOptions, ModelId};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = PathBuf::from(flag(&args, "--root").unwrap_or_else(|| "./scratch".into()));
    let install = args.iter().any(|arg| arg == "--install");

    println!("== hardware ==");
    let gpus = probe_gpus()?;
    if gpus.is_empty() {
        println!("  no usable GPU");
    }
    for gpu in &gpus {
        println!(
            "  {} — {} MiB dedicated, {} MiB free of {} MiB budget{}",
            gpu.name,
            mib(gpu.dedicated_vram_bytes),
            mib(gpu.free_bytes()),
            mib(gpu.budget_bytes),
            if gpu.is_discrete() {
                ""
            } else {
                " (integrated)"
            }
        );
    }
    match system_memory()? {
        Some(memory) => println!(
            "  RAM — {} MiB available of {} MiB",
            mib(memory.available_bytes),
            mib(memory.total_bytes)
        ),
        None => println!("  RAM — unknown on this platform"),
    }

    let backend = match flag(&args, "--backend").as_deref() {
        Some("vulkan") => Backend::Vulkan,
        Some("cuda12") => Backend::Cuda12,
        Some("cpu") => Backend::Cpu,
        Some(other) => return Err(format!("unknown backend {other:?}").into()),
        None => Backend::recommended_for(&gpus),
    };
    println!(
        "\n== llama.cpp {LLAMA_CPP_TAG} / {} ({} MiB to download) ==",
        backend.as_str(),
        mib(backend.download_bytes())
    );

    let downloader = Downloader::new()?;

    if let (Some(repo), Some(file)) = (flag(&args, "--pull"), flag_at(&args, "--pull", 2)) {
        println!("\n== pulling {repo}/{file} ==");
        let store = ModelStore::new(&root, downloader.clone());
        let model = store
            .install(
                &HubModel::new(repo, file),
                Digest::FromHub,
                Some(progress()),
            )
            .await?;
        println!(
            "\n  installed {} ({} MiB)",
            model.id,
            mib(model.gguf.file_size)
        );
    }

    if install || flag(&args, "--model").is_some() {
        let started = Instant::now();
        let runtime =
            LlamaRuntime::install(&root, backend, &downloader, Some(install_progress())).await?;
        println!("\n  server binary: {}", runtime.server_bin.display());
        println!("  ready in {:?}", started.elapsed());
    }

    println!("\n== models in {} ==", root.join("models").display());
    let store = ModelStore::new(&root, downloader.clone());
    let models = store.list().await?;
    if models.is_empty() {
        println!("  none — pull one with --pull <repo> <file>");
    }
    let memory = system_memory()?;
    for model in &models {
        let placement = plan(PlacementRequest {
            model: &model.gguf,
            gpu: gpus.iter().find(|gpu| gpu.is_discrete()).or(gpus.first()),
            system: memory,
            ctx_size: 4096,
            policy: PlacementPolicy::default(),
        });
        println!(
            "  {} — {} {} blocks, {} MiB",
            model.id,
            model.gguf.architecture,
            model.gguf.block_count,
            mib(model.gguf.file_size)
        );
        println!(
            "      -ngl {} ({:?}), ~{} MiB VRAM, ~{} MiB RAM",
            placement.n_gpu_layers,
            placement.verdict,
            mib(placement.estimated_vram_bytes),
            mib(placement.estimated_host_bytes)
        );
        for warning in &placement.warnings {
            println!("      ! {warning}");
        }
        if let Some(reason) = &model.suspect {
            println!(
                "      ! suspect: {}",
                reason.lines().next().unwrap_or(reason)
            );
        }
    }

    if let Some(id) = flag(&args, "--model") {
        let id = ModelId::new(id)?;
        println!("\n== launching {id} ==");
        let started = Instant::now();
        let llm = LocalLlm::launch(
            &root,
            &id,
            LocalLlmOptions {
                backend: Some(backend),
                ..Default::default()
            },
        )
        .await?;
        println!("  ready in {:?}", started.elapsed());
        println!("  {:?}", llm.env());

        // Prove the OpenAI-compatible surface IronClaw will talk to is live.
        let response = reqwest::Client::new()
            .get(format!("{}/models", llm.sidecar().base_url()))
            .bearer_auth(llm.sidecar().api_key())
            .send()
            .await?;
        println!("  GET /v1/models -> {}", response.status());
        println!("  {}", response.text().await?);

        llm.stop().await;
        println!("  stopped");
    }

    Ok(())
}

fn progress() -> ic_llama::download::ProgressFn {
    Arc::new(|progress| {
        if let Some(fraction) = progress.fraction() {
            print!(
                "\r  {:.1}% ({} MiB)",
                fraction * 100.0,
                mib(progress.downloaded)
            );
        } else {
            print!("\r  {} MiB", mib(progress.downloaded));
        }
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
    })
}

fn install_progress() -> ic_llama::runtime::InstallProgressFn {
    Arc::new(|asset, progress| {
        let percent = progress.fraction().unwrap_or(0.0) * 100.0;
        print!("\r  {asset}: {percent:.1}%");
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
    })
}

/// `--flag value` → `Some(value)`.
fn flag(args: &[String], name: &str) -> Option<String> {
    flag_at(args, name, 1)
}

/// The `offset`-th value after `name`.
fn flag_at(args: &[String], name: &str, offset: usize) -> Option<String> {
    let index = args.iter().position(|arg| arg == name)?;
    args.get(index + offset).cloned()
}

fn mib(bytes: u64) -> u64 {
    bytes / (1 << 20)
}
