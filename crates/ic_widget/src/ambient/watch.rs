//! Ambient watchers (Phase 7d): user-defined "when X happens, ask the agent to
//! Y" rules over three opt-in signals — the foreground window's title, watched
//! folders, and the local clock.
//!
//! The v1 gate is **rule-based, not LLM-based**, by design: on iGPU machines a
//! constant "is this worth suggesting?" model competes with the chat model (the
//! Phase 3 perf rule), so the only inference a watcher ever runs is the prompt
//! the user themselves wrote, and only after its rule fired *and* the guardrail
//! said the answer could be shown.
//!
//! The engine is a pure state machine the app feeds signals into, so every
//! firing rule is a unit test rather than a wait:
//!
//! - **Edges, not levels.** A foreground rule fires when the title *starts*
//!   matching — a match that has been true for an hour is state, not an event.
//!   The first sample only primes.
//! - **Once a day, and never for the past.** A time rule fires when the clock
//!   crosses its mark; a rule whose mark already passed when the app started is
//!   primed as spent — greeting the user at 3 pm with their 9 am rule is how a
//!   companion gets switched off.
//! - **A per-rule cooldown** (30 min) bounds every kind, so one noisy rule — a
//!   folder mid-build, a window flickering in and out of focus — cannot spend
//!   the whole hourly guardrail cap by itself.
//!
//! A firing then takes the same road as every other proactive thing: a fresh
//! thread (like the gateway's own trigger fires — the ambient thread stays the
//! app's private conversation), one turn, and a guardrailed suggestion whose
//! Accept opens the thread. Nothing is captured but a title, a path, and the
//! time; nothing leaves the machine.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Local, NaiveDate, Timelike};
use uuid::Uuid;

use crate::settings::{WatchRule, WatchTrigger, WatcherSettings};
use crate::voice::{TurnResult, drive_turn};

use super::{AmbientService, Suggestion, SuggestionKind, summarize};

/// How long a rule stays quiet after firing, whatever its kind.
pub const RULE_COOLDOWN: Duration = Duration::from_secs(30 * 60);

/// One observation the app feeds the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signal {
    /// The foreground window's current title (sampled; empty when none).
    Foreground(String),
    /// Something under a watched folder changed, at this path.
    FolderEvent(std::path::PathBuf),
    /// A heartbeat for the time rules; carries no data, the clock does.
    Tick,
}

/// A rule that fired: everything needed to run its prompt and surface the
/// answer, with the guardrail identities already minted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Firing {
    /// The rule that fired.
    pub rule_id: String,
    /// This exact fire — never shown twice.
    pub key: String,
    /// The rule as a source — what a "Not now" quiets.
    pub source: String,
    /// The user's prompt, materialized verbatim.
    pub prompt: String,
    /// The bubble headline: what happened, in the trigger's own words.
    pub headline: String,
}

/// The watchers' memory between signals. Pure — time comes in as an argument.
#[derive(Debug, Default)]
pub struct WatchEngine {
    /// The previous foreground sample, for edge detection. `None` until the
    /// first sample, which only primes.
    last_title: Option<String>,
    /// rule id → when it last fired (the cooldown's memory).
    fired_at: HashMap<String, DateTime<Local>>,
    /// rule id → the day its time trigger last fired (or was primed as spent).
    time_spent_on: HashMap<String, NaiveDate>,
    /// Whether the first tick has primed the time rules.
    time_primed: bool,
}

impl WatchEngine {
    /// An engine that has seen nothing. Its first samples prime.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one signal; get every rule that fires on it.
    ///
    /// Both switches are honoured here — the kind toggle and the rule's own —
    /// even though the caller usually gates too: an engine that trusts its
    /// caller is one settings refactor away from watching something the user
    /// switched off.
    pub fn observe(
        &mut self,
        settings: &WatcherSettings,
        signal: &Signal,
        now: DateTime<Local>,
    ) -> Vec<Firing> {
        let mut firings = Vec::new();
        match signal {
            Signal::Foreground(title) => {
                if settings.foreground_enabled {
                    self.observe_foreground(settings, title, now, &mut firings);
                }
                // The sample is remembered even when no rule uses it, so a rule
                // enabled later starts from the truth, not from `None`.
                self.last_title = Some(title.clone());
            }
            Signal::FolderEvent(path) => {
                if settings.folders_enabled {
                    self.observe_folder(settings, path, now, &mut firings);
                }
            }
            Signal::Tick => {
                if settings.time_enabled {
                    self.observe_tick(settings, now, &mut firings);
                }
            }
        }
        firings
    }

