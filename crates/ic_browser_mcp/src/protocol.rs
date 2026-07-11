//! The MCP wire contract this sidecar speaks, and the tool catalogue it serves.
//!
//! This is **real MCP**, not a bespoke framing: streamable-HTTP JSON-RPC 2.0,
//! protocol version `2025-06-18`, with `initialize` →
//! `notifications/initialized` → `tools/list` / `tools/call`. The Reborn runtime
//! drives it through `ironclaw_mcp::McpHostHttpClient`, which is why the shapes
//! below are not ours to choose — each one is pinned by what that client sends
//! and what it will accept back. The tests at the bottom encode those
//! expectations so a drift in either direction fails here rather than as a
//! silent "provider returned no discoverable tools" at activation.
//!
//! Two consequences of the host client are worth stating plainly, because they
//! decide how much of this file exists:
//!
//! - **`tools/list` is the only schema source.** Reborn discards the capability
//!   schemas declared in our manifest and rebuilds every capability from the
//!   `inputSchema` we return here (`hosted_mcp_discovery::discovered_capability_manifest`).
//!   So [`Tool::input_schema`] *is* the agent-facing tool signature. There are no
//!   schema files on disk.
//! - **Annotations decide effects.** `readOnlyHint` / `destructiveHint` are what
//!   promote a discovered tool to `EffectKind::ExternalWrite`, which is what the
//!   safety layer keys off. They are advisory in the MCP spec; here they are
//!   load-bearing.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The MCP revision `ironclaw_mcp` initializes with. Answering `initialize` with
/// a different string is legal MCP (the server picks), but the host validates it
/// against a charset and stores it, so we simply agree.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// The six tools this sidecar exposes.
///
/// The wire name is what `tools/list` publishes and what `tools/call` dispatches
/// on. Reborn turns each into the capability `ic-browser.<wire_name>`, so these
/// names are also the agent-facing tool ids — they must satisfy MCP's tool-name
/// charset (lowercase, digits, `_`, `-`), which `browser_navigate` &c. do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tool {
    /// Load a URL and wait for the page to settle.
    BrowserNavigate,
    /// Return the visible text of the page (or of one element).
    BrowserGetText,
    /// Report whether/where elements matching a selector exist.
    BrowserFind,
    /// Type text into an input.
    BrowserFill,
    /// Click the first element matching a selector.
    BrowserClick,
    /// Capture a screenshot of the viewport.
    BrowserScreenshot,
}

/// Every tool, in the order `tools/list` publishes them.
pub const ALL_TOOLS: [Tool; 6] = [
    Tool::BrowserNavigate,
    Tool::BrowserGetText,
    Tool::BrowserFind,
    Tool::BrowserFill,
    Tool::BrowserClick,
    Tool::BrowserScreenshot,
];

