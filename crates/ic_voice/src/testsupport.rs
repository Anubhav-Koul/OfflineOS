//! Controllable fakes for the model-backed stages, so the driver can be exercised
//! end to end with no models and no sound card.
//!
//! Each fake is scriptable from the test: the wake word fires when armed, the VAD
//! reports speech for loud frames and silence for quiet ones (so a test "speaks" by
//! feeding non-zero samples), the transcriber and synthesizer return canned data
//! and record what they were asked, and the player completes instantly while
//! remembering what it played. Gated behind `test-support` so it never reaches a
//! shipping build — the same policy as [`crate::capture::FakeCapture`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::envelope::rms;
use crate::error::Result;
use crate::stages::{
    AmplitudeSink, Playback, Player, Speech, Synthesizer, Transcriber, Vad, WakeWord,
};

/// A wake word that fires when armed. The test calls [`arm`](Self::arm) just before
/// feeding the audio that should count as the wake phrase.
#[derive(Clone, Default)]
pub struct FakeWakeWord {
    armed: Arc<AtomicBool>,
}

impl FakeWakeWord {
    /// A disarmed wake word.
    pub fn new() -> Self {
        Self::default()
    }

    /// A shared arm control, so a test can trigger a detection from outside.
    pub fn control(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.armed)
    }

    /// Arm it: the next non-empty [`process`](WakeWord::process) call detects.
    pub fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }
}

impl WakeWord for FakeWakeWord {
    fn process(&mut self, samples: &[f32]) -> bool {
        if samples.is_empty() {
            return false;
        }
        self.armed.swap(false, Ordering::SeqCst)
    }
}

/// A VAD that calls a frame speech when its RMS exceeds a threshold. A test feeds
/// non-zero samples for "speech" and zeros for "silence".
pub struct FakeVad {
    frame: usize,
    threshold: f32,
}

impl FakeVad {
    /// A VAD scoring `frame`-sample chunks, speech when RMS ≥ `threshold`.
    pub fn new(frame: usize, threshold: f32) -> Self {
        Self { frame, threshold }
    }
}

impl Vad for FakeVad {
    fn frame_size(&self) -> usize {
        self.frame
    }

    fn predict(&mut self, frame: &[f32]) -> f32 {
        if rms(frame) >= self.threshold {
            0.95
        } else {
            0.0
        }
    }
}

/// A transcriber that returns a fixed string and records every utterance length it
/// was handed.
#[derive(Clone)]
pub struct FakeTranscriber {
    reply: String,
    seen: Arc<Mutex<Vec<usize>>>,
}

impl FakeTranscriber {
    /// A transcriber that always returns `reply`.
    pub fn new(reply: impl Into<String>) -> Self {
        Self {
            reply: reply.into(),
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// The sample counts of every utterance transcribed so far.
    pub fn utterance_lengths(&self) -> Vec<usize> {
        self.seen.lock().map(|v| v.clone()).unwrap_or_default()
    }
}

impl Transcriber for FakeTranscriber {
    fn transcribe(&mut self, samples: &[f32]) -> Result<String> {
        if let Ok(mut seen) = self.seen.lock() {
            seen.push(samples.len());
        }
        Ok(self.reply.clone())
    }
}

/// A synthesizer that renders each text to a fixed number of non-zero samples and
/// records the texts it was asked to speak.
#[derive(Clone)]
pub struct FakeSynthesizer {
    sample_rate: u32,
    samples_per_call: usize,
    spoken: Arc<Mutex<Vec<String>>>,
}

impl FakeSynthesizer {
    /// A synthesizer producing `samples_per_call` samples at `sample_rate`.
    pub fn new(sample_rate: u32, samples_per_call: usize) -> Self {
        Self {
            sample_rate,
            samples_per_call,
            spoken: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Everything it was asked to speak, in order.
    pub fn spoken(&self) -> Vec<String> {
        self.spoken.lock().map(|v| v.clone()).unwrap_or_default()
    }
}

impl Synthesizer for FakeSynthesizer {
    fn synthesize(&mut self, text: &str) -> Result<Speech> {
        if text.trim().is_empty() {
            return Ok(Speech {
                samples: Vec::new(),
                sample_rate: self.sample_rate,
            });
        }
        if let Ok(mut spoken) = self.spoken.lock() {
            spoken.push(text.to_string());
        }
        Ok(Speech {
            samples: vec![0.2; self.samples_per_call],
            sample_rate: self.sample_rate,
        })
    }
}

/// A player that "plays" instantly: it records the clip, emits one amplitude, and
/// completes. [`hold`](Self::hold) makes it stay playing until stopped, for
/// barge-in tests.
#[derive(Clone)]
pub struct FakePlayer {
    played: Arc<Mutex<Vec<Speech>>>,
    hold: Arc<AtomicBool>,
    stops: Arc<std::sync::atomic::AtomicUsize>,
}

impl Default for FakePlayer {
    fn default() -> Self {
        Self::new()
    }
}

impl FakePlayer {
    /// A player that finishes each clip immediately.
    pub fn new() -> Self {
        Self {
            played: Arc::new(Mutex::new(Vec::new())),
            hold: Arc::new(AtomicBool::new(false)),
            stops: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// Make playback hang until explicitly stopped — so a test can inject barge-in
    /// while "speaking".
    pub fn hold(&self) {
        self.hold.store(true, Ordering::SeqCst);
    }

    /// The clips it was asked to play.
    pub fn played(&self) -> Vec<Speech> {
        self.played.lock().map(|v| v.clone()).unwrap_or_default()
    }

    /// How many playbacks were stopped early (barge-in / mute).
    pub fn stop_count(&self) -> usize {
        self.stops.load(Ordering::SeqCst)
    }
}

impl Player for FakePlayer {
    fn play(&self, speech: Speech, amplitude: AmplitudeSink) -> Result<Playback> {
        if let Ok(mut played) = self.played.lock() {
            played.push(speech);
        }
        amplitude(0.5);
        let (tx, rx) = tokio::sync::oneshot::channel();
        let stops = Arc::clone(&self.stops);
        if self.hold.load(Ordering::SeqCst) {
            // Stay "playing": only stop() completes it, and it counts the stop.
            Ok(Playback::new(
                move || {
                    stops.fetch_add(1, Ordering::SeqCst);
                    let _ = tx.send(());
                },
                rx,
            ))
        } else {
            // Finish immediately.
            let _ = tx.send(());
            Ok(Playback::new(
                move || {
                    stops.fetch_add(1, Ordering::SeqCst);
                },
                rx,
            ))
        }
    }
}
