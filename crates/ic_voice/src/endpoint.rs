//! Turning a stream of VAD probabilities into utterance boundaries.
//!
//! The Silero VAD ([`crate::vad`]) scores each short frame with a speech
//! probability in `[0, 1]`. That raw signal is too jittery to gate an utterance
//! directly — a single dip below threshold mid-word would end the turn, and a lone
//! spike of noise would start one. [`Endpointer`] is the hysteresis in between: it
//! debounces the onset (speech must persist briefly before we call it speech) and
//! holds the tail open through a *hangover* of silence (a natural pause between
//! words must not end the turn), and it caps the utterance so a stuck-open mic can
//! never record forever.
//!
//! It is pure and clockless: time is threaded in as the millisecond duration of
//! each frame, so the whole thing is deterministic and testable with no VAD, no
//! microphone, and no wall clock. The same endpointer detects **barge-in** — the
//! user starting to speak while the character is talking — because that is exactly
//! a fresh onset ([`EndpointEvent::SpeechStarted`]).

/// How the endpointer decides an utterance has begun and ended.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EndpointConfig {
    /// A frame counts as speech when the VAD probability is at least this. Silero
    /// v5 is fairly confident; 0.5 is the usual midpoint.
    pub speech_threshold: f32,
    /// Speech must persist for at least this long before an utterance is declared
    /// started — debounces a lone noise spike into a false onset.
    pub min_speech_ms: u32,
    /// After speech, this much *continuous* silence ends the utterance. Long enough
    /// to ride over the gaps between words, short enough to feel responsive.
    pub hangover_ms: u32,
    /// A hard cap: an utterance is force-ended after this long regardless of
    /// silence, so a wedged-open mic (or steady background noise the VAD keeps
    /// scoring as speech) cannot capture without bound.
    pub max_utterance_ms: u32,
}

impl Default for EndpointConfig {
    fn default() -> Self {
        Self {
            speech_threshold: 0.5,
            min_speech_ms: 160,
            hangover_ms: 700,
            max_utterance_ms: 15_000,
        }
    }
}

/// A boundary the endpointer crossed on the frame just pushed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointEvent {
    /// Speech began (onset debounce satisfied). In the pipeline this both opens a
    /// capture and, during playback, signals barge-in.
    SpeechStarted,
    /// The utterance ended — either the hangover silence elapsed, or the max
    /// length was hit. Time to transcribe what was captured.
    SpeechEnded,
}

/// Where the endpointer is between utterances.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// No utterance in progress; accumulating candidate speech toward onset.
    Waiting,
    /// Inside an utterance; accumulating trailing silence toward the hangover.
    Speaking,
}

/// Hysteresis over a VAD probability stream. Feed it one frame at a time with
/// [`push`](Self::push); it returns a boundary event on the frame that crosses one.
#[derive(Debug, Clone)]
pub struct Endpointer {
    config: EndpointConfig,
    phase: Phase,
    /// Milliseconds of (near-)continuous speech accumulated while `Waiting`, toward
    /// the onset debounce. Reset by a silent frame before onset.
    candidate_speech_ms: u32,
    /// Milliseconds of continuous silence accumulated while `Speaking`, toward the
    /// hangover. Reset by any speech frame.
    trailing_silence_ms: u32,
    /// Milliseconds since the utterance started, for the max-length cap.
    utterance_ms: u32,
}

impl Endpointer {
    /// A fresh endpointer with the given thresholds, waiting for speech.
    pub fn new(config: EndpointConfig) -> Self {
        Self {
            config,
            phase: Phase::Waiting,
            candidate_speech_ms: 0,
            trailing_silence_ms: 0,
            utterance_ms: 0,
        }
    }

    /// Feed one frame's speech probability and its duration in milliseconds; return
    /// a boundary event if this frame crossed one, else `None`.
    ///
    /// At most one event per frame. `SpeechStarted` fires the moment accumulated
    /// speech reaches `min_speech_ms`; `SpeechEnded` fires when trailing silence
    /// reaches `hangover_ms` or the utterance reaches `max_utterance_ms`.
    pub fn push(&mut self, speech_prob: f32, frame_ms: u32) -> Option<EndpointEvent> {
        let is_speech = speech_prob >= self.config.speech_threshold;
        match self.phase {
            Phase::Waiting => {
                if is_speech {
                    self.candidate_speech_ms = self.candidate_speech_ms.saturating_add(frame_ms);
                    if self.candidate_speech_ms >= self.config.min_speech_ms {
                        self.phase = Phase::Speaking;
                        self.trailing_silence_ms = 0;
                        // The debounced speech is part of the utterance already.
                        self.utterance_ms = self.candidate_speech_ms;
                        self.candidate_speech_ms = 0;
                        return Some(EndpointEvent::SpeechStarted);
                    }
                } else {
                    // A gap before onset resets the candidate — noise doesn't
                    // accumulate into a false start.
                    self.candidate_speech_ms = 0;
                }
                None
            }
            Phase::Speaking => {
                self.utterance_ms = self.utterance_ms.saturating_add(frame_ms);
                if is_speech {
                    self.trailing_silence_ms = 0;
                } else {
                    self.trailing_silence_ms = self.trailing_silence_ms.saturating_add(frame_ms);
                }
                if self.trailing_silence_ms >= self.config.hangover_ms
                    || self.utterance_ms >= self.config.max_utterance_ms
                {
                    self.reset();
                    return Some(EndpointEvent::SpeechEnded);
                }
                None
            }
        }
    }

