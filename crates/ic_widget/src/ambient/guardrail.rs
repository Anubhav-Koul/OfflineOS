//! What is allowed to interrupt, and how often.
//!
//! Every unsolicited surfacing passes through [`check`] first. It is pure: the
//! clock, the settings, and the history come in as arguments, so the rules are
//! unit-tested rather than observed by waiting an hour in front of the app.
//!
//! It **fails closed** — the four ways to be suppressed are checked before the one
//! way to be allowed, and a suggestion that has already been shown, or whose source
//! the user has just waved away, does not come back.

use std::time::Duration;

use chrono::{DateTime, TimeZone, Timelike, Utc};

use crate::settings::AmbientSettings;

use super::log::{LogEntry, LogEvent};

/// How long a "Not now" quiets the *source* it was aimed at.
///
/// Not forever: the source is a standing thing (a scheduled automation fires again
/// tomorrow), and "not now" means *now*, not "never". Not a minute either — a
/// dismissal the character ignores on its next tick is worse than no dismissal.
pub const DISMISS_COOLDOWN: Duration = Duration::from_secs(60 * 60);

/// Why the character stayed quiet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Suppression {
    /// Ambient mode is off. This is the default, and it is not an error.
    Disabled,
    /// It is the middle of the night (or whatever window the user set).
    QuietHours,
    /// This exact suggestion has already been shown once.
    AlreadySurfaced,
    /// The user waved this source away recently.
    Dismissed,
    /// The hourly cap is spent.
    RateCap {
        /// The cap that was hit.
        max: u32,
    },
}

impl Suppression {
    /// A short reason, for the log.
    pub fn as_str(&self) -> &'static str {
        match self {
            Suppression::Disabled => "ambient mode is off",
            Suppression::QuietHours => "quiet hours",
            Suppression::AlreadySurfaced => "already surfaced",
            Suppression::Dismissed => "the user dismissed this source recently",
            Suppression::RateCap { .. } => "the hourly cap is spent",
        }
    }
}

