//! The one call the widget makes: give me a local model, wired to IronClaw.
//!
//! [`LocalLlm::launch`] installs the pinned llama.cpp build, reads the model's
//! header, decides how much of it fits on the GPU, starts the supervised
//! sidecar, and hands back the four environment variables that make
//! `ironclaw-reborn` talk to it.

use std::path::{Path, PathBuf};

use crate::download::Downloader;
use crate::error::{Error, Result};
use crate::hardware::{GpuAdapter, probe_gpus, system_memory};
use crate::ids::ModelId;
use crate::models::{InstalledModel, ModelStore};
use crate::placement::{Placement, PlacementPolicy, PlacementRequest, plan};
use crate::proxy::SchemaProxy;
use crate::release::Backend;
use crate::runtime::{InstallProgressFn, LlamaRuntime};
use crate::server::{Sidecar, SidecarConfig, SidecarState, SpawnHook};
use crate::wiring::LlmEnv;

/// Knobs for [`LocalLlm::launch`].
#[derive(Default, Clone)]
pub struct LocalLlmOptions {
    /// Which llama.cpp build to use. `None` picks one from the detected
    /// hardware.
    pub backend: Option<Backend>,
    /// Context window. `None` derives one from the model — see
    /// [`agent_ctx_size`].
    pub ctx_size: Option<u32>,
    /// How aggressively to fill VRAM.
    pub policy: PlacementPolicy,
    /// Reports progress while the llama.cpp archives download on first run.
    pub install_progress: Option<InstallProgressFn>,
    /// Run against the `llama-server` child on every (re)spawn. A desktop caller
    /// passes this to enlist the child in its process job, so it dies with the
    /// app even on a hard kill. See [`SpawnHook`].
    pub on_sidecar_spawn: Option<SpawnHook>,
}

/// A running local model.
///
/// Dropping this stops `llama-server` and the proxy in front of it.
pub struct LocalLlm {
    runtime: LlamaRuntime,
    model: InstalledModel,
    placement: Placement,
    sidecar: Sidecar,
    /// IronClaw talks to this, not to the sidecar directly. See
    /// [`crate::proxy`] for why.
    proxy: SchemaProxy,
}

impl LocalLlm {
    /// Start `model_id` from the store under `root`.
    ///
    /// Fails without starting anything when the model is marked suspect (a
    /// previous run crashed the server twice) or when the machine cannot hold
    /// it. Once running, a model that goes on to crash the server twice is
    /// marked suspect for next time.
    pub async fn launch(root: &Path, model_id: &ModelId, options: LocalLlmOptions) -> Result<Self> {
        let downloader = Downloader::new()?;
        let store = ModelStore::new(root, downloader.clone());

        let model = store.load(model_id).await?;
        if let Some(reason) = model.suspect {
            return Err(Error::ModelSuspect {
                model: model_id.to_string(),
                crashes: 0,
                last_output: Some(reason),
            });
        }

        let gpus = probe_gpus().unwrap_or_else(|error| {
            // silent-ok: a GPU we cannot see is a GPU we do not use; CPU inference
            // still works, and failing the launch over a probe would be worse.
            tracing::warn!(%error, "GPU probe failed; assuming no GPU");
            Vec::new()
        });
        let backend = options
            .backend
            .unwrap_or_else(|| Backend::recommended_for(&gpus));

        let runtime =
            LlamaRuntime::install(root, backend, &downloader, options.install_progress).await?;

        // A CPU build cannot offload, whatever the hardware says.
        let gpu = match backend {
            Backend::Cpu => None,
            Backend::Vulkan | Backend::Cuda12 => offload_target(&gpus),
        };
        let memory = system_memory().unwrap_or_else(|error| {
            // silent-ok: without a RAM reading the planner declines to refuse
            // rather than guessing, which is the conservative behavior.
            tracing::warn!(%error, "system memory probe failed");
            None
        });

        let mut config = SidecarConfig::new(
            runtime.server_bin.clone(),
            model.path.clone(),
            model.id.clone(),
        )?;
        config.ctx_size = options
            .ctx_size
            .unwrap_or_else(|| agent_ctx_size(model.gguf.context_length));
        config.on_spawn = options.on_sidecar_spawn;

        let placement = plan(PlacementRequest {
            model: &model.gguf,
            gpu,
            system: memory,
            ctx_size: config.ctx_size,
            policy: options.policy,
        });
        for warning in &placement.warnings {
            tracing::warn!(model = %model.id, "{warning}");
        }
        if let Some(reason) = placement.refusal() {
            return Err(Error::ModelDoesNotFit {
                reason: reason.to_string(),
            });
        }
        config.n_gpu_layers = placement.n_gpu_layers;

        tracing::info!(
            model = %model.id,
            backend = backend.as_str(),
            n_gpu_layers = placement.n_gpu_layers,
            ctx_size = config.ctx_size,
            port = config.port,
            "starting llama-server"
        );

        let sidecar = match Sidecar::start(config).await {
            Ok(sidecar) => sidecar,
            Err(error @ Error::ModelSuspect { .. }) => {
                store.mark_suspect(&model.id, &error.to_string()).await?;
                return Err(error);
            }
            Err(error) => return Err(error),
        };

        watch_for_suspicion(store, model.id.clone(), sidecar.subscribe());

        // llama.cpp cannot compile IronClaw's tool schemas as they stand, so
        // IronClaw is pointed at a proxy that repairs them in flight. The
        // sidecar's port is stable across restarts, so this upstream stays valid.
        let proxy = SchemaProxy::start(format!("http://127.0.0.1:{}", sidecar.port())).await?;

        Ok(Self {
            runtime,
            model,
            placement,
            sidecar,
            proxy,
        })
    }

