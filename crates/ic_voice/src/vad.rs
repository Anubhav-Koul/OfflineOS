//! Voice-activity detection via Silero v5.
//!
//! [`SileroVad`] wraps the `voice_activity_detector` crate (MIT), which bundles the
//! Silero v5 ONNX model and runs it through ONNX Runtime (`ort`). It scores each
//! frame with the probability that the frame contains speech; the [`Endpointer`]
//! ([`crate::endpoint`]) turns that stream of probabilities into utterance
//! boundaries and barge-in.
//!
//! Silero at 16 kHz requires a **512-sample** window, no other size — the driver
//! reads the ring in exactly `frame_size()` blocks so every [`predict`](Vad::predict)
//! gets a whole frame. The bundled model means there is nothing to download for
//! VAD; `ort` pulls the ONNX Runtime binary at build time.

use voice_activity_detector::VoiceActivityDetector;

use crate::error::{Error, Result};
use crate::format::SAMPLE_RATE;
use crate::stages::Vad;

/// Silero's mandated window at 16 kHz.
const CHUNK: usize = 512;

/// A Silero v5 voice-activity detector at the pipeline's 16 kHz.
pub struct SileroVad {
    detector: VoiceActivityDetector,
}

impl SileroVad {
    /// Build the detector. The Silero model is embedded in the crate, so this only
    /// fails if ONNX Runtime cannot initialise.
    pub fn new() -> Result<Self> {
        let detector = VoiceActivityDetector::builder()
            .sample_rate(SAMPLE_RATE as i64)
            .chunk_size(CHUNK)
            .build()
            .map_err(|error| Error::model(format!("initialising Silero VAD: {error}")))?;
        Ok(Self { detector })
    }
}

impl Vad for SileroVad {
    fn frame_size(&self) -> usize {
        CHUNK
    }

    fn predict(&mut self, frame: &[f32]) -> f32 {
        // The detector pads/truncates to the required window, but the driver always
        // hands us exactly `CHUNK` samples.
        self.detector.predict(frame.iter().copied())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silero_scores_silence_low_and_reports_the_right_frame_size() {
        let mut vad = match SileroVad::new() {
            Ok(vad) => vad,
            Err(error) => {
                // ONNX Runtime may be unavailable in a bare CI image; don't fail
                // the suite over the environment.
                eprintln!("skipping Silero test: {error}");
                return;
            }
        };
        assert_eq!(vad.frame_size(), CHUNK);
        // Pure silence should score well below the speech threshold.
        let silence = vec![0.0f32; CHUNK];
        let prob = vad.predict(&silence);
        assert!(
            (0.0..=1.0).contains(&prob),
            "probability out of range: {prob}"
        );
        assert!(prob < 0.5, "silence scored as speech: {prob}");
    }
}
