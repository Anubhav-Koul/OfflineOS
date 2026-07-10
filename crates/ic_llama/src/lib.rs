//! Local LLM inference for the desktop fork: a supervised `llama-server`
//! sidecar and the GGUF models it runs.
//!
//! # Why there are no IronClaw changes here
//!
//! IronClaw already ships an `openai_compatible` LLM provider, and
//! `llama-server` already speaks the OpenAI Chat Completions API. The entire
//! integration is therefore four environment variables handed to
//! `ironclaw-reborn` when the widget spawns it — see [`wiring`]. Nothing in this
//! crate touches an IronClaw core crate, which is what keeps upstream merges
//! cheap.
//!
//! # The pieces
//!
//! | Module | Responsibility |
//! |---|---|
//! | [`release`] | The exact llama.cpp build we ship, with digests |
//! | [`download`] | Resumable, checksum-verified transfers |
//! | [`runtime`] | Installing those binaries |
//! | [`models`] | The GGUF store, HuggingFace downloads, suspect markers |
//! | [`gguf`] | Reading a model's shape out of its header |
//! | [`hardware`] | How much VRAM and RAM this machine actually has free |
//! | [`placement`] | Turning all of the above into one `-ngl` number |
//! | [`server`] | Keeping `llama-server` alive, and knowing when to stop trying |
//! | [`proxy`] | Repairing tool schemas llama.cpp's grammar compiler rejects |
//! | [`wiring`] | The four environment variables |
//! | [`local_llm`] | The facade that runs the whole sequence |
//!
//! # Usage
//!
//! ```no_run
//! use ic_llama::{LocalLlm, LocalLlmOptions, ModelId};
//!
//! # async fn run() -> ic_llama::Result<()> {
//! let model = ModelId::new("Qwen3-4B-Q4_K_M")?;
//! let llm = LocalLlm::launch("C:/ProgramData/IronClaw".as_ref(), &model, LocalLlmOptions::default()).await?;
//!
//! for warning in &llm.placement().warnings {
//!     eprintln!("warning: {warning}");
//! }
//!
//! let mut command = std::process::Command::new("ironclaw-reborn");
//! llm.env().apply(&mut command);
//! # Ok(())
//! # }
//! ```

#![deny(missing_docs)]

pub mod download;
pub mod error;
pub mod gguf;
pub mod hardware;
pub mod ids;
pub mod local_llm;
pub mod models;
pub mod placement;
pub mod proxy;
pub mod release;
pub mod runtime;
pub mod server;
pub mod wiring;

pub use error::{Error, Result};
pub use gguf::GgufModel;
pub use ids::{ModelId, Sha256Hex};
pub use local_llm::{LocalLlm, LocalLlmOptions};
pub use models::{Digest, HubModel, InstalledModel, ModelStore};
pub use placement::{Placement, PlacementPolicy, Verdict};
pub use proxy::SchemaProxy;
pub use release::{Backend, LLAMA_CPP_TAG};
pub use runtime::LlamaRuntime;
pub use server::{Sidecar, SidecarConfig, SidecarState};
pub use wiring::LlmEnv;
