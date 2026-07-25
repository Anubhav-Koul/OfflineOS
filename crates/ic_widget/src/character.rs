//! The character's animation state, derived from what the app is doing.
//!
//! Phase 3 puts an animated character on the desktop. Its expression follows the
//! agent: calm when idle, busy while a run is in flight, worried when it needs
//! the user, upset when the backend is down. That mapping is pure logic and has
//! nothing to do with *how* the character is drawn (Live2D or a flat sprite), so
//! it lives here, is unit-tested, and is emitted to whichever renderer the
//! frontend ends up using.
//!
//! The inputs are states the app already tracks: the gateway's health and the
//! active run's phase, plus two Phase 5 hooks (`listening`, `speaking`) that are
//! always `false` until voice and TTS land.

use serde::Serialize;

use crate::gateway_client::RunPhase;
use crate::supervisor::GatewayState;

/// What the character is doing. Renderer-agnostic; a `character.json` maps each
/// of these to a Live2D expression/motion or a sprite pose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CharacterState {
    /// Nothing in flight; the resting animation.
    Idle,
    /// Taking input — input focus now, wake word in Phase 5.
    Listening,
    /// A run is in flight. Because there is no token streaming, this holds until
    /// the reply is fetched — see `docs/desktop/chat-rendering.md`.
    Thinking,
    /// A run is in flight **and it handed the work to a subagent** (Phase 8g).
    /// Distinct from `Thinking` because it is a different kind of waiting: the
    /// agent is not working on the answer, something it spawned is — and a wait
    /// with a reason reads very differently from a wait without one.
    Delegating,
    /// Rendering a reply (and speaking it, once TTS lands in Phase 5).
    Speaking,
    /// Offering something nobody asked for: an ambient suggestion is on screen,
    /// waiting for Accept or Not now (Phase 7a). Interruptible — answering it, or
    /// anything more urgent happening, moves the character straight on.
    Suggesting,
    /// Waiting on the user: a tool-approval or auth gate is open.
    Concerned,
    /// The backend is unhealthy — the gateway failed or rejects our token.
    Error,
}

/// The signals the state is derived from.
///
/// The typed-chat signals (`listening`/`speaking`) and the voice pipeline's
/// (`voice_*`) are **separate fields, OR-ed in [`derive`]** — deliberately. Both
/// sources fire independently (a typed reply's reading-time window can overlap a
/// spoken turn), and when they shared one pair, whichever wrote last clobbered the
/// other: a stale typed timer would freeze the mouth mid-TTS by clearing the flag
/// voice had set.
#[derive(Debug, Clone)]
pub struct CharacterInputs {
    /// The supervised gateway's health.
    pub gateway: GatewayState,
    /// The active run's phase, or `None` when no run is in flight.
    pub run: Option<RunPhase>,
    /// The user is addressing the agent by keyboard (composer focus).
    pub listening: bool,
    /// A typed reply is being rendered (a reading-time window).
    pub speaking: bool,
    /// The voice pipeline is capturing an utterance (wake word / push-to-talk).
    pub voice_listening: bool,
    /// The voice pipeline is transcribing or awaiting the gateway — thinking, even
    /// though the run rides voice's own thread and never sets `run`.
    pub voice_thinking: bool,
    /// TTS playback is audible.
    pub voice_speaking: bool,
    /// The browser sidecar is waiting for the user to approve a sensitive fill.
    /// Like a gateway gate, this makes the character `concerned` — but it is not a
    /// gateway run, so it needs its own input.
    pub browser_approval_pending: bool,
    /// An ambient suggestion is on screen, unanswered (Phase 7a).
    pub suggestion_pending: bool,
    /// A run is **parked on an auth gate**: a connector's credential was refused
    /// and the turn cannot continue until a better one is supplied (Phase 8b).
    ///
    /// Its own input, like `browser_approval_pending`, because the projection's
    /// run status does not carry it — the gate arrives as its own SSE event, and
    /// without this the character would look like it was still thinking about a
    /// question it has quietly stopped answering.
    pub auth_gate_pending: bool,
    /// The in-flight run has spawned a subagent that has not finished (8g).
    ///
    /// Its own input for the same reason `auth_gate_pending` is: the projection's
    /// run status says only `running`, and the `capability_progress` SSE variant
    /// that would have carried this **never fires** under our profile (8g's
    /// VERIFY, the same dormancy 8d found on `gate`). It is read from the
    /// timeline instead, which is where the runtime does record it.
    pub subagent_running: bool,
}

impl Default for CharacterInputs {
    /// The state at launch: the gateway is still coming up, nothing else set.
    fn default() -> Self {
        Self {
            gateway: GatewayState::Starting,
            run: None,
            listening: false,
            speaking: false,
            voice_listening: false,
            voice_thinking: false,
            voice_speaking: false,
            browser_approval_pending: false,
            suggestion_pending: false,
            auth_gate_pending: false,
            subagent_running: false,
        }
    }
}

