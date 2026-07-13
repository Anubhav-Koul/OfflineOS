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

/// Decode raw little-endian 16-bit PCM bytes to `f32` in `[-1, 1]`.
///
/// This is what Piper writes to stdout under `--output-raw`: signed 16-bit mono
/// samples, little-endian, no header. A trailing odd byte (a torn read) is dropped
/// rather than misaligned into a garbage sample.
pub fn pcm_i16le_to_f32(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(2)
        .map(|pair| i16::from_le_bytes([pair[0], pair[1]]) as f32 / 32768.0)
        .collect()
}

/// The peak level Whisper is fed at.
///
/// Whisper's accuracy falls off sharply on quiet input — `base.en` starts
/// substituting similar-sounding words and, on very quiet audio, hallucinating short
/// filler ("No.", "Thank you."). Measured on real hardware, a Bluetooth headset
/// delivers a normal speaking voice at a peak of only ~0.03, which is where that
/// degradation lives. Lifting the utterance to a healthy level costs nothing and is
/// what a human would do with a gain knob.
const TARGET_PEAK: f32 = 0.7;

/// Below this there is no voice to amplify — only room noise, which gain would
/// merely make louder.
const NOISE_FLOOR: f32 = 0.005;

/// The most we will amplify. Without a ceiling, near-silence would be multiplied by
/// hundreds and a quiet room would arrive at Whisper as a roar of hiss, which
/// transcribes as confident nonsense.
const MAX_GAIN: f32 = 20.0;

/// Padding kept either side of the speech, in samples (~100 ms). Cutting hard on the
/// first loud sample clips the attack of the first word.
const TRIM_PADDING: usize = SAMPLE_RATE as usize / 10;

/// A level estimate that a single loud sample cannot move.
///
/// **Not the peak.** A Bluetooth microphone emits a click as its HFP endpoint
/// engages, and that one transient can be twenty times louder than the speech behind
/// it. Every decision keyed off the peak then goes wrong at once: the trim gate
/// (10% of peak) rises above the actual words, so the speech is discarded and the
/// click is kept; and the gain, seeing a "loud" clip, declines to amplify the quiet
/// voice. Observed: a clip peaking at 0.67 whose speech sat at 0.03 transcribed as
/// "can we? Yeah."
///
/// The 95th percentile of magnitude ignores a handful of outliers while still
/// tracking the body of the signal.
fn robust_level(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut magnitudes: Vec<f32> = samples.iter().map(|sample| sample.abs()).collect();
    let index = (magnitudes.len() as f32 * 0.95) as usize;
    let index = index.min(magnitudes.len() - 1);
    magnitudes.select_nth_unstable_by(index, f32::total_cmp);
    magnitudes[index]
}

/// Drop the silence either side of the speech.
///
/// Whisper *fills* leading silence with plausible filler: a push-to-talk clip that
/// opens with a held breath and a pause comes back as "No," or "Thank you." prefixed
/// onto the real sentence — confident, grammatical, and never said. (Observed: a
/// half-second pause before "what's 2 plus 2" produced "No, what's 2 plus 2?".) The
/// cure is not to give it silence to explain.
///
/// The threshold is relative to the utterance's own peak, so it works on a loud mic
/// and a quiet one alike. An utterance with no speech in it is returned untouched —
/// an empty transcript is the honest answer there, not a trimmed one.
pub fn trim_silence(samples: &[f32]) -> &[f32] {
    let level = robust_level(samples);
    if level < NOISE_FLOOR {
        return samples;
    }
    let gate = level * 0.1;

    let Some(first) = samples.iter().position(|sample| sample.abs() >= gate) else {
        return samples;
    };
    let last = samples
        .iter()
        .rposition(|sample| sample.abs() >= gate)
        .unwrap_or(samples.len() - 1);

    let start = first.saturating_sub(TRIM_PADDING);
    let end = (last + TRIM_PADDING).min(samples.len() - 1);
    &samples[start..=end]
}

