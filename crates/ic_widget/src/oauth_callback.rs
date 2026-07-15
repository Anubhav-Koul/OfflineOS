//! The connector OAuth callback listener (Phase 8b.1).
//!
//! # Why this exists
//!
//! Google matches a registered redirect URI **byte-for-byte**, and restricted
//! Gmail scopes are bound to an OAuth **client** only a human can create in the
//! Cloud console. Meanwhile the widget takes a *fresh, OS-assigned* port for
//! `ironclaw-reborn serve` at every launch (two instances must coexist), so
//! `serve`'s own callback URL changes every time. A user cannot re-register a
//! redirect URI with Google on every launch.
//!
//! So the redirect lands here instead: a widget-owned loopback listener on a
//! **fixed** port (default 51789), the one stable address in the system. The
//! user registers `http://127.0.0.1:<port>/api/reborn/product-auth/oauth/google/callback`
//! with Google once, and it survives relaunches.
//!
//! # Why it proxies rather than handles
//!
//! `serve` owns the token exchange — it holds the PKCE verifier it minted during
//! `oauth/start`, in a process-local cache — so only `serve` can complete the
//! flow. This listener therefore **proxies** the browser's callback into
//! `serve`'s dynamic callback route verbatim (`serve` does not care which port
//! the browser hit, only that the request reaches its route with the right
//! query), and streams `serve`'s completion page back to the browser.
//!
//! # What this listener owns
//!
//! - **Loopback bind only** (`127.0.0.1`), never `0.0.0.0`.
//! - **A CSRF check.** `serve` generates the opaque `state` and validates it
//!   cryptographically, but this listener adds its own binding: it is armed with
//!   the exact `state` embedded in the authorization URL `serve` returned, and a
//!   callback whose `state` does not match is refused (`400`) and never
//!   forwarded. A forged hit to the fixed port cannot drive `serve`.
//! - **One-shot.** After the first matching callback it stops; a replay is `409`.
//! - **Closed when idle.** The listener is bound only for the duration of one
//!   flow and shut down on completion or timeout.

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::{RawQuery, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Serialize;
use tokio::sync::oneshot;

/// The path the Google OAuth redirect lands on — the same path `serve` serves
/// its Google callback at, so the proxy is a straight pass-through of path and
/// query.
pub const CALLBACK_PATH: &str = "/api/reborn/product-auth/oauth/google/callback";

/// The redirect URI to register with Google and hand to `serve` as
/// `IRONCLAW_REBORN_GOOGLE_OAUTH_REDIRECT_URI`, for a given fixed port.
pub fn redirect_uri(port: u16) -> String {
    format!("http://{}:{port}{CALLBACK_PATH}", Ipv4Addr::LOCALHOST)
}

/// How a single OAuth callback flow ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub enum FlowOutcome {
    /// The callback reached `serve` and it accepted it (`2xx`). The credential
    /// exchange ran; the caller confirms the credential landed by polling the
    /// setup projection.
    Completed,
    /// `serve` received the callback but refused it (non-`2xx`) — typically a
    /// token-exchange failure.
    ServeRejected {
        /// `serve`'s status code.
        status: u16,
    },
    /// The provider returned an `error` on the redirect (the user declined
    /// consent, or the client is misconfigured). Forwarded to `serve` so it can
    /// tear the flow down, then reported here.
    ProviderError {
        /// The provider's `error` code, e.g. `access_denied`.
        reason: String,
    },
}

/// Why a flow could not run to an outcome.
#[derive(Debug, thiserror::Error)]
pub enum OAuthCallbackError {
    /// The fixed port could not be bound — most likely another process (or a
    /// previous, un-closed flow) holds it.
    #[error(
        "the OAuth callback port {port} is not available — another application may be using it, \
         or a previous sign-in did not finish. Close it or choose a different port in Settings."
    )]
    PortUnavailable {
        /// The port that could not be bound.
        port: u16,
        /// The underlying bind error.
        #[source]
        source: std::io::Error,
    },
    /// The user did not finish signing in within the allowed window.
    #[error("sign-in timed out — the authorization was not completed in time")]
    TimedOut,
    /// The listener stopped before an outcome was produced.
    #[error("the OAuth callback listener stopped unexpectedly")]
    Interrupted,
}

