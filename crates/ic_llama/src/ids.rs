//! Validated domain types.
//!
//! Both follow the canonical newtype template in `.claude/rules/types.md`:
//! validation lives in one `validate` function reached from both `new` and
//! `TryFrom<String>` (so the wire and explicit construction agree), and there is
//! deliberately no `From<String>` or `Deref<Target = str>` that would let an
//! unvalidated string slip in.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::Error;

/// A lowercase, 64-character hex SHA-256 digest.
///
/// Exists so a digest can never be confused with the many other strings in this
/// crate (URLs, file names, model ids) at a call site like
/// `verify(path, digest)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct Sha256Hex(String);

impl Sha256Hex {
    fn validate(value: &str) -> Result<(), Error> {
        let reject = |reason: &'static str| {
            Err(Error::InvalidDigest {
                value: value.to_string(),
                reason,
            })
        };
        if value.len() != 64 {
            return reject("expected 64 hex characters");
        }
        if !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return reject("expected lowercase hex characters only");
        }
        Ok(())
    }

    /// Construct from an already-lowercased hex string.
    pub fn new(raw: impl Into<String>) -> Result<Self, Error> {
        let value = raw.into();
        Self::validate(&value)?;
        Ok(Self(value))
    }

    /// Parse a digest that may carry a `sha256:` prefix and/or uppercase hex —
    /// the shape GitHub's release API and HuggingFace's file API return.
    pub fn parse_prefixed(raw: &str) -> Result<Self, Error> {
        let bare = raw.strip_prefix("sha256:").unwrap_or(raw);
        Self::new(bare.trim().to_ascii_lowercase())
    }

    /// Borrow the hex text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Sha256Hex {
    type Error = Error;

    fn try_from(value: String) -> Result<Self, Error> {
        Self::validate(&value)?;
        Ok(Self(value))
    }
}

impl AsRef<str> for Sha256Hex {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Sha256Hex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<Sha256Hex> for String {
    fn from(digest: Sha256Hex) -> Self {
        digest.0
    }
}

/// The name a model is known by, both to IronClaw (as `LLM_MODEL`) and to this
/// crate (as a directory name under the model store).
///
/// Because it is used as a path component, the validator rejects anything that
/// could escape the store root or confuse Windows path handling.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct ModelId(String);

impl ModelId {
    /// Windows reserves these regardless of extension.
    const RESERVED: [&'static str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];

    fn validate(value: &str) -> Result<(), Error> {
        let reject = |reason: &'static str| {
            Err(Error::InvalidModelId {
                value: value.to_string(),
                reason,
            })
        };
        if value.is_empty() {
            return reject("must not be empty");
        }
        if value.chars().count() > 128 {
            return reject("must be at most 128 characters");
        }
        if !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return reject("may only contain ASCII letters, digits, '-', '_' and '.'");
        }
        // `.` and `..` are path traversal; a leading `.` also hides the
        // directory on Unix for no benefit.
        if value.starts_with('.') {
            return reject("must not start with '.'");
        }
        // Windows strips trailing dots and spaces, so `foo.` and `foo` would
        // collide on disk.
        if value.ends_with('.') {
            return reject("must not end with '.'");
        }
        let stem = value.split('.').next().unwrap_or(value);
        if Self::RESERVED
            .iter()
            .any(|reserved| stem.eq_ignore_ascii_case(reserved))
        {
            return reject("is a reserved Windows device name");
        }
        Ok(())
    }

    /// Construct a model id, rejecting anything unusable as a path component.
    pub fn new(raw: impl Into<String>) -> Result<Self, Error> {
        let value = raw.into();
        Self::validate(&value)?;
        Ok(Self(value))
    }

    /// Derive an id from a GGUF file name by dropping the `.gguf` extension.
    ///
    /// `Qwen3-4B-Q4_K_M.gguf` → `Qwen3-4B-Q4_K_M`.
    pub fn from_gguf_file_name(file_name: &str) -> Result<Self, Error> {
        let stem = file_name
            .strip_suffix(".gguf")
            .or_else(|| file_name.strip_suffix(".GGUF"))
            .unwrap_or(file_name);
        Self::new(stem)
    }

    /// Borrow the id text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ModelId {
    type Error = Error;

    fn try_from(value: String) -> Result<Self, Error> {
        Self::validate(&value)?;
        Ok(Self(value))
    }
}

impl AsRef<str> for ModelId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ModelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<ModelId> for String {
    fn from(id: ModelId) -> Self {
        id.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_accepts_lowercase_hex_and_strips_prefix() {
        let raw = "18d1c0d56792e6a9f5082d4343c2431617cd2914243bafdac852758240bb9bfa";
        assert_eq!(Sha256Hex::new(raw).expect("valid").as_str(), raw);
        let prefixed = format!("sha256:{}", raw.to_ascii_uppercase());
        assert_eq!(
            Sha256Hex::parse_prefixed(&prefixed)
                .expect("valid")
                .as_str(),
            raw
        );
    }

    #[test]
    fn digest_rejects_wrong_length_and_uppercase() {
        assert!(Sha256Hex::new("abc").is_err());
        assert!(Sha256Hex::new("A".repeat(64)).is_err());
        // 'g' is not hex.
        assert!(Sha256Hex::new("g".repeat(64)).is_err());
    }

    #[test]
    fn model_id_rejects_path_traversal_and_reserved_names() {
        for bad in [
            "",
            "..",
            ".hidden",
            "trailing.",
            "with/slash",
            "with\\backslash",
            "with space",
            "con",
            "NUL.gguf",
        ] {
            assert!(ModelId::new(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn model_id_from_gguf_file_name_drops_extension() {
        let id = ModelId::from_gguf_file_name("Qwen3-4B-Q4_K_M.gguf").expect("valid");
        assert_eq!(id.as_str(), "Qwen3-4B-Q4_K_M");
    }
}
