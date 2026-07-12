//! Speech-to-text via whisper.cpp (whisper-rs).
//!
//! [`WhisperStt`] loads a GGML/GGUF whisper model (we ship `base.en` q5_1) and
//! transcribes a finished utterance of 16 kHz mono `f32` — exactly the pipeline's
//! format, so samples pass straight in. Transcription is CPU-heavy and blocking;
//! the driver runs it on a blocking thread, and the trait is synchronous to match.
//!
//! **CPU first, deliberately.** whisper-rs is built with default features (no
//! backend), so whisper.cpp runs on the CPU. Vulkan *silently no-ops* on Windows
//! static builds (whisper.cpp #3750) — enabling it would look like acceleration
//! while doing nothing — so it stays off until that is fixed upstream.
//!
//! whisper.cpp is chatty on stderr; [`install_logging_hooks`] is called once to
//! route its logs through `tracing` instead of the terminal.

use std::path::Path;
use std::sync::Once;

use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use crate::error::{Error, Result};
use crate::format::SAMPLE_RATE;
use crate::stages::Transcriber;

/// whisper.cpp's log hooks are global; install them at most once.
static LOGGING: Once = Once::new();

/// Utterances shorter than this hold no word worth transcribing; skip the model
/// entirely and report "heard nothing".
const MIN_UTTERANCE_SAMPLES: usize = SAMPLE_RATE as usize / 10; // 100 ms

/// A loaded whisper model, ready to transcribe utterances.
pub struct WhisperStt {
    ctx: WhisperContext,
    threads: i32,
}

impl WhisperStt {
    /// Load the whisper model at `model_path` (a GGML/GGUF `.bin`). CPU only.
    pub fn new(model_path: impl AsRef<Path>) -> Result<Self> {
        LOGGING.call_once(whisper_rs::install_logging_hooks);

        let mut params = WhisperContextParameters::new();
        params.use_gpu(false); // CPU first — see the module docs.
        let ctx = WhisperContext::new_with_params(model_path.as_ref(), params)
            .map_err(|error| Error::model(format!("loading the whisper model: {error}")))?;

        // Leave a couple of cores for capture, the gateway, and the UI.
        let threads = std::thread::available_parallelism()
            .map(|n| n.get() as i32)
            .unwrap_or(4)
            .clamp(1, 8);

        Ok(Self { ctx, threads })
    }
}

impl Transcriber for WhisperStt {
    fn transcribe(&mut self, samples: &[f32]) -> Result<String> {
        if samples.len() < MIN_UTTERANCE_SAMPLES {
            return Ok(String::new());
        }

        let mut state = self
            .ctx
            .create_state()
            .map_err(|error| Error::Transcribe(format!("creating a whisper state: {error}")))?;

        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_language(Some("en"));
        params.set_translate(false);
        params.set_n_threads(self.threads);
        // Silence whisper.cpp's own console output; we read the segments ourselves.
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);

        state
            .full(params, samples)
            .map_err(|error| Error::Transcribe(format!("running transcription: {error}")))?;

        let mut text = String::new();
        for segment in state.as_iter() {
            if let Ok(piece) = segment.to_str_lossy() {
                text.push_str(&piece);
            }
        }
        Ok(text.trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_minimum_utterance_is_a_tenth_of_a_second() {
        // The short-circuit threshold must track the sample rate — a hardcoded
        // count that drifted from 16 kHz would silently skip real words.
        assert_eq!(MIN_UTTERANCE_SAMPLES, SAMPLE_RATE as usize / 10);
    }

    /// Transcribe a real recording. Ignored: needs the bundled whisper model.
    #[test]
    #[ignore = "needs a real whisper model + audio; run with --ignored"]
    fn transcribes_a_spoken_wav() {
        let model = std::env::var("IC_VOICE_WHISPER_MODEL").expect("set IC_VOICE_WHISPER_MODEL");
        let wav = std::env::var("IC_VOICE_WHISPER_WAV").expect("set IC_VOICE_WHISPER_WAV");
        let mut stt = WhisperStt::new(model).expect("load model");

        let mut reader = hound::WavReader::open(wav).expect("open wav");
        let samples: Vec<f32> = reader
            .samples::<f32>()
            .map(|s| s.expect("sample"))
            .collect();
        let text = stt.transcribe(&samples).expect("transcribe");
        assert!(!text.is_empty(), "expected a transcript, got empty");
        eprintln!("transcript: {text}");
    }
}
