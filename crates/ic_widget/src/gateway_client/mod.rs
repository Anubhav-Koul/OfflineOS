//! The one place this app talks to `ironclaw-reborn serve`.
//!
//! Reborn is beta and its wire shapes move. Every request, every response, and
//! every event is decoded here so that protocol drift is a compile error or a
//! single failing test in this module, not a mystery in the UI. Nothing outside
//! this module constructs a URL or a JSON body for the gateway.
//!
//! The contract is documented in `docs/desktop/gateway-api-notes.md`. Two of its
//! consequences shape this API:
//!
//! - **The assistant's reply text never arrives on the event stream.** Watch
//!   [`events::RunPhase`] for the run to go terminal, then call
//!   [`GatewayClient::timeline`]. See [`events`].
//! - **Only three streams may be open per caller.** [`sse::EventStream`] fails
//!   fast on the `429` rather than retrying.

pub mod events;
pub mod ids;
mod sse;

use serde::Deserialize;

pub use events::{
    AuthPrompt, CapabilityActivity, DisplayPreview, GatePrompt, GatewayEvent, ProjectionItem,
    ProjectionState, RunPhase, RunStatusItem, StreamError,
};
pub use ids::{ClientActionId, GateRef, RunId, ThreadId};
pub use sse::EventStream;

use crate::error::{Error, Result};

/// Every WebChat v2 route hangs off this prefix.
pub const API_PREFIX: &str = "/api/webchat/v2";

/// The gateway caps a message body at 64 KiB (`webui_inbound.rs:237`). Checking
/// here turns a `400` round-trip into an immediate, specific error.
const MAX_MESSAGE_BYTES: usize = 64 * 1024;

/// A typed client for one `ironclaw-reborn serve` instance.
#[derive(Clone)]
pub struct GatewayClient {
    client: reqwest::Client,
    base_url: String,
    token: String,
}

impl std::fmt::Debug for GatewayClient {
    /// Redacts the bearer token, which would otherwise reach any log line that
    /// formats the client.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayClient")
            .field("base_url", &self.base_url)
            .field("token", &"<redacted>")
            .finish()
    }
}