    /// The environment that points `ironclaw-reborn` at this model.
    ///
    /// `LLM_BASE_URL` names the [`crate::proxy::SchemaProxy`], not the sidecar.
    pub fn env(&self) -> LlmEnv {
        LlmEnv::openai_compatible(
            self.proxy.base_url(),
            self.sidecar.api_key(),
            self.model.id.as_str(),
        )
    }

    /// What we decided about GPU offload, including any warnings the user should
    /// see.
    pub fn placement(&self) -> &Placement {
        &self.placement
    }

    /// The model being served.
    pub fn model(&self) -> &InstalledModel {
        &self.model
    }

    /// The llama.cpp build in use.
    pub fn backend(&self) -> Backend {
        self.runtime.backend
    }

    /// The directory the llama.cpp binaries live in.
    pub fn runtime_dir(&self) -> &PathBuf {
        &self.runtime.dir
    }

    /// The supervised server, for health badges and diagnostics.
    pub fn sidecar(&self) -> &Sidecar {
        &self.sidecar
    }

    /// Stop the server and wait for it to exit.
    pub async fn stop(mut self) {
        self.sidecar.stop().await;
    }
}

/// The floor an agent turn needs. IronClaw's turn is a system prompt plus every
/// tool schema plus the thread so far; a reasoning model then spends hundreds of
/// tokens thinking before it emits its first word of `content`. Below this the
/// prompt alone can exhaust the window, `llama-server` answers `400
/// exceed_context_size_error`, and the run dies as a `driver_protocol_violation`
/// — with no hint that the context was the problem.
const MIN_AGENT_CTX: u32 = 16_384;

/// The ceiling we default to. The KV cache grows linearly with the window (it is
/// what [`crate::placement::plan`] budgets per layer), so defaulting to a model's
/// full 128k+ would evict layers off the GPU to buy context nobody asked for.
const MAX_DEFAULT_CTX: u32 = 32_768;

/// The context window to run an agent model at, given what the GGUF says it was
/// trained for.
///
/// The old default was a flat 4096, which is fine for a chat completion and far
/// too small for an agent turn — see [`MIN_AGENT_CTX`]. A model trained for less
/// than the floor gets its trained window: exceeding it degrades the model, and
/// [`crate::placement::plan`] already warns when a caller asks for that anyway.
fn agent_ctx_size(trained: Option<u32>) -> u32 {
    match trained {
        // Never above what it was trained for, never above our ceiling. A model
        // trained below the floor simply gets its trained window — we cannot
        // conjure context it does not have.
        Some(trained) => trained.min(MAX_DEFAULT_CTX),
        // No header field to read. The floor is the safer guess: too small is a
        // dead agent, too large is only a slower one.
        None => MIN_AGENT_CTX,
    }
}

/// The adapter to offload to: the discrete GPU with the most headroom, or an
/// integrated one if that is all there is. [`crate::placement::plan`] makes the
/// final call on whether to actually use it.
fn offload_target(gpus: &[GpuAdapter]) -> Option<&GpuAdapter> {
    gpus.iter()
        .find(|gpu| gpu.is_discrete())
        .or_else(|| gpus.first())
}

/// Persist a suspect marker if the sidecar gives up after the initial launch.
///
/// Without this, only crashes *during* startup would be recorded, and a model
/// that dies twice mid-conversation would greet the user with the same restart
/// loop on the next run.
fn watch_for_suspicion(
    store: ModelStore,
    model_id: ModelId,
    mut states: tokio::sync::watch::Receiver<SidecarState>,
) {
    tokio::spawn(async move {
        // `changed()` errors once the sidecar is dropped, which ends this task.
        while states.changed().await.is_ok() {
            let reason = match &*states.borrow_and_update() {
                SidecarState::Suspect { reason } => reason.clone(),
                _ => continue,
            };
            if let Err(error) = store.mark_suspect(&model_id, &reason).await {
                tracing::error!(model = %model_id, %error, "could not record the suspect marker");
            } else {
                tracing::warn!(model = %model_id, "model marked suspect; it will not auto-load");
            }
            return;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The regression these constants exist for: Qwen3-4B (trained 40960) ran at
    /// the old flat 4096 default, and *every* agent turn failed — the prompt plus
    /// the model's own reasoning tokens overran the window, `llama-server`
    /// answered `400 exceed_context_size_error`, and the gateway reported a
    /// `driver_protocol_violation` that never mentioned context.
    #[test]
    fn an_agent_model_gets_a_window_an_agent_turn_fits_in() {
        assert_eq!(agent_ctx_size(Some(40_960)), 32_768);
        assert!(agent_ctx_size(Some(40_960)) >= MIN_AGENT_CTX);
    }

    #[test]
    fn a_huge_trained_window_is_capped_so_the_kv_cache_does_not_evict_layers() {
        assert_eq!(agent_ctx_size(Some(131_072)), MAX_DEFAULT_CTX);
    }

    /// We cannot conjure context the model was not trained for; asking for more
    /// degrades it. A small model runs small.
    #[test]
    fn a_model_trained_below_the_floor_gets_its_trained_window() {
        assert_eq!(agent_ctx_size(Some(8_192)), 8_192);
    }

    #[test]
    fn a_header_with_no_context_length_falls_back_to_the_floor() {
        assert_eq!(agent_ctx_size(None), MIN_AGENT_CTX);
    }
}