/// The armed state one callback flow runs against.
struct FlowState {
    /// The exact `state` the callback must carry, taken from the authorization
    /// URL. This is the CSRF binding.
    expected_state: String,
    /// `serve`'s base URL, e.g. `http://127.0.0.1:38080`, to proxy the callback
    /// into.
    serve_base: String,
    http: reqwest::Client,
    /// Set true once a matching callback has been consumed — the one-shot latch.
    consumed: AtomicBool,
    /// Delivers the outcome to the awaiting caller. Taken on first send.
    done: Mutex<Option<oneshot::Sender<FlowOutcome>>>,
}

/// A bound, listening OAuth callback flow, ready to receive the browser's
/// redirect. Produced by [`arm`]; consumed by [`ArmedListener::wait`].
///
/// Splitting binding from waiting matters: the fixed port is bound *before* the
/// browser is opened, so a port clash surfaces as an error the caller can show
/// instead of a browser that opens onto a dead redirect.
pub struct ArmedListener {
    outcome_rx: oneshot::Receiver<FlowOutcome>,
    shutdown_tx: oneshot::Sender<()>,
    server_task: tokio::task::JoinHandle<()>,
}

/// Bind the fixed loopback port and arm the listener with the CSRF `state` and
/// `serve`'s base URL. Returns once the port is bound and listening.
///
/// `expected_state` is the `state` parameter from the authorization URL `serve`
/// returned — see [`state_from_authorization_url`].
pub async fn arm(
    port: u16,
    expected_state: String,
    serve_base: String,
) -> Result<ArmedListener, OAuthCallbackError> {
    // Loopback only — never a routable interface.
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port))
        .await
        .map_err(|source| OAuthCallbackError::PortUnavailable { port, source })?;

    let (outcome_tx, outcome_rx) = oneshot::channel();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let state = Arc::new(FlowState {
        expected_state,
        serve_base,
        http: reqwest::Client::new(),
        consumed: AtomicBool::new(false),
        done: Mutex::new(Some(outcome_tx)),
    });

    let server = axum::serve(listener, router(state)).with_graceful_shutdown(async move {
        // Shut down when the caller says so (after it has the outcome, or on
        // timeout). Graceful shutdown lets the in-flight callback response flush
        // before the listener closes.
        let _ = shutdown_rx.await;
    });
    let server_task = tokio::spawn(async move {
        if let Err(error) = server.await {
            tracing::warn!(%error, "OAuth callback listener exited with an error");
        }
    });

    Ok(ArmedListener {
        outcome_rx,
        shutdown_tx,
        server_task,
    })
}

impl ArmedListener {
    /// Wait for the browser to complete the callback, or `timeout` to elapse. The
    /// listener is torn down before returning either way, so the fixed port is
    /// free the moment this returns.
    pub async fn wait(self, timeout: Duration) -> Result<FlowOutcome, OAuthCallbackError> {
        let result = match tokio::time::timeout(timeout, self.outcome_rx).await {
            Ok(Ok(outcome)) => Ok(outcome),
            // The sender was dropped without sending — the listener died.
            Ok(Err(_)) => Err(OAuthCallbackError::Interrupted),
            Err(_elapsed) => Err(OAuthCallbackError::TimedOut),
        };
        // Trigger graceful shutdown and wait for the listener to actually stop.
        let _ = self.shutdown_tx.send(());
        let _ = self.server_task.await;
        result
    }
}

/// The router, split out so tests can drive it without a socket.
fn router(state: Arc<FlowState>) -> Router {
    Router::new()
        .route(CALLBACK_PATH, get(handle))
        .with_state(state)
}