impl GatewayClient {
    /// Build a client for `base_url` (e.g. `http://127.0.0.1:3000`, no trailing
    /// slash) authenticating with `token`.
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            // Deliberately no overall request timeout: `send_message` returns
            // as soon as the turn is admitted, but the gateway may be busy, and
            // the SSE stream must stay open for minutes.
            .build()
            .map_err(Error::ClientInit)?;
        Ok(Self {
            client,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
        })
    }

    /// The base URL, for display.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn url(&self, path: &str) -> String {
        format!("{}{API_PREFIX}{path}", self.base_url)
    }

    /// Whether the gateway is up and our token is accepted.
    ///
    /// There is no dedicated health route, so this uses the cheapest
    /// authenticated read. A `401` here means the token is wrong, which is a
    /// different problem from the process being down.
    pub async fn health(&self) -> Result<()> {
        self.get::<serde_json::Value>("/threads").await?;
        Ok(())
    }

    /// Create a thread.
    pub async fn create_thread(&self) -> Result<ThreadId> {
        let body = serde_json::json!({ "client_action_id": ClientActionId::new() });
        let response: CreateThreadResponse = self.post("/threads", &body).await?;
        Ok(response.thread.thread_id)
    }

    /// List the caller's threads, newest page first.
    pub async fn list_threads(&self, limit: Option<u32>) -> Result<Vec<ThreadSummary>> {
        let path = match limit {
            Some(limit) => format!("/threads?limit={limit}"),
            None => "/threads".to_string(),
        };
        let response: ListThreadsResponse = self.get(&path).await?;
        Ok(response.threads)
    }

    /// List the caller's scheduled automations.
    ///
    /// This is the closest thing the serve API has to a "jobs" view. It returns
    /// **schedule entries, not run history** — no run-history route exists. See
    /// `docs/desktop/dashboard-gaps.md`.
    ///
    /// The response is a single capped page with no cursor, so there is nothing
    /// to paginate through.
    pub async fn list_automations(&self, limit: Option<u32>) -> Result<Vec<Automation>> {
        let path = match limit {
            Some(limit) => format!("/automations?limit={limit}"),
            None => "/automations".to_string(),
        };
        let response: ListAutomationsResponse = self.get(&path).await?;
        Ok(response.automations)
    }

    /// Send a user message, starting a turn.
    ///
    /// `client_action_id` is the idempotency key: replaying the *same* id after
    /// a dropped connection yields [`SubmitOutcome::AlreadySubmitted`] rather
    /// than running the turn twice. Mint a fresh one per user action.
    pub async fn send_message(
        &self,
        thread_id: &ThreadId,
        content: &str,
        client_action_id: &ClientActionId,
    ) -> Result<SubmitOutcome> {
        validate_message(content)?;
        let body = serde_json::json!({
            "client_action_id": client_action_id,
            "content": content,
        });
        let response: SubmitTurnResponse = self
            .post(&format!("/threads/{thread_id}/messages"), &body)
            .await?;
        Ok(response.into())
    }

    /// Read the message history. **This is where the assistant's reply text
    /// lives** — it is never on the event stream.
    pub async fn timeline(&self, thread_id: &ThreadId, limit: Option<u32>) -> Result<Timeline> {
        let path = match limit {
            Some(limit) => format!("/threads/{thread_id}/timeline?limit={limit}"),
            None => format!("/threads/{thread_id}/timeline"),
        };
        self.get(&path).await
    }

    /// Cancel a run. This is the Stop button.
    ///
    /// `already_terminal` is `true` when the run had already finished, which is
    /// the common race when the user clicks Stop as the answer lands.
    pub async fn cancel_run(&self, thread_id: &ThreadId, run_id: &RunId) -> Result<CancelOutcome> {
        let body = serde_json::json!({
            "client_action_id": ClientActionId::new(),
            "reason": "user_requested",
        });
        self.post(&format!("/threads/{thread_id}/runs/{run_id}/cancel"), &body)
            .await
    }

    /// Answer a tool-approval or auth gate.
    pub async fn resolve_gate(
        &self,
        thread_id: &ThreadId,
        run_id: &RunId,
        gate_ref: &GateRef,
        resolution: GateResolution,
    ) -> Result<()> {
        let mut body = serde_json::json!({
            "client_action_id": ClientActionId::new(),
            "resolution": resolution.wire_value(),
        });
        if let GateResolution::CredentialProvided { credential_ref } = &resolution
            && let Some(object) = body.as_object_mut()
        {
            object.insert(
                "credential_ref".into(),
                serde_json::Value::String(credential_ref.clone()),
            );
        }
        // The response is a tagged `resumed`/`cancelled` envelope; the caller
        // learns the outcome from the event stream either way.
        let _: serde_json::Value = self
            .post(
                &format!("/threads/{thread_id}/runs/{run_id}/gates/{gate_ref}/resolve"),
                &body,
            )
            .await?;
        Ok(())
    }

    /// Open the event stream for a thread. It reconnects itself.
    pub fn events(&self, thread_id: ThreadId) -> EventStream {
        EventStream::new(self.client.clone(), &self.base_url, &self.token, thread_id)
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let url = self.url(path);
        let response = self
            .client
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|source| Error::Http {
                url: url.clone(),
                source,
            })?;
        self.decode("GET", path, &url, response).await
    }

    async fn post<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<T> {
        let url = self.url(path);
        let response = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await
            .map_err(|source| Error::Http {
                url: url.clone(),
                source,
            })?;
        self.decode("POST", path, &url, response).await
    }

    async fn decode<T: serde::de::DeserializeOwned>(
        &self,
        method: &'static str,
        path: &str,
        url: &str,
        response: reqwest::Response,
    ) -> Result<T> {
        if !response.status().is_success() {
            return Err(gateway_error(method, url, response).await);
        }
        let body = response.bytes().await.map_err(|source| Error::Http {
            url: url.to_string(),
            source,
        })?;
        serde_json::from_slice(&body).map_err(|source| Error::Protocol {
            path: path.to_string(),
            reason: source.to_string(),
        })
    }
}

