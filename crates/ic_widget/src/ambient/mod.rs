//! The character speaking first (Phase 7a).
//!
//! Everything the app volunteers — a scheduled automation that just finished today,
//! a skill it wants to learn tomorrow — goes through one path:
//!
//! ```text
//! a source  →  Suggestion  →  guardrail::check  →  the bubble (Accept / Not now)
//!                                    │                      │
//!                                    └── suppressed         └── recorded in the log,
//!                                        (logged, silent)       which feeds the guardrail
//! ```
//!
//! Three things are worth knowing before changing any of it, all verified against a
//! running gateway rather than read off the code:
//!
//! - **The gateway's trigger poller is off by default** (`TriggerPollerSettings::
//!   default().enabled == false`). Without `IRONCLAW_TRIGGER_POLLER_ENABLED=true` a
//!   scheduled automation *never fires* — `GET /automations` lists it, and nothing
//!   ever runs it. The widget sets that variable only when ambient mode is on, so
//!   "ambient off" means no unprompted agent run exists at all.
//! - **A trigger fire lands in a brand-new thread of its own**, one per fire — not
//!   the ambient thread, not the chat thread. It shows up in `GET /threads` and its
//!   timeline reads like any other, so the reply is fetchable; but
//!   `GET /automations` carries **no thread id and no run id**, so nothing correlates
//!   the two but timing. See [`automations`].
//! - **The runtime never prompts before a tool runs** (the Phase 4 finding —
//!   `default_permission` is read by nothing). `builtin__trigger_create` therefore
//!   arms a recurring background prompt with no approval. That is precisely why the
//!   poller rides the ambient toggle: a prompt-injected agent cannot give itself a
//!   heartbeat the user never switched on.
//!
//! The module is Tauri-free (like [`crate::browser`]): the caller passes a
//! [`SuggestionSink`] that emits the event, so the coupling lives in one place and
//! the logic is testable without a webview.

pub mod automations;
pub mod guardrail;
pub mod log;
pub mod reflection;
pub mod watch;

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{Local, Utc};
use serde::Serialize;
use tokio::sync::Mutex;

use crate::gateway_client::{GatewayClient, ThreadId};
use crate::settings::AmbientSettings;
use crate::voice::{TurnResult, drive_turn};

use guardrail::Suppression;
use log::{LogEntry, LogEvent, SurfacingLog};

/// What kind of thing is being suggested — it decides how the bubble renders
/// the card and what Accept *does*.
///
/// An automation's result is an offer to look (blue; Accept opens the thread).
/// A skill draft is an offer to **install** (red, the Phase 4 consent-gate
/// pattern; Accept writes the skill, so the card must read as a consent
/// prompt, not a notification).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SuggestionKind {
    /// A completed automation run worth a look.
    Automation,
    /// A draft SKILL.md awaiting the user's consent to install (Phase 7b).
    SkillDraft,
    /// A third-party skill folder awaiting the user's consent to import (7c).
    /// Solicited — the user initiated it in the dashboard — so it is answered
    /// even while ambient mode is off, and it never touches the guardrail.
    SkillImport,
    /// A watcher rule fired and the agent's answer is worth a look (7d).
    /// Renders like an automation: a calm offer, Accept opens the thread.
    Watcher,
}

/// What the bubble shows when the character speaks first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Suggestion {
    /// Unique per popup; the id the UI answers with.
    pub id: String,
    /// What is being offered, and therefore what Accept does.
    pub kind: SuggestionKind,
    /// The exact thing being suggested — the dedupe key. Shown once, ever.
    pub key: String,
    /// Where it came from, e.g. `automation:01K…`. A "Not now" quiets *this*.
    pub source: String,
    /// One line: what happened.
    pub headline: String,
    /// The detail — an automation's answer, a draft skill. May be empty.
    pub body: String,
    /// The thread the detail lives in, when there is one. Accepting opens it.
    pub thread_id: Option<String>,
}

/// Emits a suggestion to the UI. The widget's is a Tauri event.
pub type SuggestionSink = Arc<dyn Fn(Suggestion) + Send + Sync>;

