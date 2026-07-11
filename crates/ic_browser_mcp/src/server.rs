//! The MCP streamable-HTTP server.
//!
//! Binds `127.0.0.1:<port>` and serves JSON-RPC at `POST /mcp`. The browser work
//! sits behind the [`ToolExecutor`] trait, so everything here — routing, method
//! dispatch, error mapping — is testable against a fake executor with no browser
//! on the machine.
//!
//! **Loopback only, by construction.** It binds `127.0.0.1`, never `0.0.0.0`.
//! This server holds a real browser carrying whatever the user logged into; it
//! has no authentication, because the only thing that can reach it is a process
//! on this machine. That is also exactly the contract CP-4 relies on: the core
//! patch waives private-IP denial for a loopback MCP endpoint and nothing else,
//! so binding any other interface would both break the patch's premise and
//! expose the browser. The bind address is asserted in the tests.
//!
//! The session is stateless on the wire. `ironclaw_mcp` re-runs
//! `initialize` → `notifications/initialized` before *every* `tools/call`, and
//! drops the session afterwards, so there is no session state worth keeping —
//! the browser *is* the session, and it lives in this process. We therefore
//! never issue an `Mcp-Session-Id`, which the host treats as "no session id" and
//! simply omits from later requests.

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Value, json};

use crate::error::{Error, Result};
use crate::protocol::{
    self, JsonRpcRequest, Tool, error_code, failure, initialize_result, success, tools_list_result,
};

/// The path the manifest's `url` must point at.
pub const MCP_PATH: &str = "/mcp";

/// Runs the six browser tools. One method rather than six, so dispatch stays a
/// single match and a new tool cannot bypass it.
#[async_trait]
pub trait ToolExecutor: Send + Sync + 'static {
    /// Run `tool` with `arguments`, returning a JSON-RPC `result` object — i.e.
    /// already in MCP's `content`/`isError` shape, so an executor can decide
    /// whether a failure is recoverable (`isError`) or fatal (`Err`).
    async fn call(&self, tool: Tool, arguments: Value) -> Result<Value>;
}

/// The bound MCP server.
pub struct Server {
    listener: tokio::net::TcpListener,
    local_addr: SocketAddr,
}

impl Server {
    /// Bind a loopback port. `port` 0 asks the OS for a free one; read it back
    /// with [`Server::local_addr`].
    pub async fn bind(port: u16) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .map_err(|source| Error::io("binding the browser sidecar port", source))?;
        let local_addr = listener
            .local_addr()
            .map_err(|source| Error::io("reading back the sidecar port", source))?;
        Ok(Self {
            listener,
            local_addr,
        })
    }

    /// The address the server is listening on, with the resolved port.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// The URL to write into the extension manifest.
    pub fn mcp_url(&self) -> String {
        format!("http://{}{MCP_PATH}", self.local_addr)
    }

    /// Serve until the process is killed.
    pub async fn serve<E: ToolExecutor>(self, executor: Arc<E>) -> Result<()> {
        tracing::info!(url = %self.mcp_url(), "browser MCP sidecar listening");
        let app = router(executor);
        axum::serve(self.listener, app)
            .await
            .map_err(|source| Error::io("serving the browser MCP sidecar", source))
    }
}

/// Build the router. Split out so tests can drive it without a socket.
pub fn router<E: ToolExecutor>(executor: Arc<E>) -> Router {
    Router::new()
        .route(MCP_PATH, post(handle::<E>))
        .with_state(executor)
}

/// One JSON-RPC message in, one out.
async fn handle<E: ToolExecutor>(State(executor): State<Arc<E>>, body: String) -> Response {
    let request: JsonRpcRequest = match serde_json::from_str(&body) {
        Ok(request) => request,
        Err(error) => {
            // No id is recoverable from an unparseable body, so answer with the
            // JSON-RPC null-id form rather than dropping the connection.
            return json_response(failure(
                Value::Null,
                error_code::PARSE_ERROR,
                format!("malformed JSON-RPC request: {error}"),
            ));
        }
    };

    // A notification has no id and takes no result body. `notifications/initialized`
    // is the only one the host sends, and it expects `202 Accepted` — answering
    // 200-with-a-body would make it try to parse a response it has no id for.
    let Some(id) = request.id.clone() else {
        return StatusCode::ACCEPTED.into_response();
    };

    let response = match request.method.as_str() {
        "initialize" => success(id, initialize_result()),
        "tools/list" => success(id, tools_list_result()),
        "tools/call" => call_tool(executor.as_ref(), id, request.params).await,
        "ping" => success(id, json!({})),
        other => failure(
            id,
            error_code::METHOD_NOT_FOUND,
            format!("unsupported method: {other}"),
        ),
    };
    json_response(response)
}

