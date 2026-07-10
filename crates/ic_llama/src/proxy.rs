//! A loopback proxy that makes IronClaw's tool schemas compilable by llama.cpp.
//!
//! # Why this exists
//!
//! Tool calling against `llama-server` requires `--jinja` (it answers
//! `500 tools param requires --jinja flag` without it). With `--jinja`, every
//! request carrying `tools` makes llama.cpp compile the tool schemas into a GBNF
//! grammar that constrains the model's output. That compiler turns
//! `"maxLength": N` into a `char{0,N}` repetition — and its GBNF parser rejects
//! any repetition count of 2000 or more:
//!
//! ```text
//! parse: error parsing grammar: number of repetitions exceeds sane defaults
//! srv send_error: Failed to initialize samplers: failed to parse grammar
//! ```
//!
//! IronClaw's built-in `builtin__spawn_subagent` declares `maxLength: 65536` on
//! its `task` and `handoff` fields, so **every** agent turn fails against a local
//! model. The limit was measured, not guessed: 1999 compiles, 2000 does not (see
//! `docs/desktop/llama-cpp-tool-grammar.md`).
//!
//! # Why a proxy rather than a core patch
//!
//! Lowering that one schema's `maxLength` would edit an IronClaw core crate for a
//! problem that is not specific to it. Any WASM tool, any MCP server the *user*
//! installs, may declare a bound of its own — and a `maxLength: 4096` on someone
//! else's tool would break local inference just as completely. The incompatibility
//! is between llama.cpp and the OpenAI tool-schema dialect at large, so it is
//! fixed at the boundary between them, which is ours.
//!
//! Bounds below the limit are left alone: they are useful constraints and they
//! compile. Only bounds llama.cpp cannot express are dropped, and dropping them
//! only widens what the model is allowed to emit.
//!
//! Everything else is forwarded verbatim, body streamed, so the proxy stays
//! transparent to responses (including SSE, though `RigAdapter` does not stream).

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use futures_util::TryStreamExt as _;
use http_body_util::{BodyExt as _, Full, Limited, StreamBody, combinators::BoxBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{CONTENT_LENGTH, CONTENT_TYPE, HOST, HeaderValue, TRANSFER_ENCODING};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::error::{Error, Result};

/// The largest repetition count llama.cpp's GBNF parser accepts. Measured
/// against `b9948`: `char{0,1999}` compiles, `char{0,2000}` does not.
pub const MAX_GRAMMAR_REPETITIONS: u64 = 1999;

/// JSON Schema keywords that llama.cpp turns into a GBNF repetition bound.
const REPETITION_KEYWORDS: [&str; 4] = ["maxLength", "minLength", "maxItems", "minItems"];

/// Requests larger than this are refused rather than buffered. IronClaw's
/// payloads are tens of kilobytes; a full context of messages is well under this.
const MAX_REQUEST_BYTES: usize = 64 << 20;

/// A running proxy in front of `llama-server`.
///
/// Dropping it stops the listener. In-flight requests are aborted.
pub struct SchemaProxy {
    port: u16,
    handle: JoinHandle<()>,
}

impl SchemaProxy {
    /// Start listening on a free loopback port, forwarding to `upstream`.
    ///
    /// `upstream` is an origin with no trailing slash, e.g.
    /// `http://127.0.0.1:8080`. The path of each request is preserved, so the
    /// proxy's own `/v1/...` maps to the upstream's `/v1/...`.
    pub async fn start(upstream: impl Into<String>) -> Result<Self> {
        let upstream = Arc::new(upstream.into());
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|source| Error::io("binding the llama.cpp schema proxy", source))?;
        let port = listener
            .local_addr()
            .map_err(|source| Error::io("reading back the proxy port", source))?
            .port();

        // No timeout: a long generation legitimately holds the connection open
        // for minutes, and the sidecar supervisor is what notices a dead server.
        let client = reqwest::Client::builder()
            .build()
            .map_err(Error::ClientInit)?;

        tracing::info!(port, upstream = %upstream, "schema proxy listening");

        let handle = tokio::spawn(async move {
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        tracing::warn!(%error, "schema proxy stopped accepting connections");
                        return;
                    }
                };
                let upstream = Arc::clone(&upstream);
                let client = client.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request| {
                        forward(request, Arc::clone(&upstream), client.clone())
                    });
                    if let Err(error) = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await
                    {
                        // A client that hangs up mid-request lands here; it is
                        // not actionable.
                        tracing::debug!(%error, "schema proxy connection ended");
                    }
                });
            }
        });

        Ok(Self { port, handle })
    }

    /// The address IronClaw should be pointed at.
    pub fn addr(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], self.port))
    }

    /// The OpenAI-compatible base URL, including the `/v1` suffix.
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }
}

