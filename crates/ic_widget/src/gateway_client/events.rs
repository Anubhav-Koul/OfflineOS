//! The SSE event model.
//!
//! # What the stream actually carries
//!
//! Reading `WebChatV2Event` (`crates/ironclaw_webui_v2/src/schema.rs:51`) suggests
//! a rich stream with `final_reply`, `accepted`, `running`, and `failed` events.
//! **Most of those are unreachable.** The SSE handler drains
//! `RebornServices::stream_events`, which only ever yields projection payloads;
//! `ProductOutboundPayload::FinalReply` is produced solely on the Telegram/Slack
//! push-delivery path (`outbound_delivery.rs:229`) and never reaches a browser.
//!
//! So a client sees exactly: `keep_alive`, `projection_snapshot`,
//! `projection_update`, `capability_activity`, `capability_display_preview`,
//! `gate`, `auth_required`, and a terminal `error`.
//!
//! **The assistant's reply text is not on this stream at all.**
//! `ProductProjectionItem::Text` has no producer anywhere in the workspace — its
//! only construction sites are the wire `Deserialize` impl and tests. Watch the
//! run's [`RunPhase`] here, and fetch the text from
//! `GET /threads/{id}/timeline` once it goes terminal. See
//! `docs/desktop/chat-rendering.md`.
//!
//! # Wire encodings, which differ between neighbours
//!
//! - [`GatewayEvent`] is *internally* tagged: `{"type": "gate", "prompt": {…}}`.
//! - [`ProjectionItem`] is *externally* tagged: `{"run_status": {…}}` — the
//!   upstream enum derives `rename_all` with no `tag`.
//!
//! Both are decoded tolerantly. An event or item this build has never heard of
//! becomes an `Unknown` variant rather than an error, because the gateway is
//! beta and adding a variant must not brick the widget.

use serde::Deserialize;

use super::ids::{GateRef, RunId, ThreadId};

/// One event off the `GET /threads/{id}/events` stream.
#[derive(Debug, Clone, PartialEq)]
pub enum GatewayEvent {
    /// Liveness only. Emitted every 15 s.
    KeepAlive,
    /// The full projection for the thread. Sent on connect and on resume.
    ProjectionSnapshot(ProjectionState),
    /// A delta. Apply over the snapshot, keyed by run id.
    ProjectionUpdate(ProjectionState),
    /// Tool lifecycle metadata. Never carries arguments or results.
    CapabilityActivity(CapabilityActivity),
    /// A bounded, sanitized preview of a tool's output.
    CapabilityDisplayPreview(DisplayPreview),
    /// A tool-approval prompt. Resolve it with
    /// [`super::GatewayClient::resolve_gate`].
    Gate(GatePrompt),
    /// A credential/OAuth prompt.
    AuthRequired(AuthPrompt),
    /// The gateway gave up on this stream. It is closed after this event.
    Error(StreamError),
    /// An event type this build does not know. Ignore it; do not fail.
    Unknown {
        /// The SSE `event:` name.
        event: String,
    },
}

/// Terminal error frame (`event: error`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct StreamError {
    /// `RebornServicesErrorCode`, e.g. `unavailable`.
    pub error: String,
    /// `RebornServicesErrorKind`, e.g. `service_unavailable`.
    pub kind: String,
    /// Whether reconnecting may succeed.
    pub retryable: bool,
}

/// `ProductProjectionState` — everything currently renderable for a thread.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ProjectionState {
    /// The thread this projection describes.
    pub thread_id: ThreadId,
    /// Renderable items. Unknown kinds are preserved as
    /// [`ProjectionItem::Unknown`].
    pub items: Vec<ProjectionItem>,
}

impl ProjectionState {
    /// The status of `run_id`, if this projection mentions it.
    pub fn run_phase(&self, run_id: &RunId) -> Option<&RunStatusItem> {
        self.items.iter().find_map(|item| match item {
            ProjectionItem::RunStatus(status) if &status.run_id == run_id => Some(status),
            _ => None,
        })
    }

    /// Every gate awaiting the user in this projection.
    pub fn gates(&self) -> impl Iterator<Item = &ProjectionGate> {
        self.items.iter().filter_map(|item| match item {
            ProjectionItem::Gate(gate) => Some(gate),
            _ => None,
        })
    }
}

