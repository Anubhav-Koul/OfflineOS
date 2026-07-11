//! The MCP wire contract, and the single `canvas_render` tool.
//!
//! Real MCP: streamable-HTTP JSON-RPC 2.0, protocol version `2025-06-18`, the
//! same shapes `ironclaw_mcp::McpHostHttpClient` drives against the browser
//! sidecar. This is a second, in-process provider — see [`crate::server`] — so the
//! contract is identical to the browser crate's; only the tool differs.
//!
//! There is deliberately **one** tool. Canvas is a display surface, not an API:
//! the agent hands over a block of HTML/SVG and the widget renders it. The tool's
//! `inputSchema` is what the model sees, because Reborn rebuilds the capability
//! from this crate's live `tools/list` (it discards the manifest's declared
//! schema — see [`crate::manifest`]).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The MCP revision `ironclaw_mcp` initializes with.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// The agent-facing tool id. Becomes the capability `ic-canvas.canvas_render`.
pub const CANVAS_RENDER: &str = "canvas_render";

/// The `tools/list` descriptor for `canvas_render`.
///
/// Kept bound-free and shallow: `ironclaw_mcp` rejects a discovered schema deeper
/// than 8 levels or wider than 512 nodes, and (CP-3) llama.cpp chokes on large
/// `maxLength` bounds compiled into its grammar. Neither is close to being hit.
pub fn canvas_render_descriptor() -> Value {
    json!({
        "name": CANVAS_RENDER,
        "description":
            "Display HTML or SVG on the desktop canvas window — a chart, a table, a diagram, a \
             formatted document. Pass a complete, self-contained fragment or document: it renders \
             in an isolated sandbox with no network access and no scripting, so inline styles and \
             inline SVG work, but external resources and JavaScript do not. Calling this again \
             replaces what is shown.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "html": {
                    "type": "string",
                    "description":
                        "The HTML or SVG markup to display. Self-contained: inline any CSS, and \
                         embed images as data: URIs. Scripts and external URLs are ignored."
                },
                "title": {
                    "type": "string",
                    "description": "Optional title for the canvas window."
                }
            },
            "required": ["html"],
            "additionalProperties": false
        },
        // A pure display action: it shows something, but nothing leaves the
        // machine and no external state changes. Read-only keeps it off the
        // external-write effect path.
        "annotations": {
            "title": "Render to canvas",
            "readOnlyHint": true,
            "destructiveHint": false,
            "openWorldHint": false
        }
    })
}

/// One `canvas_render` call's arguments.
#[derive(Debug, Clone, Deserialize)]
pub struct RenderArgs {
    /// The markup to display.
    pub html: String,
    /// An optional window title.
    #[serde(default)]
    pub title: Option<String>,
}

/// A JSON-RPC 2.0 request or notification.
#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcRequest {
    /// Present on a request, absent on a notification.
    #[serde(default)]
    pub id: Option<Value>,
    /// The JSON-RPC method.
    pub method: String,
    /// Method parameters, if any.
    #[serde(default)]
    pub params: Option<Value>,
}

/// JSON-RPC error codes.
pub mod error_code {
    /// The request was not valid JSON.
    pub const PARSE_ERROR: i32 = -32700;
    /// No such method.
    pub const METHOD_NOT_FOUND: i32 = -32601;
    /// The method exists but the params were wrong.
    pub const INVALID_PARAMS: i32 = -32602;
}

/// A JSON-RPC success response.
pub fn success(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// A JSON-RPC error response.
pub fn failure(id: Value, code: i32, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message.into() },
    })
}

/// The `initialize` result.
pub fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "ic-canvas-mcp", "version": env!("CARGO_PKG_VERSION") },
    })
}

/// The `tools/list` result: just `canvas_render`.
pub fn tools_list_result() -> Value {
    json!({ "tools": [canvas_render_descriptor()] })
}

/// A successful `canvas_render` result.
pub fn render_ok_result() -> Value {
    json!({
        "content": [{ "type": "text", "text": "Rendered to the canvas window." }],
        "isError": false,
    })
}

/// A recoverable tool error (e.g. the markup was too large): the agent sees it and
/// can adjust, rather than the run failing.
pub fn render_error_result(message: impl Into<String>) -> Value {
    json!({
        "content": [{ "type": "text", "text": message.into() }],
        "isError": true,
    })
}

/// What the widget is handed to render. Deliberately a plain, serializable struct:
/// it crosses the crate boundary to `ic_widget`, which emits it to the canvas
/// window untouched.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RenderRequest {
    /// The markup to display.
    pub html: String,
    /// The window title, if the agent gave one.
    pub title: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tool_publishes_an_object_schema_the_host_will_accept() {
        let descriptor = canvas_render_descriptor();
        assert_eq!(descriptor["name"], CANVAS_RENDER);
        assert_eq!(descriptor["inputSchema"]["type"], "object");
        assert!(descriptor["inputSchema"]["properties"]["html"].is_object());
        // The name must satisfy the host's tool-name charset (lowercase, `_`).
        assert!(
            CANVAS_RENDER
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b == b'_')
        );
    }

    #[test]
    fn canvas_render_is_read_only_so_it_stays_off_the_external_write_path() {
        // If this flips, every chart render would be classified as a write.
        let descriptor = canvas_render_descriptor();
        assert_eq!(descriptor["annotations"]["readOnlyHint"], true);
        assert_eq!(descriptor["annotations"]["destructiveHint"], false);
    }

    #[test]
    fn render_args_accept_html_with_an_optional_title() {
        let with_title: RenderArgs =
            serde_json::from_value(json!({ "html": "<b>hi</b>", "title": "Chart" })).unwrap();
        assert_eq!(with_title.title.as_deref(), Some("Chart"));

        let without: RenderArgs = serde_json::from_value(json!({ "html": "<b>hi</b>" })).unwrap();
        assert!(without.title.is_none());
    }
}
