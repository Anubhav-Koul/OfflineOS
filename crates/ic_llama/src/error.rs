//! The crate's error type.
//!
//! Every variant names *what* was being done and *which* resource it was being
//! done to, because these errors surface in the desktop UI where the user has
//! no logs to cross-reference. Per `.claude/rules/error-handling.md` nothing in
//! this crate swallows a failure into a default — an I/O error on a model file
//! or a checksum mismatch on a downloaded binary is always propagated.

use std::path::PathBuf;
use std::time::Duration;

/// Convenience alias for fallible operations in this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Anything that can go wrong managing llama.cpp binaries, GGUF models, or the
/// `llama-server` sidecar.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An I/O operation failed. `context` describes the operation, not just the
    /// path, so the message reads as a sentence.
    #[error("{context}")]
    Io {
        /// e.g. `creating model directory D:\models`.
        context: String,
        /// The underlying OS error.
        #[source]
        source: std::io::Error,
    },

    /// The transport itself failed (DNS, TLS, connection reset, body stream).
    #[error("HTTP request to {url} failed")]
    Http {
        /// The request URL.
        url: String,
        /// The underlying transport error.
        #[source]
        source: reqwest::Error,
    },

    /// The HTTP client could not be constructed (bad TLS backend, no system
    /// root certificates).
    #[error("could not initialize the HTTP client")]
    ClientInit(#[source] reqwest::Error),

    /// The server answered, but not with a status we can use.
    #[error("HTTP {status} from {url}")]
    HttpStatus {
        /// The request URL.
        url: String,
        /// The response status code.
        status: u16,
    },

    /// A downloaded artifact did not match its pinned digest. The partial file
    /// is deleted before this is returned, so a retry starts clean.
    #[error("checksum mismatch for {url}: expected sha256 {expected}, got {actual}")]
    ChecksumMismatch {
        /// Where the bytes came from.
        url: String,
        /// The digest we pinned.
        expected: String,
        /// The digest the bytes actually hashed to.
        actual: String,
    },

    /// A value that should have been a lowercase 64-character hex SHA-256
    /// wasn't.
    #[error("invalid sha256 digest {value:?}: {reason}")]
    InvalidDigest {
        /// The offending value.
        value: String,
        /// Why it was rejected.
        reason: &'static str,
    },

    /// A value that should have been a usable model identifier wasn't.
    #[error("invalid model id {value:?}: {reason}")]
    InvalidModelId {
        /// The offending value.
        value: String,
        /// Why it was rejected.
        reason: &'static str,
    },

    /// The file is not a GGUF file, is a GGUF version we don't read, or is
    /// structurally corrupt.
    #[error("{path} is not a readable GGUF model: {reason}")]
    Gguf {
        /// The model file.
        path: PathBuf,
        /// What specifically was wrong.
        reason: String,
    },

    /// The downloaded llama.cpp archive could not be turned into a usable
    /// runtime directory.
    #[error("llama.cpp archive {path} is unusable: {reason}")]
    Archive {
        /// The `.zip` we downloaded.
        path: PathBuf,
        /// What specifically was wrong.
        reason: String,
    },

    /// `llama-server` never reported healthy before the deadline. It is still
    /// loading, wedged, or dying in a way that leaves the process alive.
    #[error("llama-server did not become healthy within {0:?}")]
    StartupTimeout(Duration),

    /// `llama-server` crashed `crashes` times in a row, so the model is marked
    /// suspect and will not be auto-restarted again.
    #[error("model {model} is suspect: llama-server exited {crashes} times without staying healthy{}", .last_output.as_ref().map(|o| format!("; last output: {o}")).unwrap_or_default())]
    ModelSuspect {
        /// The model that kept taking the server down.
        model: String,
        /// How many consecutive failures were observed.
        crashes: u32,
        /// Tail of the server's stderr, when we captured any.
        last_output: Option<String>,
    },

    /// The machine cannot hold the model, so we decline to start a server that
    /// would only thrash or be killed by the OS.
    #[error("{reason}")]
    ModelDoesNotFit {
        /// User-facing explanation from the placement planner.
        reason: String,
    },

    /// The sidecar was asked for something after it had been stopped.
    #[error("llama-server sidecar is not running")]
    NotRunning,

    /// We only pin llama.cpp binaries for Windows x64. The rest of the crate
    /// (GGUF parsing, placement, supervision) builds and runs everywhere.
    #[error("prebuilt llama.cpp binaries are only pinned for Windows x64")]
    UnsupportedPlatform,

    /// The GPU could not be queried. Callers treat this as "no GPU" and fall
    /// back to CPU rather than failing the launch.
    #[error("GPU probe failed: {0}")]
    GpuProbe(String),

    /// A HuggingFace repository, revision, or file could not be resolved.
    #[error("cannot resolve {file} in HuggingFace repo {repo}@{revision}: {reason}")]
    HubResolve {
        /// `owner/name`.
        repo: String,
        /// Branch, tag, or commit.
        revision: String,
        /// Path within the repo.
        file: String,
        /// What specifically was wrong.
        reason: String,
    },
}

impl Error {
    /// Attach an operation description to an [`std::io::Error`].
    ///
    /// Use a present-participle phrase naming the resource, e.g.
    /// `Error::io("reading GGUF header from foo.gguf", err)`.
    pub(crate) fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Error::Io {
            context: context.into(),
            source,
        }
    }
}
