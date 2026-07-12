//! Resampling the mic to 16 kHz mono.
//!
//! WASAPI capture arrives at the device's rate (44.1 or 48 kHz, typically); every
//! model wants 16 kHz. cpal does not resample, so we do — with `rubato`'s FFT
//! resampler, which handles the non-integer 44100→16000 ratio correctly (a plain
//! decimation only works for the exact 48000→16000 3:1 case).
//!
//! The resampler processes fixed-size chunks, but a capture callback delivers
//! arbitrary sizes, so [`Resampler`] buffers input and emits output as whole
//! chunks become available — the caller feeds any length and takes back whatever
//! is ready. When the input is already 16 kHz there is an identity fast path with
//! no FFT at all.

use rubato::{FftFixedIn, Resampler as _};

use crate::error::{Error, Result};
use crate::format::SAMPLE_RATE;

/// The input-side chunk size the FFT resampler consumes per step. A power of two
/// keeps the FFT efficient; ~64 ms at 16 kHz is small enough that latency from
/// buffering a chunk is imperceptible.
const CHUNK: usize = 1024;

/// Streams mono `f32` from `input_rate` to `output_rate`.
pub struct Resampler {
    engine: Option<FftFixedIn<f32>>,
    staging: Vec<f32>,
    input_rate: u32,
    output_rate: u32,
}

impl Resampler {
    /// A resampler from `input_rate` Hz mono to 16 kHz mono (the capture path).
    ///
    /// `input_rate == 16000` yields an identity resampler (the fast path).
    pub fn new(input_rate: u32) -> Result<Self> {
        Self::to_rate(input_rate, SAMPLE_RATE)
    }

    /// A resampler from `input_rate` to `output_rate`, both mono. Used by playback
    /// to lift Piper's 22.05 kHz up to the sound card's rate; equal rates take the
    /// identity fast path with no FFT.
    pub fn to_rate(input_rate: u32, output_rate: u32) -> Result<Self> {
        let engine = if input_rate == output_rate {
            None
        } else {
            Some(
                FftFixedIn::<f32>::new(
                    input_rate as usize,
                    output_rate as usize,
                    CHUNK,
                    2, // sub-chunks: a rubato smoothness/latency knob
                    1, // mono
                )
                .map_err(|error| Error::audio(format!("building the resampler: {error}")))?,
            )
        };
        Ok(Self {
            engine,
            staging: Vec::new(),
            input_rate,
            output_rate,
        })
    }

    /// The input sample rate this was built for.
    pub fn input_rate(&self) -> u32 {
        self.input_rate
    }

    /// The output sample rate this resamples to.
    pub fn output_rate(&self) -> u32 {
        self.output_rate
    }

    /// Flush any buffered tail by padding the input with silence to the next chunk
    /// boundary, emitting the last partial chunk. For one-shot playback, where the
    /// final &lt;1 chunk of a finite clip would otherwise be dropped. Trailing silence
    /// in the output is harmless (it plays as a brief pause).
    pub fn flush(&mut self) -> Vec<f32> {
        let Some(engine) = self.engine.as_ref() else {
            return Vec::new(); // identity path buffers nothing
        };
        let need = engine.input_frames_next();
        if self.staging.is_empty() || need == 0 {
            return Vec::new();
        }
        // Pad with just enough silence to complete the pending chunk; the single
        // push then emits it.
        let pad = need.saturating_sub(self.staging.len());
        self.push(&vec![0.0f32; pad])
    }