/// Turn a non-2xx response into an [`Error::Gateway`], preserving the gateway's
/// own sanitized `code`/`kind`/`retryable` taxonomy.
///
/// Falls back to the status alone when the body is not the documented shape —
/// a proxy or a panic can produce a non-JSON error page, and losing the status
/// to a parse failure would be worse than losing the detail.
pub(crate) async fn gateway_error(
    method: &'static str,
    url: &str,
    response: reqwest::Response,
) -> Error {
    let status = response.status().as_u16();
    let path = response.url().path().to_string();
    let body = response.text().await.unwrap_or_default(); // silent-ok: the status is the signal

    let error = match serde_json::from_str::<GatewayErrorBody>(&body) {
        Ok(parsed) => Error::Gateway {
            method,
            path,
            status,
            code: parsed.error,
            kind: parsed.kind,
            retryable: parsed.retryable,
        },
        Err(_) => Error::Gateway {
            method,
            path,
            status,
            code: "unknown".into(),
            kind: "unknown".into(),
            // 429 and 5xx are worth retrying even without a parsed body.
            retryable: status == 429 || (500..600).contains(&status),
        },
    };
    tracing::debug!(%url, %error, "gateway returned an error");
    error
}

/// `WebUiV2HttpErrorBody` (`crates/ironclaw_webui_v2/src/error.rs:64`).
#[derive(Debug, Deserialize)]
struct GatewayErrorBody {
    error: String,
    kind: String,
    #[serde(default)]
    retryable: bool,
}

/// Reject a message the gateway would reject, before spending a round trip.
fn validate_message(content: &str) -> Result<()> {
    let reject = |reason: &'static str| {
        Err(Error::InvalidId {
            kind: "message",
            value: content.chars().take(32).collect(),
            reason,
        })
    };
    if content.trim().is_empty() {
        return reject("must not be blank");
    }
    if content.len() > MAX_MESSAGE_BYTES {
        return reject("is longer than the gateway's 64 KiB limit");
    }
    // `webui_inbound.rs:442` rejects control characters other than newline and
    // tab.
    if content
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t' && c != '\r')
    {
        return reject("contains a control character");
    }
    Ok(())
}

/// What `POST /threads/{id}/messages` decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitOutcome {
    /// The turn is running. Cancel it with this run id.
    Submitted {
        /// The new run.
        run_id: RunId,
    },
    /// Another run is already in flight on this thread; the message was
    /// accepted but not started.
    DeferredBusy {
        /// The run that is holding the thread.
        active_run_id: RunId,
    },
    /// This `client_action_id` was already used; the original run is returned.
    AlreadySubmitted {
        /// The run the first send created.
        run_id: RunId,
    },
}

impl SubmitOutcome {
    /// The run this send is associated with, whichever way it went. This is the
    /// id the Stop button cancels.
    pub fn run_id(&self) -> &RunId {
        match self {
            SubmitOutcome::Submitted { run_id }
            | SubmitOutcome::AlreadySubmitted { run_id }
            | SubmitOutcome::DeferredBusy {
                active_run_id: run_id,
            } => run_id,
        }
    }
}

/// `RebornSubmitTurnResponse` — internally tagged on `outcome`.
#[derive(Debug, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum SubmitTurnResponse {
    Submitted { run_id: RunId },
    DeferredBusy { active_run_id: RunId },
    AlreadySubmitted { run_id: RunId },
}

impl From<SubmitTurnResponse> for SubmitOutcome {
    fn from(value: SubmitTurnResponse) -> Self {
        match value {
            SubmitTurnResponse::Submitted { run_id } => SubmitOutcome::Submitted { run_id },
            SubmitTurnResponse::DeferredBusy { active_run_id } => {
                SubmitOutcome::DeferredBusy { active_run_id }
            }
            SubmitTurnResponse::AlreadySubmitted { run_id } => {
                SubmitOutcome::AlreadySubmitted { run_id }
            }
        }
    }
}

/// `RebornCancelRunResponse`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CancelOutcome {
    /// The run that was cancelled.
    pub run_id: RunId,
    /// `true` when the run had already finished — the usual race when the user
    /// clicks Stop as the answer arrives.
    pub already_terminal: bool,
}

/// How the user answered a gate.
///
/// `always: true` is deliberately not offered: the facade refuses persistent
/// approvals until an approval-policy port lands
/// (`ironclaw_product_workflow/CLAUDE.md`), so a "don't ask again" checkbox
/// would be a lie.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateResolution {
    /// Let the tool run, this once.
    Approved,
    /// Refuse the tool.
    Denied,
    /// Abandon the run.
    Cancelled,
    /// Supply a host-held credential reference. Never a raw secret.
    CredentialProvided {
        /// An opaque host reference, not the secret itself.
        credential_ref: String,
    },
}