/// Map the app's state onto the character's.
///
/// Priority, highest first: a broken backend (`Error`) overrides everything; a
/// gateway still coming up rests (`Idle`) since no run can be active yet; then
/// an open gate (`Concerned`) outranks an in-flight run (`Thinking`), which
/// outranks an unanswered suggestion (`Suggesting`), which outranks rendering a
/// reply (`Speaking`), which outranks taking input (`Listening`). Everything else
/// is `Idle`.
///
/// A suggestion sits below work and above speech deliberately: what the user asked
/// for always beats what the character volunteered, but an unanswered question the
/// character asked outranks it merely finishing a sentence.
pub fn derive(inputs: &CharacterInputs) -> CharacterState {
    match inputs.gateway {
        // A dead or rejecting gateway is the error face, whatever else is set.
        GatewayState::Unhealthy { .. } | GatewayState::Stopped => return CharacterState::Error,
        // Still booting or respawning: nothing to react to yet. A run cannot be
        // in flight against a gateway that is not answering.
        GatewayState::Starting | GatewayState::Restarting { .. } => return CharacterState::Idle,
        GatewayState::Ready => {}
    }

    // A pending browser fill approval is a gate too: the user must act before the
    // agent can type. It outranks in-flight work, the same as a gateway gate.
    //
    // So does a parked auth gate (Phase 8b): the run is not thinking, it is
    // *waiting* — and a character that keeps thinking at the user is how an
    // endless spinner is built with extra steps.
    if inputs.browser_approval_pending || inputs.auth_gate_pending {
        return CharacterState::Concerned;
    }

    if let Some(run) = &inputs.run {
        // A gate outranks plain work: the user has to act before anything moves.
        if run.is_blocked() {
            return CharacterState::Concerned;
        }
        // Any non-terminal run is work in progress. `RunPhase::Other` is
        // deliberately non-terminal, so an unknown status keeps the character
        // thinking rather than snapping to idle mid-turn.
        if !run.is_terminal() {
            // Same wait, better answer: when the run has handed the work to a
            // subagent, say so rather than showing generic progress (8g).
            return if inputs.subagent_running {
                CharacterState::Delegating
            } else {
                CharacterState::Thinking
            };
        }
    }
    // A voice turn's run rides voice's own thread, so it never reaches `run`;
    // its transcribing/awaiting window is thinking all the same.
    if inputs.voice_thinking {
        return CharacterState::Thinking;
    }

    // Something the character offered, still unanswered. It outranks speech but not
    // work: interrupting the user's own turn to pitch an idea is exactly the
    // behaviour that gets an ambient companion switched off.
    if inputs.suggestion_pending {
        return CharacterState::Suggesting;
    }

    if inputs.speaking || inputs.voice_speaking {
        return CharacterState::Speaking;
    }
    if inputs.listening || inputs.voice_listening {
        return CharacterState::Listening;
    }
    CharacterState::Idle
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(gateway: GatewayState, run: Option<RunPhase>) -> CharacterInputs {
        CharacterInputs {
            gateway,
            run,
            ..CharacterInputs::default()
        }
    }

    #[test]
    fn an_unhealthy_or_stopped_gateway_is_the_error_face() {
        assert_eq!(
            derive(&inputs(
                GatewayState::Unhealthy {
                    reason: "boom".into()
                },
                None
            )),
            CharacterState::Error
        );
        assert_eq!(
            derive(&inputs(GatewayState::Stopped, None)),
            CharacterState::Error
        );
    }

    #[test]
    fn error_outranks_an_in_flight_run() {
        // A run phase left over from before the gateway died must not keep the
        // character thinking while the backend is broken.
        let mut given = inputs(
            GatewayState::Unhealthy {
                reason: "rejected".into(),
            },
            Some(RunPhase::Running),
        );
        given.speaking = true;
        given.listening = true;
        assert_eq!(derive(&given), CharacterState::Error);
    }

    #[test]
    fn a_gateway_still_coming_up_rests() {
        assert_eq!(
            derive(&inputs(GatewayState::Starting, None)),
            CharacterState::Idle
        );
        assert_eq!(
            derive(&inputs(
                GatewayState::Restarting {
                    attempt: 1,
                    backoff_ms: 500
                },
                None
            )),
            CharacterState::Idle
        );
    }

    #[test]
    fn an_open_gate_is_concerned_and_outranks_thinking() {
        // `RecoveryRequired` (a wedged run needing operator action) is blocked
        // too — a worried face fits a run that needs a hand.
        for blocked in [
            RunPhase::BlockedApproval,
            RunPhase::BlockedAuth,
            RunPhase::BlockedResource,
            RunPhase::BlockedDependentRun,
            RunPhase::RecoveryRequired,
        ] {
            assert_eq!(
                derive(&inputs(GatewayState::Ready, Some(blocked))),
                CharacterState::Concerned
            );
        }
    }

    #[test]
    fn a_running_turn_is_thinking() {
        for running in [
            RunPhase::Queued,
            RunPhase::Running,
            RunPhase::CancelRequested,
            RunPhase::Other("something_new".into()),
        ] {
            assert_eq!(
                derive(&inputs(GatewayState::Ready, Some(running))),
                CharacterState::Thinking
            );
        }
    }

    #[test]
    fn a_finished_run_falls_through_to_speaking_then_listening_then_idle() {
        // A terminal run does not itself paint a face; the reply-render and input
        // signals do, in that order.
        let base = inputs(GatewayState::Ready, Some(RunPhase::Completed));
        assert_eq!(derive(&base), CharacterState::Idle);

        let speaking = CharacterInputs {
            speaking: true,
            ..base.clone()
        };
        assert_eq!(derive(&speaking), CharacterState::Speaking);

        let listening = CharacterInputs {
            listening: true,
            ..base.clone()
        };
        assert_eq!(derive(&listening), CharacterState::Listening);

        // Speaking outranks listening when both are set.
        let both = CharacterInputs {
            speaking: true,
            listening: true,
            ..base
        };
        assert_eq!(derive(&both), CharacterState::Speaking);
    }

    #[test]
    fn an_unanswered_suggestion_outranks_speech_but_not_work() {
        let base = inputs(GatewayState::Ready, None);

        let suggesting = CharacterInputs {
            suggestion_pending: true,
            speaking: true,
            ..base.clone()
        };
        assert_eq!(derive(&suggesting), CharacterState::Suggesting);

        // The user's own turn always wins: the character does not pitch an idea
        // over the answer it is in the middle of producing.
        let working = CharacterInputs {
            suggestion_pending: true,
            run: Some(RunPhase::Running),
            ..base.clone()
        };
        assert_eq!(derive(&working), CharacterState::Thinking);

        // Delegating is the same wait with a better answer (8g): a run that has
        // spawned a subagent says so instead of showing generic progress.
        let delegating = CharacterInputs {
            run: Some(RunPhase::Running),
            subagent_running: true,
            ..base.clone()
        };
        assert_eq!(derive(&delegating), CharacterState::Delegating);

        // It is still work, so a gate outranks it and a finished run clears it.
        let gated_delegation = CharacterInputs {
            browser_approval_pending: true,
            ..delegating.clone()
        };
        assert_eq!(derive(&gated_delegation), CharacterState::Concerned);
        let done = CharacterInputs {
            run: Some(RunPhase::Completed),
            subagent_running: true,
            ..base.clone()
        };
        assert_ne!(
            derive(&done),
            CharacterState::Delegating,
            "a terminal run is not delegating, whatever the stale flag says"
        );

        // And a gate still outranks it — that one is blocking.
        let gated = CharacterInputs {
            suggestion_pending: true,
            browser_approval_pending: true,
            ..base
        };
        assert_eq!(derive(&gated), CharacterState::Concerned);
    }

    /// A run parked on an auth gate is *waiting*, not working — the character has
    /// to show that, or the user watches a thinking face over a stopped turn.
    #[test]
    fn a_parked_auth_gate_is_concern_not_thought() {
        let ready = CharacterInputs {
            gateway: GatewayState::Ready,
            ..CharacterInputs::default()
        };

        // The projection does not carry the gate, so the run still reads as
        // in-flight. That is precisely why the gate needs an input of its own.
        let parked = CharacterInputs {
            run: Some(RunPhase::Running),
            auth_gate_pending: true,
            ..ready.clone()
        };
        assert_eq!(derive(&parked), CharacterState::Concerned);

        let working = CharacterInputs {
            run: Some(RunPhase::Running),
            ..ready
        };
        assert_eq!(derive(&working), CharacterState::Thinking);
    }

    #[test]
    fn a_ready_idle_gateway_with_no_run_rests() {
        assert_eq!(
            derive(&inputs(GatewayState::Ready, None)),
            CharacterState::Idle
        );
    }

    /// The voice pipeline's signals are independent of the typed-chat pair: either
    /// source alone paints the face, and a stale typed flag cannot cancel a live
    /// voice one (the clobbering bug this split fixes).
    #[test]
    fn voice_signals_are_ord_with_the_typed_ones() {
        let base = inputs(GatewayState::Ready, None);

        let voice_speaking = CharacterInputs {
            voice_speaking: true,
            // The typed reading-time window just expired — irrelevant to TTS.
            speaking: false,
            ..base.clone()
        };
        assert_eq!(derive(&voice_speaking), CharacterState::Speaking);

        let voice_listening = CharacterInputs {
            voice_listening: true,
            ..base.clone()
        };
        assert_eq!(derive(&voice_listening), CharacterState::Listening);

        // A voice turn's transcribe/await window thinks, even with no `run`
        // (voice rides its own thread, invisible to the typed pump).
        let voice_thinking = CharacterInputs {
            voice_thinking: true,
            voice_speaking: true, // thinking outranks speaking, as for typed runs
            ..base
        };
        assert_eq!(derive(&voice_thinking), CharacterState::Thinking);
    }
}
