//! The voice conversation state machine.
//!
//! This is the pure control logic: it decides *what happens next* from an event,
//! and never touches an audio device or a model. The widget is the hands — it runs
//! capture, whisper, the gateway call, and Piper — and feeds this the events those
//! produce ([`VoiceEvent`]), then performs the [`VoiceEffect`] it returns. Keeping
//! the brain pure is what makes the whole flow — including barge-in and mute mid-
//! utterance — testable with no microphone.
//!
//! The state also maps to the character's animation ([`VoiceState::character_state`]),
//! so the mascot shows what the voice loop is doing (listening → thinking →
//! speaking) through the same channel Phase 3 built.

use serde::Serialize;

/// Where the voice loop is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceState {
    /// Microphone off. No wake word, no capture. The resting state when the user
    /// has muted.
    Muted,
    /// Listening for the wake word; not yet capturing an utterance.
    Idle,
    /// Wake word fired; capturing the user's utterance, VAD watching for its end.
    Listening,
    /// Utterance ended; whisper is transcribing it.
    Transcribing,
    /// Transcript sent; waiting for the gateway's reply.
    Sending,
    /// Playing the reply through TTS. Barge-in (the user speaking) interrupts.
    Speaking,
}

impl VoiceState {
    /// The character animation state this maps to, for the mic-live indicator.
    /// Returns `None` when voice should not override the character (idle/muted —
    /// the character rests or reflects the gateway instead).
    pub fn character_state(self) -> Option<&'static str> {
        match self {
            VoiceState::Muted | VoiceState::Idle => None,
            VoiceState::Listening => Some("listening"),
            VoiceState::Transcribing | VoiceState::Sending => Some("thinking"),
            VoiceState::Speaking => Some("speaking"),
        }
    }

    /// Whether the microphone should be capturing in this state. Capture runs
    /// whenever we might need audio: to spot the wake word (`Idle`), to record the
    /// utterance (`Listening`), and during `Speaking` so barge-in can be heard.
    pub fn wants_capture(self) -> bool {
        matches!(
            self,
            VoiceState::Idle | VoiceState::Listening | VoiceState::Speaking
        )
    }
}

/// Something that happened, that the state machine reacts to.
#[derive(Debug, Clone)]
pub enum VoiceEvent {
    /// The user toggled the mic. `true` = now muted.
    MuteChanged(bool),
    /// The wake word was spotted.
    WakeDetected,
    /// VAD saw the utterance end (a stretch of silence after speech).
    SpeechEnded,
    /// Listening began but no speech onset arrived within the driver's timeout —
    /// a false wake or an abandoned push-to-talk. Give up without transcribing.
    ListenTimeout,
    /// Whisper produced a transcript. Empty means it heard nothing usable.
    Transcribed(String),
    /// The gateway returned a reply.
    ReplyReceived(String),
    /// The gateway turn failed or returned nothing to speak.
    ReplyFailed,
    /// Say something that did not come from a spoken turn — the reply to a message
    /// the user **typed**.
    ///
    /// Without this, `Speaking` was reachable only from `Sending`, i.e. only as the
    /// tail of a conversation the user had started *with their voice*. So an app set
    /// to speak its replies stayed silent for every typed message, which is most of
    /// them.
    SpeakRequested(String),
    /// TTS playback finished on its own.
    SpeakingEnded,
    /// The user cut in *while we were speaking* — the wake word fired or the
    /// summon hotkey was pressed during playback. (Deliberately not raw VAD: the
    /// microphone hears the character's own TTS through the speakers, and a
    /// VAD-driven barge-in self-triggers on that echo.)
    BargeIn,
}

/// What the widget should do in response. Exactly one per event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceEffect {
    /// Nothing to do; the state may still have changed.
    None,
    /// Drain the captured utterance and run transcription.
    BeginTranscription,
    /// Send this transcript to the gateway as a chat message.
    Send(String),
    /// Speak this reply through TTS.
    Speak(String),
    /// Stop TTS playback immediately (barge-in or mute).
    StopSpeaking,
}

