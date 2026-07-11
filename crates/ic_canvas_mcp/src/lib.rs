//! The canvas render tool for the IronClaw desktop app.
//!
//! An MCP server that exposes one tool, `canvas_render`, letting the agent display
//! HTML/SVG on a desktop window. Unlike the browser sidecar, this runs **in the
//! widget process**: the render must reach a Tauri window, and every other channel
//! from the agent to the widget (the gateway's SSE previews, its timeline)
//! sanitizes and 16 KiB-truncates content. Serving the tool in-process means the
//! `tools/call` handler is widget code holding the raw markup, which it hands to a
//! [`server::CanvasSink`] — wired by `ic_widget` to emit into a sandboxed canvas
//! window.
//!
//! Reborn reaches it through the same hosted-MCP lane as the browser (CP-4 loopback
//! exemption; see `docs/desktop/core-patches.md`), so this crate needs no new core
//! change: it reuses the exemption CP-4 already opened for any host-bundled
//! loopback MCP provider.
//!
//! | Module | Responsibility |
//! |---|---|
//! | [`server`] | The in-process MCP HTTP server and its `CanvasSink` seam |
//! | [`protocol`] | MCP wire types + the `canvas_render` tool |
//! | [`manifest`] | The `ic-canvas` extension package |
//!
//! ## Rendering safety (enforced by the widget, documented here)
//!
//! The markup this tool accepts is agent-authored and therefore untrusted (a
//! prompt-injected agent could emit hostile HTML). The widget renders it in an
//! iframe with an empty `sandbox` — no scripts, no same-origin, no forms, no
//! navigation — under a `default-src 'none'` CSP that permits only inline styles
//! and `data:` images. Static HTML and inline SVG (a chart, a table, a diagram)
//! render; scripts and every network fetch are inert. Strip-sanitizing the markup
//! (e.g. ammonia) is deliberately *not* used: it would break legitimate inline SVG
//! while adding nothing the sandbox+CSP does not already guarantee. Isolation, not
//! mutation, is the mechanism.

pub mod manifest;
pub mod protocol;
pub mod server;

pub use manifest::{EXTENSION_ID, install_package, manifest_toml};
pub use protocol::{CANVAS_RENDER, PROTOCOL_VERSION, RenderRequest};
pub use server::{CanvasSink, MCP_PATH, Server};