    fn observe_foreground(
        &mut self,
        settings: &WatcherSettings,
        title: &str,
        now: DateTime<Local>,
        firings: &mut Vec<Firing>,
    ) {
        // The first sample primes: the app just started, and whatever is
        // already in front is state, not news.
        let Some(previous) = self.last_title.as_deref() else {
            return;
        };
        let title_lower = title.to_lowercase();
        let previous_lower = previous.to_lowercase();
        for rule in enabled(settings) {
            let WatchTrigger::ForegroundApp { title_contains } = &rule.trigger else {
                continue;
            };
            let needle = title_contains.trim().to_lowercase();
            if needle.is_empty() {
                continue; // an empty needle would match every window, always
            }
            let matches_now = title_lower.contains(&needle);
            let matched_before = previous_lower.contains(&needle);
            if matches_now && !matched_before {
                self.fire(rule, now, firings);
            }
        }
    }

    fn observe_folder(
        &mut self,
        settings: &WatcherSettings,
        event_path: &Path,
        now: DateTime<Local>,
        firings: &mut Vec<Firing>,
    ) {
        for rule in enabled(settings) {
            let WatchTrigger::FolderChanged { path } = &rule.trigger else {
                continue;
            };
            if event_path.starts_with(path) {
                self.fire(rule, now, firings);
            }
        }
    }

    fn observe_tick(
        &mut self,
        settings: &WatcherSettings,
        now: DateTime<Local>,
        firings: &mut Vec<Firing>,
    ) {
        let today = now.date_naive();
        if !self.time_primed {
            // A mark that already passed before the app was watching is spent:
            // firing a 9 am rule at 3 pm because the app just launched is
            // exactly the stale greeting the automations watcher also refuses.
            self.time_primed = true;
            for rule in enabled(settings) {
                if let WatchTrigger::TimeOfDay { hour, minute } = rule.trigger
                    && past_mark(now, hour, minute)
                {
                    self.time_spent_on.insert(rule.id.clone(), today);
                }
            }
            return;
        }
        let mut due = Vec::new();
        for rule in enabled(settings) {
            let WatchTrigger::TimeOfDay { hour, minute } = rule.trigger else {
                continue;
            };
            if past_mark(now, hour, minute) && self.time_spent_on.get(&rule.id) != Some(&today) {
                due.push(rule.clone());
            }
        }
        for rule in due {
            self.time_spent_on.insert(rule.id.clone(), today);
            self.fire(&rule, now, firings);
        }
    }

    /// Apply the cooldown, then mint the firing.
    fn fire(&mut self, rule: &WatchRule, now: DateTime<Local>, firings: &mut Vec<Firing>) {
        if let Some(last) = self.fired_at.get(&rule.id)
            && now
                .signed_duration_since(*last)
                .to_std()
                .unwrap_or_default()
                < RULE_COOLDOWN
        {
            tracing::debug!(rule = %rule.id, "a watch rule fired inside its cooldown; staying quiet");
            return;
        }
        self.fired_at.insert(rule.id.clone(), now);
        firings.push(Firing {
            rule_id: rule.id.clone(),
            key: format!("watch:{}:{}", rule.id, now.to_rfc3339()),
            source: format!("watch:{}", rule.id),
            prompt: rule.prompt.clone(),
            headline: format!("Noticed {}", rule.trigger.describe()),
        });
    }
}

fn enabled(settings: &WatcherSettings) -> impl Iterator<Item = &WatchRule> {
    settings.rules.iter().filter(|rule| rule.enabled)
}

fn past_mark(now: DateTime<Local>, hour: u32, minute: u32) -> bool {
    (now.hour(), now.minute()) >= (hour, minute)
}