/// Normalize an utterance to a level Whisper transcribes well.
///
/// Returns the gain applied (`1.0` = untouched), for logging.
pub fn normalize(samples: &mut [f32]) -> f32 {
    // Robust, not peak: a single click must not convince us the clip is already loud
    // and leave the speech beneath it unamplified. Clipping is handled by the clamp.
    let level = robust_level(samples);
    if !(NOISE_FLOOR..TARGET_PEAK).contains(&level) {
        return 1.0;
    }
    let gain = (TARGET_PEAK / level).min(MAX_GAIN);
    for sample in samples.iter_mut() {
        *sample = (*sample * gain).clamp(-1.0, 1.0);
    }
    gain
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
    fn pcm_bytes_decode_little_endian_and_drop_a_torn_trailing_byte() {
        // 0x0000 -> 0.0, 0x00FF? use known values: i16 256 = 0x0100 LE bytes [0x00,0x01].
        let bytes = [0x00, 0x00, 0x00, 0x40, 0xFF]; // 0, 16384, then a lone byte
        let decoded = pcm_i16le_to_f32(&bytes);
        assert_eq!(decoded.len(), 2, "the torn trailing byte must be dropped");
        assert_abs_diff_eq!(decoded[0], 0.0, epsilon = 1e-6);
        assert_abs_diff_eq!(decoded[1], 16384.0 / 32768.0, epsilon = 1e-6);
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

#[cfg(test)]
mod normalize_tests {
    use super::*;

    /// The regression: a push-to-talk clip that opens with a pause comes back with
    /// filler bolted onto the front — "No, what's 2 plus 2?" for a clip whose speech
    /// was only "what's 2 plus 2". Whisper explains silence rather than ignoring it,
    /// so the silence must not reach it.
    #[test]
    fn leading_and_trailing_silence_are_trimmed_away() {
        let silence = vec![0.0_f32; SAMPLE_RATE as usize]; // one second
        let speech: Vec<f32> = (0..SAMPLE_RATE as usize / 2)
            .map(|i| 0.4 * (i as f32 / 8.0).sin())
            .collect();

        let mut clip = silence.clone();
        clip.extend_from_slice(&speech);
        clip.extend_from_slice(&silence);

        let trimmed = trim_silence(&clip);

        // The speech survives, with padding either side — but the seconds of silence
        // whisper would have narrated are gone.
        assert!(trimmed.len() >= speech.len(), "the speech was cut");
        assert!(
            trimmed.len() <= speech.len() + 2 * TRIM_PADDING + 2,
            "silence survived: {} samples for {} of speech",
            trimmed.len(),
            speech.len()
        );
    }

    /// The regression that made a *loud* clip transcribe as badly as a quiet one: a
    /// Bluetooth mic clicks as its endpoint engages, and that transient is far louder
    /// than the speech. Keyed off the peak, the trim gate rose above the words —
    /// keeping the click, discarding the sentence — and the gain saw a "loud" clip
    /// and left the quiet voice alone. Observed transcript: "can we? Yeah."
    #[test]
    fn one_loud_click_does_not_swallow_the_speech() {
        let mut clip = vec![0.0_f32; SAMPLE_RATE as usize / 10];
        clip.push(0.9); // the click
        clip.extend(vec![0.0_f32; SAMPLE_RATE as usize / 10]);
        let speech_start = clip.len();
        // A whole second of quiet speech, well below the click.
        clip.extend((0..SAMPLE_RATE as usize).map(|i| 0.03 * (i as f32 / 8.0).sin()));

        let trimmed = trim_silence(&clip);

        // The speech survives the trim...
        assert!(
            trimmed.len() > SAMPLE_RATE as usize,
            "the speech was trimmed away and the click kept: {} samples",
            trimmed.len()
        );
        assert!(speech_start > 0);

        // ...and is then amplified, rather than being written off as already loud.
        let mut owned = trimmed.to_vec();
        let gain = normalize(&mut owned);
        assert!(
            gain > 2.0,
            "quiet speech under a click was not amplified: {gain}"
        );
    }

    /// A clip with nothing in it must not be "trimmed" into a sliver that whisper
    /// then invents words for. An empty transcript is the honest answer.
    #[test]
    fn a_silent_clip_is_left_alone() {
        let quiet = vec![0.001_f32; 1000];
        assert_eq!(trim_silence(&quiet).len(), quiet.len());
    }

    /// The attack of the first word must survive — cutting hard on the first loud
    /// sample clips the consonant and whisper drops or mangles the word.
    #[test]
    fn padding_keeps_the_attack_of_the_first_word() {
        let mut clip = vec![0.0_f32; SAMPLE_RATE as usize / 2];
        clip.extend((0..1000).map(|i| 0.5 * (i as f32).sin()));

        let trimmed = trim_silence(&clip);

        assert!(
            trimmed.len() > 1000,
            "no padding was kept before the speech"
        );
    }

    /// The measured case: a Bluetooth headset delivers a normal speaking voice at a
    /// peak of ~0.03, and whisper mishears it ("Nova, can you search the internet?"
    /// came back as "Now, can you start the internet?"). It must be lifted.
    #[test]
    fn a_quiet_voice_is_amplified_to_a_level_whisper_can_read() {
        let mut samples: Vec<f32> = (0..1000).map(|i| 0.03 * (i as f32 / 10.0).sin()).collect();

        let gain = normalize(&mut samples);

        assert!(
            gain > 1.0,
            "a quiet utterance must be amplified, got {gain}"
        );
        let peak = samples.iter().fold(0.0_f32, |m, s| m.max(s.abs()));
        // The gain ceiling binds first here (0.03 × 20 = 0.6), which is the point:
        // the utterance lands in a range whisper reads well, without lifting the cap
        // to chase an exact target and amplifying noise along with the voice.
        assert!(peak >= 0.5, "peak after gain: {peak}");
        assert!(peak <= 1.0, "peak after gain: {peak}");
    }

    /// Amplifying a silent room turns hiss into a roar, which whisper transcribes as
    /// confident nonsense. Leave it alone and let the empty transcript say so.
    #[test]
    fn room_noise_is_left_alone_rather_than_amplified_into_hiss() {
        let mut samples = vec![0.001, -0.002, 0.0015];
        assert_eq!(normalize(&mut samples), 1.0);
    }

    #[test]
    fn an_already_loud_utterance_is_untouched() {
        let mut samples = vec![0.9, -0.8, 0.75];
        assert_eq!(normalize(&mut samples), 1.0);
    }

    /// Gain is capped, and clamping keeps it inside the valid range whatever happens.
    #[test]
    fn gain_is_bounded_and_never_clips_out_of_range() {
        let mut samples = vec![0.006, -0.006];
        let gain = normalize(&mut samples);
        assert!(gain <= MAX_GAIN, "{gain}");
        assert!(samples.iter().all(|s| (-1.0..=1.0).contains(s)));
    }
}
