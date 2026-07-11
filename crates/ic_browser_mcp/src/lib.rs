//! Browser automation for the IronClaw desktop app.
//!
//! A sidecar process (`ic-browser-mcp`, see `main.rs`) launches a dedicated
//! Chrome/Edge under CDP and serves the six browser tools as a **real MCP
//! server** — streamable-HTTP JSON-RPC on `http://127.0.0.1:<port>/mcp`. The
//! Reborn runtime drives it through the hosted-MCP lane it already has, so tool
//! discovery, schema exposure to the model, and approval gating all work without
//! anything bespoke on our side.
//!
//! ## Why HTTP-on-loopback and not stdio
//!
//! `CLAUDE.md`'s Phase 4 plan said "standalone MCP server (stdio), register
//! through IronClaw's MCP config". **That is not possible against
//! `ironclaw-reborn`**: `ironclaw_mcp` hard-rejects `transport = "stdio"`
//! (*"unsupported until process-level egress controls land"*) and spawns no
//! processes at all. A stdio manifest installs and activates cleanly and then
//! fails at every single `tools/call` — the worst failure mode available.
//!
//! Hosted HTTP is the lane that does work, so the sidecar speaks that instead.
//! It required one narrow core patch (**CP-4**) to let a hosted MCP provider live
//! on loopback: `http` is accepted for, and only for, a literal loopback IP, and
//! private-range egress denial is waived for exactly that one endpoint. Remote
//! providers are untouched. See `docs/desktop/core-patches.md`.
//!
//! ## Layout
//!
//! | Module | Responsibility |
//! |---|---|
//! | [`launcher`] | Probe Chrome → Edge; never touch the user's real profile |
//! | [`browser`] | Drive the browser over CDP; the six tools |
//! | [`classify`] | Is a fill target sensitive? Fail-closed, browser-free rules |
//! | [`consent`] | Ask the human before a sensitive fill; deny on every other path |
//! | [`server`] | The MCP JSON-RPC HTTP server and its `ToolExecutor` seam |
//! | [`protocol`] | MCP wire types + the tool catalogue (schemas, annotations) |
//! | [`manifest`] | The Reborn extension package, and the timing rules it obeys |
//!
//! ## The consent gate
//!
//! Typing into a password or payment field routes through a human first
//! ([`consent`]), classified by fail-closed rules ([`classify`]). This is enforced
//! in the sidecar, not the agent runtime, because the runtime's own approval flow
//! does not run — `default_permission: Ask` is stamped on every discovered MCP tool
//! and never read. The sidecar is the last boundary the model cannot route around.
//! See `docs/desktop/core-patches.md`.

pub mod browser;
pub mod classify;
pub mod consent;
pub mod error;
pub mod launcher;
pub mod manifest;
pub mod protocol;
pub mod server;

pub use browser::{BrowserSession, LaunchOptions};
pub use consent::{Approver, DenyAll, StdioApprover};
pub use error::{Error, Result};
pub use launcher::{BrowserExecutable, BrowserKind, find_browser};
pub use manifest::{EXTENSION_ID, install_package, manifest_toml};
pub use protocol::{ALL_TOOLS, PROTOCOL_VERSION, Tool};
pub use server::{MCP_PATH, Server, ToolExecutor};
