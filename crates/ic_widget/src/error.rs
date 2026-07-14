//! The crate's error type.
//!
//! These messages reach a desktop user through a widget with no log pane, so
//! each one names the operation and the resource. Per
//! `.claude/rules/error-handling.md`, nothing here collapses a failure into a
//! default.

use std::time::Duration;

/// Convenience alias for fallible operations in this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Anything that can go wrong talking to `ironclaw-reborn`, supervising it, or
/// remembering where the widget was.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An I/O operation failed. `context` describes the operation.
    #[error("{context}")]
    Io {
        /// e.g. `reading the widget window state`.
        context: String,
        /// The underlying OS error.
        #[source]
        source: std::io::Error,
    },

    /// The HTTP client could not be constructed.
    #[error("could not initialize the HTTP client")]
    ClientInit(#[source] reqwest::Error),

    /// The transport failed (connection refused, reset, TLS).
    #[error("request to {url} failed")]
    Http {
        /// The request URL.
        url: String,
        /// The underlying transport error.
        #[source]
        source: reqwest::Error,
    },

    /// `serve` answered with an error. Carries the gateway's own sanitized
    /// taxonomy so the UI can distinguish "busy" from "denied" without parsing
    /// prose.
    #[error("{method} {path} failed with HTTP {status}: {code}/{kind}")]
    Gateway {
        /// The HTTP method.
        method: &'static str,
        /// The request path, without the base URL.
        path: String,
        /// The response status code.
        status: u16,
        /// `RebornServicesErrorCode`, e.g. `invalid_request`, `rate_limited`.
        code: String,
        /// `RebornServicesErrorKind`, e.g. `validation`, `busy`.
        kind: String,
        /// Whether the gateway says a retry may succeed.
        retryable: bool,
    },

    /// A response body did not deserialize into the shape this client expects —
    /// the single place upstream protocol drift surfaces.
    #[error("{path} returned a body this client does not understand: {reason}")]
    Protocol {
        /// The request path.
        path: String,
        /// What specifically failed to parse.
        reason: String,
    },

    /// A value that should have been a valid identifier wasn't.
    #[error("invalid {kind} {value:?}: {reason}")]
    InvalidId {
        /// Which identifier, e.g. `thread id`.
        kind: &'static str,
        /// The offending value.
        value: String,
        /// Why it was rejected.
        reason: &'static str,
    },

    /// The SSE stream ended or could not be re-established.
    #[error("the event stream for thread {thread_id} failed: {reason}")]
    EventStream {
        /// The thread being streamed.
        thread_id: String,
        /// What went wrong.
        reason: String,
    },

    /// `ironclaw-reborn serve` never became ready.
    #[error("ironclaw-reborn did not become ready within {0:?}")]
    GatewayStartupTimeout(Duration),

    /// `ironclaw-reborn serve` exited repeatedly and will not be restarted.
    #[error("ironclaw-reborn exited {crashes} times in a row{}", .last_output.as_ref().map(|o| format!("; last output: {o}")).unwrap_or_default())]
    GatewayUnhealthy {
        /// How many consecutive failures were observed.
        crashes: u32,
        /// Tail of the process output, when captured.
        last_output: Option<String>,
    },

    /// The OS credential store refused to hand over (or store) a secret.
    #[error("could not {operation} the {entry} in the credential store")]
    Keyring {
        /// `read`, `store`, or `delete`.
        operation: &'static str,
        /// Which credential. Owned rather than `&'static str`: provider key
        /// entries are named after catalog ids, which are not known at compile
        /// time. Never the secret itself — this string reaches log lines.
        entry: String,
        /// The underlying keyring error.
        #[source]
        source: keyring::Error,
    },

    /// The user tried to save an empty API key.
    #[error("the {provider} API key is empty")]
    BlankProviderKey {
        /// The provider's catalog id. Never the key.
        provider: String,
    },

    /// A JSON value could not be produced or consumed.
    #[error("{context}")]
    Json {
        /// What was being (de)serialized.
        context: String,
        /// The underlying serde error.
        #[source]
        source: serde_json::Error,
    },
}

impl Error {
    /// Attach an operation description to an [`std::io::Error`].
    pub(crate) fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Error::Io {
            context: context.into(),
            source,
        }
    }

    /// Whether the caller may usefully retry. Transport failures and any
    /// gateway error the gateway itself flagged retryable qualify.
    pub fn is_retryable(&self) -> bool {
        match self {
            Error::Http { .. } => true,
            Error::Gateway { retryable, .. } => *retryable,
            _ => false,
        }
    }

    /// Whether this is the gateway's per-caller stream cap (3 concurrent SSE +
    /// WS streams). The UI should close a stream before opening another rather
    /// than retrying blindly.
    pub fn is_stream_cap(&self) -> bool {
        matches!(self, Error::Gateway { status: 429, .. })
    }

    /// Whether the bearer token was rejected. Distinct from the gateway being
    /// down: the process is answering, it just does not believe us.
    ///
    /// Note the gateway returns a bare text body here, not its usual JSON error
    /// shape (`webui_serve.rs:737`), so [`Error::Gateway::code`] is `unknown`.
    pub fn is_unauthorized(&self) -> bool {
        matches!(self, Error::Gateway { status: 401, .. })
    }

    /// Whether a mutation was refused because its `client_action_id` had already
    /// been used.
    ///
    /// A replayed `send_message` is only answered with
    /// `SubmitOutcome::AlreadySubmitted` while the original message is still in
    /// the `Submitted` state (`reborn_services.rs:711`). Once the turn reaches a
    /// terminal state, the *same* replay becomes a `409` instead
    /// (`reborn_services.rs:741`). Both mean "your message was accepted exactly
    /// once"; only the timing differs. A caller retrying a send it is unsure
    /// about should treat this as success, not as a failure to show the user.
    pub fn is_duplicate_action(&self) -> bool {
        matches!(self, Error::Gateway { status: 409, .. })
    }

    /// Whether the gateway says the thing we named does not exist.
    ///
    /// The Stop button lives in this race: a run id the UI has been holding can
    /// be gone by the time the user clicks (a gateway restart replaces the run,
    /// not the thread). Verified against the running gateway — an unknown run is
    /// a clean `404`, not an error state. The UI must refresh on it, never show a
    /// dialog. See `ic_integration_tests/tests/chat_control.rs`.
    pub fn is_not_found(&self) -> bool {
        matches!(self, Error::Gateway { status: 404, .. })
    }
}