/// Handle the browser's redirect: CSRF-check, one-shot, then proxy into `serve`.
async fn handle(State(state): State<Arc<FlowState>>, RawQuery(raw): RawQuery) -> Response {
    let raw = raw.unwrap_or_default();
    let pairs: Vec<(String, String)> = serde_urlencoded::from_str(&raw).unwrap_or_default();
    let param = |key: &str| {
        pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    };

    // CSRF: the callback must carry the exact state we armed with. A missing or
    // mismatched state is refused and never forwarded — and does NOT consume the
    // one-shot, so the genuine callback can still arrive.
    match param("state") {
        Some(value) if value == state.expected_state => {}
        _ => {
            tracing::warn!("rejected an OAuth callback whose state did not match the armed flow");
            return (
                StatusCode::BAD_REQUEST,
                page("This sign-in link does not match the request that started it."),
            )
                .into_response();
        }
    }

    // One-shot: only the first matching callback is honored.
    if state.consumed.swap(true, Ordering::SeqCst) {
        return (
            StatusCode::CONFLICT,
            page("This sign-in has already been completed."),
        )
            .into_response();
    }

    // Forward the callback verbatim into `serve`, which holds the PKCE verifier
    // and does the token exchange. We ask for HTML so `serve` returns its
    // "you can close this window" completion page, which we pass straight back.
    let target = format!("{}{CALLBACK_PATH}?{raw}", state.serve_base);
    let proxied = state
        .http
        .get(&target)
        .header(header::ACCEPT, "text/html")
        .send()
        .await;

    // If the provider itself reported an error, the outcome is a denial even
    // when `serve` answers cleanly (it tears the flow down and 200s).
    let provider_error = param("error")
        .filter(|value| !value.is_empty())
        .map(String::from);

    match proxied {
        Ok(response) => {
            let status = response.status();
            let body = response.bytes().await.unwrap_or_default();
            let outcome = if let Some(reason) = provider_error.clone() {
                FlowOutcome::ProviderError { reason }
            } else if status.is_success() {
                FlowOutcome::Completed
            } else {
                FlowOutcome::ServeRejected {
                    status: status.as_u16(),
                }
            };
            send_outcome(&state, outcome);
            // Relay serve's own completion page (or its error page) to the browser.
            (
                StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::OK),
                [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                body,
            )
                .into_response()
        }
        Err(error) => {
            tracing::warn!(%error, "could not reach serve to complete the OAuth callback");
            send_outcome(
                &state,
                provider_error
                    .map(|reason| FlowOutcome::ProviderError { reason })
                    .unwrap_or(FlowOutcome::ServeRejected { status: 502 }),
            );
            (
                StatusCode::BAD_GATEWAY,
                page("Could not reach the local agent to finish signing in."),
            )
                .into_response()
        }
    }
}

/// Deliver the outcome to the awaiting caller, once.
fn send_outcome(state: &FlowState, outcome: FlowOutcome) {
    if let Ok(mut slot) = state.done.lock()
        && let Some(sender) = slot.take()
    {
        let _ = sender.send(outcome);
    }
}

/// A minimal self-contained HTML page for the browser. `serve`'s own completion
/// page is used on the happy path; this is only for the listener's own refusals.
fn page(message: &str) -> Response {
    let html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <title>Sign-in</title></head><body><p>{}</p></body></html>",
        html_escape(message)
    );
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response()
}

