//! The crate's error type.

use std::fmt;

/// A browser-automation failure.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No Chrome or Edge could be located to launch.
    #[error("no compatible browser (Chrome or Edge) was found")]
    NoBrowser,
    /// The browser process could not be launched or driven.
    #[error("browser control failed: {0}")]
    Browser(String),
    /// A tool was asked to act on an element it could not find.
    #[error("no element matched the selector {selector:?}")]
    NoElement {
        /// The selector that matched nothing.
        selector: String,
    },
    /// The user declined to let the agent type into a sensitive field (or there
    /// was no one to ask). A refusal, not a fault — mapped to a recoverable tool
    /// error so the agent can carry on.
    #[error("the user did not approve typing into {field:?}")]
    NotApproved {
        /// The field the fill was declined for.
        field: String,
    },
    /// The loopback protocol server failed.
    #[error("{context}: {source}")]
    Io {
        /// What was being attempted.
        context: String,
        /// The underlying error.
        source: std::io::Error,
    },
    /// A protocol message could not be parsed or encoded.
    #[error("protocol error: {0}")]
    Protocol(String),
}

impl Error {
    /// An IO error with context about the operation that failed.
    pub fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }

    /// A browser-control failure from a message.
    pub fn browser(message: impl fmt::Display) -> Self {
        Self::Browser(message.to_string())
    }
}

/// The crate's result alias.
pub type Result<T> = std::result::Result<T, Error>;
