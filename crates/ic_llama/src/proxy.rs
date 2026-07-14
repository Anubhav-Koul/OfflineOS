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
//!
//! # Two more things it does, for the same reason
//!
//! Once something sits between the gateway and the model, it is the only place
//! that sees every request *and* every response. Two features the fork owes the
//! user live here rather than in a core crate:
//!
//! - **Cloud failover** ([`CloudFallback`]). `LLM_BACKEND` holds exactly one
//!   value, so the gateway cannot be told about two providers; and upstream's
//!   `FailoverProvider` is only ever constructed same-backend. Rather than patch
//!   a core crate (`docs/desktop/llm-provider-selection.md`, option 2), the proxy
//!   retries a failed chat completion against a cloud endpoint itself. The
//!   gateway keeps seeing one `openai_compatible` endpoint and needs no change —
//!   and the cloud key never enters the gateway's environment at all, which is
//!   strictly better for the secrets rule.
//! - **Token metrics** ([`Metrics`]). `llama-server` reports `timings` on every
//!   non-streamed completion; the proxy reads them on the way past. Nothing else
//!   in the stack sees a token count, because the gateway's event stream carries
//!   no usage data.
//!
//! # TODO: an abort endpoint, so Stop actually stops the GPU
//!
//! Verified in Phase 8a (`ic_integration_tests/tests/chat_control.rs`):
//! `cancel_run` does **not** abort the gateway's in-flight HTTP request to the
//! provider. So when the user presses Stop, `llama-server` keeps generating to
//! completion — the GPU burns tokens for an answer nobody will ever read, and on
//! a 4B model on an iGPU that is tens of seconds of stolen compute.
//!
//! The proxy is the one place that can fix this without a core patch: it holds
//! the upstream request. Give it a small loopback control endpoint (`POST
//! /_ic/abort`, say) that drops the in-flight upstream connection — llama.cpp
//! aborts a completion the moment its client disconnects — and have the widget
//! call it from the same click that calls `cancel_run`. The gateway is untouched
//! and none the wiser; it still gets its cancelled run.
//!
//! Not built yet because the widget must know *which* proxy request belongs to
//! the run being cancelled, and today it does not: the gateway multiplexes turns
//! over one client. Tracking a run→request mapping (the gateway sends no
//! correlation id, so it would have to be inferred) is the actual work.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use bytes::Bytes;
use futures_util::TryStreamExt as _;
use http_body_util::{BodyExt as _, Full, Limited, StreamBody, combinators::BoxBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{
    AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, HOST, HeaderValue, TRANSFER_ENCODING,
};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::Serialize;
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

/// Where a request goes when the local model cannot answer it.
///
/// The endpoint must speak the OpenAI Chat Completions dialect — which Anthropic
/// and OpenAI both do — because the request is forwarded in the shape the gateway
/// already produced. That is the cost of the route-around: a cloud provider's
/// *native* surface is not reachable this way. It buys never touching a core
/// crate, and never handing the gateway a cloud key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudFallback {
    /// Origin including the `/v1` suffix, e.g. `https://api.openai.com/v1`.
    pub base_url: String,
    /// The bearer token. Never logged.
    pub api_key: String,
    /// The model name to ask the cloud for — the local model's name means
    /// nothing there, so it is swapped into the body.
    pub model: String,
}

impl CloudFallback {
    /// The chat-completions URL, tolerating a trailing slash on `base_url`.
    fn chat_url(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }
}

/// What the proxy has seen. Read by the dashboard's model panel.
///
/// Every field describes the **most recent completion**, except the counters.
/// `None` means no completion has been observed yet (or the server reported no
/// timings — an older llama.cpp, or a cloud answer).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Metrics {
    /// Generation speed of the last completion.
    pub tokens_per_second: Option<f64>,
    /// Tokens the model generated in the last completion.
    pub completion_tokens: Option<u64>,
    /// Tokens the last prompt occupied.
    pub prompt_tokens: Option<u64>,
    /// Wall-clock time the proxy waited for the last completion.
    pub latency_ms: Option<u64>,
    /// Completions observed since the proxy started.
    pub completions: u64,
    /// How many requests the local model could not answer and the cloud did.
    pub failovers: u64,
    /// Whether the last completion was answered by the cloud rather than locally.
    pub last_was_cloud: bool,
}