impl Drop for SchemaProxy {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Sanitize a chat-completions body and forward everything else untouched.
async fn forward(
    request: Request<Incoming>,
    upstream: Arc<String>,
    client: reqwest::Client,
) -> std::result::Result<Response<BoxBody<Bytes, std::io::Error>>, Infallible> {
    Ok(match relay(request, upstream, client).await {
        Ok(response) => response,
        Err(message) => {
            tracing::warn!(%message, "schema proxy could not reach llama-server");
            bad_gateway(&message)
        }
    })
}

async fn relay(
    request: Request<Incoming>,
    upstream: Arc<String>,
    client: reqwest::Client,
) -> std::result::Result<Response<BoxBody<Bytes, std::io::Error>>, String> {
    let (parts, body) = request.into_parts();

    let body = Limited::new(body, MAX_REQUEST_BYTES)
        .collect()
        .await
        .map_err(|error| format!("reading the request body: {error}"))?
        .to_bytes();

    let is_chat_completions =
        parts.method == hyper::Method::POST && parts.uri.path().ends_with("/chat/completions");
    let body = if is_chat_completions {
        sanitize_body(&body)
    } else {
        body
    };

    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or("/");
    let url = format!("{upstream}{path_and_query}");

    // `Host` names the proxy, and the body length changed under us; let reqwest
    // set both. `Transfer-Encoding` describes framing we have already undone.
    let mut headers = parts.headers.clone();
    headers.remove(HOST);
    headers.remove(CONTENT_LENGTH);
    headers.remove(TRANSFER_ENCODING);

    let response = client
        .request(parts.method, &url)
        .headers(headers)
        .body(body)
        .send()
        .await
        .map_err(|error| format!("forwarding to {url}: {error}"))?;

    let mut builder = Response::builder().status(response.status());
    for (name, value) in response.headers() {
        if name == TRANSFER_ENCODING {
            continue; // hyper re-frames the body itself
        }
        builder = builder.header(name, value);
    }

    // Stream the body through rather than buffering it, so a streamed response
    // stays streamed.
    let stream = response
        .bytes_stream()
        .map_ok(Frame::data)
        .map_err(std::io::Error::other);
    builder
        .body(StreamBody::new(stream).boxed())
        .map_err(|error| format!("building the response: {error}"))
}

/// Built by hand rather than through `Response::builder`, whose fallible
/// `body()` would need an `unwrap` for a response that cannot fail to construct.
fn bad_gateway(message: &str) -> Response<BoxBody<Bytes, std::io::Error>> {
    let body = serde_json::json!({
        "error": {
            "message": format!("local model unreachable: {message}"),
            "type": "server_error",
            "code": 502,
        }
    })
    .to_string();
    let body = Full::new(Bytes::from(body))
        .map_err(|never: Infallible| match never {})
        .boxed();

    let mut response = Response::new(body);
    *response.status_mut() = StatusCode::BAD_GATEWAY;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}

/// Strip repetition bounds llama.cpp cannot compile.
///
/// Returns the body unchanged when it is not JSON, when it carries no tools, or
/// when every bound is already within range — a body we do not understand is a
/// body we forward verbatim rather than corrupt.
fn sanitize_body(body: &Bytes) -> Bytes {
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return body.clone();
    };

    let mut stripped = 0;
    // Only the places a JSON Schema can appear. `messages` is deliberately not
    // walked: its contents are the user's and the model's, not a schema.
    for key in ["tools", "response_format"] {
        if let Some(node) = value.get_mut(key) {
            stripped += strip_oversized_bounds(node);
        }
    }
    if stripped == 0 {
        return body.clone();
    }

    tracing::debug!(
        stripped,
        "removed tool-schema bounds that exceed llama.cpp's grammar repetition limit"
    );
    match serde_json::to_vec(&value) {
        Ok(bytes) => Bytes::from(bytes),
        // Re-serializing a value we just parsed cannot fail; forward the
        // original rather than drop the request if it somehow does.
        Err(error) => {
            tracing::error!(%error, "could not re-serialize the sanitized request");
            body.clone()
        }
    }
}