/// One renderable item.
///
/// `Text` is declared because the upstream enum declares it, and because a
/// future release may start producing it. Nothing produces it today.
#[derive(Debug, Clone, PartialEq)]
pub enum ProjectionItem {
    /// Assistant-visible text. **No producer exists**; see the module docs.
    Text {
        /// Stable item id.
        id: String,
        /// The text.
        body: String,
    },
    /// The model's reasoning, when the run profile surfaces it.
    Thinking {
        /// Stable item id.
        id: String,
        /// The run this belongs to.
        run_id: Option<RunId>,
        /// The text.
        body: String,
    },
    /// Tool lifecycle metadata.
    CapabilityActivity(CapabilityActivity),
    /// A summary of work performed in a phase of the run.
    WorkSummary {
        /// Stable item id.
        id: String,
        /// The run this belongs to.
        run_id: RunId,
        /// The text.
        body: String,
    },
    /// Where a run is. The only way to know a turn finished.
    RunStatus(RunStatusItem),
    /// A pending approval, mirrored from the `gate` event.
    Gate(ProjectionGate),
    /// Skills the agent activated for this run.
    SkillActivation {
        /// Stable item id.
        id: String,
        /// The run this belongs to.
        run_id: RunId,
        /// Skills that were activated.
        skill_names: Vec<String>,
    },
    /// An item kind this build does not know.
    Unknown {
        /// The externally-tagged key, e.g. `some_new_item`.
        kind: String,
    },
}

/// A run's current position in its lifecycle.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RunStatusItem {
    /// Which run.
    pub run_id: RunId,
    /// Where it is.
    #[serde(deserialize_with = "deserialize_run_phase")]
    pub status: RunPhase,
    /// A sanitized, user-facing explanation. Present on terminal failures.
    #[serde(default)]
    pub failure_summary: Option<String>,
}

/// A gate as it appears inside a projection.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ProjectionGate {
    /// Pass this back to resolve the gate.
    pub gate_ref: GateRef,
    /// One-line description of what is being asked.
    pub headline: String,
}

/// The `status` string on a `run_status` item.
///
/// The vocabulary is closed upstream — `turn_status_wire`
/// (`projection/turn_events.rs:602`) and `run_status_wire`
/// (`projection.rs:1060`) between them emit exactly these — but an unrecognized
/// value becomes [`RunPhase::Other`] rather than a parse error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunPhase {
    /// Admitted, not started.
    Queued,
    /// Executing.
    Running,
    /// A cancel was requested; the run has not stopped yet.
    CancelRequested,
    /// Waiting on the user: tool approval.
    BlockedApproval,
    /// Waiting on the user: credentials.
    BlockedAuth,
    /// Waiting on a resource.
    BlockedResource,
    /// Waiting on another run.
    BlockedDependentRun,
    /// Wedged; needs operator action.
    RecoveryRequired,
    /// Finished successfully.
    Completed,
    /// Stopped by the user.
    Cancelled,
    /// Finished with an error.
    Failed,
    /// Killed by the runtime.
    Killed,
    /// A status this build does not know. Treated as still in flight.
    Other(String),
}

impl RunPhase {
    /// Parse a wire status.
    pub fn from_wire(status: &str) -> Self {
        match status {
            "queued" => RunPhase::Queued,
            "running" => RunPhase::Running,
            "cancel_requested" => RunPhase::CancelRequested,
            "blocked_approval" => RunPhase::BlockedApproval,
            "blocked_auth" => RunPhase::BlockedAuth,
            "blocked_resource" => RunPhase::BlockedResource,
            "blocked_dependent_run" => RunPhase::BlockedDependentRun,
            "recovery_required" => RunPhase::RecoveryRequired,
            "completed" => RunPhase::Completed,
            "cancelled" => RunPhase::Cancelled,
            "failed" => RunPhase::Failed,
            "killed" => RunPhase::Killed,
            other => RunPhase::Other(other.to_string()),
        }
    }

    /// Whether the run has stopped for good.
    ///
    /// [`RunPhase::Other`] is deliberately **not** terminal: an unknown status
    /// from a newer gateway is more likely a new in-flight state than a new way
    /// of finishing, and treating it as terminal would make the widget stop
    /// listening while the agent was still working.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            RunPhase::Completed | RunPhase::Cancelled | RunPhase::Failed | RunPhase::Killed
        )
    }

    /// Whether the run is parked waiting for the user to answer something.
    pub fn is_blocked(&self) -> bool {
        matches!(
            self,
            RunPhase::BlockedApproval
                | RunPhase::BlockedAuth
                | RunPhase::BlockedResource
                | RunPhase::BlockedDependentRun
                | RunPhase::RecoveryRequired
        )
    }

    /// Whether the run ended in anything other than success.
    pub fn is_failure(&self) -> bool {
        matches!(self, RunPhase::Failed | RunPhase::Killed)
    }
}

