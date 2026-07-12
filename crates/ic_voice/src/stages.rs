//! The model-backed stages of the pipeline, as traits.
//!
//! Each expensive or hardware-bound step — wake word, VAD, transcription, speech
//! synthesis, playback — sits behind a trait, exactly like [`crate::capture`] does
//! for the microphone. The driver ([`crate::pipeline`]) is written against these
//! traits, so it can be exercised end to end by fakes with no models loaded and no
//! sound card, while the real implementations ([`crate::wake`], [`crate::vad`],
//! [`crate::stt`], [`crate::tts`], [`crate::playback`]) are only touched by
//! `#[ignore]`d tests that need the actual assets.
//!
//! All audio here is the pipeline's canonical format — 16 kHz mono `f32` in
//! `[-1, 1]` — except [`Speech`], which carries its own rate because a TTS engine
//! renders at whatever rate its voice was trained on (Piper's is 22.05 kHz).

use std::sync::Arc;

use crate::error::Result;

/// Spots the wake phrase in a stream of captured audio.
///
/// Fed newly-captured samples as they arrive; maintains its own sliding window and
/// detection state. Returns `true` on the batch that completes a detection.
pub trait WakeWord: Send {
    /// Feed newly-captured 16 kHz mono samples; return `true` if the wake phrase
    /// was spotted. Successive calls compose — the detector buffers internally.
    fn process(&mut self, samples: &[f32]) -> bool;

    /// Forget partial state after a detection, so the tail of the wake phrase does
    /// not immediately re-trigger. Default is a no-op for detectors that self-reset.
    fn reset(&mut self) {}
}

/// Scores short frames of audio with a speech probability, for endpointing.
pub trait Vad: Send {
    /// The exact number of samples the model scores per call. The driver reads the
    /// ring in blocks of this size so [`predict`](Self::predict) always gets a full
    /// frame (Silero v5 at 16 kHz wants 512 samples ≈ 32 ms).
    fn frame_size(&self) -> usize;

    /// Score one frame of exactly [`frame_size`](Self::frame_size) samples; return
    /// the probability that it contains speech, in `[0, 1]`.
    fn predict(&mut self, frame: &[f32]) -> f32;
}

/// Transcribes a captured utterance to text.
///
/// Transcription is CPU-heavy and blocking (whisper.cpp), so the driver runs it off
/// the async runtime; the trait is deliberately synchronous.
pub trait Transcriber: Send {
    /// Transcribe a whole utterance of 16 kHz mono samples. An utterance the model
    /// could not turn into words yields an empty string (the session treats that as
    /// "heard nothing").
    fn transcribe(&mut self, samples: &[f32]) -> Result<String>;
}

/// A rendered chunk of speech audio, at the synthesizer's native rate.
#[derive(Debug, Clone, PartialEq)]
pub struct Speech {
    /// Mono `f32` PCM in `[-1, 1]`.
    pub samples: Vec<f32>,
    /// The rate `samples` are at — not necessarily 16 kHz (Piper is 22.05 kHz).
    pub sample_rate: u32,
}

impl Speech {
    /// Duration in seconds, for pacing and reading-time estimates.
    pub fn duration_secs(&self) -> f32 {
        if self.sample_rate == 0 {
            return 0.0;
        }
        self.samples.len() as f32 / self.sample_rate as f32
    }

    /// Whether there is nothing to play.
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }
}

/// Renders text to speech audio.
///
/// Blocking (a subprocess round-trip for Piper), run off the async runtime by the
/// driver.
pub trait Synthesizer: Send {
    /// Render `text` to speech PCM. Empty or whitespace-only text yields empty
    /// [`Speech`] rather than an error.
    fn synthesize(&mut self, text: &str) -> Result<Speech>;
}

/// A callback the player invokes with the lip-sync level (`0.0`..=`1.0`) as
/// playback progresses. The widget forwards it to the character as
/// `voice://amplitude`. Must not block — it runs on or near the audio path.
pub type AmplitudeSink = Arc<dyn Fn(f32) + Send + Sync>;

/// An amplitude sink that discards every value, for playback without lip sync.
pub fn null_amplitude() -> AmplitudeSink {
    Arc::new(|_| {})
}

/// Plays synthesized speech to the default output device.
pub trait Player: Send + Sync {
    /// Begin playing `speech`, invoking `amplitude` with the lip-sync level as the
    /// audio plays. Returns immediately; playback continues in the background and
    /// the returned [`Playback`] both signals completion and can stop it early.
    fn play(&self, speech: Speech, amplitude: AmplitudeSink) -> Result<Playback>;
}

/// A running playback: await its completion, or stop it early for barge-in / mute.
pub struct Playback {
    stop: Option<Box<dyn FnOnce() + Send>>,
    finished: tokio::sync::oneshot::Receiver<()>,
}

impl Playback {
    /// Build a playback handle from a stop action and a completion signal. The
    /// concrete [`Player`] constructs this; callers only observe it.
    pub fn new(
        stop: impl FnOnce() + Send + 'static,
        finished: tokio::sync::oneshot::Receiver<()>,
    ) -> Self {
        Self {
            stop: Some(Box::new(stop)),
            finished,
        }
    }

    /// Resolve when playback finishes on its own. Resolves immediately if it was
    /// already stopped (the sender was dropped).
    pub async fn finished(&mut self) {
        let _ = (&mut self.finished).await;
    }

    /// Stop playback immediately (barge-in or mute). Idempotent.
    pub fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            stop();
        }
    }
}

impl std::fmt::Debug for Playback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Playback")
            .field("stoppable", &self.stop.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speech_duration_is_samples_over_rate() {
        let speech = Speech {
            samples: vec![0.0; 22_050],
            sample_rate: 22_050,
        };
        assert!((speech.duration_secs() - 1.0).abs() < 1e-6);
        assert!(!speech.is_empty());
    }

    #[test]
    fn empty_speech_has_zero_duration_and_a_zero_rate_is_safe() {
        assert_eq!(
            Speech {
                samples: vec![],
                sample_rate: 22_050
            }
            .duration_secs(),
            0.0
        );
        // A malformed zero rate must not divide by zero.
        assert_eq!(
            Speech {
                samples: vec![0.0; 10],
                sample_rate: 0
            }
            .duration_secs(),
            0.0
        );
    }

    #[tokio::test]
    async fn a_playback_resolves_finished_when_signalled() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut playback = Playback::new(|| {}, rx);
        tx.send(()).unwrap();
        playback.finished().await; // returns promptly
    }

    #[tokio::test]
    async fn stopping_a_playback_runs_the_stop_action_once() {
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        let (_tx, rx) = tokio::sync::oneshot::channel();
        let playback = Playback::new(
            move || {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            },
            rx,
        );
        playback.stop();
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
