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
    /// Rendering a reply (and speaking it, once TTS lands in Phase 5).
    Speaking,
    /// Waiting on the user: a tool-approval or auth gate is open.
    Concerned,
    /// The backend is unhealthy — the gateway failed or rejects our token.
    Error,
}

/// The signals the state is derived from.
#[derive(Debug, Clone)]
pub struct CharacterInputs {
    /// The supervised gateway's health.
    pub gateway: GatewayState,
    /// The active run's phase, or `None` when no run is in flight.
    pub run: Option<RunPhase>,
    /// The user is addressing the agent (input focus; wake word in Phase 5).
    pub listening: bool,
    /// A reply is being rendered (TTS playback in Phase 5).
    pub speaking: bool,
}

/// Map the app's state onto the character's.
///
/// Priority, highest first: a broken backend (`Error`) overrides everything; a
/// gateway still coming up rests (`Idle`) since no run can be active yet; then
/// an open gate (`Concerned`) outranks an in-flight run (`Thinking`), which
/// outranks rendering a reply (`Speaking`), which outranks taking input
/// (`Listening`). Everything else is `Idle`.
pub fn derive(inputs: &CharacterInputs) -> CharacterState {
    match inputs.gateway {
        // A dead or rejecting gateway is the error face, whatever else is set.
        GatewayState::Unhealthy { .. } | GatewayState::Stopped => return CharacterState::Error,
        // Still booting or respawning: nothing to react to yet. A run cannot be
        // in flight against a gateway that is not answering.
        GatewayState::Starting | GatewayState::Restarting { .. } => return CharacterState::Idle,
        GatewayState::Ready => {}
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
            return CharacterState::Thinking;
        }
    }

    if inputs.speaking {
        return CharacterState::Speaking;
    }
    if inputs.listening {
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
            listening: false,
            speaking: false,
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
    fn a_ready_idle_gateway_with_no_run_rests() {
        assert_eq!(
            derive(&inputs(GatewayState::Ready, None)),
            CharacterState::Idle
        );
    }
}