/// A running proxy in front of `llama-server`.
///
/// Dropping it stops the listener. In-flight requests are aborted.
pub struct SchemaProxy {
    port: u16,
    metrics: Arc<Mutex<Metrics>>,
    handle: JoinHandle<()>,
}

/// The shared state one connection needs.
struct Context {
    upstream: String,
    fallback: Option<CloudFallback>,
    metrics: Arc<Mutex<Metrics>>,
    client: reqwest::Client,
}

impl SchemaProxy {
    /// Start listening on a free loopback port, forwarding to `upstream`.
    ///
    /// `upstream` is an origin with no trailing slash, e.g.
    /// `http://127.0.0.1:8080`. The path of each request is preserved, so the
    /// proxy's own `/v1/...` maps to the upstream's `/v1/...`.
    pub async fn start(upstream: impl Into<String>) -> Result<Self> {
        Self::start_with(upstream, None).await
    }

    /// Start with a cloud endpoint to fall back to when the local model fails.
    ///
    /// A `None` fallback behaves exactly like [`SchemaProxy::start`]: a local
    /// failure is a `502` the gateway reports, which is the honest answer when
    /// there is nowhere else to ask.
    pub async fn start_with(
        upstream: impl Into<String>,
        fallback: Option<CloudFallback>,
    ) -> Result<Self> {
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

        let metrics = Arc::new(Mutex::new(Metrics::default()));
        let upstream = upstream.into();
        tracing::info!(
            port,
            upstream = %upstream,
            failover = fallback.is_some(),
            "schema proxy listening"
        );
        let context = Arc::new(Context {
            upstream,
            fallback,
            metrics: Arc::clone(&metrics),
            client,
        });

        let handle = tokio::spawn(async move {
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        tracing::warn!(%error, "schema proxy stopped accepting connections");
                        return;
                    }
                };
                let context = Arc::clone(&context);
                tokio::spawn(async move {
                    let service = service_fn(move |request| forward(request, Arc::clone(&context)));
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

        Ok(Self {
            port,
            metrics,
            handle,
        })
    }

    /// The address IronClaw should be pointed at.
    pub fn addr(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], self.port))
    }

    /// The OpenAI-compatible base URL, including the `/v1` suffix.
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    /// What the proxy has seen: tokens/sec, token counts, failover count.
    pub fn metrics(&self) -> Metrics {
        *self
            .metrics
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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
    context: Arc<Context>,
) -> std::result::Result<Response<BoxBody<Bytes, std::io::Error>>, Infallible> {
    Ok(match relay(request, context).await {
        Ok(response) => response,
        Err(message) => {
            tracing::warn!(%message, "schema proxy could not reach llama-server");
            bad_gateway(&message)
        }
    })
}