/// Dispatch `tools/call`.
async fn call_tool<E: ToolExecutor>(executor: &E, id: Value, params: Option<Value>) -> Value {
    let params = params.unwrap_or(Value::Null);
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return failure(
            id,
            error_code::INVALID_PARAMS,
            "tools/call requires a tool name",
        );
    };
    let Some(tool) = Tool::from_wire_name(name) else {
        return failure(
            id,
            error_code::INVALID_PARAMS,
            format!("no such tool: {name}"),
        );
    };
    // An absent `arguments` is legal for a no-arg tool (browser_screenshot).
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    match executor.call(tool, arguments).await {
        Ok(result) => success(id, result),
        // The executor already turned recoverable failures (a selector that
        // matched nothing) into an `isError` result. Reaching here means the
        // browser itself is in trouble, which the run should see as an error.
        Err(error) => failure(id, error_code::INTERNAL_ERROR, error.to_string()),
    }
}

fn json_response(body: Value) -> Response {
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

/// Turn a tool error into the right MCP shape.
///
/// Two errors are the agent's answer rather than the browser's fault, so both come
/// back as a *recoverable* `isError` result the model can read and act on:
///
/// - [`Error::NoElement`] — the selector matched nothing; look at the page and try
///   another one.
/// - [`Error::NotApproved`] — the user declined the fill (or there was no one to
///   ask). The agent should do something else, not fail the run.
///
/// Everything else is a real fault and stays a JSON-RPC error.
pub fn tool_result_from(outcome: Result<Value>) -> Result<Value> {
    match outcome {
        Ok(value) => Ok(protocol::tool_success_result(value)),
        Err(error @ (Error::NoElement { .. } | Error::NotApproved { .. })) => {
            Ok(protocol::tool_error_result(error.to_string()))
        }
        Err(other) => Err(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt as _;

    /// Echoes its input, and fails `browser_click` two different ways so both
    /// error lanes are exercised.
    struct FakeExecutor;

    #[async_trait]
    impl ToolExecutor for FakeExecutor {
        async fn call(&self, tool: Tool, arguments: Value) -> Result<Value> {
            match tool {
                // Recoverable: surfaces as an `isError` result.
                Tool::BrowserClick => tool_result_from(Err(Error::NoElement {
                    selector: "#missing".into(),
                })),
                // Fatal: surfaces as a JSON-RPC error.
                Tool::BrowserFill => Err(Error::NoBrowser),
                _ => tool_result_from(Ok(json!({ "tool": tool.wire_name(), "echo": arguments }))),
            }
        }
    }

    /// POST one JSON-RPC message and return (status, parsed body).
    async fn rpc(body: Value) -> (StatusCode, Value) {
        let app = router(Arc::new(FakeExecutor));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(MCP_PATH)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .expect("build request"),
            )
            .await
            .expect("response");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .expect("read body");
        if bytes.is_empty() {
            return (status, Value::Null);
        }
        (status, serde_json::from_slice(&bytes).expect("decode json"))
    }

    #[tokio::test]
    async fn the_server_binds_loopback_only() {
        // The core patch (CP-4) waives private-IP egress denial for this
        // endpoint. That is only defensible because nothing off this machine can
        // reach it.
        let server = Server::bind(0).await.expect("bind");
        assert!(server.local_addr().ip().is_loopback());
        assert!(server.mcp_url().starts_with("http://127.0.0.1:"));
        assert!(server.mcp_url().ends_with("/mcp"));
    }

    #[tokio::test]
    async fn initialize_answers_with_the_protocol_version_the_host_stores() {
        let (status, body) = rpc(json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": protocol::PROTOCOL_VERSION },
        }))
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["id"], 1);
        assert_eq!(
            body["result"]["protocolVersion"],
            protocol::PROTOCOL_VERSION
        );
    }

    /// The host sends this as a notification (no id) and treats `202` with an
    /// empty body as success. If we answered `200` with a body, it would try to
    /// parse a JSON-RPC response that has no id to match — and fail the session.
    #[tokio::test]
    async fn the_initialized_notification_is_accepted_with_no_body() {
        let (status, body) = rpc(json!({
            "jsonrpc": "2.0", "method": "notifications/initialized",
        }))
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(body, Value::Null, "a notification must not get a body");
    }

    #[tokio::test]
    async fn tools_list_publishes_the_six_browser_tools() {
        let (status, body) =
            rpc(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" })).await;
        assert_eq!(status, StatusCode::OK);

        let tools = body["result"]["tools"].as_array().expect("an array");
        let mut names: Vec<&str> = tools
            .iter()
            .map(|tool| tool["name"].as_str().expect("a name"))
            .collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "browser_click",
                "browser_fill",
                "browser_find",
                "browser_get_text",
                "browser_navigate",
                "browser_screenshot",
            ]
        );
    }

    #[tokio::test]
    async fn a_tool_call_is_dispatched_and_its_result_round_trips() {
        let (status, body) = rpc(json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": "browser_navigate", "arguments": { "url": "https://example.com" } },
        }))
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["id"], 3);
        assert_eq!(body["result"]["isError"], false);
        let structured = &body["result"]["structuredContent"];
        assert_eq!(structured["tool"], "browser_navigate");
        assert_eq!(structured["echo"]["url"], "https://example.com");
    }

    /// The distinction the whole error mapping exists for.
    #[tokio::test]
    async fn a_missing_selector_is_a_recoverable_is_error_not_a_protocol_error() {
        let (status, body) = rpc(json!({
            "jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": { "name": "browser_click", "arguments": { "selector": "#missing" } },
        }))
        .await;
        assert_eq!(status, StatusCode::OK);
        // Not a JSON-RPC error: the model must get to see this and retry.
        assert!(body.get("error").is_none(), "{body}");
        assert_eq!(body["result"]["isError"], true);
        assert!(
            body["result"]["content"][0]["text"]
                .as_str()
                .expect("text")
                .contains("#missing")
        );
    }

    #[tokio::test]
    async fn a_dead_browser_is_a_json_rpc_error() {
        let (_, body) = rpc(json!({
            "jsonrpc": "2.0", "id": 5, "method": "tools/call",
            "params": { "name": "browser_fill", "arguments": { "selector": "#u", "value": "x" } },
        }))
        .await;
        assert!(body.get("result").is_none(), "{body}");
        assert_eq!(body["error"]["code"], error_code::INTERNAL_ERROR);
    }

    #[tokio::test]
    async fn an_unknown_tool_is_rejected_without_reaching_the_browser() {
        let (_, body) = rpc(json!({
            "jsonrpc": "2.0", "id": 6, "method": "tools/call",
            "params": { "name": "browser_exfiltrate", "arguments": {} },
        }))
        .await;
        assert_eq!(body["error"]["code"], error_code::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn an_unknown_method_is_method_not_found() {
        let (_, body) = rpc(json!({ "jsonrpc": "2.0", "id": 7, "method": "resources/list" })).await;
        assert_eq!(body["error"]["code"], error_code::METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn a_malformed_body_still_gets_a_structured_error() {
        let app = router(Arc::new(FakeExecutor));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(MCP_PATH)
                    .body(Body::from("not json at all"))
                    .expect("build request"),
            )
            .await
            .expect("response");
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("read body");
        let body: Value = serde_json::from_slice(&bytes).expect("decode json");
        assert_eq!(body["error"]["code"], error_code::PARSE_ERROR);
    }

    /// A no-arg tool must work when the host omits `arguments` entirely.
    #[tokio::test]
    async fn a_call_with_no_arguments_object_is_accepted() {
        let (status, body) = rpc(json!({
            "jsonrpc": "2.0", "id": 8, "method": "tools/call",
            "params": { "name": "browser_screenshot" },
        }))
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"]["isError"], false);
    }
}
