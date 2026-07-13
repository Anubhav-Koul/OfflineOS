//! The voice pipeline for the IronClaw desktop app.
//!
//! A **library** linked into `ic_widget` (not a separate process): voice is an
//! alternate *input* to the widget's existing chat, and it needs the widget's
//! `AppHandle` (for lip-sync events and the tray mute) and its `GatewayClient`. The
//! one subprocess is Piper (`piper.exe`), supervised via the same Job Object as the
//! other children.
//!
//! ## The loop
//!
//! ```text
//! mic ─cpal─▶ downmix+resample ─▶ SampleRing ─┬─▶ wake word (rustpotter)
//!                                              └─▶ VAD (Silero) ─▶ whisper STT
//!                                                                      │
//!   character mouth ◀─ RMS envelope ◀─ cpal playback ◀─ piper.exe ◀─ gateway reply
//!                                                          ▲              │
//!                                                          └── transcript ┘ (GatewayClient)
//! ```
//!
//! Voice reuses the widget's chat path wholesale: a transcript is just a
//! `send_message`, and the reply comes back through the same `timeline()` read the
//! typed UI uses. There is no gateway "voice channel".
//!
//! ## What this crate owns vs. the widget
//!
//! This crate owns the audio and the models. The widget owns the wiring: it feeds
//! transcripts to the gateway, forwards the lip-sync envelope as a Tauri event,
//! and drives the tray mute. The seams between them are traits and plain channels,
//! so the pure control logic ([`session`]) is testable with no audio hardware.
//!
//! | Module | Responsibility |
//! |---|---|
//! | [`format`] | Downmix + sample conversion to 16 kHz mono `f32` |
//! | [`resample`] | Rate conversion to 16 kHz (rubato FFT resampler) |
//! | [`capture`] | Microphone capture (cpal) behind a `Capture` trait |
//! | [`ring`] | The bounded sample ring shared by capture and the readers |
//! | [`envelope`] | RMS → mouth-open value for lip sync |
//! | [`endpoint`] | VAD probabilities → utterance boundaries (hysteresis) |
//! | [`stages`] | The wake / VAD / STT / TTS / playback trait seams |
//! | [`session`] | The pure voice state machine |
//! | [`error`] | The crate's error type |

pub mod assets;
pub mod capture;
pub mod device;
pub mod endpoint;
pub mod envelope;
pub mod error;
pub mod format;
pub mod pipeline;
pub mod playback;
pub mod resample;
pub mod ring;
pub mod session;
pub mod stages;
pub mod stt;
pub mod train;
pub mod tts;
pub mod vad;
pub mod wake;

#[cfg(any(test, feature = "test-support"))]
pub mod testsupport;

pub use assets::{PinnedAsset, VoiceAssets, bundled_wake_models};
pub use capture::{Capture, CpalCapture, input_devices};
pub use device::{DeviceChangeFn, DeviceWatcher};
pub use endpoint::{EndpointConfig, EndpointEvent, Endpointer};
pub use envelope::EnvelopeFollower;
pub use error::{Error, Result};
pub use format::SAMPLE_RATE;
pub use pipeline::{
    CaptureFactory, PipelineConfig, ReplyFn, RestartTrigger, StateFn, TranscriptFn, VoiceHandle,
    spawn,
};
pub use playback::CpalPlayer;
pub use resample::Resampler;
pub use ring::SampleRing;
pub use session::{VoiceEffect, VoiceEvent, VoiceSession, VoiceState};
pub use stages::{
    AmplitudeSink, Playback, Player, Speech, Synthesizer, Transcriber, Vad, WakeWord,
    null_amplitude,
};
pub use stt::WhisperStt;
pub use train::{MIN_SAMPLES as WAKE_MIN_SAMPLES, peak as sample_peak, train as train_wake_word};
pub use tts::{ChildEnlist, PiperTts, no_enlist};
pub use vad::SileroVad;
pub use wake::{NullWakeWord, RustpotterWake};
