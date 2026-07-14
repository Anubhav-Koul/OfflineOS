//! The record of every time the character spoke first, and how that went.
//!
//! Append-only JSONL beside the settings. Two reasons it is a log and not a
//! counter:
//!
//! - The rate limiter, quiet hours, and "don't ask me that again" are all
//!   questions about *history*, and a counter can only answer one of them.
//! - `CLAUDE.md`'s inherited invariant: LLM data is never deleted by code. A
//!   "Not now" is the user's answer to the agent — it is retained and timestamped,
//!   never rewritten and never dropped. Only an explicit user-initiated wipe of the
//!   app's data removes it.
//!
//! A line that will not parse (a half-written record from a power cut, a field
//! from a newer build) is skipped on read and **left on disk**. Rewriting the file
//! to "clean" it would be the deletion this invariant forbids.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// One thing that happened, and when.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    /// When it happened, UTC.
    pub at: DateTime<Utc>,
    /// What happened.
    #[serde(flatten)]
    pub event: LogEvent,
}

/// The three things worth remembering about a surfacing.
///
/// `key` identifies this *exact* suggestion (so it is never shown twice); `source`
/// identifies where it came from (so a "Not now" can quiet that source for a while
/// without silencing everything).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum LogEvent {
    /// The character spoke first.
    Surfaced {
        /// This surfacing's id, unique per popup.
        id: String,
        /// The exact suggestion, e.g. `automation:01K…:2026-07-13T21:07:00Z`.
        key: String,
        /// Its origin, e.g. `automation:01K…`.
        source: String,
        /// What the bubble said, for the record.
        headline: String,
    },
    /// The user said yes.
    Accepted {
        /// The surfacing being answered.
        id: String,
        /// Its exact key.
        key: String,
        /// Its origin.
        source: String,
    },
    /// The user said "Not now". Feeds the rate limiter and quiets the source.
    Dismissed {
        /// The surfacing being answered.
        id: String,
        /// Its exact key.
        key: String,
        /// Its origin.
        source: String,
    },
}

impl LogEvent {
    /// The exact-suggestion key this event is about.
    pub fn key(&self) -> &str {
        match self {
            LogEvent::Surfaced { key, .. }
            | LogEvent::Accepted { key, .. }
            | LogEvent::Dismissed { key, .. } => key,
        }
    }

    /// The source this event is about.
    pub fn source(&self) -> &str {
        match self {
            LogEvent::Surfaced { source, .. }
            | LogEvent::Accepted { source, .. }
            | LogEvent::Dismissed { source, .. } => source,
        }
    }
}

/// The append-only history of everything the character volunteered.
#[derive(Debug)]
pub struct SurfacingLog {
    path: PathBuf,
    entries: Vec<LogEntry>,
}

impl SurfacingLog {
    /// Read the log at `path`. A missing file is an empty history — the first run.
    ///
    /// An unreadable *file* is an error: the rate limiter would otherwise start
    /// from zero and the character would talk over a cap the user had already
    /// spent. An unreadable *line* is skipped and reported, and stays on disk.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let contents = match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(source) => {
                return Err(Error::io(format!("reading {}", path.display()), source));
            }
        };

        let mut entries = Vec::new();
        for (number, line) in contents.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<LogEntry>(line) {
                Ok(entry) => entries.push(entry),
                Err(error) => tracing::warn!(
                    path = %path.display(),
                    line = number + 1,
                    %error,
                    "skipping an ambient log line this build cannot read (it is kept on disk)"
                ),
            }
        }
        Ok(Self { path, entries })
    }

    /// Everything recorded so far, oldest first.
    pub fn entries(&self) -> &[LogEntry] {
        &self.entries
    }

    /// Append one entry, to memory and to disk.
    pub fn record(&mut self, entry: LogEntry) -> Result<()> {
        let line = serde_json::to_string(&entry).map_err(|source| Error::Json {
            context: "serializing an ambient log entry".into(),
            source,
        })?;
        self.append_line(&line)?;
        self.entries.push(entry);
        Ok(())
    }

    fn append_line(&self, line: &str) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|source| Error::io(format!("creating {}", parent.display()), source))?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|source| Error::io(format!("opening {}", self.path.display()), source))?;
        writeln!(file, "{line}")
            .map_err(|source| Error::io(format!("appending to {}", self.path.display()), source))
    }

    /// Where the log lives.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn surfaced(key: &str) -> LogEvent {
        LogEvent::Surfaced {
            id: format!("id-{key}"),
            key: key.to_string(),
            source: "automation:a".into(),
            headline: "it ran".into(),
        }
    }

    #[test]
    fn entries_round_trip_through_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ambient-log.jsonl");

        let mut log = SurfacingLog::open(&path).expect("a missing file is an empty history");
        assert!(log.entries().is_empty());
        log.record(LogEntry {
            at: Utc::now(),
            event: surfaced("k1"),
        })
        .expect("record");

        let reopened = SurfacingLog::open(&path).expect("reopen");
        assert_eq!(reopened.entries().len(), 1);
        assert_eq!(reopened.entries()[0].event.key(), "k1");
    }

    #[test]
    fn a_line_this_build_cannot_read_is_skipped_and_kept() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ambient-log.jsonl");
        let good = serde_json::to_string(&LogEntry {
            at: Utc::now(),
            event: surfaced("k1"),
        })
        .expect("serialize");
        std::fs::write(&path, format!("{{ truncated\n{good}\n")).expect("write");

        let log = SurfacingLog::open(&path).expect("a bad line must not fail the load");
        assert_eq!(log.entries().len(), 1, "the readable entry survives");

        // The invariant: nothing is deleted. The bad line is still there.
        let raw = std::fs::read_to_string(&path).expect("read back");
        assert!(
            raw.contains("{ truncated"),
            "the unreadable line was rewritten"
        );
    }

    #[test]
    fn a_dismissal_records_the_source_it_quiets() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut log = SurfacingLog::open(dir.path().join("l.jsonl")).expect("open");
        log.record(LogEntry {
            at: Utc::now(),
            event: LogEvent::Dismissed {
                id: "s-1".into(),
                key: "automation:a:t1".into(),
                source: "automation:a".into(),
            },
        })
        .expect("record");
        assert_eq!(log.entries()[0].event.source(), "automation:a");
    }
}