fn deserialize_run_phase<'de, D>(deserializer: D) -> Result<RunPhase, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    Ok(RunPhase::from_wire(&raw))
}

/// `CapabilityActivityView` — tool lifecycle metadata. Never raw args/results.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CapabilityActivity {
    /// Unique per tool invocation.
    pub invocation_id: String,
    /// Which tool.
    pub capability_id: String,
    /// `started` | `running` | `completed` | `failed` | `killed`.
    pub status: String,
    /// The run this belongs to.
    #[serde(default)]
    pub turn_run_id: Option<RunId>,
    /// Sanitized failure class, when it failed.
    #[serde(default)]
    pub error_kind: Option<String>,
}

/// `CapabilityDisplayPreviewView` — a bounded, sanitized rendering of a tool's
/// output. Not the source of truth; the full result stays behind `result_ref`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct DisplayPreview {
    /// Headline, e.g. the tool name.
    pub title: String,
    /// Secondary line.
    #[serde(default)]
    pub subtitle: Option<String>,
    /// What went in, summarized (≤ 2 KiB).
    #[serde(default)]
    pub input_summary: Option<String>,
    /// What came out, summarized (≤ 2 KiB).
    #[serde(default)]
    pub output_summary: Option<String>,
    /// A slice of the output (≤ 16 KiB).
    #[serde(default)]
    pub output_preview: Option<String>,
    /// Whether the preview was cut short.
    #[serde(default)]
    pub truncated: bool,
}

/// `GatePromptView` — the tool-approval prompt.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct GatePrompt {
    /// The run that is parked.
    pub turn_run_id: RunId,
    /// Pass back to resolve.
    pub gate_ref: GateRef,
    /// One line: what is being asked.
    pub headline: String,
    /// The detail.
    pub body: String,
}

/// `AuthPromptView` — a credential or OAuth prompt.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AuthPrompt {
    /// The run that is parked.
    pub turn_run_id: RunId,
    /// Opaque reference to the auth request.
    pub auth_request_ref: String,
    /// One line: what is being asked.
    pub headline: String,
    /// The detail.
    pub body: String,
    /// `oauth_url` | `manual_token` | `other`.
    #[serde(default)]
    pub challenge_kind: Option<String>,
    /// Which provider, when known.
    #[serde(default)]
    pub provider: Option<String>,
    /// Where to send the user. Never carries a secret.
    #[serde(default)]
    pub authorization_url: Option<String>,
}

impl<'de> Deserialize<'de> for ProjectionItem {
    /// Decodes the externally-tagged upstream enum by hand so that an unknown
    /// key becomes [`ProjectionItem::Unknown`] instead of failing the whole
    /// projection — one new item kind upstream must not blank the widget.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;

        let value = serde_json::Value::deserialize(deserializer)?;
        let object = value
            .as_object()
            .ok_or_else(|| D::Error::custom("a projection item must be a JSON object"))?;
        let (kind, payload) = match object.iter().next() {
            Some(entry) if object.len() == 1 => entry,
            _ => {
                return Err(D::Error::custom(
                    "a projection item must have exactly one key",
                ));
            }
        };