/// Run one firing to the bubble: guardrail pre-check, a fresh thread, one turn,
/// and a guardrailed suggestion pointing at that thread.
///
/// The pre-check is what keeps a suppressed watcher *cheap*: no thread and no
/// LLM turn are spent on an answer quiet hours would swallow. [`AmbientService::
/// propose`] still re-checks and records when the answer arrives — the window
/// between the two is real but only ever costs one wasted turn, never a
/// surfacing past the cap.
pub async fn run_rule_fire(service: &AmbientService, firing: &Firing) {
    if let Err(suppression) = service.would_allow(&firing.key, &firing.source).await {
        tracing::debug!(
            rule = %firing.rule_id,
            reason = suppression.as_str(),
            "a watch firing was suppressed before spending a turn"
        );
        return;
    }

    // A fresh thread per fire, like the gateway's own trigger fires: the
    // ambient thread stays the app's private conversation, and Accept gets a
    // transcript that contains exactly this rule's question and answer.
    let thread_id = match service.client().create_thread().await {
        Ok(thread_id) => thread_id,
        Err(error) => {
            tracing::warn!(%error, rule = %firing.rule_id, "a watch firing could not open a thread");
            return;
        }
    };
    let reply = match drive_turn(service.client(), &thread_id, &firing.prompt).await {
        TurnResult::Reply(text) => text,
        TurnResult::NothingToSpeak | TurnResult::SendFailed => {
            tracing::warn!(rule = %firing.rule_id, "a watch firing's turn produced no reply");
            return;
        }
    };

    let suggestion = Suggestion {
        id: Uuid::new_v4().to_string(),
        kind: SuggestionKind::Watcher,
        key: firing.key.clone(),
        source: firing.source.clone(),
        headline: firing.headline.clone(),
        body: summarize(&reply),
        thread_id: Some(thread_id.to_string()),
    };
    if let Err(suppression) = service.propose(suggestion).await {
        tracing::debug!(
            rule = %firing.rule_id,
            reason = suppression.as_str(),
            "a watch firing's answer arrived but was suppressed"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    fn at(hour: u32, minute: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 7, 14, hour, minute, 0)
            .single()
            .expect("a valid local time")
    }

    fn rule(id: &str, trigger: WatchTrigger) -> WatchRule {
        WatchRule {
            id: id.into(),
            enabled: true,
            trigger,
            prompt: format!("prompt for {id}"),
        }
    }

    fn settings(rules: Vec<WatchRule>) -> WatcherSettings {
        WatcherSettings {
            foreground_enabled: true,
            folders_enabled: true,
            time_enabled: true,
            rules,
        }
    }

    #[test]
    fn a_foreground_rule_fires_on_the_edge_not_the_state() {
        let settings = settings(vec![rule(
            "r1",
            WatchTrigger::ForegroundApp {
                title_contains: "Figma".into(),
            },
        )]);
        let mut engine = WatchEngine::new();

        // The first sample primes, even when it already matches.
        assert!(
            engine
                .observe(
                    &settings,
                    &Signal::Foreground("Figma — mockups".into()),
                    at(10, 0)
                )
                .is_empty(),
            "what was already in front at start is state, not news"
        );
        // Still in front: a level, not an edge.
        assert!(
            engine
                .observe(
                    &settings,
                    &Signal::Foreground("Figma — mockups".into()),
                    at(10, 1)
                )
                .is_empty()
        );
        // Away, then back: that is the edge.
        assert!(
            engine
                .observe(&settings, &Signal::Foreground("Terminal".into()), at(11, 0))
                .is_empty()
        );
        let firings = engine.observe(
            &settings,
            &Signal::Foreground("figma — other file".into()),
            at(11, 5),
        );
        assert_eq!(firings.len(), 1, "case-insensitive, on the transition");
        assert_eq!(firings[0].rule_id, "r1");
        assert!(firings[0].headline.contains("Figma"));
    }

    #[test]
    fn the_cooldown_holds_across_kinds() {
        let settings = settings(vec![rule(
            "r1",
            WatchTrigger::FolderChanged {
                path: "C:\\drop".into(),
            },
        )]);
        let mut engine = WatchEngine::new();
        let event = Signal::FolderEvent("C:\\drop\\new.pdf".into());

        assert_eq!(engine.observe(&settings, &event, at(10, 0)).len(), 1);
        assert!(
            engine.observe(&settings, &event, at(10, 5)).is_empty(),
            "a folder mid-download must not fire every write"
        );
        assert_eq!(
            engine.observe(&settings, &event, at(10, 31)).len(),
            1,
            "the cooldown ends"
        );
    }

    #[test]
    fn a_folder_rule_only_matches_under_its_path() {
        let settings = settings(vec![rule(
            "r1",
            WatchTrigger::FolderChanged {
                path: "C:\\drop".into(),
            },
        )]);
        let mut engine = WatchEngine::new();
        assert!(
            engine
                .observe(
                    &settings,
                    &Signal::FolderEvent("C:\\elsewhere\\file".into()),
                    at(10, 0)
                )
                .is_empty()
        );
    }

    #[test]
    fn a_time_rule_fires_once_a_day_and_never_for_the_past() {
        let settings = settings(vec![rule(
            "morning",
            WatchTrigger::TimeOfDay { hour: 9, minute: 0 },
        )]);
        let mut engine = WatchEngine::new();

        // First tick at 15:00: the 9:00 mark already passed — primed as spent.
        assert!(
            engine
                .observe(&settings, &Signal::Tick, at(15, 0))
                .is_empty(),
            "a mark that passed before the app watched is stale, not due"
        );
        assert!(
            engine
                .observe(&settings, &Signal::Tick, at(16, 0))
                .is_empty()
        );

        // The next day, the mark crosses while watching: one fire.
        let tomorrow = at(9, 1) + chrono::Duration::days(1);
        assert_eq!(engine.observe(&settings, &Signal::Tick, tomorrow).len(), 1);
        let later = at(12, 0) + chrono::Duration::days(1);
        assert!(
            engine.observe(&settings, &Signal::Tick, later).is_empty(),
            "once per day"
        );
    }

    #[test]
    fn a_time_rule_still_due_at_priming_waits_for_its_mark() {
        let settings = settings(vec![rule(
            "evening",
            WatchTrigger::TimeOfDay {
                hour: 18,
                minute: 0,
            },
        )]);
        let mut engine = WatchEngine::new();
        assert!(
            engine
                .observe(&settings, &Signal::Tick, at(15, 0))
                .is_empty()
        );
        assert!(
            engine
                .observe(&settings, &Signal::Tick, at(17, 59))
                .is_empty()
        );
        assert_eq!(
            engine.observe(&settings, &Signal::Tick, at(18, 0)).len(),
            1,
            "a mark still ahead at priming fires when it arrives"
        );
    }

    #[test]
    fn switched_off_kinds_and_paused_rules_never_fire() {
        let mut all_off = settings(vec![
            rule(
                "fg",
                WatchTrigger::ForegroundApp {
                    title_contains: "Figma".into(),
                },
            ),
            rule(
                "folder",
                WatchTrigger::FolderChanged {
                    path: "C:\\drop".into(),
                },
            ),
        ]);
        all_off.foreground_enabled = false;
        all_off.folders_enabled = false;

        let mut engine = WatchEngine::new();
        engine.observe(&all_off, &Signal::Foreground("Terminal".into()), at(10, 0));
        assert!(
            engine
                .observe(&all_off, &Signal::Foreground("Figma".into()), at(10, 1))
                .is_empty(),
            "the kind switch is honoured in the engine, not only the caller"
        );
        assert!(
            engine
                .observe(
                    &all_off,
                    &Signal::FolderEvent("C:\\drop\\f".into()),
                    at(10, 2)
                )
                .is_empty()
        );

        let mut paused = settings(vec![WatchRule {
            enabled: false,
            ..rule(
                "folder",
                WatchTrigger::FolderChanged {
                    path: "C:\\drop".into(),
                },
            )
        }]);
        paused.folders_enabled = true;
        assert!(
            engine
                .observe(
                    &paused,
                    &Signal::FolderEvent("C:\\drop\\f".into()),
                    at(10, 3)
                )
                .is_empty(),
            "a paused rule is kept but silent"
        );
    }

    #[test]
    fn an_empty_needle_matches_nothing() {
        let settings = settings(vec![rule(
            "r1",
            WatchTrigger::ForegroundApp {
                title_contains: "  ".into(),
            },
        )]);
        let mut engine = WatchEngine::new();
        engine.observe(&settings, &Signal::Foreground("a".into()), at(10, 0));
        assert!(
            engine
                .observe(&settings, &Signal::Foreground("b".into()), at(10, 1))
                .is_empty(),
            "an empty needle would fire on every window change"
        );
    }

    #[test]
    fn the_key_is_per_fire_and_the_source_is_per_rule() {
        let settings = settings(vec![rule(
            "r1",
            WatchTrigger::FolderChanged {
                path: "C:\\drop".into(),
            },
        )]);
        let mut engine = WatchEngine::new();
        let event = Signal::FolderEvent("C:\\drop\\a".into());
        let first = engine.observe(&settings, &event, at(10, 0)).remove(0);
        let second = engine.observe(&settings, &event, at(11, 0)).remove(0);
        assert_ne!(first.key, second.key, "each fire is shown once, ever");
        assert_eq!(first.source, second.source, "a Not-now quiets the rule");
    }
}