/// May the character surface `key` (from `source`) right now?
///
/// `now` carries its own zone: quiet hours are a *local* thing (the user's night),
/// while the rate window is elapsed time, so both readings come from one instant.
pub fn check<Tz: TimeZone>(
    enabled: bool,
    settings: &AmbientSettings,
    now: DateTime<Tz>,
    key: &str,
    source: &str,
    history: &[LogEntry],
) -> Result<(), Suppression> {
    if !enabled {
        return Err(Suppression::Disabled);
    }
    if let Some(quiet) = settings.quiet_hours
        && quiet.contains(now.hour())
    {
        return Err(Suppression::QuietHours);
    }

    let now_utc = now.with_timezone(&Utc);
    let hour_ago =
        now_utc - chrono::Duration::from_std(Duration::from_secs(3600)).unwrap_or_default();
    let cooldown = now_utc - chrono::Duration::from_std(DISMISS_COOLDOWN).unwrap_or_default();

    let mut surfaced_this_hour = 0u32;
    for entry in history {
        match &entry.event {
            // Shown once is shown. A suggestion the user ignored (never answered)
            // must not reappear on the next poll either — that is a nag, not a
            // reminder.
            LogEvent::Surfaced { key: seen, .. } if seen == key => {
                return Err(Suppression::AlreadySurfaced);
            }
            LogEvent::Surfaced { .. } if entry.at >= hour_ago => surfaced_this_hour += 1,
            LogEvent::Dismissed { source: quiet, .. }
                if quiet == source && entry.at >= cooldown =>
            {
                return Err(Suppression::Dismissed);
            }
            _ => {}
        }
    }

    if surfaced_this_hour >= settings.max_per_hour {
        return Err(Suppression::RateCap {
            max: settings.max_per_hour,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::QuietHours;

    fn at(hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 13, hour, minute, 0)
            .single()
            .expect("a real instant")
    }

    fn settings() -> AmbientSettings {
        AmbientSettings {
            max_per_hour: 2,
            quiet_hours: None,
            thread_id: None,
        }
    }

    fn surfaced(at: DateTime<Utc>, key: &str, source: &str) -> LogEntry {
        LogEntry {
            at,
            event: LogEvent::Surfaced {
                id: format!("id-{key}"),
                key: key.into(),
                source: source.into(),
                headline: "ran".into(),
            },
        }
    }

    fn dismissed(at: DateTime<Utc>, key: &str, source: &str) -> LogEntry {
        LogEntry {
            at,
            event: LogEvent::Dismissed {
                id: format!("id-{key}"),
                key: key.into(),
                source: source.into(),
            },
        }
    }

    #[test]
    fn off_is_the_default_and_nothing_gets_through_it() {
        assert_eq!(
            check(false, &settings(), at(12, 0), "k", "s", &[]),
            Err(Suppression::Disabled)
        );
    }

    #[test]
    fn a_first_suggestion_with_a_clean_history_is_allowed() {
        assert_eq!(check(true, &settings(), at(12, 0), "k", "s", &[]), Ok(()));
    }

    #[test]
    fn the_hourly_cap_counts_only_the_last_hour() {
        let history = vec![
            surfaced(at(11, 55), "k1", "s"),
            surfaced(at(11, 58), "k2", "s"),
        ];
        // Two in the last hour, cap of two.
        assert_eq!(
            check(true, &settings(), at(12, 0), "k3", "s", &history),
            Err(Suppression::RateCap { max: 2 })
        );
        // The same two, an hour and a bit later: the window has moved past them.
        assert_eq!(
            check(true, &settings(), at(13, 0), "k3", "s", &history),
            Ok(())
        );
    }

    #[test]
    fn the_same_suggestion_is_never_shown_twice() {
        let history = vec![surfaced(at(11, 0), "k1", "s")];
        assert_eq!(
            check(true, &settings(), at(12, 0), "k1", "s", &history),
            Err(Suppression::AlreadySurfaced)
        );
        // An hour old, so it does not count against the cap either — but the
        // dedupe still holds, which is the point: age does not make it new.
        assert_eq!(
            check(true, &settings(), at(12, 0), "k2", "s", &history),
            Ok(())
        );
    }

    #[test]
    fn not_now_quiets_that_source_for_the_cooldown_and_no_longer() {
        let history = vec![dismissed(at(11, 30), "k1", "automation:a")];
        assert_eq!(
            check(true, &settings(), at(12, 0), "k2", "automation:a", &history),
            Err(Suppression::Dismissed)
        );
        // A different source is unaffected — "not now" is not a mute button for
        // everything.
        assert_eq!(
            check(true, &settings(), at(12, 0), "k2", "automation:b", &history),
            Ok(())
        );
        // And it expires: the automation may speak again tomorrow.
        assert_eq!(
            check(true, &settings(), at(13, 0), "k2", "automation:a", &history),
            Ok(())
        );
    }

    #[test]
    fn quiet_hours_are_local_and_may_wrap_midnight() {
        let night = AmbientSettings {
            quiet_hours: Some(QuietHours {
                start_hour: 22,
                end_hour: 8,
            }),
            ..settings()
        };
        for hour in [22, 23, 0, 3, 7] {
            assert_eq!(
                check(true, &night, at(hour, 0), "k", "s", &[]),
                Err(Suppression::QuietHours),
                "{hour}:00 should be quiet"
            );
        }
        for hour in [8, 12, 21] {
            assert_eq!(
                check(true, &night, at(hour, 0), "k", "s", &[]),
                Ok(()),
                "{hour}:00 should be loud"
            );
        }
    }

    #[test]
    fn an_empty_quiet_window_silences_nothing() {
        // start == end must not read as "quiet all day", which would look exactly
        // like ambient mode being broken.
        let all_day = AmbientSettings {
            quiet_hours: Some(QuietHours {
                start_hour: 9,
                end_hour: 9,
            }),
            ..settings()
        };
        assert_eq!(check(true, &all_day, at(9, 0), "k", "s", &[]), Ok(()));
    }
}