async fn relay(
    request: Request<Incoming>,
    context: Arc<Context>,
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
    let url = format!("{}{path_and_query}", context.upstream);

    // `Host` names the proxy, and the body length changed under us; let reqwest
    // set both. `Transfer-Encoding` describes framing we have already undone.
    let mut headers = parts.headers.clone();
    headers.remove(HOST);
    headers.remove(CONTENT_LENGTH);
    headers.remove(TRANSFER_ENCODING);

    let started = Instant::now();
    let attempt = context
        .client
        .request(parts.method.clone(), &url)
        .headers(headers.clone())
        .body(body.clone())
        .send()
        .await;

    // Only a *chat completion* is worth failing over. A `/v1/models` probe that
    // fails means the local server is down, and answering it from the cloud
    // would advertise models the sidecar does not have.
    let local_failure = match &attempt {
        Err(error) => Some(error.to_string()),
        Ok(response) if response.status().is_server_error() => {
            Some(format!("local model answered {}", response.status()))
        }
        Ok(_) => None,
    };

    if let (true, Some(reason), Some(fallback)) =
        (is_chat_completions, &local_failure, &context.fallback)
    {
        tracing::warn!(%reason, "the local model failed; falling back to the cloud");
        return cloud_completion(&context, fallback, &headers, &body, started).await;
    }

    let response = attempt.map_err(|error| format!("forwarding to {url}: {error}"))?;

    if is_chat_completions && !is_event_stream(response.headers()) {
        // A non-streamed completion is small JSON that carries the token counts
        // — buffer it to read them, then hand it on whole. Streamed responses
        // are passed through untouched (`RigAdapter` never asks for one).
        return buffered_completion(&context, response, started, false).await;
    }

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

/// Ask the cloud the same question the local model could not answer.
async fn cloud_completion(
    context: &Context,
    fallback: &CloudFallback,
    headers: &hyper::HeaderMap,
    body: &Bytes,
    started: Instant,
) -> std::result::Result<Response<BoxBody<Bytes, std::io::Error>>, String> {
    // The gateway's bearer is the sidecar's throwaway key and means nothing to a
    // cloud provider; replace it rather than leak it upstream.
    let mut headers = headers.clone();
    headers.remove(AUTHORIZATION);
    let bearer = HeaderValue::from_str(&format!("Bearer {}", fallback.api_key))
        .map_err(|_| "the cloud API key is not a valid header value".to_string())?;
    headers.insert(AUTHORIZATION, bearer);

    let response = context
        .client
        .post(fallback.chat_url())
        .headers(headers)
        .body(retarget_model(body, &fallback.model))
        .send()
        .await
        .map_err(|error| format!("the cloud fallback also failed: {error}"))?;

    if is_event_stream(response.headers()) {
        // Not a shape we can read metrics from, but still a valid answer.
        let mut builder = Response::builder().status(response.status());
        for (name, value) in response.headers() {
            if name != TRANSFER_ENCODING {
                builder = builder.header(name, value);
            }
        }
        let stream = response
            .bytes_stream()
            .map_ok(Frame::data)
            .map_err(std::io::Error::other);
        return builder
            .body(StreamBody::new(stream).boxed())
            .map_err(|error| format!("building the response: {error}"));
    }
    buffered_completion(context, response, started, true).await
}

/// Buffer a completion, record what it says about itself, and pass it on whole.
async fn buffered_completion(
    context: &Context,
    response: reqwest::Response,
    started: Instant,
    from_cloud: bool,
) -> std::result::Result<Response<BoxBody<Bytes, std::io::Error>>, String> {
    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .bytes()
        .await
        .map_err(|error| format!("reading the completion body: {error}"))?;

    if status.is_success() {
        record_metrics(&context.metrics, &body, started.elapsed(), from_cloud);
    }

    let mut builder = Response::builder().status(status);
    for (name, value) in &headers {
        // The length is re-derived from the buffered body; the framing is ours.
        if name == TRANSFER_ENCODING || name == CONTENT_LENGTH {
            continue;
        }
        builder = builder.header(name, value);
    }
    builder
        .body(
            Full::new(body)
                .map_err(|never: Infallible| match never {})
                .boxed(),
        )
        .map_err(|error| format!("building the response: {error}"))
}

/// Pull the token counts out of a completion body.
///
/// `llama-server` adds a `timings` object OpenAI does not define — it is the
/// only place a real tokens/sec figure exists, since the gateway's event stream
/// carries no usage at all. When it is absent (an older build, or a cloud
/// answer), the rate is derived from `usage.completion_tokens` over the wall
/// clock, which includes the prompt pass and so reads a little low; it is
/// labelled the same because both answer the question the user is asking.
fn record_metrics(
    metrics: &Mutex<Metrics>,
    body: &Bytes,
    elapsed: std::time::Duration,
    from_cloud: bool,
) {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return;
    };
    // A tool-call turn has no `usage` in some builds; a body we cannot read is
    // one we do not count.
    let completion_tokens = value["usage"]["completion_tokens"].as_u64();
    let prompt_tokens = value["usage"]["prompt_tokens"].as_u64();

    let timings = &value["timings"];
    let tokens_per_second = timings["predicted_per_second"]
        .as_f64()
        .filter(|rate| rate.is_finite() && *rate > 0.0)
        .or_else(|| {
            let tokens = completion_tokens? as f64;
            let seconds = elapsed.as_secs_f64();
            (seconds > 0.0 && tokens > 0.0).then(|| tokens / seconds)
        });

    let mut guard = metrics
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.completions += 1;
    if from_cloud {
        guard.failovers += 1;
    }
    guard.last_was_cloud = from_cloud;
    guard.tokens_per_second = tokens_per_second;
    guard.completion_tokens = completion_tokens;
    guard.prompt_tokens = prompt_tokens;
    guard.latency_ms = Some(elapsed.as_millis().min(u64::MAX as u128) as u64);
}

