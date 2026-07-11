//! The in-process MCP server.
//!
//! Unlike the browser sidecar, this runs **inside the widget process**, not as a
//! child. That is the whole point: when the agent calls `canvas_render(html)`,
//! Reborn POSTs the arguments here (over the CP-4 loopback exemption), and the
//! handler is widget code holding the raw HTML. It hands that straight to a
//! [`CanvasSink`] — which the widget wires to a Tauri `emit` into the canvas
//! window. The markup never crosses the gateway's SSE/timeline path, which would
//! sanitize and 16 KiB-truncate it.
//!
//! Loopback only, by construction: `Server::bind` binds `127.0.0.1`. Nothing off
//! this machine can reach it, which is what makes the CP-4 exemption defensible.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Value, json};

use crate::protocol::{
    self, CANVAS_RENDER, JsonRpcRequest, RenderArgs, RenderRequest, error_code, failure,
    initialize_result, success, tools_list_result,
};

/// The path the manifest's `url` points at.
pub const MCP_PATH: &str = "/mcp";

/// The largest markup we will render. Canvas content is a chart or a document, not
/// a website; past this we refuse rather than push megabytes into a webview. A
/// refusal is a recoverable tool error, so the agent can send something smaller.
pub const MAX_MARKUP_BYTES: usize = 512 * 1024;

/// Receives markup to display. The widget provides one that emits to the canvas
/// window; tests provide a recording fake.
///
/// Returning `Err` surfaces to the agent as a recoverable tool error. It is *not*
/// for "the user closed the window" — rendering is fire-and-forget — but for "this
/// could not be accepted at all".
pub trait CanvasSink: Send + Sync + 'static {
    /// Display `request`. Fast and non-blocking: the actual paint happens in the
    /// webview, asynchronously.
    fn render(&self, request: RenderRequest) -> Result<(), String>;
}

impl<F> CanvasSink for F
where
    F: Fn(RenderRequest) -> Result<(), String> + Send + Sync + 'static,
{
    fn render(&self, request: RenderRequest) -> Result<(), String> {
        self(request)
    }
}

/// The bound MCP server.
pub struct Server {
    listener: tokio::net::TcpListener,
    local_addr: std::net::SocketAddr,
}

impl Server {
    /// Bind a loopback port. `port` 0 asks the OS for a free one.
    pub async fn bind(port: u16) -> std::io::Result<Self> {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
        let local_addr = listener.local_addr()?;
        Ok(Self {
            listener,
            local_addr,
        })
    }

    /// The resolved listening address.
    pub fn local_addr(&self) -> std::net::SocketAddr {
        self.local_addr
    }

    /// The URL to write into the extension manifest.
    pub fn mcp_url(&self) -> String {
        format!("http://{}{MCP_PATH}", self.local_addr)
    }

    /// Serve until the process ends.
    pub async fn serve(self, sink: Arc<dyn CanvasSink>) -> std::io::Result<()> {
        tracing::info!(url = %self.mcp_url(), "canvas MCP server listening (in-process)");
        axum::serve(self.listener, router(sink)).await
    }
}

/// Build the router. Split out so tests can drive it without a socket.
pub fn router(sink: Arc<dyn CanvasSink>) -> Router {
    Router::new().route(MCP_PATH, post(handle)).with_state(sink)
}

async fn handle(State(sink): State<Arc<dyn CanvasSink>>, body: String) -> Response {
    let request: JsonRpcRequest = match serde_json::from_str(&body) {
        Ok(request) => request,
        Err(error) => {
            return json_response(failure(
                Value::Null,
                error_code::PARSE_ERROR,
                format!("malformed JSON-RPC request: {error}"),
            ));
        }
    };

    // A notification (no id) — `notifications/initialized` — gets 202 and no body.
    let Some(id) = request.id.clone() else {
        return StatusCode::ACCEPTED.into_response();
    };

    let response = match request.method.as_str() {
        "initialize" => success(id, initialize_result()),
        "tools/list" => success(id, tools_list_result()),
        "tools/call" => call_tool(sink.as_ref(), id, request.params),
        "ping" => success(id, json!({})),
        other => failure(
            id,
            error_code::METHOD_NOT_FOUND,
            format!("unsupported method: {other}"),
        ),
    };
    json_response(response)
}