impl Tool {
    /// The agent-facing tool id.
    pub fn wire_name(self) -> &'static str {
        match self {
            Tool::BrowserNavigate => "browser_navigate",
            Tool::BrowserGetText => "browser_get_text",
            Tool::BrowserFind => "browser_find",
            Tool::BrowserFill => "browser_fill",
            Tool::BrowserClick => "browser_click",
            Tool::BrowserScreenshot => "browser_screenshot",
        }
    }

    /// Resolve a `tools/call` name back to a tool.
    pub fn from_wire_name(name: &str) -> Option<Self> {
        ALL_TOOLS.into_iter().find(|tool| tool.wire_name() == name)
    }

    /// What the model is told this tool does. This is the whole prompt the model
    /// gets for the tool, so it carries the operational caveats too.
    pub fn description(self) -> &'static str {
        match self {
            Tool::BrowserNavigate => {
                "Load a URL in the browser and wait for it to settle. Returns the final URL and \
                 page title. Use this before reading or interacting with a page."
            }
            Tool::BrowserGetText => {
                "Read the visible text of the current page, or of one element if a CSS selector is \
                 given. Use this to read a page you have navigated to."
            }
            Tool::BrowserFind => {
                "Check whether elements matching a CSS selector exist on the current page, and how \
                 many. Matching nothing is a normal answer, not an error — use this to probe a \
                 selector before acting on it."
            }
            Tool::BrowserFill => {
                "Type text into the input matching a CSS selector. Only use this on a field you \
                 have confirmed exists."
            }
            Tool::BrowserClick => {
                "Click the first element matching a CSS selector on the current page."
            }
            Tool::BrowserScreenshot => {
                "Capture a screenshot of the current viewport. Use this when the page's text is \
                 not enough — for example to see a layout, or when a selector keeps failing and \
                 you need to look at the page."
            }
        }
    }

    /// The tool's JSON-Schema input. Reborn publishes this verbatim as the
    /// capability's `parameters_schema`, so it is the model's tool signature.
    ///
    /// Kept deliberately small and bound-free: `ironclaw_mcp` rejects a
    /// discovered schema deeper than 8 levels or wider than 512 nodes, and (see
    /// CP-3) llama.cpp compiles schema bounds into a GBNF grammar and chokes on
    /// large `maxLength`. Neither limit is close to being hit here, and that is
    /// on purpose.
    pub fn input_schema(self) -> Value {
        match self {
            Tool::BrowserNavigate => json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "Absolute URL to load, including the scheme (https://...)."
                    }
                },
                "required": ["url"],
                "additionalProperties": false,
            }),
            Tool::BrowserGetText => json!({
                "type": "object",
                "properties": {
                    "selector": {
                        "type": "string",
                        "description": "Optional CSS selector. Omit to read the whole page."
                    }
                },
                "additionalProperties": false,
            }),
            Tool::BrowserFind => json!({
                "type": "object",
                "properties": {
                    "selector": {
                        "type": "string",
                        "description": "CSS selector to look for."
                    }
                },
                "required": ["selector"],
                "additionalProperties": false,
            }),
            Tool::BrowserFill => json!({
                "type": "object",
                "properties": {
                    "selector": {
                        "type": "string",
                        "description": "CSS selector of the input to fill."
                    },
                    "value": {
                        "type": "string",
                        "description": "The text to type into it."
                    }
                },
                "required": ["selector", "value"],
                "additionalProperties": false,
            }),
            Tool::BrowserClick => json!({
                "type": "object",
                "properties": {
                    "selector": {
                        "type": "string",
                        "description": "CSS selector of the element to click."
                    }
                },
                "required": ["selector"],
                "additionalProperties": false,
            }),
            Tool::BrowserScreenshot => json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false,
            }),
        }
    }

    /// MCP tool annotations.
    ///
    /// These are not cosmetic. `ironclaw_extensions::discovered_tool_requires_external_write`
    /// reads them to decide whether a discovered tool carries
    /// `EffectKind::ExternalWrite` — so `destructiveHint` here is what makes
    /// "type into a login form" a write-effect capability rather than a read.
    /// Every tool is `default_permission: Ask` regardless (Reborn hardcodes that
    /// for discovered MCP tools), so this governs effect classification, not
    /// whether the user is prompted.
    pub fn annotations(self) -> Value {
        let (read_only, destructive) = match self {
            // Reads: they move the browser, but nothing leaves it.
            Tool::BrowserNavigate
            | Tool::BrowserGetText
            | Tool::BrowserFind
            | Tool::BrowserScreenshot => (true, false),
            // Writes: these two are how the agent submits a form, spends money,
            // or logs in as the user. Classified conservatively.
            Tool::BrowserFill | Tool::BrowserClick => (false, true),
        };
        json!({
            "title": self.wire_name(),
            "readOnlyHint": read_only,
            "destructiveHint": destructive,
            "openWorldHint": true,
        })
    }

    /// The `tools/list` entry for this tool.
    pub fn descriptor(self) -> Value {
        json!({
            "name": self.wire_name(),
            "description": self.description(),
            "inputSchema": self.input_schema(),
            "annotations": self.annotations(),
        })
    }
}

/// Per-tool parameter shapes, deserialized from `tools/call`'s `arguments`.
pub mod params {
    use serde::{Deserialize, Serialize};

    /// `browser_navigate`.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Navigate {
        /// Absolute URL to load.
        pub url: String,
    }

    /// `browser_get_text`. With no selector, returns the whole page's text.
    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    pub struct GetText {
        /// Optional CSS selector to scope the text to one element.
        #[serde(default)]
        pub selector: Option<String>,
    }

    /// `browser_find`.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Find {
        /// CSS selector to look for.
        pub selector: String,
    }

    /// `browser_fill`.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Fill {
        /// CSS selector of the input to fill.
        pub selector: String,
        /// The text to type.
        pub value: String,
    }

    /// `browser_click`.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Click {
        /// CSS selector of the element to click.
        pub selector: String,
    }
}

/// A JSON-RPC 2.0 request or notification.
///
/// `id` is absent on a notification (`notifications/initialized`), which is the
/// one case where the host expects no result body — see [`crate::server`].
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

/// JSON-RPC error codes we emit. Only the standard ones; MCP adds no others we
/// need, because a *tool* failure is a successful JSON-RPC call carrying
/// `isError` (see [`tool_error_result`]) rather than a protocol error.
pub mod error_code {
    /// The request was not valid JSON.
    pub const PARSE_ERROR: i32 = -32700;
    /// The request was JSON, but not a valid JSON-RPC request.
    pub const INVALID_REQUEST: i32 = -32600;
    /// No such method.
    pub const METHOD_NOT_FOUND: i32 = -32601;
    /// The method exists but the params were wrong.
    pub const INVALID_PARAMS: i32 = -32602;
    /// Anything else that went wrong server-side.
    pub const INTERNAL_ERROR: i32 = -32603;
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
        // We serve tools and nothing else: no resources, prompts, or sampling.
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": {
            "name": "ic-browser-mcp",
            "version": env!("CARGO_PKG_VERSION"),
        },
    })
}

/// The `tools/list` result.
pub fn tools_list_result() -> Value {
    json!({ "tools": ALL_TOOLS.map(Tool::descriptor) })
}