impl GateResolution {
    fn wire_value(&self) -> &'static str {
        match self {
            GateResolution::Approved => "approved",
            GateResolution::Denied => "denied",
            GateResolution::Cancelled => "cancelled",
            GateResolution::CredentialProvided { .. } => "credential_provided",
        }
    }
}

#[derive(Debug, Deserialize)]
struct CreateThreadResponse {
    thread: ThreadSummary,
}

#[derive(Debug, Deserialize)]
struct ListThreadsResponse {
    threads: Vec<ThreadSummary>,
}

/// `SessionThreadRecord`, trimmed to what the widget renders.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ThreadSummary {
    /// The thread.
    pub thread_id: ThreadId,
    /// Its title, once the agent has named it.
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ListAutomationsResponse {
    automations: Vec<Automation>,
}

/// `RebornAutomationInfo`, trimmed to what the dashboard renders.
///
/// Timestamps stay as RFC 3339 strings. They are display-only and the frontend
/// formats them with `Date`; decoding to a `chrono` type here would pull in a
/// dependency to turn a string back into a string. The `source` field is not
/// decoded — WebUI v2 only ever exposes user schedules.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Automation {
    /// Stable id for the schedule.
    pub automation_id: String,
    /// Its user-facing name.
    pub name: String,
    /// Where it is in its lifecycle.
    #[serde(deserialize_with = "deserialize_automation_state")]
    pub state: AutomationState,
    /// When it next fires, if it is scheduled to.
    #[serde(default)]
    pub next_run_at: Option<String>,
    /// When it last fired, if ever.
    #[serde(default)]
    pub last_run_at: Option<String>,
    /// How the last run ended. `None` before the first run.
    #[serde(default, deserialize_with = "deserialize_last_status")]
    pub last_status: Option<AutomationRunStatus>,
    /// Whether the schedule is currently armed.
    #[serde(default)]
    pub is_active: bool,
}

/// Browser-visible automation state.
///
/// The gateway already collapses states it does not expose into `unknown`, so
/// an unrecognized value carries no information worth preserving — unlike
/// [`RunPhase::Other`], which keeps the raw string because an unknown *run*
/// status is a real state the widget must not treat as terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomationState {
    /// Armed and currently running.
    Active,
    /// Armed, waiting for its next trigger.
    Scheduled,
    /// Temporarily suspended by the user.
    Paused,
    /// Switched off.
    Disabled,
    /// Not armed and not scheduled.
    Inactive,
    /// Finished; it will not fire again.
    Completed,
    /// Not a state this build knows, or one the gateway declined to name.
    Unknown,
}

impl AutomationState {
    /// Parse a wire state. Anything unrecognized becomes [`Self::Unknown`].
    pub fn from_wire(state: &str) -> Self {
        match state {
            "active" => AutomationState::Active,
            "scheduled" => AutomationState::Scheduled,
            "paused" => AutomationState::Paused,
            "disabled" => AutomationState::Disabled,
            "inactive" => AutomationState::Inactive,
            "completed" => AutomationState::Completed,
            _ => AutomationState::Unknown,
        }
    }

    /// The snake_case wire spelling, for rendering.
    pub fn as_str(&self) -> &'static str {
        match self {
            AutomationState::Active => "active",
            AutomationState::Scheduled => "scheduled",
            AutomationState::Paused => "paused",
            AutomationState::Disabled => "disabled",
            AutomationState::Inactive => "inactive",
            AutomationState::Completed => "completed",
            AutomationState::Unknown => "unknown",
        }
    }
}

/// How an automation's last run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutomationRunStatus {
    /// The run finished successfully.
    Ok,
    /// The run finished with an error.
    Error,
    /// A terminal status this build does not know.
    Other(String),
}

impl AutomationRunStatus {
    /// Parse a wire status.
    pub fn from_wire(status: &str) -> Self {
        match status {
            "ok" => AutomationRunStatus::Ok,
            "error" => AutomationRunStatus::Error,
            other => AutomationRunStatus::Other(other.to_string()),
        }
    }

