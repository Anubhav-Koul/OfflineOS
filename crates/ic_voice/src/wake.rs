//! Wake-word spotting via rustpotter.
//!
//! [`RustpotterWake`] wraps the vendored rustpotter 2.x (`crates-src/rustpotter`) —
//! the classic correlation-based spotter that runs a small set of *reference*
//! wakeword models built from a handful of recordings of the phrase. We ship our
//! own reference models, so the licence is clean (openWakeWord's pretrained models
//! are CC-BY-NC; rustpotter's own format is not).
//!
//! rustpotter's default audio format is already the pipeline's — 16 kHz mono `f32`
//! — so samples flow straight through. It buffers internally and evaluates one
//! feature frame at a time; we feed it in whole-frame chunks
//! (`get_samples_per_frame`) and report a detection the moment any chunk fires.
//!
//! This is the one real stage the crate could not verify against crates.io: the
//! only published rustpotter (3.0.2) no longer builds. See
//! `docs/desktop/voice-notes.md`.

use std::path::Path;

use rustpotter::{Rustpotter, RustpotterConfig};

use crate::error::{Error, Result};
use crate::stages::WakeWord;

/// A rustpotter spotter loaded with one or more reference wakeword models.
pub struct RustpotterWake {
    engine: Rustpotter,
    frame: usize,
    buf: Vec<f32>,
}

impl RustpotterWake {
    /// Build a spotter from the given `.rpw` reference model files. At least one
    /// must load, or this errors — a spotter with no wakewords never fires and is
    /// almost certainly a misconfiguration.
    ///
    /// The default [`RustpotterConfig`] expects exactly the pipeline's format
    /// (16 kHz mono `f32`), so no format wiring is needed.
    pub fn new<P: AsRef<Path>>(model_paths: &[P]) -> Result<Self> {
        let config = RustpotterConfig::default();
        let mut engine = Rustpotter::new(&config)
            .map_err(|reason| Error::model(format!("initialising rustpotter: {reason}")))?;

        let mut loaded = 0usize;
        for path in model_paths {
            let path = path.as_ref();
            let path_str = path.display().to_string();
            match engine.add_wakeword_from_file(&path_str) {
                Ok(()) => loaded += 1,
                Err(reason) => {
                    tracing::warn!(model = %path_str, %reason, "skipping an unloadable wakeword model");
                }
            }
        }
        if loaded == 0 {
            return Err(Error::model(
                "no wakeword models could be loaded; wake word would never fire".to_string(),
            ));
        }

        let frame = engine.get_samples_per_frame().max(1);
        Ok(Self {
            engine,
            frame,
            buf: Vec::with_capacity(frame * 2),
        })
    }
}

impl WakeWord for RustpotterWake {
    fn process(&mut self, samples: &[f32]) -> bool {
        self.buf.extend_from_slice(samples);
        let mut detected = false;
        while self.buf.len() >= self.frame {
            let frame: Vec<f32> = self.buf.drain(..self.frame).collect();
            if self.engine.process_f32(&frame).is_some() {
                detected = true;
            }
        }
        detected
    }
}

/// A wake word that never fires — for the push-to-talk-only configuration, when no
/// wakeword models are bundled. The pipeline still runs; listening is started by
/// the summon hotkey ([`crate::VoiceHandle::trigger_listen`]) instead.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullWakeWord;

impl WakeWord for NullWakeWord {
    fn process(&mut self, _samples: &[f32]) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spotter_with_no_models_is_rejected() {
        let empty: &[&Path] = &[];
        let result = RustpotterWake::new(empty);
        assert!(result.is_err(), "an empty spotter should be an error");
    }

    #[test]
    fn a_missing_model_file_is_skipped_and_leaves_no_wakewords() {
        // A path that cannot load leaves zero wakewords -> error, not a silent
        // spotter that never fires.
        let missing = [Path::new("does-not-exist.rpw")];
        assert!(RustpotterWake::new(&missing).is_err());
    }

    /// Loads a real reference model and feeds a recording of the phrase. Ignored:
    /// needs bundled assets. Run with `--ignored` once a model is present.
    #[test]
    #[ignore = "needs a real .rpw wakeword model + audio; run with --ignored"]
    fn detects_the_wake_phrase_in_a_recording() {
        let model = std::env::var("IC_VOICE_WAKE_MODEL").expect("set IC_VOICE_WAKE_MODEL");
        let wav = std::env::var("IC_VOICE_WAKE_WAV").expect("set IC_VOICE_WAKE_WAV");
        let mut spotter = RustpotterWake::new(&[model]).expect("load spotter");

        let mut reader = hound::WavReader::open(wav).expect("open wav");
        let samples: Vec<f32> = reader
            .samples::<f32>()
            .map(|s| s.expect("sample"))
            .collect();
        let mut fired = false;
        for chunk in samples.chunks(512) {
            if spotter.process(chunk) {
                fired = true;
                break;
            }
        }
        assert!(fired, "the wake phrase should have been detected");
    }
}