    /// Whether an utterance is currently in progress.
    pub fn is_speaking(&self) -> bool {
        self.phase == Phase::Speaking
    }

    /// Abandon any in-progress utterance and return to waiting, without emitting an
    /// event. Used when the pipeline changes mode (e.g. mute) and the accumulated
    /// audio is being discarded rather than transcribed.
    pub fn reset(&mut self) {
        self.phase = Phase::Waiting;
        self.candidate_speech_ms = 0;
        self.trailing_silence_ms = 0;
        self.utterance_ms = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame at 32 ms (512 samples at 16 kHz — Silero's chunk).
    const FRAME_MS: u32 = 32;

    fn feed(ep: &mut Endpointer, prob: f32, frames: usize) -> Vec<EndpointEvent> {
        (0..frames)
            .filter_map(|_| ep.push(prob, FRAME_MS))
            .collect()
    }

    #[test]
    fn a_lone_noise_spike_below_the_debounce_does_not_start_an_utterance() {
        let mut ep = Endpointer::new(EndpointConfig::default());
        // One loud frame (32 ms) then silence: below the 160 ms onset debounce.
        assert_eq!(ep.push(0.9, FRAME_MS), None);
        assert_eq!(ep.push(0.0, FRAME_MS), None);
        assert!(!ep.is_speaking());
    }

    #[test]
    fn sustained_speech_starts_the_utterance_at_the_debounce_boundary() {
        let mut ep = Endpointer::new(EndpointConfig::default());
        // 160 ms / 32 ms = 5 frames to reach the threshold. The 5th crosses it.
        assert_eq!(ep.push(0.9, FRAME_MS), None); // 32
        assert_eq!(ep.push(0.9, FRAME_MS), None); // 64
        assert_eq!(ep.push(0.9, FRAME_MS), None); // 96
        assert_eq!(ep.push(0.9, FRAME_MS), None); // 128
        assert_eq!(ep.push(0.9, FRAME_MS), Some(EndpointEvent::SpeechStarted)); // 160
        assert!(ep.is_speaking());
    }

    #[test]
    fn a_short_gap_between_words_does_not_end_the_utterance() {
        let mut ep = Endpointer::new(EndpointConfig::default());
        feed(&mut ep, 0.9, 6); // start speaking
        assert!(ep.is_speaking());
        // 300 ms of silence (< 700 ms hangover) — a pause, not the end.
        let events = feed(&mut ep, 0.0, 300 / FRAME_MS as usize);
        assert!(
            events.is_empty(),
            "a pause must not end the turn: {events:?}"
        );
        assert!(ep.is_speaking());
        // Speech resumes and clears the trailing silence.
        feed(&mut ep, 0.9, 3);
        assert!(ep.is_speaking());
    }

    #[test]
    fn the_hangover_silence_ends_the_utterance() {
        let mut ep = Endpointer::new(EndpointConfig::default());
        feed(&mut ep, 0.9, 6);
        // 700 ms of continuous silence ends it.
        let events = feed(&mut ep, 0.0, 700 / FRAME_MS as usize + 1);
        assert_eq!(events, [EndpointEvent::SpeechEnded]);
        assert!(!ep.is_speaking());
    }

    #[test]
    fn a_stuck_open_mic_is_force_ended_at_the_max_length() {
        let cfg = EndpointConfig {
            max_utterance_ms: 1_000,
            ..EndpointConfig::default()
        };
        let mut ep = Endpointer::new(cfg);
        feed(&mut ep, 0.9, 6); // start
        assert!(ep.is_speaking());
        // Continuous "speech" (steady noise the VAD never drops) must still end.
        // Feed until the force-end fires — it must arrive before we run forever.
        // (After the cap the endpointer resets and a still-hot mic legitimately
        // re-onsets, so we stop at the first end rather than over-feeding.)
        let mut ended_after = None;
        for frame in 1..100 {
            if ep.push(0.9, FRAME_MS) == Some(EndpointEvent::SpeechEnded) {
                ended_after = Some(frame);
                break;
            }
        }
        assert!(
            ended_after.is_some(),
            "the max-length cap must force-end a stuck-open utterance"
        );
        assert!(!ep.is_speaking());
    }

    #[test]
    fn reset_abandons_an_in_progress_utterance_silently() {
        let mut ep = Endpointer::new(EndpointConfig::default());
        feed(&mut ep, 0.9, 6);
        assert!(ep.is_speaking());
        ep.reset();
        assert!(!ep.is_speaking());
        // And it can start cleanly again afterward.
        let events = feed(&mut ep, 0.9, 6);
        assert_eq!(events, [EndpointEvent::SpeechStarted]);
    }

    #[test]
    fn onset_and_end_compose_into_one_clean_turn() {
        let mut ep = Endpointer::new(EndpointConfig::default());
        let mut all = Vec::new();
        all.extend(feed(&mut ep, 0.9, 20)); // ~640 ms speech
        all.extend(feed(&mut ep, 0.0, 30)); // ~960 ms silence, ends the turn
        assert_eq!(
            all,
            [EndpointEvent::SpeechStarted, EndpointEvent::SpeechEnded]
        );
    }
}
