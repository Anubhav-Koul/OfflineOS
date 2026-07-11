//! The crate's error type.

use std::fmt;

/// A voice-pipeline failure.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No usable capture (or playback) device.
    #[error("no audio device: {0}")]
    NoDevice(String),
    /// The audio backend (cpal/WASAPI) failed.
    #[error("audio error: {0}")]
    Audio(String),
    /// A model could not be loaded (missing file, bad format).
    #[error("voice model error: {0}")]
    Model(String),
    /// Transcription failed.
    #[error("transcription failed: {0}")]
    Transcribe(String),
    /// The TTS engine failed.
    #[error("text-to-speech failed: {0}")]
    Tts(String),
    /// An IO error with context.
    #[error("{context}: {source}")]
    Io {
        /// What was being attempted.
        context: String,
        /// The underlying error.
        source: std::io::Error,
    },
}

impl Error {
    /// An audio-backend failure from a message.
    pub fn audio(message: impl fmt::Display) -> Self {
        Self::Audio(message.to_string())
    }

    /// A model-load failure from a message.
    pub fn model(message: impl fmt::Display) -> Self {
        Self::Model(message.to_string())
    }

    /// An IO error with context about the operation.
    pub fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}

/// The crate's result alias.
pub type Result<T> = std::result::Result<T, Error>;
