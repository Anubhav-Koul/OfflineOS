//! The desktop widget's non-UI core.
//!
//! Everything here builds and tests without Tauri, so `cargo test -p ic_widget`
//! does not pay for a WebView. The Tauri shell lives behind the `app` feature
//! and in `src/main.rs`.
//!
//! | Module | Responsibility |
//! |---|---|
//! | [`ambient`] | The character speaking first: suggestions, guardrails, the log |
//! | [`skill_import`] | Reviewing and installing a third-party skill folder |
//! | [`browser`] | Supervising the browser MCP sidecar; registering its tools |
//! | [`canvas`] | The in-process canvas MCP server; rendering agent HTML/SVG |
//! | [`gateway_client`] | The only place we talk to `ironclaw-reborn serve` |
//! | [`hit_test`] | The click-through mask for the transparent window |
//! | [`job_object`] | Making child processes die with us |
//! | [`secrets`] | The bearer token, in the OS credential store |
//! | [`supervisor`] | Keeping `ironclaw-reborn serve` alive |
//! | [`window_state`] | Where the widget sat, per monitor arrangement |
//! | [`error`] | The crate's error type |

#![deny(missing_docs)]

pub mod ambient;
pub mod browser;
pub mod canvas;
pub mod character;
pub mod error;
pub mod gateway_client;
pub mod hit_test;
pub mod job_object;
pub mod model_catalog;
pub mod persona;
pub mod providers;
pub mod secrets;
pub mod settings;
pub mod skill_import;
pub mod supervisor;
pub mod voice;
pub mod window_state;

pub use browser::BrowserSidecar;
pub use error::{Error, Result};
pub use gateway_client::{GatewayClient, GatewayEvent, RunPhase, ThreadId};
pub use job_object::ProcessJob;
pub use secrets::SecretStore;
pub use supervisor::{GatewayConfig, GatewayState, GatewaySupervisor};
pub use window_state::{LayoutHash, MonitorInfo, WindowPosition, WindowState, WindowStateStore};
