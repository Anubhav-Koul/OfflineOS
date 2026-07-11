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
//! | [`ring`] | The bounded sample ring shared by capture and the readers |
//! | [`envelope`] | RMS → mouth-open value for lip sync |
//! | [`session`] | The pure voice state machine |
//! | [`error`] | The crate's error type |

pub mod envelope;
pub mod error;
pub mod format;
pub mod ring;
pub mod session;

pub use envelope::EnvelopeFollower;
pub use error::{Error, Result};
pub use format::SAMPLE_RATE;
pub use ring::SampleRing;
pub use session::{VoiceEvent, VoiceSession, VoiceState};
