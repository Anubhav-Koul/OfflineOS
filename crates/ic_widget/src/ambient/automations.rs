//! Watching scheduled automations, and surfacing the ones that just ran.
//!
//! ## What the gateway does and does not tell us
//!
//! Verified on 2026-07-14 by driving a real fire through a running
//! `ironclaw-reborn serve` (`ic_integration_tests::ambient_surfacing`), not read
//! off the types:
//!
//! - `GET /automations` is a **schedule list**. A row's `last_run_at` and
//!   `last_status` move when a fire completes — that transition is the only "a run
//!   finished" signal the HTTP API has.
//! - The run itself lands in a **brand-new thread**, one per fire, owned by the same
//!   caller scope. It appears in `GET /threads`; its timeline holds the trigger's
//!   prompt and the agent's answer; its projection SSE works. All ordinary.
//! - **Nothing joins the two.** The automation row carries no `thread_id` and no
//!   `run_id`, and the thread carries no automation id. (`TriggerRecord` *has* an
//!   `active_run_ref`; `trigger_output` does not emit it.)
//!
//! So the correlation is timing, and it is honest about its limits: between two
//! polls, if exactly one automation completed and exactly one thread appeared, they
//! are the same event. If two automations fire into the same window, the suggestion
//! still surfaces — with the automation's name and status, and no body. A wrong body
//! would be worse than none.
//!
//! ## Priming
//!
//! The first tick records what it sees and surfaces **nothing**. A run that finished
//! while the app was closed is not news, and greeting the user with last night's
//! digest every morning is exactly the kind of thing that gets a companion switched
//! off for good.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use uuid::Uuid;

use crate::gateway_client::{Automation, AutomationRunStatus, ThreadId};

use super::{AmbientService, Suggestion};

/// How often the automations are polled. The gateway's own minimum fire cadence is
/// 60 s, so a 30 s poll cannot miss a fire it could have caught.
pub const POLL_INTERVAL: Duration = Duration::from_secs(30);

/// The longest body the bubble carries. The full answer stays in the thread, which
/// Accept opens.
const MAX_BODY: usize = 400;

/// What the watcher remembers between ticks.
///
/// Public and tickable one poll at a time so the gate test can drive it against a
/// **real** trigger fire on a running gateway, rather than against a fake that
/// agrees with us. See `ic_integration_tests/tests/ambient_surfacing.rs`.
#[derive(Debug, Default)]
pub struct AutomationWatch {
    /// automation id → the `last_run_at` we have already accounted for.
    runs: HashMap<String, String>,
    /// Every thread that existed at the last tick.
    threads: HashSet<String>,
    /// Whether the first tick has run. Until it has, nothing surfaces.
    primed: bool,
}

impl AutomationWatch {
    /// A watch that has seen nothing yet. Its first [`tick`](Self::tick) primes.
    pub fn new() -> Self {
        Self::default()
    }

    /// One poll: read the automations and the threads, surface what is new.
    pub async fn tick(&mut self, service: &AmbientService) -> crate::error::Result<()> {
        tick(service, self).await
    }
}

/// The automations whose `last_run_at` moved since the baseline.
///
/// Pure, so the "is this news?" rule is a unit test rather than a wait. A row with
/// no `last_run_at` has never run and is not news; a row seen for the first time
/// *after* priming (a schedule the user just created that has already fired) is.
fn newly_completed<'a>(
    baseline: &AutomationWatch,
    current: &'a [Automation],
) -> Vec<&'a Automation> {
    current
        .iter()
        .filter(|automation| {
            let Some(last_run) = automation.last_run_at.as_deref() else {
                return false;
            };
            baseline
                .runs
                .get(&automation.automation_id)
                .map(String::as_str)
                != Some(last_run)
        })
        .collect()
}

/// The suggestion an automation's completed run becomes.
fn suggestion_for(automation: &Automation, last_run: &str, body: Option<String>) -> Suggestion {
    let failed = automation.last_status == Some(AutomationRunStatus::Error);
    let headline = if failed {
        format!("{} ran into a problem", automation.name)
    } else {
        format!("{} just ran", automation.name)
    };
    let body = body.unwrap_or_else(|| {
        if failed {
            "It did not finish. Open it to see how far it got.".to_string()
        } else {
            "It finished. Open it to see what it did.".to_string()
        }
    });
    Suggestion {
        id: Uuid::new_v4().to_string(),
        key: format!("automation:{}:{last_run}", automation.automation_id),
        source: format!("automation:{}", automation.automation_id),
        headline,
        body,
        thread_id: None,
    }
}

/// Shorten a reply to something a speech bubble can hold.
fn summarize(reply: &str) -> String {
    let trimmed = reply.trim();
    if trimmed.chars().count() <= MAX_BODY {
        return trimmed.to_string();
    }
    let short: String = trimmed.chars().take(MAX_BODY).collect();
    format!("{}…", short.trim_end())
}

/// Poll the gateway's automations forever, surfacing each completed run once.
///
/// Runs for as long as ambient mode is on; the caller aborts the task when it is
/// switched off. Every gateway error is a warning and a retry — a poll that fails
/// because the gateway is restarting must not kill the watcher.
pub async fn watch(service: Arc<AmbientService>) {
    let mut baseline = AutomationWatch::new();
    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        ticker.tick().await;
        if !service.enabled() {
            // The toggle went off under us: stop watching, but leave the baseline —
            // if it comes back on in this process, the runs we already saw are still
            // not news.
            continue;
        }
        if let Err(error) = tick(&service, &mut baseline).await {
            tracing::warn!(%error, "the automation watch could not read the gateway");
        }
    }
}