    /// The wire spelling, for rendering.
    pub fn as_str(&self) -> &str {
        match self {
            AutomationRunStatus::Ok => "ok",
            AutomationRunStatus::Error => "error",
            AutomationRunStatus::Other(raw) => raw,
        }
    }
}

fn deserialize_automation_state<'de, D>(deserializer: D) -> Result<AutomationState, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    Ok(AutomationState::from_wire(&raw))
}

fn deserialize_last_status<'de, D>(deserializer: D) -> Result<Option<AutomationRunStatus>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<String>::deserialize(deserializer)?;
    Ok(raw.as_deref().map(AutomationRunStatus::from_wire))
}

/// `RebornTimelineResponse`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Timeline {
    /// Oldest first.
    pub messages: Vec<Message>,
    /// Pass back as `?cursor=` to load the page before this one. `None` means
    /// the start of the thread.
    #[serde(default)]
    pub next_cursor: Option<String>,
}

impl Timeline {
    /// The last finalized assistant message, which is the answer to the most
    /// recent turn.
    pub fn latest_assistant_reply(&self) -> Option<&Message> {
        self.messages
            .iter()
            .rev()
            .find(|message| message.kind == MessageKind::Assistant && message.content.is_some())
    }
}

/// `ThreadMessageRecord`, trimmed. Render `content` — never the provider side
/// channel, which the gateway does not serialize anyway.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Message {
    /// Per-thread ordering.
    pub sequence: u64,
    /// Who said it.
    pub kind: MessageKind,
    /// `finalized` for a completed assistant answer.
    pub status: String,
    /// The text. `None` for messages whose body is a reference (tool results).
    #[serde(default)]
    pub content: Option<String>,
}

