//! Turning whatever the sound card gives us into what the models want.
//!
//! WASAPI hands back the device's shared-mode mix format — commonly 44.1 or
//! 48 kHz, stereo, `f32` or `i16`. Every model downstream (rustpotter, Silero VAD,
//! whisper) wants the same thing: **16 kHz, mono, `f32` in `[-1, 1]`**. This module
//! is the two pure steps that get there — downmix to mono, and sample-type
//! conversion — plus the constant everything shares. Resampling (the rate change)
//! needs a stateful filter and lives with capture, where the input rate is known;
//! these steps are pure and fully testable without any audio device.

/// The sample rate every model in the pipeline runs at.
pub const SAMPLE_RATE: u32 = 16_000;

/// Downmix interleaved multi-channel `f32` to mono by averaging each frame's
/// channels.
///
/// `channels` must be ≥ 1. A partial trailing frame (fewer than `channels`
/// samples — a torn buffer) is dropped rather than averaged against silence, which
/// would inject a quiet click.
pub fn downmix_to_mono(interleaved: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    let frames = interleaved.len() / channels;
    let mut mono = Vec::with_capacity(frames);
    for frame in 0..frames {
        let start = frame * channels;
        let sum: f32 = interleaved[start..start + channels].iter().sum();
        mono.push(sum / channels as f32);
    }
    mono
}

/// Convert `i16` PCM to `f32` in `[-1, 1]`.
///
/// Divides by 32768 (`1 << 15`), so full-scale negative (`i16::MIN`) maps exactly
/// to `-1.0` and full-scale positive (`i16::MAX`) to just under `+1.0` — the
/// standard asymmetric mapping that never clips above 1.
pub fn i16_to_f32(samples: &[i16]) -> Vec<f32> {
    samples
        .iter()
        .map(|&sample| sample as f32 / 32768.0)
        .collect()
}

/// Convert `f32` in `[-1, 1]` to `i16` PCM, clamping out-of-range values.
///
/// Piper (and WAV playback) speak `i16`. Scaling by 32767 keeps `+1.0` at
/// `i16::MAX`; the clamp guards against a synth that overshoots `±1`.
pub fn f32_to_i16(samples: &[f32]) -> Vec<i16> {
    samples
        .iter()
        .map(|&sample| (sample.clamp(-1.0, 1.0) * 32767.0).round() as i16)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_abs_diff_eq;

    #[test]
    fn mono_input_passes_through_unchanged() {
        let mono = [0.1, -0.2, 0.3];
        assert_eq!(downmix_to_mono(&mono, 1), mono);
    }

    #[test]
    fn stereo_is_averaged_per_frame() {
        // frames: (1.0, 0.0) -> 0.5, (-0.4, -0.6) -> -0.5
        let stereo = [1.0, 0.0, -0.4, -0.6];
        let mono = downmix_to_mono(&stereo, 2);
        assert_eq!(mono.len(), 2);
        assert_abs_diff_eq!(mono[0], 0.5, epsilon = 1e-6);
        assert_abs_diff_eq!(mono[1], -0.5, epsilon = 1e-6);
    }

    #[test]
    fn a_torn_trailing_frame_is_dropped_not_averaged_against_silence() {
        // 3 samples, 2 channels: one whole frame + a lone sample. The lone sample
        // must not become a half-volume mono frame.
        let torn = [0.5, 0.5, 0.9];
        let mono = downmix_to_mono(&torn, 2);
        assert_eq!(mono.len(), 1);
        assert_abs_diff_eq!(mono[0], 0.5, epsilon = 1e-6);
    }

    #[test]
    fn i16_full_scale_maps_to_the_unit_range() {
        let converted = i16_to_f32(&[i16::MIN, 0, i16::MAX]);
        assert_abs_diff_eq!(converted[0], -1.0, epsilon = 1e-6);
        assert_abs_diff_eq!(converted[1], 0.0, epsilon = 1e-6);
        // i16::MAX / 32768 is just under 1.0 — the standard asymmetric mapping.
        assert!(converted[2] < 1.0 && converted[2] > 0.999);
    }

    #[test]
    fn f32_to_i16_clamps_overshoot() {
        // Symmetric scaling by 32767: clamped -1.0 maps to -32767 (not i16::MIN),
        // +1.0 to i16::MAX. The point of the test is that ±2.0 does not wrap.
        let converted = f32_to_i16(&[-2.0, 0.0, 2.0]);
        assert_eq!(converted, [-32767, 0, i16::MAX]);
    }

    #[test]
    fn i16_f32_round_trips_within_a_quantum() {
        let original: Vec<i16> = vec![-30000, -1, 0, 1, 12345, 30000];
        let round_tripped = f32_to_i16(&i16_to_f32(&original));
        for (a, b) in original.iter().zip(&round_tripped) {
            assert!((a - b).abs() <= 1, "{a} vs {b}");
        }
    }
}