fn call_tool(sink: &dyn CanvasSink, id: Value, params: Option<Value>) -> Value {
    let params = params.unwrap_or(Value::Null);
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return failure(
            id,
            error_code::INVALID_PARAMS,
            "tools/call requires a tool name",
        );
    };
    if name != CANVAS_RENDER {
        return failure(
            id,
            error_code::INVALID_PARAMS,
            format!("no such tool: {name}"),
        );
    }

    let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
    let args: RenderArgs = match serde_json::from_value(arguments) {
        Ok(args) => args,
        Err(error) => {
            return failure(
                id,
                error_code::INVALID_PARAMS,
                format!("invalid canvas_render arguments: {error}"),
            );
        }
    };

    // A too-large payload is the agent's problem to fix, so it is a recoverable
    // tool result, not a JSON-RPC error.
    if args.html.len() > MAX_MARKUP_BYTES {
        return success(
            id,
            protocol::render_error_result(format!(
                "the markup is {} bytes; the canvas accepts at most {MAX_MARKUP_BYTES}. \
                 Send something smaller.",
                args.html.len()
            )),
        );
    }

    let request = RenderRequest {
        html: args.html,
        title: args.title,
    };
    match sink.render(request) {
        Ok(()) => success(id, protocol::render_ok_result()),
        Err(reason) => success(id, protocol::render_error_result(reason)),
    }
}

fn json_response(body: Value) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use std::sync::Mutex;
    use tower::ServiceExt as _;

    #[derive(Default)]
    struct RecordingSink {
        last: Mutex<Option<RenderRequest>>,
        fail: bool,
    }

    impl CanvasSink for RecordingSink {
        fn render(&self, request: RenderRequest) -> Result<(), String> {
            if self.fail {
                return Err("the canvas window is unavailable".into());
            }
            *self.last.lock().unwrap() = Some(request);
            Ok(())
        }
    }

    async fn rpc_with(sink: Arc<dyn CanvasSink>, body: Value) -> (StatusCode, Value) {
        let response = router(sink)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(MCP_PATH)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        if bytes.is_empty() {
            return (status, Value::Null);
        }
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    async fn rpc(body: Value) -> (StatusCode, Value) {
        rpc_with(Arc::new(RecordingSink::default()), body).await
    }

    #[tokio::test]
    async fn the_server_binds_loopback_only() {
        let server = Server::bind(0).await.unwrap();
        assert!(server.local_addr().ip().is_loopback());
        assert!(server.mcp_url().starts_with("http://127.0.0.1:"));
        assert!(server.mcp_url().ends_with("/mcp"));
    }

    #[tokio::test]
    async fn initialize_and_tools_list_answer_the_handshake() {
        let (status, init) =
            rpc(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" })).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            init["result"]["protocolVersion"],
            protocol::PROTOCOL_VERSION
        );

        let (_, list) = rpc(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" })).await;
        let tools = list["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], CANVAS_RENDER);
    }

    #[tokio::test]
    async fn the_initialized_notification_is_accepted_with_no_body() {
        let (status, body) =
            rpc(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(body, Value::Null);
    }

    #[tokio::test]
    async fn a_render_call_reaches_the_sink_with_the_raw_html() {
        // The whole point: the exact markup the agent sent arrives at the sink,
        // unsanitized and untruncated.
        let sink = Arc::new(RecordingSink::default());
        let html = "<svg><rect width='10' height='10'/></svg>";
        let (status, body) = rpc_with(
            sink.clone(),
            json!({
                "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": { "name": CANVAS_RENDER, "arguments": { "html": html, "title": "Box" } },
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"]["isError"], false);

        let seen = sink
            .last
            .lock()
            .unwrap()
            .clone()
            .expect("the sink got a request");
        assert_eq!(seen.html, html, "the markup must reach the sink verbatim");
        assert_eq!(seen.title.as_deref(), Some("Box"));
    }

    #[tokio::test]
    async fn oversized_markup_is_a_recoverable_error_not_a_crash() {
        let sink = Arc::new(RecordingSink::default());
        let huge = "a".repeat(MAX_MARKUP_BYTES + 1);
        let (_, body) = rpc_with(
            sink.clone(),
            json!({
                "jsonrpc": "2.0", "id": 4, "method": "tools/call",
                "params": { "name": CANVAS_RENDER, "arguments": { "html": huge } },
            }),
        )
        .await;
        assert!(body.get("error").is_none(), "{body}");
        assert_eq!(body["result"]["isError"], true);
        assert!(
            sink.last.lock().unwrap().is_none(),
            "nothing should reach the sink"
        );
    }

    #[tokio::test]
    async fn a_sink_failure_is_a_recoverable_tool_error() {
        let sink = Arc::new(RecordingSink {
            fail: true,
            ..Default::default()
        });
        let (_, body) = rpc_with(
            sink,
            json!({
                "jsonrpc": "2.0", "id": 5, "method": "tools/call",
                "params": { "name": CANVAS_RENDER, "arguments": { "html": "<b>x</b>" } },
            }),
        )
        .await;
        assert!(body.get("error").is_none(), "{body}");
        assert_eq!(body["result"]["isError"], true);
    }

    #[tokio::test]
    async fn an_unknown_tool_is_rejected() {
        let (_, body) = rpc(json!({
            "jsonrpc": "2.0", "id": 6, "method": "tools/call",
            "params": { "name": "canvas_exfiltrate", "arguments": {} },
        }))
        .await;
        assert_eq!(body["error"]["code"], error_code::INVALID_PARAMS);
    }
}