/// `MessageKind`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    /// The user.
    User,
    /// The agent.
    Assistant,
    /// System framing.
    System,
    /// A rollup of older messages.
    Summary,
    /// A kind this build does not know.
    #[serde(other)]
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_automation_list_decodes_a_full_row_and_a_never_run_one() {
        let response: ListAutomationsResponse = serde_json::from_str(
            r#"{"automations":[
                {"automation_id":"a-1","name":"Nightly digest","source":{"kind":"schedule"},
                 "state":"scheduled","next_run_at":"2026-07-11T03:00:00Z",
                 "last_run_at":"2026-07-10T03:00:00Z","last_status":"ok",
                 "is_active":true,"created_at":"2026-07-01T00:00:00Z"},
                {"automation_id":"a-2","name":"Never ran","source":{"kind":"schedule"},
                 "state":"paused","is_active":false}
            ]}"#,
        )
        .expect("automations");

        let [first, second] = response.automations.as_slice() else {
            panic!("expected two automations");
        };

        assert_eq!(first.state, AutomationState::Scheduled);
        assert_eq!(first.last_status, Some(AutomationRunStatus::Ok));
        assert_eq!(first.next_run_at.as_deref(), Some("2026-07-11T03:00:00Z"));
        assert!(first.is_active);

        // A schedule that has never fired omits every timestamp and the status.
        // Those fields must not be required, or the whole page fails to decode.
        assert_eq!(second.state, AutomationState::Paused);
        assert_eq!(second.last_status, None);
        assert_eq!(second.next_run_at, None);
        assert!(!second.is_active);
    }

    #[test]
    fn an_unknown_automation_state_or_status_does_not_fail_the_page() {
        // One unrecognized row must not cost us the rows we *can* render.
        let response: ListAutomationsResponse = serde_json::from_str(
            r#"{"automations":[
                {"automation_id":"a-1","name":"From a newer gateway",
                 "state":"hibernating","last_status":"timed_out","is_active":true}
            ]}"#,
        )
        .expect("tolerant decode");

        let automation = &response.automations[0];
        assert_eq!(automation.state, AutomationState::Unknown);
        assert_eq!(
            automation.last_status,
            Some(AutomationRunStatus::Other("timed_out".into()))
        );
        // The raw status survives for display; the state does not, because the
        // gateway itself already collapsed anything it would not name.
        assert_eq!(
            automation.last_status.as_ref().unwrap().as_str(),
            "timed_out"
        );
        assert_eq!(automation.state.as_str(), "unknown");
    }

    #[test]
    fn a_submit_response_decodes_each_outcome_and_exposes_one_run_id() {
        let submitted: SubmitTurnResponse = serde_json::from_str(
            r#"{"outcome":"submitted","thread_id":"t","accepted_message_ref":"msg:1",
                "turn_id":"turn","run_id":"r-1","status":"Queued",
                "resolved_run_profile_id":"p","resolved_run_profile_version":1,"event_cursor":1}"#,
        )
        .expect("submitted");
        assert_eq!(SubmitOutcome::from(submitted).run_id().as_str(), "r-1");

        let busy: SubmitTurnResponse = serde_json::from_str(
            r#"{"outcome":"deferred_busy","thread_id":"t","accepted_message_ref":"msg:2",
                "active_run_id":"r-2","status":"Running","event_cursor":2}"#,
        )
        .expect("deferred_busy");
        // The Stop button must cancel the run that is actually holding the
        // thread, not a run that was never created.
        assert_eq!(SubmitOutcome::from(busy).run_id().as_str(), "r-2");

        let replayed: SubmitTurnResponse = serde_json::from_str(
            r#"{"outcome":"already_submitted","thread_id":"t","accepted_message_ref":"msg:1",
                "run_id":"r-1","status":"Running","event_cursor":3}"#,
        )
        .expect("already_submitted");
        assert_eq!(SubmitOutcome::from(replayed).run_id().as_str(), "r-1");
    }

    #[test]
    fn message_validation_matches_the_gateways_own_rules() {
        assert!(validate_message("hello").is_ok());
        // Newline and tab are explicitly allowed.
        assert!(validate_message("a\nb\tc").is_ok());
        assert!(validate_message("").is_err());
        assert!(validate_message("   \n ").is_err());
        assert!(validate_message("bell\u{7}").is_err());
        assert!(validate_message(&"x".repeat(MAX_MESSAGE_BYTES + 1)).is_err());
        validate_message(&"x".repeat(MAX_MESSAGE_BYTES)).expect("at the limit");
    }

    #[test]
    fn the_latest_assistant_reply_ignores_user_turns_and_empty_bodies() {
        let timeline = Timeline {
            next_cursor: None,
            messages: vec![
                Message {
                    sequence: 1,
                    kind: MessageKind::User,
                    status: "accepted".into(),
                    content: Some("what is 21 * 2?".into()),
                },
                Message {
                    sequence: 2,
                    kind: MessageKind::Assistant,
                    status: "finalized".into(),
                    content: Some("42".into()),
                },
                // A tool-result reference carries no renderable content.
                Message {
                    sequence: 3,
                    kind: MessageKind::Assistant,
                    status: "finalized".into(),
                    content: None,
                },
            ],
        };
        assert_eq!(
            timeline
                .latest_assistant_reply()
                .and_then(|m| m.content.as_deref()),
            Some("42")
        );
    }

    #[test]
    fn an_unknown_message_kind_does_not_fail_the_timeline() {
        let timeline: Timeline = serde_json::from_str(
            r#"{"messages":[{"sequence":1,"kind":"telepathy","status":"finalized","content":"x"}]}"#,
        )
        .expect("decode");
        assert_eq!(timeline.messages[0].kind, MessageKind::Other);
    }

    #[test]
    fn gate_resolutions_map_to_the_facades_vocabulary() {
        assert_eq!(GateResolution::Approved.wire_value(), "approved");
        assert_eq!(GateResolution::Denied.wire_value(), "denied");
        assert_eq!(GateResolution::Cancelled.wire_value(), "cancelled");
        assert_eq!(
            GateResolution::CredentialProvided {
                credential_ref: "host-ref".into()
            }
            .wire_value(),
            "credential_provided"
        );
    }

    #[test]
    fn the_bearer_token_never_appears_in_debug_output() {
        let client = GatewayClient::new("http://127.0.0.1:3000", "super-secret").expect("client");
        let rendered = format!("{client:?}");
        assert!(!rendered.contains("super-secret"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }

    #[test]
    fn a_trailing_slash_on_the_base_url_does_not_double_up() {
        let client = GatewayClient::new("http://127.0.0.1:3000/", "t").expect("client");
        assert_eq!(
            client.url("/threads"),
            "http://127.0.0.1:3000/api/webchat/v2/threads"
        );
    }
}