/// Escape the few characters that matter for a text node. The messages are
/// static and contain none of them today, but rendering user-influenced text
/// unescaped is the kind of habit that grows a hole.
fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Pull the `state` parameter out of an authorization URL, decoded — this is the
/// CSRF token [`run_flow`] arms against.
pub fn state_from_authorization_url(url: &str) -> Option<String> {
    let query = url.split_once('?').map(|(_, query)| query)?;
    let pairs: Vec<(String, String)> = serde_urlencoded::from_str(query).ok()?;
    pairs
        .into_iter()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn armed(expected_state: &str, serve_base: &str) -> Arc<FlowState> {
        Arc::new(FlowState {
            expected_state: expected_state.to_string(),
            serve_base: serve_base.to_string(),
            http: reqwest::Client::new(),
            consumed: AtomicBool::new(false),
            done: Mutex::new(None),
        })
    }

    async fn call(state: Arc<FlowState>, query: &str) -> Response {
        router(state)
            .oneshot(
                Request::builder()
                    .uri(format!("{CALLBACK_PATH}?{query}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    #[test]
    fn redirect_uri_is_loopback_on_the_fixed_port() {
        assert_eq!(
            redirect_uri(51789),
            "http://127.0.0.1:51789/api/reborn/product-auth/oauth/google/callback"
        );
    }

    #[test]
    fn state_is_read_and_decoded_from_the_authorization_url() {
        let url = "https://accounts.google.com/o/oauth2/v2/auth?client_id=abc\
                   &redirect_uri=http%3A%2F%2F127.0.0.1%3A51789%2Fcb&state=xy%2Fz%2Babc&scope=s";
        assert_eq!(
            state_from_authorization_url(url).as_deref(),
            Some("xy/z+abc"),
            "the state must come back decoded, matching how axum decodes the callback"
        );
    }

    #[test]
    fn no_state_in_a_url_without_a_query() {
        assert_eq!(state_from_authorization_url("https://example.com/x"), None);
    }

    #[tokio::test]
    async fn a_callback_whose_state_does_not_match_is_refused_and_not_consumed() {
        let state = armed("the-real-state", "http://127.0.0.1:1");
        let response = call(state.clone(), "state=forged&code=abc").await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        // A mismatch must not spend the one-shot: the genuine callback still comes.
        assert!(
            !state.consumed.load(Ordering::SeqCst),
            "a forged callback must not consume the one-shot latch"
        );
    }

    #[tokio::test]
    async fn a_missing_state_is_refused() {
        let state = armed("the-real-state", "http://127.0.0.1:1");
        let response = call(state, "code=abc").await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_matching_callback_is_proxied_to_serve_and_reports_completed() {
        // A stand-in for serve: returns a completion page on the callback path,
        // and asserts the state reached it forwarded verbatim.
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let serve_base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route(
            CALLBACK_PATH,
            get(|RawQuery(raw): RawQuery| async move {
                assert!(raw.unwrap_or_default().contains("state=the-real-state"));
                ([(header::CONTENT_TYPE, "text/html")], "<p>done</p>")
            }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let (tx, rx) = oneshot::channel();
        let state = Arc::new(FlowState {
            expected_state: "the-real-state".to_string(),
            serve_base,
            http: reqwest::Client::new(),
            consumed: AtomicBool::new(false),
            done: Mutex::new(Some(tx)),
        });

        let response = call(state.clone(), "state=the-real-state&code=auth-code&scope=s").await;
        assert_eq!(response.status(), StatusCode::OK);
        let outcome = rx.await.expect("outcome delivered");
        assert_eq!(outcome, FlowOutcome::Completed);

        // One-shot: a replay is a 409.
        let replay = call(state, "state=the-real-state&code=auth-code").await;
        assert_eq!(replay.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn a_provider_error_reports_provider_error_even_when_serve_answers_ok() {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let serve_base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route(
            CALLBACK_PATH,
            get(|| async { ([(header::CONTENT_TYPE, "text/html")], "<p>denied</p>") }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let (tx, rx) = oneshot::channel();
        let state = Arc::new(FlowState {
            expected_state: "s".to_string(),
            serve_base,
            http: reqwest::Client::new(),
            consumed: AtomicBool::new(false),
            done: Mutex::new(Some(tx)),
        });

        let response = call(state, "state=s&error=access_denied").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            rx.await.expect("outcome"),
            FlowOutcome::ProviderError {
                reason: "access_denied".to_string()
            }
        );
    }
}