    /// Feed mono input; return whatever 16 kHz output is ready. Buffers a partial
    /// chunk internally, so successive calls with any lengths compose. A resampler
    /// error drops that chunk (logged) rather than propagating — a glitch in the
    /// mic stream must not tear down capture.
    pub fn push(&mut self, mono_in: &[f32]) -> Vec<f32> {
        let Some(engine) = self.engine.as_mut() else {
            return mono_in.to_vec(); // identity: already 16 kHz
        };

        self.staging.extend_from_slice(mono_in);
        let mut out = Vec::new();
        loop {
            let need = engine.input_frames_next();
            if self.staging.len() < need {
                break;
            }
            let chunk: Vec<f32> = self.staging.drain(..need).collect();
            match engine.process(&[chunk], None) {
                Ok(resampled) => out.extend_from_slice(&resampled[0]),
                Err(error) => {
                    tracing::warn!(%error, "dropping a chunk the resampler rejected");
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_identity_path_returns_input_unchanged() {
        let mut resampler = Resampler::new(SAMPLE_RATE).expect("build");
        let input = vec![0.1, -0.2, 0.3, -0.4];
        assert_eq!(resampler.push(&input), input);
    }

    #[test]
    fn downsampling_48k_to_16k_yields_roughly_a_third_of_the_samples() {
        let mut resampler = Resampler::new(48_000).expect("build");
        // One second of 48 kHz input, fed in small pieces to exercise buffering.
        let total_in = 48_000usize;
        let mut produced = 0usize;
        let piece = vec![0.0f32; 480]; // 10 ms pieces
        for _ in 0..(total_in / piece.len()) {
            produced += resampler.push(&piece).len();
        }
        // 48k -> 16k is 3:1, so ~16000 out. FFT resampling has edge/latency
        // effects, so allow a generous band rather than an exact count.
        assert!(
            (14_000..=16_000).contains(&produced),
            "expected ~16000 output samples, got {produced}"
        );
    }

    #[test]
    fn a_non_integer_ratio_44100_to_16000_also_produces_output() {
        let mut resampler = Resampler::new(44_100).expect("build");
        let mut produced = 0usize;
        let piece = vec![0.0f32; 441];
        for _ in 0..100 {
            produced += resampler.push(&piece).len();
        }
        // 44100 -> 16000 over ~1s should give ~16000; just assert it flows.
        assert!(produced > 12_000, "expected output to flow, got {produced}");
    }

    #[test]
    fn upsampling_22050_to_48000_produces_more_samples() {
        let mut resampler = Resampler::to_rate(22_050, 48_000).expect("build");
        assert_eq!(resampler.output_rate(), 48_000);
        let mut produced = 0usize;
        let piece = vec![0.0f32; 441];
        for _ in 0..100 {
            produced += resampler.push(&piece).len();
        }
        // 22050 -> 48000 is ~2.18x upsampling of ~44100 input samples.
        assert!(
            produced > 44_100,
            "expected upsampled output, got {produced}"
        );
    }

    #[test]
    fn flush_emits_the_buffered_tail_of_a_short_clip() {
        let mut resampler = Resampler::to_rate(22_050, 48_000).expect("build");
        // A clip shorter than one input chunk: push emits nothing, flush emits it.
        let clip = vec![0.1f32; 500];
        let from_push = resampler.push(&clip).len();
        let from_flush = resampler.flush().len();
        assert!(
            from_flush > 0,
            "flush must emit the tail push withheld ({from_push} + {from_flush})"
        );
    }

    #[test]
    fn flush_on_the_identity_path_is_empty() {
        let mut resampler = Resampler::to_rate(16_000, 16_000).expect("build");
        resampler.push(&[0.1, 0.2, 0.3]);
        assert!(resampler.flush().is_empty());
    }

    #[test]
    fn small_feeds_below_a_chunk_buffer_until_enough_arrives() {
        let mut resampler = Resampler::new(48_000).expect("build");
        // A single tiny feed can't fill a 1024-input-frame chunk, so nothing out
        // yet — but it must not error or lose the samples.
        let first = resampler.push(&[0.0; 100]);
        assert!(first.is_empty());
        // Keep feeding; eventually output appears.
        let mut total = 0;
        for _ in 0..50 {
            total += resampler.push(&[0.0; 100]).len();
        }
        assert!(total > 0, "output should appear once chunks fill");
    }
}
