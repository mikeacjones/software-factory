//! Identifiers, the event envelope and failure classes: the words every other
//! crate exchanges. See docs/glossary.md for what each one means.

use std::fmt;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

macro_rules! id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

id!(
    /// A repository the factory works on.
    ProjectId
);
id!(
    /// An issue in a project's bead database.
    BeadId
);
id!(
    /// The factory's processing of one bead.
    UnitId
);
id!(
    /// One stage of a flow.
    StepId
);
id!(
    /// One Claude Code conversation.
    SessionId
);

/// How far a failure reaches, which decides the response to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FailureClass {
    /// One unit is affected: retry, send back a step, or park.
    Unit,
    /// Nothing can usefully be dispatched: pause and resume without help.
    FactoryWide,
    /// Only the owner can fix it.
    NeedsOwner,
}

/// Seconds since the Unix epoch. The one place the system clock is read, until
/// the clock crate replaces it.
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// A time as `2026-11-15T13:00:00Z`, from seconds since the Unix epoch.
pub fn clock(unix: u64) -> String {
    let days = (unix / 86_400) as i64;
    let (hour, minute, second) = (unix % 86_400 / 3600, unix % 3600 / 60, unix % 60);
    // The civil date from the day count, by the usual era arithmetic.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Write a line to the service's log, with the time it was written. The
/// log is where a process nobody is watching says what went wrong.
#[macro_export]
macro_rules! note {
    ($($arg:tt)*) => {
        eprintln!("{} {}", $crate::clock($crate::now_unix()), format!($($arg)*))
    };
}

pub const EVENT_SCHEMA_VERSION: u32 = 1;

/// One line of a unit's audit log: something the factory decided or did.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub schema_version: u32,
    pub time_unix: u64,
    pub kind: String,
    pub project: ProjectId,
    pub unit: UnitId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<StepId>,
    #[serde(default)]
    pub data: serde_json::Value,
}

#[derive(Debug, thiserror::Error)]
pub enum LogError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("event could not be written as JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// An append-only file of events, one JSON object per line.
#[derive(Clone, Debug)]
pub struct EventLog {
    path: PathBuf,
    project: ProjectId,
    unit: UnitId,
}

impl EventLog {
    pub fn new(path: impl Into<PathBuf>, project: ProjectId, unit: UnitId) -> Self {
        Self {
            path: path.into(),
            project,
            unit,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(
        &self,
        kind: &str,
        step: Option<&StepId>,
        data: serde_json::Value,
    ) -> Result<(), LogError> {
        let event = Event {
            schema_version: EVENT_SCHEMA_VERSION,
            time_unix: now_unix(),
            kind: kind.to_string(),
            project: self.project.clone(),
            unit: self.unit.clone(),
            step: step.cloned(),
            data,
        };
        let mut line = serde_json::to_string(&event)?;
        line.push('\n');
        let io = |source| LogError::Io {
            path: self.path.clone(),
            source,
        };
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(io)?;
        file.write_all(line.as_bytes()).map_err(io)
    }

    /// Every event written so far, oldest first.
    pub fn read(&self) -> Result<Vec<Event>, LogError> {
        let text = std::fs::read_to_string(&self.path).map_err(|source| LogError::Io {
            path: self.path.clone(),
            source,
        })?;
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).map_err(LogError::Json))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_round_trip_through_the_log() {
        let path = std::env::temp_dir().join(format!("factory-core-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let log = EventLog::new(&path, ProjectId::new("toy"), UnitId::new("toy-1"));
        log.append("unit.started", None, serde_json::json!({"bead": "toy-1"}))
            .unwrap();
        log.append(
            "step.ended",
            Some(&StepId::new("build")),
            serde_json::json!({"outcome": "done"}),
        )
        .unwrap();
        let events = log.read().unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].kind, "step.ended");
        assert_eq!(events[1].step, Some(StepId::new("build")));
        assert_eq!(events[1].data["outcome"], "done");
    }

    #[test]
    fn ids_serialize_as_plain_strings() {
        let json = serde_json::to_string(&BeadId::new("sf-1")).unwrap();
        assert_eq!(json, "\"sf-1\"");
    }
}