/// Reads the live ambient settings. Called on every check, so a toggle in the
/// dashboard takes effect on the next tick rather than the next launch.
pub type ConfigFn = Arc<dyn Fn() -> AmbientConfig + Send + Sync>;

/// The ambient settings as the guardrail sees them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmbientConfig {
    /// The master switch.
    pub enabled: bool,
    /// The guardrails.
    pub settings: AmbientSettings,
}

/// The proactive side of the app: one gateway, one log, one sink.
pub struct AmbientService {
    client: GatewayClient,
    config: ConfigFn,
    sink: SuggestionSink,
    log: Mutex<SurfacingLog>,
    /// Surfacings the user has not answered yet, by id — so a reply can be recorded
    /// against the right key and source without the UI having to hand them back.
    pending: Mutex<HashMap<String, Suggestion>>,
}

impl AmbientService {
    /// Build the service around a running gateway.
    pub fn new(
        client: GatewayClient,
        config: ConfigFn,
        sink: SuggestionSink,
        log: SurfacingLog,
    ) -> Self {
        Self {
            client,
            config,
            sink,
            log: Mutex::new(log),
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// The gateway this service watches.
    pub fn client(&self) -> &GatewayClient {
        &self.client
    }

    /// Whether ambient mode is on right now.
    pub fn enabled(&self) -> bool {
        (self.config)().enabled
    }

    /// Offer a suggestion to the user, if the guardrail allows it.
    ///
    /// Returns the suppression when it does not — the caller logs it and moves on.
    /// A surfaced suggestion is recorded *before* it reaches the UI, so a crash
    /// between the two cannot spend a cap slot twice or show the same thing again.
    pub async fn propose(&self, suggestion: Suggestion) -> Result<(), Suppression> {
        let config = (self.config)();
        let mut log = self.log.lock().await;
        guardrail::check(
            config.enabled,
            &config.settings,
            Local::now(),
            &suggestion.key,
            &suggestion.source,
            log.entries(),
        )?;

        let entry = LogEntry {
            at: Utc::now(),
            event: LogEvent::Surfaced {
                id: suggestion.id.clone(),
                key: suggestion.key.clone(),
                source: suggestion.source.clone(),
                headline: suggestion.headline.clone(),
            },
        };
        if let Err(error) = log.record(entry) {
            // The log is the rate limiter's memory. If it cannot be written, the
            // cap cannot be enforced — so do not surface. Failing loud (staying
            // quiet) beats a character that can talk without limit.
            tracing::error!(%error, "could not record the surfacing; staying quiet");
            return Err(Suppression::RateCap {
                max: config.settings.max_per_hour,
            });
        }
        drop(log);

        self.pending
            .lock()
            .await
            .insert(suggestion.id.clone(), suggestion.clone());
        tracing::info!(id = %suggestion.id, key = %suggestion.key, "the character is speaking first");
        (self.sink)(suggestion);
        Ok(())
    }

    /// Record the user's answer. `accepted` is Accept; `false` is "Not now".
    ///
    /// Returns the suggestion that was answered, so the caller can act on an Accept
    /// (open the thread it points at). An id that is not pending — a double-click, a
    /// stale popup after a restart — is not an error and records nothing.
    pub async fn respond(&self, id: &str, accepted: bool) -> Option<Suggestion> {
        let suggestion = self.pending.lock().await.remove(id)?;
        let event = if accepted {
            LogEvent::Accepted {
                id: suggestion.id.clone(),
                key: suggestion.key.clone(),
                source: suggestion.source.clone(),
            }
        } else {
            LogEvent::Dismissed {
                id: suggestion.id.clone(),
                key: suggestion.key.clone(),
                source: suggestion.source.clone(),
            }
        };
        if let Err(error) = self.log.lock().await.record(LogEntry {
            at: Utc::now(),
            event,
        }) {
            // A lost "Not now" means the character asks again. Worth a loud line.
            tracing::error!(%error, id, accepted, "could not record the answer to a suggestion");
        }
        Some(suggestion)
    }

    /// Whether a surfacing would currently pass the guardrail, recording
    /// nothing. Watchers (7d) ask this *before* spending an LLM turn on a
    /// prompt whose answer could never be shown; [`AmbientService::propose`]
    /// still re-checks and records when the answer arrives.
    pub async fn would_allow(&self, key: &str, source: &str) -> Result<(), Suppression> {
        let config = (self.config)();
        guardrail::check(
            config.enabled,
            &config.settings,
            Local::now(),
            key,
            source,
            self.log.lock().await.entries(),
        )
    }

    /// Distinct `source` values carrying an `Accepted` entry that starts with
    /// `prefix` — e.g. every `reflection:<name>` the user ever said yes to.
    /// The log is the only durable record of consent, so the self-learned cap
    /// counts from here (intersected with the disk by the caller).
    pub async fn accepted_sources_with_prefix(
        &self,
        prefix: &str,
    ) -> std::collections::HashSet<String> {
        self.log
            .lock()
            .await
            .entries()
            .iter()
            .filter_map(|entry| match &entry.event {
                LogEvent::Accepted { source, .. } if source.starts_with(prefix) => {
                    Some(source.clone())
                }
                _ => None,
            })
            .collect()
    }

    /// Ask the agent something on the ambient thread and return its reply.
    ///
    /// This is the app talking to the agent on its own initiative — a reflection
    /// turn (7b), a skill review (7c). It rides the same contract as every other
    /// turn: send, watch the run to terminal, then read the reply off the timeline,
    /// because the reply text is never on the event stream
    /// (`docs/desktop/chat-rendering.md`).
    pub async fn ask(&self, thread_id: &ThreadId, prompt: &str) -> Option<String> {
        match drive_turn(&self.client, thread_id, prompt).await {
            TurnResult::Reply(text) => Some(text),
            TurnResult::NothingToSpeak | TurnResult::SendFailed => None,
        }
    }
}

/// The longest body the bubble carries. The full answer stays in the thread,
/// which Accept opens.
pub(crate) const MAX_BODY: usize = 400;

/// Shorten a reply to something a speech bubble can hold.
pub(crate) fn summarize(reply: &str) -> String {
    let trimmed = reply.trim();
    if trimmed.chars().count() <= MAX_BODY {
        return trimmed.to_string();
    }
    let short: String = trimmed.chars().take(MAX_BODY).collect();
    format!("{}…", short.trim_end())
}

/// The ambient thread: the conversation the *app* starts.
///
/// Separate from the chat thread on purpose — a reflection turn the user never
/// asked for must not appear in the middle of their transcript, and the widget's
/// event pump would otherwise render its status as if the user were waiting on it.
///
/// `saved` is the id from the last launch. Threads outlive the gateway process, so
/// it is reused when it still resolves; a wiped store (or a thread from another
/// machine's settings) yields a fresh one rather than a permanently broken ambient
/// mode.
pub async fn ensure_thread(client: &GatewayClient, saved: Option<&str>) -> Option<ThreadId> {
    if let Some(id) = saved {
        match ThreadId::new(id) {
            Ok(thread_id) => match client.timeline(&thread_id, Some(1)).await {
                Ok(_) => return Some(thread_id),
                Err(error) => tracing::info!(
                    %error,
                    "the saved ambient thread is gone; opening a new one"
                ),
            },
            Err(error) => tracing::warn!(%error, "the saved ambient thread id is not valid"),
        }
    }
    match client.create_thread().await {
        Ok(thread_id) => {
            tracing::info!(%thread_id, "opened the ambient thread");
            Some(thread_id)
        }
        Err(error) => {
            tracing::warn!(%error, "could not open the ambient thread");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::QuietHours;
    use std::sync::Mutex as StdMutex;

    fn service(
        enabled: bool,
        settings: AmbientSettings,
    ) -> (
        Arc<AmbientService>,
        Arc<StdMutex<Vec<Suggestion>>>,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = SurfacingLog::open(dir.path().join("ambient-log.jsonl")).expect("log");
        let shown = Arc::new(StdMutex::new(Vec::new()));
        let sink_shown = Arc::clone(&shown);
        let service = AmbientService::new(
            GatewayClient::new("http://127.0.0.1:1", "token").expect("client"),
            Arc::new(move || AmbientConfig {
                enabled,
                settings: settings.clone(),
            }),
            Arc::new(move |suggestion| {
                sink_shown
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(suggestion)
            }),
            log,
        );
        (Arc::new(service), shown, dir)
    }

    fn suggestion(key: &str) -> Suggestion {
        Suggestion {
            id: format!("s-{key}"),
            kind: SuggestionKind::Automation,
            key: key.into(),
            source: "automation:a".into(),
            headline: "Nightly digest just ran".into(),
            body: "3 new things".into(),
            thread_id: None,
        }
    }

    fn loud() -> AmbientSettings {
        AmbientSettings {
            max_per_hour: 2,
            quiet_hours: None,
            thread_id: None,
        }
    }

    #[tokio::test]
    async fn a_suggestion_reaches_the_sink_and_is_recorded() {
        let (service, shown, _dir) = service(true, loud());
        service.propose(suggestion("k1")).await.expect("allowed");
        assert_eq!(shown.lock().expect("lock").len(), 1);
        assert_eq!(service.log.lock().await.entries().len(), 1);
    }

    #[tokio::test]
    async fn with_ambient_off_nothing_is_shown_and_nothing_is_written() {
        let (service, shown, _dir) = service(false, loud());
        assert_eq!(
            service.propose(suggestion("k1")).await,
            Err(Suppression::Disabled)
        );
        assert!(shown.lock().expect("lock").is_empty());
        assert!(service.log.lock().await.entries().is_empty());
    }

    #[tokio::test]
    async fn the_cap_holds_across_calls() {
        let (service, shown, _dir) = service(true, loud());
        service.propose(suggestion("k1")).await.expect("first");
        service.propose(suggestion("k2")).await.expect("second");
        assert_eq!(
            service.propose(suggestion("k3")).await,
            Err(Suppression::RateCap { max: 2 })
        );
        assert_eq!(shown.lock().expect("lock").len(), 2);
    }

    #[tokio::test]
    async fn not_now_is_recorded_against_the_source_and_quiets_it() {
        let (service, _shown, _dir) = service(true, loud());
        service.propose(suggestion("k1")).await.expect("first");
        let answered = service.respond("s-k1", false).await.expect("pending");
        assert_eq!(answered.key, "k1");

        // A *different* run of the same automation is now suppressed — that is what
        // "Not now" has to mean, or the next fire asks again a minute later.
        assert_eq!(
            service.propose(suggestion("k2")).await,
            Err(Suppression::Dismissed)
        );
    }

    #[tokio::test]
    async fn answering_a_suggestion_twice_records_once() {
        let (service, _shown, _dir) = service(true, loud());
        service.propose(suggestion("k1")).await.expect("first");
        assert!(service.respond("s-k1", true).await.is_some());
        assert!(
            service.respond("s-k1", true).await.is_none(),
            "a second answer to the same popup is not a second decision"
        );
        // Surfaced + Accepted, and no duplicate.
        assert_eq!(service.log.lock().await.entries().len(), 2);
    }

    #[test]
    fn a_long_reply_is_cut_to_a_bubble() {
        let long = "x".repeat(MAX_BODY + 50);
        let short = summarize(&long);
        assert_eq!(short.chars().count(), MAX_BODY + 1, "cut plus an ellipsis");
        assert!(short.ends_with('…'));
        assert_eq!(summarize("  hello  "), "hello");
    }

    #[tokio::test]
    async fn quiet_hours_are_enforced_at_the_service_edge() {
        // Quiet for the hour the test is actually running in, so it holds whenever
        // it runs — the service reads the local clock, and a fixed window would pass
        // or fail depending on the time of day.
        use chrono::Timelike as _;
        let hour = Local::now().hour();
        let quiet_now = AmbientSettings {
            quiet_hours: Some(QuietHours {
                start_hour: hour,
                end_hour: (hour + 1) % 24,
            }),
            ..loud()
        };
        let (service, shown, _dir) = service(true, quiet_now);
        assert_eq!(
            service.propose(suggestion("k1")).await,
            Err(Suppression::QuietHours)
        );
        assert!(shown.lock().expect("lock").is_empty());
    }
}