/// Swap the `model` field for the cloud's own name. The local model's id means
/// nothing to a cloud provider, which would answer `404 model_not_found`.
fn retarget_model(body: &Bytes, model: &str) -> Bytes {
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return body.clone();
    };
    value["model"] = serde_json::Value::String(model.to_string());
    match serde_json::to_vec(&value) {
        Ok(bytes) => Bytes::from(bytes),
        Err(error) => {
            tracing::error!(%error, "could not retarget the request at the cloud model");
            body.clone()
        }
    }
}

fn is_event_stream(headers: &reqwest::header::HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"))
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

    // ------------------------------------------------------------- metrics

    fn metrics_of(body: serde_json::Value, elapsed_ms: u64, from_cloud: bool) -> Metrics {
        let metrics = Mutex::new(Metrics::default());
        record_metrics(
            &metrics,
            &Bytes::from(serde_json::to_vec(&body).expect("serialize")),
            std::time::Duration::from_millis(elapsed_ms),
            from_cloud,
        );
        metrics.into_inner().expect("not poisoned")
    }

    #[test]
    fn llama_cpps_own_timings_are_the_tokens_per_second() {
        // `predicted_per_second` is the server's measurement of generation
        // alone; it is what a user means by "tokens/sec".
        let metrics = metrics_of(
            json!({
                "usage": {"prompt_tokens": 120, "completion_tokens": 40},
                "timings": {"predicted_per_second": 42.5}
            }),
            4_000,
            false,
        );
        assert_eq!(metrics.tokens_per_second, Some(42.5));
        assert_eq!(metrics.completion_tokens, Some(40));
        assert_eq!(metrics.prompt_tokens, Some(120));
        assert_eq!(metrics.latency_ms, Some(4_000));
        assert_eq!(metrics.completions, 1);
        assert_eq!(metrics.failovers, 0);
        assert!(!metrics.last_was_cloud);
    }

    #[test]
    fn without_timings_the_rate_falls_back_to_the_wall_clock() {
        // A cloud provider reports no `timings`. Tokens over elapsed time reads
        // a little low (it includes the prompt pass) but answers the question.
        let metrics = metrics_of(json!({"usage": {"completion_tokens": 50}}), 2_000, true);
        assert_eq!(metrics.tokens_per_second, Some(25.0));
        assert!(metrics.last_was_cloud);
        assert_eq!(metrics.failovers, 1);
    }

    #[test]
    fn a_body_with_no_usage_counts_the_completion_but_claims_no_rate() {
        let metrics = metrics_of(json!({"choices": []}), 1_000, false);
        assert_eq!(metrics.completions, 1);
        assert_eq!(metrics.tokens_per_second, None);
        assert_eq!(metrics.completion_tokens, None);
    }

    #[test]
    fn a_zero_rate_is_not_reported_as_a_rate() {
        // Guards a division that would otherwise emit 0 tok/s or an infinity and
        // have the dashboard render it as fact.
        let metrics = metrics_of(
            json!({"usage": {"completion_tokens": 0}, "timings": {"predicted_per_second": 0.0}}),
            1_000,
            false,
        );
        assert_eq!(metrics.tokens_per_second, None);
    }

    // ------------------------------------------------------------ failover

    #[test]
    fn the_cloud_request_asks_for_the_cloud_model() {
        // The local model's id means nothing to a cloud provider, which would
        // answer 404 model_not_found.
        let body = Bytes::from(
            serde_json::to_vec(&json!({"model": "Qwen3-4B-Q4_K_M", "messages": []}))
                .expect("serialize"),
        );
        let retargeted: serde_json::Value =
            serde_json::from_slice(&retarget_model(&body, "claude-sonnet-4-20250514"))
                .expect("valid json");
        assert_eq!(retargeted["model"], "claude-sonnet-4-20250514");
        assert!(retargeted["messages"].is_array(), "the rest is untouched");
    }

    #[test]
    fn a_body_that_is_not_json_is_retargeted_by_leaving_it_alone() {
        let original = Bytes::from_static(b"not json");
        assert_eq!(retarget_model(&original, "m"), original);
    }

    #[test]
    fn the_chat_url_tolerates_a_trailing_slash() {
        let with = CloudFallback {
            base_url: "https://api.openai.com/v1/".into(),
            api_key: "k".into(),
            model: "m".into(),
        };
        let without = CloudFallback {
            base_url: "https://api.openai.com/v1".into(),
            ..with.clone()
        };
        assert_eq!(
            with.chat_url(),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(with.chat_url(), without.chat_url());
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

    // ------------------------------------------- failover, end to end

    /// A one-shot HTTP server that answers every request with `body` and records
    /// what it was asked. Stands in for a cloud provider.
    async fn mock_endpoint(
        body: String,
    ) -> (u16, Arc<Mutex<Vec<(String, String)>>>, JoinHandle<()>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        // (authorization header, request body) for every call.
        let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);

        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let body = body.clone();
                let sink = Arc::clone(&sink);
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let mut chunk = [0u8; 4096];
                    // Read until the body is complete (Content-Length is enough
                    // here: the proxy always sends a length, never chunked).
                    loop {
                        let read = stream.read(&mut chunk).await.unwrap_or(0);
                        if read == 0 {
                            break;
                        }
                        buffer.extend_from_slice(&chunk[..read]);
                        let text = String::from_utf8_lossy(&buffer);
                        if let Some((head, tail)) = text.split_once("\r\n\r\n") {
                            let length: usize = head
                                .lines()
                                .find_map(|line| {
                                    let (name, value) = line.split_once(':')?;
                                    name.trim()
                                        .eq_ignore_ascii_case("content-length")
                                        .then(|| value.trim().parse().ok())?
                                })
                                .unwrap_or(0);
                            if tail.len() >= length {
                                let authorization = head
                                    .lines()
                                    .find_map(|line| {
                                        let (name, value) = line.split_once(':')?;
                                        name.trim()
                                            .eq_ignore_ascii_case("authorization")
                                            .then(|| value.trim().to_string())
                                    })
                                    .unwrap_or_default();
                                sink.lock()
                                    .expect("lock")
                                    .push((authorization, tail.to_string()));
                                break;
                            }
                        }
                    }
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.flush().await;
                });
            }
        });
        (port, seen, handle)
    }

    /// A port nothing is listening on — a sidecar that is down.
    fn dead_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        port
    }

    #[tokio::test]
    async fn a_dead_local_model_is_answered_by_the_cloud() {
        let cloud_body = json!({
            "choices": [{"message": {"role": "assistant", "content": "from the cloud"}}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 20}
        })
        .to_string();
        let (cloud_port, seen, _cloud) = mock_endpoint(cloud_body).await;

        let proxy = SchemaProxy::start_with(
            format!("http://127.0.0.1:{}", dead_port()),
            Some(CloudFallback {
                base_url: format!("http://127.0.0.1:{cloud_port}/v1"),
                api_key: "sk-test".into(),
                model: "cloud-model".into(),
            }),
        )
        .await
        .expect("start the proxy");

        let response = reqwest::Client::new()
            .post(format!("{}/chat/completions", proxy.base_url()))
            .header(AUTHORIZATION, "Bearer the-sidecars-throwaway-key")
            .json(&json!({"model": "local-model", "messages": [{"role": "user", "content": "hi"}]}))
            .send()
            .await
            .expect("the proxy should answer");
        assert!(response.status().is_success());
        let answered: serde_json::Value = response.json().await.expect("json");
        assert_eq!(
            answered["choices"][0]["message"]["content"], "from the cloud",
            "the gateway sees one endpoint and never learns the local model died"
        );

        let calls = seen.lock().expect("lock").clone();
        let [(authorization, body)] = calls.as_slice() else {
            panic!("expected exactly one cloud call, got {calls:?}");
        };
        assert_eq!(
            authorization, "Bearer sk-test",
            "the sidecar's throwaway key must never be forwarded to a cloud provider"
        );
        let sent: serde_json::Value = serde_json::from_str(body).expect("json");
        assert_eq!(
            sent["model"], "cloud-model",
            "retargeted at the cloud model"
        );
        assert_eq!(sent["messages"][0]["content"], "hi", "same question");

        let metrics = proxy.metrics();
        assert_eq!(metrics.failovers, 1);
        assert!(metrics.last_was_cloud);
        assert_eq!(metrics.completion_tokens, Some(20));
    }

    #[tokio::test]
    async fn with_no_fallback_a_dead_local_model_is_an_honest_502() {
        let proxy = SchemaProxy::start(format!("http://127.0.0.1:{}", dead_port()))
            .await
            .expect("start the proxy");

        let response = reqwest::Client::new()
            .post(format!("{}/chat/completions", proxy.base_url()))
            .json(&json!({"model": "local-model", "messages": []}))
            .send()
            .await
            .expect("the proxy should answer");
        assert_eq!(
            response.status(),
            StatusCode::BAD_GATEWAY,
            "with nowhere else to ask, saying so beats inventing an answer"
        );
        assert_eq!(proxy.metrics().failovers, 0);
    }

    #[tokio::test]
    async fn a_healthy_local_model_is_never_sent_to_the_cloud() {
        let local_body = json!({
            "choices": [{"message": {"role": "assistant", "content": "from the sidecar"}}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 7},
            "timings": {"predicted_per_second": 31.25}
        })
        .to_string();
        let (local_port, _local_seen, _local) = mock_endpoint(local_body).await;
        let (cloud_port, cloud_seen, _cloud) = mock_endpoint("{}".to_string()).await;

        let proxy = SchemaProxy::start_with(
            format!("http://127.0.0.1:{local_port}"),
            Some(CloudFallback {
                base_url: format!("http://127.0.0.1:{cloud_port}/v1"),
                api_key: "sk-test".into(),
                model: "cloud-model".into(),
            }),
        )
        .await
        .expect("start the proxy");

        let answered: serde_json::Value = reqwest::Client::new()
            .post(format!("{}/chat/completions", proxy.base_url()))
            .json(&json!({"model": "local-model", "messages": []}))
            .send()
            .await
            .expect("answer")
            .json()
            .await
            .expect("json");

        assert_eq!(
            answered["choices"][0]["message"]["content"],
            "from the sidecar"
        );
        assert!(
            cloud_seen.lock().expect("lock").is_empty(),
            "a working local model must never leak a request (or a key) to the cloud"
        );

        // And the sidecar's own timings became the tokens/sec the panel shows.
        let metrics = proxy.metrics();
        assert_eq!(metrics.tokens_per_second, Some(31.25));
        assert_eq!(metrics.completions, 1);
        assert_eq!(metrics.failovers, 0);
        assert!(!metrics.last_was_cloud);
    }
}