/// Recursively remove `maxLength`/`minLength`/`maxItems`/`minItems` whose value
/// exceeds [`MAX_GRAMMAR_REPETITIONS`]. Returns how many were removed.
fn strip_oversized_bounds(node: &mut serde_json::Value) -> usize {
    let mut stripped = 0;
    match node {
        serde_json::Value::Object(map) => {
            for keyword in REPETITION_KEYWORDS {
                let oversized = map
                    .get(keyword)
                    .and_then(serde_json::Value::as_u64)
                    .is_some_and(|bound| bound > MAX_GRAMMAR_REPETITIONS);
                if oversized {
                    map.remove(keyword);
                    stripped += 1;
                }
            }
            for child in map.values_mut() {
                stripped += strip_oversized_bounds(child);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                stripped += strip_oversized_bounds(item);
            }
        }
        _ => {}
    }
    stripped
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sanitize(value: serde_json::Value) -> serde_json::Value {
        let body = Bytes::from(serde_json::to_vec(&value).expect("serialize"));
        serde_json::from_slice(&sanitize_body(&body)).expect("valid json out")
    }

    #[test]
    fn the_bound_that_breaks_spawn_subagent_is_removed() {
        // The exact shape IronClaw sends, reduced to the offending field.
        let sanitized = sanitize(json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "builtin__spawn_subagent",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "task": {"type": "string", "maxLength": 65536},
                            "handoff": {"type": "string", "maxLength": 65536}
                        }
                    }
                }
            }]
        }));

        let properties = &sanitized["tools"][0]["function"]["parameters"]["properties"];
        assert!(properties["task"].get("maxLength").is_none());
        assert!(properties["handoff"].get("maxLength").is_none());
        // Everything else survives untouched.
        assert_eq!(properties["task"]["type"], "string");
        assert_eq!(sanitized["messages"][0]["content"], "hi");
    }

    #[test]
    fn bounds_llama_cpp_can_compile_are_left_alone() {
        let sanitized = sanitize(json!({
            "tools": [{"function": {"parameters": {"properties": {
                "small": {"type": "string", "maxLength": 1999},
                "big": {"type": "string", "maxLength": 2000}
            }}}}]
        }));
        let properties = &sanitized["tools"][0]["function"]["parameters"]["properties"];
        // 1999 compiles; 2000 is the first value that does not.
        assert_eq!(properties["small"]["maxLength"], 1999);
        assert!(properties["big"].get("maxLength").is_none());
    }

    #[test]
    fn every_repetition_keyword_is_covered() {
        let sanitized = sanitize(json!({
            "tools": [{"parameters": {
                "a": {"maxLength": 70000},
                "b": {"minLength": 70000},
                "c": {"maxItems": 70000},
                "d": {"minItems": 70000}
            }}]
        }));
        let parameters = &sanitized["tools"][0]["parameters"];
        for field in ["a", "b", "c", "d"] {
            assert_eq!(
                parameters[field].as_object().expect("an object").len(),
                0,
                "{field} kept an oversized bound"
            );
        }
    }

    #[test]
    fn nested_schemas_are_reached() {
        let sanitized = sanitize(json!({
            "tools": [{"function": {"parameters": {"properties": {"outer": {
                "type": "array",
                "items": {"type": "object", "properties": {
                    "inner": {"type": "string", "maxLength": 65536}
                }}
            }}}}}]
        }));
        let inner = &sanitized["tools"][0]["function"]["parameters"]["properties"]["outer"]["items"]
            ["properties"]["inner"];
        assert!(inner.get("maxLength").is_none());
    }

    #[test]
    fn a_user_message_that_merely_mentions_max_length_is_untouched() {
        // `messages` is never walked: this is content, not schema.
        let sanitized = sanitize(json!({
            "messages": [{"role": "user", "content": {"maxLength": 65536}}],
            "tools": []
        }));
        assert_eq!(sanitized["messages"][0]["content"]["maxLength"], 65536);
    }

    #[test]
    fn a_body_with_no_oversized_bounds_is_passed_through_byte_for_byte() {
        let original = Bytes::from_static(br#"{"model":"m","tools":[],"extra":  1}"#);
        assert_eq!(sanitize_body(&original), original);
    }

    #[test]
    fn a_body_that_is_not_json_is_forwarded_verbatim() {
        let original = Bytes::from_static(b"not json at all");
        assert_eq!(sanitize_body(&original), original);
    }

    #[test]
    fn a_negative_or_non_numeric_bound_is_ignored_rather_than_removed() {
        let sanitized = sanitize(json!({
            "tools": [{"parameters": {
                "a": {"maxLength": -5},
                "b": {"maxLength": "lots"}
            }}]
        }));
        assert_eq!(sanitized["tools"][0]["parameters"]["a"]["maxLength"], -5);
        assert_eq!(
            sanitized["tools"][0]["parameters"]["b"]["maxLength"],
            "lots"
        );
    }
}
