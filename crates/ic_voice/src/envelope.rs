//! Turning TTS audio into a mouth-open value.
//!
//! Piper emits no timing or phoneme metadata — just PCM. So the character's lip
//! sync is driven by the *loudness* of the audio being played: compute a short-
//! window RMS envelope of the samples, smooth it, and map it to `[0, 1]` for
//! `ParamMouthOpenY`. This is the real signal that replaces the Phase 3 test-tone
//! stub in `ui/src/character.ts`.
//!
//! Pure and deterministic: no audio device, no model. The playback side calls
//! [`EnvelopeFollower::push`] with each chunk it hands to the speaker and forwards
//! the returned level to the widget as a `voice://amplitude` event.

/// Tracks a smoothed loudness envelope across successive audio chunks.
///
/// Speech RMS is small (a loud vowel is ~0.2–0.3 of full scale), so a raw RMS
/// would barely open the mouth. [`GAIN`](Self::GAIN) scales it into a usable
/// range, and an asymmetric one-pole smoother gives a natural mouth: quick to
/// open on an onset, slower to close, so it doesn't chatter between samples.
#[derive(Debug, Clone)]
pub struct EnvelopeFollower {
    level: f32,
    attack: f32,
    release: f32,
}

impl Default for EnvelopeFollower {
    fn default() -> Self {
        Self::new()
    }
}

impl EnvelopeFollower {
    /// Speech RMS is quiet; scale it toward a mouth that actually opens.
    const GAIN: f32 = 3.0;

    /// A follower with mouth-like attack/release.
    pub fn new() -> Self {
        Self {
            level: 0.0,
            // Open fast (little smoothing on the way up), close slower — the shape
            // of a real mouth, and it keeps the value from chattering.
            attack: 0.5,
            release: 0.15,
        }
    }

    /// Feed one chunk of mono `f32` samples; return the current mouth-open value in
    /// `[0, 1]`. An empty chunk decays the level toward closed rather than holding
    /// it open on a gap.
    pub fn push(&mut self, samples: &[f32]) -> f32 {
        let target = Self::GAIN * rms(samples);
        let coeff = if target > self.level {
            self.attack
        } else {
            self.release
        };
        self.level += coeff * (target - self.level);
        self.level.clamp(0.0, 1.0)
    }

    /// The current level without feeding new audio — used to decay the mouth shut
    /// when playback ends.
    pub fn decay(&mut self) -> f32 {
        self.level += self.release * (0.0 - self.level);
        self.level.clamp(0.0, 1.0)
    }
}

/// Root-mean-square of a chunk. `0.0` for an empty chunk (silence).
pub fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_squares: f32 = samples.iter().map(|&s| s * s).sum();
    (sum_squares / samples.len() as f32).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_abs_diff_eq;

    #[test]
    fn rms_of_silence_is_zero_and_of_full_scale_is_one() {
        assert_eq!(rms(&[]), 0.0);
        assert_eq!(rms(&[0.0; 100]), 0.0);
        assert_abs_diff_eq!(rms(&[1.0, -1.0, 1.0, -1.0]), 1.0, epsilon = 1e-6);
    }

    #[test]
    fn a_louder_chunk_opens_the_mouth_wider() {
        let mut quiet = EnvelopeFollower::new();
        let mut loud = EnvelopeFollower::new();
        // Push a few chunks so the smoother settles.
        let mut quiet_level = 0.0;
        let mut loud_level = 0.0;
        for _ in 0..10 {
            quiet_level = quiet.push(&[0.05, -0.05, 0.05, -0.05]);
            loud_level = loud.push(&[0.4, -0.4, 0.4, -0.4]);
        }
        assert!(loud_level > quiet_level, "{loud_level} vs {quiet_level}");
        assert!(loud_level <= 1.0 && quiet_level >= 0.0);
    }

    #[test]
    fn the_mouth_decays_toward_closed_on_silence() {
        let mut follower = EnvelopeFollower::new();
        for _ in 0..10 {
            follower.push(&[0.5, -0.5, 0.5, -0.5]);
        }
        let open = follower.decay();
        let mut level = open;
        for _ in 0..50 {
            level = follower.decay();
        }
        assert!(
            level < open * 0.1,
            "should have decayed: {level} from {open}"
        );
    }

    #[test]
    fn the_value_is_always_bounded() {
        let mut follower = EnvelopeFollower::new();
        // Even a clipping input must not push the mouth past fully open.
        for _ in 0..100 {
            let level = follower.push(&[5.0, -5.0, 5.0, -5.0]);
            assert!((0.0..=1.0).contains(&level), "{level}");
        }
    }
}
