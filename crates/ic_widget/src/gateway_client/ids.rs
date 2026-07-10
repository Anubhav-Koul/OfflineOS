//! Identifiers the gateway hands us and we hand back.
//!
//! These are newtypes rather than `String` because every one of them appears in
//! a URL path segment, and three of them (`thread_id`, `run_id`, `gate_ref`)
//! appear in the *same* path:
//!
//! ```text
//! POST /threads/{thread_id}/runs/{run_id}/gates/{gate_ref}/resolve
//! ```
//!
//! Getting two of those the wrong way round is a runtime 404 rather than a
//! compile error if they are all `String`. Follows the canonical template in
//! `.claude/rules/types.md`.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::Error;

/// Longest identifier we will accept from the gateway or put in a path.
const MAX_LEN: usize = 256;

/// Generates a validated, path-safe identifier newtype.
macro_rules! gateway_id {
    ($(#[$meta:meta])* $name:ident, $kind:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(try_from = "String")]
        pub struct $name(String);

        impl $name {
            fn validate(value: &str) -> Result<(), Error> {
                let reject = |reason: &'static str| {
                    Err(Error::InvalidId {
                        kind: $kind,
                        value: value.to_string(),
                        reason,
                    })
                };
                if value.is_empty() {
                    return reject("must not be empty");
                }
                if value.len() > MAX_LEN {
                    return reject("is too long");
                }
                // Anything that would change the meaning of the URL path it is
                // interpolated into, or that would let a value smuggle a query
                // string or a second path segment.
                if value
                    .chars()
                    .any(|c| c.is_control() || matches!(c, '/' | '\\' | '?' | '#' | '%' | ' '))
                {
                    return reject("contains a character that is not safe in a URL path");
                }
                Ok(())
            }

            #[doc = concat!("Construct a validated ", $kind, ".")]
            pub fn new(raw: impl Into<String>) -> Result<Self, Error> {
                let value = raw.into();
                Self::validate(&value)?;
                Ok(Self(value))
            }

            /// Borrow the underlying text.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = Error;

            fn try_from(value: String) -> Result<Self, Error> {
                Self::validate(&value)?;
                Ok(Self(value))
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<$name> for String {
            fn from(id: $name) -> Self {
                id.0
            }
        }
    };
}

gateway_id!(
    /// A conversation. `POST /threads` mints one.
    ThreadId,
    "thread id"
);

gateway_id!(
    /// One execution of a turn. `POST /threads/{id}/messages` returns one, and
    /// the Stop button cancels it.
    RunId,
    "run id"
);

gateway_id!(
    /// An opaque handle to a pending tool-approval or auth gate.
    GateRef,
    "gate ref"
);

/// The idempotency key on every mutating request.
///
/// The gateway deduplicates by this value, so replaying a send after a dropped
/// connection returns `already_submitted` instead of running the turn twice.
/// Always mint a fresh one per user action, never per retry.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ClientActionId(String);

impl ClientActionId {
    /// Mint a new key for one user action.
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    /// Borrow the underlying text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for ClientActionId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ClientActionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_reject_path_and_query_injection() {
        for bad in [
            "",
            "a/b",
            "a\\b",
            "a?b",
            "a#b",
            "a%2Fb",
            "with space",
            "with\nnewline",
        ] {
            assert!(ThreadId::new(bad).is_err(), "{bad:?} should be rejected");
            assert!(RunId::new(bad).is_err(), "{bad:?} should be rejected");
            assert!(GateRef::new(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn ids_accept_the_shapes_the_gateway_actually_mints() {
        // Observed in the Phase 0 smoke: uuids, and `msg:<uuid>`-style refs.
        ThreadId::new("0198a1b2-c3d4-7e8f-9012-3456789abcde").expect("uuid");
        RunId::new("0198a1b2-c3d4-7e8f-9012-3456789abcde").expect("uuid");
        GateRef::new("gate:approve:0198a1b2").expect("colon-separated ref");
    }

    #[test]
    fn an_over_long_id_is_rejected() {
        assert!(ThreadId::new("x".repeat(MAX_LEN + 1)).is_err());
        ThreadId::new("x".repeat(MAX_LEN)).expect("at the limit");
    }

    #[test]
    fn deserialization_revalidates() {
        let error = serde_json::from_str::<ThreadId>(r#""a/b""#)
            .expect_err("a slash must not survive the wire");
        assert!(error.to_string().contains("URL path"), "{error}");
    }

    #[test]
    fn client_action_ids_are_unique_per_action() {
        assert_ne!(ClientActionId::new(), ClientActionId::new());
    }
}