/// The voice state machine.
#[derive(Debug, Clone)]
pub struct VoiceSession {
    state: VoiceState,
}

impl Default for VoiceSession {
    fn default() -> Self {
        Self::new()
    }
}

impl VoiceSession {
    /// A new session, listening for the wake word.
    pub fn new() -> Self {
        Self {
            state: VoiceState::Idle,
        }
    }

    /// The current state.
    pub fn state(&self) -> VoiceState {
        self.state
    }

    /// Apply an event: mutate the state and return the effect to perform.
    ///
    /// Mute is handled first and uniformly — it can arrive in any state and always
    /// wins, cutting playback if we were speaking. Everything else is a normal
    /// transition; an event that doesn't apply to the current state is ignored
    /// (the state is unchanged and the effect is `None`), so a late or duplicate
    /// event can't corrupt the flow.
    pub fn on(&mut self, event: VoiceEvent) -> VoiceEffect {
        // Mute overrides everything, from any state — but only a *change*. A
        // redundant unmute (a tray double-toggle, a startup state sync) must not
        // snap an in-flight turn back to Idle and orphan a live playback.
        if let VoiceEvent::MuteChanged(muted) = event {
            if muted == (self.state == VoiceState::Muted) {
                return VoiceEffect::None; // already in the requested state
            }
            let was_speaking = self.state == VoiceState::Speaking;
            self.state = if muted {
                VoiceState::Muted
            } else {
                VoiceState::Idle
            };
            return if muted && was_speaking {
                VoiceEffect::StopSpeaking
            } else {
                VoiceEffect::None
            };
        }

        // While muted, nothing but unmute (handled above) does anything.
        if self.state == VoiceState::Muted {
            return VoiceEffect::None;
        }

        match (self.state, event) {
            (VoiceState::Idle, VoiceEvent::WakeDetected) => {
                self.state = VoiceState::Listening;
                VoiceEffect::None
            }
            (VoiceState::Listening, VoiceEvent::SpeechEnded) => {
                self.state = VoiceState::Transcribing;
                VoiceEffect::BeginTranscription
            }
            (VoiceState::Listening, VoiceEvent::ListenTimeout) => {
                // Nothing was said. Back to waiting for the wake word.
                self.state = VoiceState::Idle;
                VoiceEffect::None
            }
            (VoiceState::Transcribing, VoiceEvent::Transcribed(text)) => {
                if text.trim().is_empty() {
                    // Whisper heard nothing usable — back to waiting, no message.
                    self.state = VoiceState::Idle;
                    VoiceEffect::None
                } else {
                    self.state = VoiceState::Sending;
                    VoiceEffect::Send(text)
                }
            }
            (VoiceState::Sending, VoiceEvent::ReplyReceived(text)) => {
                self.state = VoiceState::Speaking;
                VoiceEffect::Speak(text)
            }
            // Only from Idle: mid-turn the user is either talking to us or waiting on
            // an answer, and neither is a moment to start reading something else out.
            (VoiceState::Idle, VoiceEvent::SpeakRequested(text)) => {
                self.state = VoiceState::Speaking;
                VoiceEffect::Speak(text)
            }
            (VoiceState::Sending, VoiceEvent::ReplyFailed) => {
                self.state = VoiceState::Idle;
                VoiceEffect::None
            }
            (VoiceState::Speaking, VoiceEvent::SpeakingEnded) => {
                self.state = VoiceState::Idle;
                VoiceEffect::None
            }
            (VoiceState::Speaking, VoiceEvent::BargeIn) => {
                // The user cut in. Stop talking and capture what they're saying.
                self.state = VoiceState::Listening;
                VoiceEffect::StopSpeaking
            }
            // Any other (state, event) pair is not meaningful here — ignore it.
            _ => VoiceEffect::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The happy path: wake → speak → transcribe → send → reply → speak → done.
    #[test]
    fn a_full_turn_walks_through_every_state() {
        let mut session = VoiceSession::new();
        assert_eq!(session.state(), VoiceState::Idle);

        assert_eq!(session.on(VoiceEvent::WakeDetected), VoiceEffect::None);
        assert_eq!(session.state(), VoiceState::Listening);

        assert_eq!(
            session.on(VoiceEvent::SpeechEnded),
            VoiceEffect::BeginTranscription
        );
        assert_eq!(session.state(), VoiceState::Transcribing);

        assert_eq!(
            session.on(VoiceEvent::Transcribed("what time is it".into())),
            VoiceEffect::Send("what time is it".into())
        );
        assert_eq!(session.state(), VoiceState::Sending);

        assert_eq!(
            session.on(VoiceEvent::ReplyReceived("It is noon.".into())),
            VoiceEffect::Speak("It is noon.".into())
        );
        assert_eq!(session.state(), VoiceState::Speaking);

        assert_eq!(session.on(VoiceEvent::SpeakingEnded), VoiceEffect::None);
        assert_eq!(session.state(), VoiceState::Idle);
    }

    #[test]
    fn an_empty_transcript_returns_to_idle_without_sending() {
        let mut session = VoiceSession::new();
        session.on(VoiceEvent::WakeDetected);
        session.on(VoiceEvent::SpeechEnded);
        // Whisper heard only noise.
        assert_eq!(
            session.on(VoiceEvent::Transcribed("   ".into())),
            VoiceEffect::None
        );
        assert_eq!(session.state(), VoiceState::Idle);
    }

    #[test]
    fn barge_in_stops_speaking_and_starts_listening() {
        let mut session = VoiceSession::new();
        session.on(VoiceEvent::WakeDetected);
        session.on(VoiceEvent::SpeechEnded);
        session.on(VoiceEvent::Transcribed("tell me a long story".into()));
        session.on(VoiceEvent::ReplyReceived("Once upon a time…".into()));
        assert_eq!(session.state(), VoiceState::Speaking);

        // The user interrupts.
        assert_eq!(session.on(VoiceEvent::BargeIn), VoiceEffect::StopSpeaking);
        assert_eq!(session.state(), VoiceState::Listening);
    }

    #[test]
    fn mute_wins_from_any_state_and_cuts_playback_when_speaking() {
        let mut session = VoiceSession::new();
        session.on(VoiceEvent::WakeDetected);
        session.on(VoiceEvent::SpeechEnded);
        session.on(VoiceEvent::Transcribed("hi".into()));
        session.on(VoiceEvent::ReplyReceived("Hello!".into()));
        assert_eq!(session.state(), VoiceState::Speaking);

        // Muting mid-sentence must stop the audio.
        assert_eq!(
            session.on(VoiceEvent::MuteChanged(true)),
            VoiceEffect::StopSpeaking
        );
        assert_eq!(session.state(), VoiceState::Muted);

        // While muted, a wake word does nothing.
        assert_eq!(session.on(VoiceEvent::WakeDetected), VoiceEffect::None);
        assert_eq!(session.state(), VoiceState::Muted);

        // Unmute returns to idle, listening for the wake word again.
        assert_eq!(
            session.on(VoiceEvent::MuteChanged(false)),
            VoiceEffect::None
        );
        assert_eq!(session.state(), VoiceState::Idle);
    }

    #[test]
    fn muting_while_idle_does_not_emit_stop_speaking() {
        let mut session = VoiceSession::new();
        assert_eq!(session.on(VoiceEvent::MuteChanged(true)), VoiceEffect::None);
        assert_eq!(session.state(), VoiceState::Muted);
    }

    /// A redundant unmute (already unmuted) must be a no-op — snapping to Idle
    /// mid-turn would abandon the utterance or orphan a live playback.
    #[test]
    fn a_redundant_unmute_does_not_cancel_an_in_flight_turn() {
        let mut session = VoiceSession::new();
        session.on(VoiceEvent::WakeDetected);
        session.on(VoiceEvent::SpeechEnded);
        session.on(VoiceEvent::Transcribed("hi".into()));
        session.on(VoiceEvent::ReplyReceived("Hello!".into()));
        assert_eq!(session.state(), VoiceState::Speaking);

        // We are not muted; an unmute request changes nothing.
        assert_eq!(
            session.on(VoiceEvent::MuteChanged(false)),
            VoiceEffect::None
        );
        assert_eq!(session.state(), VoiceState::Speaking);

        // And a redundant mute while muted is equally inert.
        session.on(VoiceEvent::MuteChanged(true));
        assert_eq!(session.state(), VoiceState::Muted);
        assert_eq!(session.on(VoiceEvent::MuteChanged(true)), VoiceEffect::None);
        assert_eq!(session.state(), VoiceState::Muted);
    }

    /// A wake with no speech after it gives up via the driver's timeout, without
    /// transcribing anything.
    #[test]
    fn a_listen_timeout_returns_to_idle_without_transcription() {
        let mut session = VoiceSession::new();
        session.on(VoiceEvent::WakeDetected);
        assert_eq!(session.state(), VoiceState::Listening);
        assert_eq!(session.on(VoiceEvent::ListenTimeout), VoiceEffect::None);
        assert_eq!(session.state(), VoiceState::Idle);
        // Outside Listening the event is meaningless and ignored.
        assert_eq!(session.on(VoiceEvent::ListenTimeout), VoiceEffect::None);
        assert_eq!(session.state(), VoiceState::Idle);
    }

    #[test]
    fn a_failed_reply_returns_to_idle() {
        let mut session = VoiceSession::new();
        session.on(VoiceEvent::WakeDetected);
        session.on(VoiceEvent::SpeechEnded);
        session.on(VoiceEvent::Transcribed("hi".into()));
        assert_eq!(session.on(VoiceEvent::ReplyFailed), VoiceEffect::None);
        assert_eq!(session.state(), VoiceState::Idle);
    }

    #[test]
    fn stray_events_are_ignored_rather_than_corrupting_state() {
        let mut session = VoiceSession::new();
        // A reply with no pending turn, a barge-in while idle, speech-ended before
        // a wake word — all no-ops that leave us idle.
        assert_eq!(
            session.on(VoiceEvent::ReplyReceived("ghost".into())),
            VoiceEffect::None
        );
        assert_eq!(session.on(VoiceEvent::BargeIn), VoiceEffect::None);
        assert_eq!(session.on(VoiceEvent::SpeechEnded), VoiceEffect::None);
        assert_eq!(session.state(), VoiceState::Idle);
    }

    #[test]
    fn states_map_to_the_right_character_animation() {
        assert_eq!(VoiceState::Idle.character_state(), None);
        assert_eq!(VoiceState::Muted.character_state(), None);
        assert_eq!(VoiceState::Listening.character_state(), Some("listening"));
        assert_eq!(VoiceState::Transcribing.character_state(), Some("thinking"));
        assert_eq!(VoiceState::Sending.character_state(), Some("thinking"));
        assert_eq!(VoiceState::Speaking.character_state(), Some("speaking"));
    }

    #[test]
    fn capture_runs_when_we_might_need_audio() {
        // Capture during Speaking is what makes barge-in possible.
        assert!(VoiceState::Idle.wants_capture());
        assert!(VoiceState::Listening.wants_capture());
        assert!(VoiceState::Speaking.wants_capture());
        // Not while muted, transcribing, or waiting on the gateway.
        assert!(!VoiceState::Muted.wants_capture());
        assert!(!VoiceState::Transcribing.wants_capture());
        assert!(!VoiceState::Sending.wants_capture());
    }
}