/// A successful `tools/call` result.
///
/// MCP wants human-readable `content`; Reborn hands the whole `result` object to
/// the agent as the capability's output. We supply both: `content` carries the
/// JSON as text (what the model actually reads) and `structuredContent` keeps it
/// machine-shaped.
pub fn tool_success_result(value: Value) -> Value {
    let text = serde_json::to_string(&value).unwrap_or_else(|_| value.to_string());
    json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": value,
        "isError": false,
    })
}

/// A screenshot result: an MCP image block, so a vision-capable model can look
/// at the page when a selector keeps missing (CLAUDE.md Phase 4.4).
pub fn tool_image_result(base64_png: String, mime_type: &str) -> Value {
    json!({
        "content": [{ "type": "image", "data": base64_png, "mimeType": mime_type }],
        "isError": false,
    })
}

/// A *tool-level* failure: a successful JSON-RPC call whose result says the tool
/// failed.
///
/// This is the difference between "the agent asked for a selector that isn't
/// there" (recoverable — it should look at the page and try another one) and
/// "the browser is gone" (a protocol error; the run should fail). Returning the
/// former as a JSON-RPC error would make Reborn fail the capability outright and
/// deny the model the chance to correct itself.
pub fn tool_error_result(message: impl Into<String>) -> Value {
    json!({
        "content": [{ "type": "text", "text": message.into() }],
        "isError": true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_has_a_distinct_stable_wire_name_that_round_trips() {
        let mut names: Vec<&str> = ALL_TOOLS.iter().map(|tool| tool.wire_name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), ALL_TOOLS.len(), "wire names must be unique");

        for tool in ALL_TOOLS {
            assert_eq!(Tool::from_wire_name(tool.wire_name()), Some(tool));
        }
        assert_eq!(Tool::from_wire_name("browser_rm_rf"), None);
    }

    /// `ironclaw_mcp::is_supported_mcp_tool_name` rejects anything outside
    /// `[a-z0-9_-]` (dot-separated). A discovered name that fails it takes the
    /// whole `tools/list` down with `response_error`, so this is a hard contract.
    #[test]
    fn wire_names_satisfy_the_hosts_tool_name_charset() {
        for tool in ALL_TOOLS {
            let name = tool.wire_name();
            assert!(!name.is_empty() && name.len() <= 128, "{name}");
            let first = name.as_bytes()[0];
            assert!(
                first.is_ascii_lowercase() || first.is_ascii_digit(),
                "{name} must start with a lowercase letter or digit"
            );
            assert!(
                name.bytes().all(|b| b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || matches!(b, b'_' | b'-')),
                "{name} has a character the host will reject"
            );
        }
    }

    /// The host requires `inputSchema` to be present and an object on every
    /// discovered tool; a missing one fails discovery for the whole provider.
    #[test]
    fn every_tool_publishes_an_object_input_schema() {
        for tool in ALL_TOOLS {
            let descriptor = tool.descriptor();
            assert!(
                descriptor["inputSchema"].is_object(),
                "{} must publish an object inputSchema",
                tool.wire_name()
            );
            assert_eq!(descriptor["inputSchema"]["type"], "object");
            assert!(!descriptor["description"].as_str().unwrap_or("").is_empty());
        }
    }

    /// The two tools that can act on the user's behalf must be classified as
    /// writes, because that is what promotes them to `EffectKind::ExternalWrite`
    /// in Reborn. If this ever flips, a form submission would be filed as a read.
    #[test]
    fn acting_tools_are_annotated_as_writes_and_reading_tools_are_not() {
        for tool in [Tool::BrowserFill, Tool::BrowserClick] {
            let annotations = tool.annotations();
            assert_eq!(annotations["readOnlyHint"], false, "{}", tool.wire_name());
            assert_eq!(annotations["destructiveHint"], true, "{}", tool.wire_name());
        }
        for tool in [
            Tool::BrowserNavigate,
            Tool::BrowserGetText,
            Tool::BrowserFind,
            Tool::BrowserScreenshot,
        ] {
            let annotations = tool.annotations();
            assert_eq!(annotations["readOnlyHint"], true, "{}", tool.wire_name());
            assert_eq!(
                annotations["destructiveHint"],
                false,
                "{}",
                tool.wire_name()
            );
        }
    }

    #[test]
    fn the_initialize_result_carries_the_protocol_version_the_host_validates() {
        let result = initialize_result();
        assert_eq!(result["protocolVersion"], PROTOCOL_VERSION);
        // The host parses this field and stores it; an absent or oddly-charactered
        // value is a `response_error` and the session never opens.
        let version = result["protocolVersion"].as_str().expect("a string");
        assert!(
            version
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
        );
    }

    #[test]
    fn tools_list_publishes_all_six_tools() {
        let tools = tools_list_result();
        let listed = tools["tools"].as_array().expect("an array");
        assert_eq!(listed.len(), 6);
    }

    #[test]
    fn a_tool_failure_is_a_successful_call_carrying_is_error() {
        // A missing selector must not be a JSON-RPC error: the model has to see
        // it and pick another selector.
        let result = tool_error_result("no element matched \"#nope\"");
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"].as_str().is_some());
    }
}
