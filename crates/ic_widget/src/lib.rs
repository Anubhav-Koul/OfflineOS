//! The desktop widget's non-UI core.
//!
//! Everything here builds and tests without Tauri, so `cargo test -p ic_widget`
//! does not pay for a WebView. The Tauri shell lives behind the `app` feature
//! and in `src/main.rs`.
//!
//! | Module | Responsibility |
//! |---|---|
//! | [`gateway_client`] | The only place we talk to `ironclaw-reborn serve` |
//! | [`job_object`] | Making child processes die with us |
//! | [`secrets`] | The bearer token, in the OS credential store |
//! | [`supervisor`] | Keeping `ironclaw-reborn serve` alive |
//! | [`window_state`] | Where the widget sat, per monitor arrangement |
//! | [`error`] | The crate's error type |

#![deny(missing_docs)]

pub mod error;
pub mod gateway_client;
pub mod job_object;
pub mod providers;
pub mod secrets;
pub mod supervisor;
pub mod window_state;

pub use error::{Error, Result};
pub use gateway_client::{GatewayClient, GatewayEvent, RunPhase, ThreadId};
pub use job_object::ProcessJob;
pub use secrets::SecretStore;
pub use supervisor::{GatewayConfig, GatewayState, GatewaySupervisor};
pub use window_state::{LayoutHash, MonitorInfo, WindowPosition, WindowState, WindowStateStore};