        let missing =
            |name: &str| D::Error::custom(format!("projection item {kind} is missing {name}"));
        let field = |name: &str| -> Result<String, D::Error> {
            payload
                .get(name)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| missing(name))
        };
        let optional_run_id = |name: &str| -> Result<Option<RunId>, D::Error> {
            match payload.get(name).and_then(serde_json::Value::as_str) {
                Some(raw) => RunId::new(raw).map(Some).map_err(D::Error::custom),
                None => Ok(None),
            }
        };
        let required_run_id = |name: &str| -> Result<RunId, D::Error> {
            optional_run_id(name)?.ok_or_else(|| missing(name))
        };
        let payload = payload.clone();

        Ok(match kind.as_str() {
            "text" => ProjectionItem::Text {
                id: field("id")?,
                body: field("body")?,
            },
            "thinking" => ProjectionItem::Thinking {
                id: field("id")?,
                run_id: optional_run_id("run_id")?,
                body: field("body")?,
            },
            "capability_activity" => ProjectionItem::CapabilityActivity(
                serde_json::from_value(payload).map_err(D::Error::custom)?,
            ),
            "work_summary" => ProjectionItem::WorkSummary {
                id: field("id")?,
                run_id: required_run_id("run_id")?,
                body: field("body")?,
            },
            "run_status" => ProjectionItem::RunStatus(
                serde_json::from_value(payload).map_err(D::Error::custom)?,
            ),
            "gate" => {
                ProjectionItem::Gate(serde_json::from_value(payload).map_err(D::Error::custom)?)
            }
            "skill_activation" => ProjectionItem::SkillActivation {
                id: field("id")?,
                run_id: required_run_id("run_id")?,
                skill_names: payload
                    .get("skill_names")
                    .and_then(serde_json::Value::as_array)
                    .map(|names| {
                        names
                            .iter()
                            .filter_map(serde_json::Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default(),
            },
            other => ProjectionItem::Unknown {
                kind: other.to_string(),
            },
        })
    }
}

/// Turn one SSE frame into a [`GatewayEvent`].
///
/// `name` is the SSE `event:` field. The `error` frame has no `type` field, so
/// the name — not the body — is what dispatches.
pub(crate) fn parse_event(name: &str, data: &str) -> Result<GatewayEvent, serde_json::Error> {
    /// `{"cursor": …, "type": "gate", "prompt": {…}}` — reach past the envelope
    /// to the one payload key each variant carries.
    fn payload<'a>(frame: &'a serde_json::Value, key: &str) -> &'a serde_json::Value {
        frame.get(key).unwrap_or(&serde_json::Value::Null)
    }

    if name == "keep_alive" {
        return Ok(GatewayEvent::KeepAlive);
    }
    if name == "error" {
        return Ok(GatewayEvent::Error(serde_json::from_str(data)?));
    }

    let frame: serde_json::Value = serde_json::from_str(data)?;
    Ok(match name {
        "projection_snapshot" => GatewayEvent::ProjectionSnapshot(serde_json::from_value(
            payload(&frame, "state").clone(),
        )?),
        "projection_update" => GatewayEvent::ProjectionUpdate(serde_json::from_value(
            payload(&frame, "state").clone(),
        )?),
        "capability_activity" => GatewayEvent::CapabilityActivity(serde_json::from_value(
            payload(&frame, "activity").clone(),
        )?),
        "capability_display_preview" => GatewayEvent::CapabilityDisplayPreview(
            serde_json::from_value(payload(&frame, "preview").clone())?,
        ),
        "gate" => GatewayEvent::Gate(serde_json::from_value(payload(&frame, "prompt").clone())?),
        "auth_required" => {
            GatewayEvent::AuthRequired(serde_json::from_value(payload(&frame, "prompt").clone())?)
        }
        other => GatewayEvent::Unknown {
            event: other.to_string(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_phase_covers_every_status_the_gateway_can_emit() {
        // `turn_status_wire` (turn_events.rs:602) plus `run_status_wire`
        // (projection.rs:1060). If upstream adds one, this test still passes but
        // the value lands in `Other`, which is non-terminal by design.
        let in_flight = ["queued", "running", "cancel_requested"];
        let blocked = [
            "blocked_approval",
            "blocked_auth",
            "blocked_resource",
            "blocked_dependent_run",
            "recovery_required",
        ];
        let terminal = ["completed", "cancelled", "failed", "killed"];

        for status in in_flight {
            let phase = RunPhase::from_wire(status);
            assert!(!phase.is_terminal(), "{status}");
            assert!(!phase.is_blocked(), "{status}");
        }
        for status in blocked {
            let phase = RunPhase::from_wire(status);
            assert!(!phase.is_terminal(), "{status}");
            assert!(phase.is_blocked(), "{status}");
        }
        for status in terminal {
            assert!(RunPhase::from_wire(status).is_terminal(), "{status}");
        }
        assert!(RunPhase::from_wire("failed").is_failure());
        assert!(!RunPhase::from_wire("completed").is_failure());
    }

    #[test]
    fn an_unknown_status_is_treated_as_still_running() {
        let phase = RunPhase::from_wire("paused_for_lunch");
        assert_eq!(phase, RunPhase::Other("paused_for_lunch".into()));
        // The alternative — assuming terminal — would make the widget stop
        // listening while the agent kept working.
        assert!(!phase.is_terminal());
        assert!(!phase.is_blocked());
    }

    #[test]
    fn a_projection_snapshot_decodes_the_shape_the_gateway_sends() {
        // Exactly the frame observed in the Phase 0 Windows smoke.
        let data = r#"{
            "cursor": {"runtime": 3, "turn": {"event": 7, "scope": "s"}},
            "type": "projection_snapshot",
            "state": {
                "thread_id": "t-1",
                "items": [{"run_status": {"run_id": "r-1", "status": "running"}}]
            }
        }"#;
        let GatewayEvent::ProjectionSnapshot(state) =
            parse_event("projection_snapshot", data).expect("parse")
        else {
            panic!("expected a snapshot");
        };
        assert_eq!(state.thread_id.as_str(), "t-1");
        let run = RunId::new("r-1").expect("valid");
        assert_eq!(
            state.run_phase(&run).map(|item| &item.status),
            Some(&RunPhase::Running)
        );
    }

    #[test]
    fn a_terminal_failure_carries_a_user_facing_summary() {
        let data = r#"{
            "cursor": 1, "type": "projection_update",
            "state": {"thread_id": "t-1", "items": [
                {"run_status": {"run_id": "r-1", "status": "failed",
                                "failure_category": "provider", "failure_summary": "the model is unavailable"}}
            ]}
        }"#;
        let GatewayEvent::ProjectionUpdate(state) =
            parse_event("projection_update", data).expect("parse")
        else {
            panic!("expected an update");
        };
        let run = RunId::new("r-1").expect("valid");
        let status = state.run_phase(&run).expect("the run");
        assert!(status.status.is_terminal() && status.status.is_failure());
        assert_eq!(
            status.failure_summary.as_deref(),
            Some("the model is unavailable")
        );
    }

    #[test]
    fn an_unknown_projection_item_does_not_poison_the_projection() {
        let data = r#"{
            "cursor": 1, "type": "projection_snapshot",
            "state": {"thread_id": "t-1", "items": [
                {"holographic_vibe": {"id": "x", "intensity": 11}},
                {"run_status": {"run_id": "r-1", "status": "completed"}}
            ]}
        }"#;
        let GatewayEvent::ProjectionSnapshot(state) =
            parse_event("projection_snapshot", data).expect("parse")
        else {
            panic!("expected a snapshot");
        };
        // The unknown item is preserved as opaque, and the item after it still
        // decoded — a new upstream item kind must not blank the widget.
        assert_eq!(
            state.items[0],
            ProjectionItem::Unknown {
                kind: "holographic_vibe".into()
            }
        );
        assert!(
            state
                .run_phase(&RunId::new("r-1").expect("valid"))
                .is_some()
        );
    }

    #[test]
    fn an_unknown_event_type_is_ignored_not_fatal() {
        let event = parse_event("quantum_entanglement", r#"{"cursor": 1}"#).expect("parse");
        assert_eq!(
            event,
            GatewayEvent::Unknown {
                event: "quantum_entanglement".into()
            }
        );
    }

    #[test]
    fn keep_alive_needs_no_body() {
        assert_eq!(
            parse_event("keep_alive", r#"{"cursor": 1, "type": "keep_alive"}"#).expect("parse"),
            GatewayEvent::KeepAlive
        );
    }

    #[test]
    fn the_error_frame_is_dispatched_by_name_because_it_has_no_type_field() {
        let data = r#"{"error": "unavailable", "kind": "service_unavailable", "retryable": true}"#;
        let GatewayEvent::Error(error) = parse_event("error", data).expect("parse") else {
            panic!("expected an error frame");
        };
        assert_eq!(error.kind, "service_unavailable");
        assert!(error.retryable);
    }

    #[test]
    fn a_gate_prompt_decodes_and_carries_the_ref_needed_to_resolve_it() {
        let data = r#"{
            "cursor": 1, "type": "gate",
            "prompt": {"turn_run_id": "r-1", "gate_ref": "g-1",
                       "headline": "Run shell command?", "body": "ls -la"}
        }"#;
        let GatewayEvent::Gate(prompt) = parse_event("gate", data).expect("parse") else {
            panic!("expected a gate");
        };
        assert_eq!(prompt.gate_ref.as_str(), "g-1");
        assert_eq!(prompt.turn_run_id.as_str(), "r-1");
    }

    #[test]
    fn gates_are_also_readable_off_a_projection() {
        let data = r#"{
            "cursor": 1, "type": "projection_update",
            "state": {"thread_id": "t-1", "items": [
                {"gate": {"gate_ref": "g-9", "headline": "Write file?"}}
            ]}
        }"#;
        let GatewayEvent::ProjectionUpdate(state) =
            parse_event("projection_update", data).expect("parse")
        else {
            panic!("expected an update");
        };
        let gates: Vec<_> = state.gates().collect();
        assert_eq!(gates.len(), 1);
        assert_eq!(gates[0].gate_ref.as_str(), "g-9");
    }

    #[test]
    fn a_thread_id_that_would_escape_its_url_path_is_rejected_at_the_boundary() {
        let data = r#"{"cursor": 1, "type": "projection_snapshot",
                       "state": {"thread_id": "../../etc", "items": []}}"#;
        assert!(parse_event("projection_snapshot", data).is_err());
    }
}