/// One poll. Split out from the loop so a test can drive it a tick at a time.
async fn tick(
    service: &AmbientService,
    baseline: &mut AutomationWatch,
) -> crate::error::Result<()> {
    let client = service.client();
    let automations = client.list_automations(None).await?;
    let threads: HashSet<String> = client
        .list_threads(None)
        .await?
        .into_iter()
        .map(|thread| thread.thread_id.to_string())
        .collect();

    let completed = newly_completed(baseline, &automations);

    // Threads that appeared since the last tick. Exactly one, alongside exactly one
    // completed automation, is the only case where the pairing is certain.
    let fresh: Vec<String> = threads.difference(&baseline.threads).cloned().collect();

    if !baseline.primed {
        baseline.primed = true;
        remember(baseline, &automations, threads);
        tracing::debug!(
            automations = automations.len(),
            "the automation watch is primed; earlier runs are not news"
        );
        return Ok(());
    }

    for automation in &completed {
        let Some(last_run) = automation.last_run_at.as_deref() else {
            continue;
        };
        let body = match (completed.len(), fresh.as_slice()) {
            (1, [thread]) => reply_of(service, thread).await,
            _ => None,
        };
        let mut suggestion = suggestion_for(automation, last_run, body);
        if let (1, [thread]) = (completed.len(), fresh.as_slice()) {
            suggestion.thread_id = Some(thread.clone());
        }
        match service.propose(suggestion).await {
            Ok(()) => {}
            Err(suppression) => tracing::debug!(
                automation = %automation.name,
                reason = suppression.as_str(),
                "a completed automation was not surfaced"
            ),
        }
    }

    remember(baseline, &automations, threads);
    Ok(())
}

/// Fold this tick's observations into the baseline.
fn remember(baseline: &mut AutomationWatch, automations: &[Automation], threads: HashSet<String>) {
    for automation in automations {
        if let Some(last_run) = automation.last_run_at.clone() {
            baseline
                .runs
                .insert(automation.automation_id.clone(), last_run);
        }
    }
    baseline.threads = threads;
}

/// The agent's answer in a fired run's thread, if it left one.
async fn reply_of(service: &AmbientService, thread_id: &str) -> Option<String> {
    let thread_id = ThreadId::new(thread_id).ok()?;
    match service.client().timeline(&thread_id, Some(10)).await {
        Ok(timeline) => timeline
            .latest_assistant_reply()
            .and_then(|message| message.content.as_deref())
            .map(summarize)
            .filter(|body| !body.is_empty()),
        Err(error) => {
            tracing::debug!(%error, "could not read a fired automation's reply");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway_client::AutomationState;

    fn automation(id: &str, last_run: Option<&str>) -> Automation {
        Automation {
            automation_id: id.into(),
            name: format!("{id} digest"),
            state: AutomationState::Scheduled,
            next_run_at: None,
            last_run_at: last_run.map(str::to_string),
            last_status: last_run.map(|_| AutomationRunStatus::Ok),
            is_active: true,
        }
    }

    #[test]
    fn a_schedule_that_has_never_run_is_not_news() {
        let baseline = AutomationWatch::new();
        let rows = [automation("a", None)];
        assert!(newly_completed(&baseline, &rows).is_empty());
    }

    #[test]
    fn a_run_is_news_once_and_then_it_is_not() {
        let mut baseline = AutomationWatch {
            primed: true,
            ..AutomationWatch::new()
        };
        let rows = [automation("a", Some("2026-07-13T21:07:00Z"))];

        assert_eq!(newly_completed(&baseline, &rows).len(), 1);
        remember(&mut baseline, &rows, HashSet::new());
        assert!(
            newly_completed(&baseline, &rows).is_empty(),
            "the same run must not surface on every poll"
        );

        // The next fire moves `last_run_at`, and that is news again.
        let next = [automation("a", Some("2026-07-13T21:08:00Z"))];
        assert_eq!(newly_completed(&baseline, &next).len(), 1);
    }

    #[test]
    fn a_failed_run_says_so() {
        let mut failed = automation("a", Some("t1"));
        failed.last_status = Some(AutomationRunStatus::Error);
        let suggestion = suggestion_for(&failed, "t1", None);
        assert!(suggestion.headline.contains("problem"), "{suggestion:?}");
        assert_eq!(suggestion.key, "automation:a:t1");
        assert_eq!(suggestion.source, "automation:a");
    }

    #[test]
    fn the_key_changes_per_run_but_the_source_does_not() {
        // Why it matters: the key is what stops one run being shown twice; the
        // source is what a "Not now" quiets. Sharing them would mean a dismissal
        // silenced the automation forever, or never.
        let first = suggestion_for(&automation("a", Some("t1")), "t1", None);
        let second = suggestion_for(&automation("a", Some("t2")), "t2", None);
        assert_ne!(first.key, second.key);
        assert_eq!(first.source, second.source);
    }

    #[test]
    fn a_long_reply_is_cut_to_a_bubble() {
        let long = "x".repeat(MAX_BODY + 50);
        let short = summarize(&long);
        assert_eq!(short.chars().count(), MAX_BODY + 1, "cut plus an ellipsis");
        assert!(short.ends_with('…'));
        assert_eq!(summarize("  hello  "), "hello");
    }
}
