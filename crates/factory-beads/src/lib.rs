//! Access to a project's beads. The database is always the one the project
//! record names: reads go straight to it, and a claim made through `bd` is
//! read back from it, so a `bd` that wrote somewhere else is caught.

use std::path::{Path, PathBuf};
use std::time::Duration;

use factory_supervise::{Spec, capture};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum BeadError {
    #[error("`{0}` is not a bead ID")]
    BadId(String),
    #[error("bead `{0}` does not exist in the project's database")]
    NotFound(String),
    #[error(transparent)]
    Run(#[from] factory_supervise::Error),
    #[error("`{command}` failed: {detail}")]
    Command { command: String, detail: String },
    #[error("could not read the answer of `{command}`: {detail}")]
    Answer { command: String, detail: String },
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(
        "claimed `{id}` through bd, but the project's database shows status `{status}` and assignee `{assignee}`: bd may have written to another store"
    )]
    ClaimNotVisible {
        id: String,
        status: String,
        assignee: String,
    },
}

/// A bead, as far as the factory reads it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bead {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub design: String,
    #[serde(default)]
    pub acceptance_criteria: String,
    #[serde(default)]
    pub notes: String,
    pub status: String,
    #[serde(default = "default_type")]
    pub issue_type: String,
    #[serde(default)]
    pub assignee: String,
    #[serde(default)]
    pub priority: i64,
    #[serde(default)]
    pub labels: Vec<String>,
    /// Beads that block this one and are not closed.
    #[serde(default)]
    pub open_blockers: Vec<String>,
}

fn default_type() -> String {
    "task".to_string()
}

/// Why a bead may not be launched. Empty means it may.
///
/// A bead already claimed by `actor` is launchable, so a stopped run can be
/// started again.
pub fn readiness(bead: &Bead, actor: &str) -> Vec<String> {
    let mut reasons = Vec::new();
    let claimed_by_us = bead.status == "in_progress" && bead.assignee == actor;
    if bead.status != "open" && !claimed_by_us {
        if bead.status == "in_progress" {
            reasons.push(format!("it is claimed by `{}`", bead.assignee));
        } else {
            reasons.push(format!("its status is `{}`", bead.status));
        }
    }
    if bead.status == "open" && !bead.assignee.is_empty() && bead.assignee != actor {
        reasons.push(format!("it is assigned to `{}`", bead.assignee));
    }
    if matches!(bead.issue_type.as_str(), "epic" | "milestone") {
        reasons.push(format!("it is a {}, not a piece of work", bead.issue_type));
    }
    if bead.acceptance_criteria.trim().is_empty() {
        reasons.push("it has no acceptance criteria".to_string());
    }
    if !bead.open_blockers.is_empty() {
        reasons.push(format!(
            "it is blocked by {}",
            bead.open_blockers.join(", ")
        ));
    }
    if bead.labels.iter().any(|label| label.starts_with("hold")) {
        reasons.push("it is on hold".to_string());
    }
    reasons
}

/// Where a project's beads live.
#[derive(Clone, Debug)]
pub enum Source {
    Dolt {
        host: String,
        port: u16,
        user: String,
        database: String,
        /// The project's checkout, where `bd` is run for writes.
        repo: PathBuf,
    },
    /// A directory of `<id>.json` files.
    Files { dir: PathBuf },
}

fn check_id(id: &str) -> Result<(), BeadError> {
    let ok = !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    if ok {
        Ok(())
    } else {
        Err(BeadError::BadId(id.to_string()))
    }
}

const TIMEOUT: Duration = Duration::from_secs(60);

impl Source {
    pub fn read(&self, id: &str) -> Result<Bead, BeadError> {
        check_id(id)?;
        match self {
            Source::Files { dir } => {
                let path = dir.join(format!("{id}.json"));
                let text = match std::fs::read_to_string(&path) {
                    Ok(text) => text,
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                        return Err(BeadError::NotFound(id.to_string()));
                    }
                    Err(source) => return Err(BeadError::Io { path, source }),
                };
                serde_json::from_str(&text).map_err(|err| BeadError::Answer {
                    command: path.display().to_string(),
                    detail: err.to_string(),
                })
            }
            Source::Dolt { .. } => {
                // The ID was checked above, so it is safe inside the quotes.
                let rows = self.query(&format!(
                    "select id, title, description, design, acceptance_criteria, notes, status, \
                     issue_type, coalesce(assignee, '') as assignee, priority from issues where id = '{id}'"
                ))?;
                let row = rows
                    .first()
                    .ok_or_else(|| BeadError::NotFound(id.to_string()))?;
                let text = |key: &str| {
                    row.get(key)
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string()
                };
                let column = |rows: Vec<Value>, key: &str| -> Vec<String> {
                    rows.iter()
                        .filter_map(|row| row.get(key).and_then(Value::as_str))
                        .map(str::to_string)
                        .collect()
                };
                let labels = self.query(&format!(
                    "select label from labels where issue_id = '{id}' order by label"
                ))?;
                let blockers = self.query(&format!(
                    "select b.id as id from dependencies d join issues b on b.id = d.depends_on_issue_id \
                     where d.issue_id = '{id}' and d.type = 'blocks' and b.status <> 'closed' order by b.id"
                ))?;
                Ok(Bead {
                    id: text("id"),
                    title: text("title"),
                    description: text("description"),
                    design: text("design"),
                    acceptance_criteria: text("acceptance_criteria"),
                    notes: text("notes"),
                    status: text("status"),
                    issue_type: text("issue_type"),
                    assignee: text("assignee"),
                    priority: row.get("priority").and_then(Value::as_i64).unwrap_or(0),
                    labels: column(labels, "label"),
                    open_blockers: column(blockers, "id"),
                })
            }
        }
    }

    /// Claim a bead for `actor`, and confirm the claim in the project's database.
    pub fn claim(&self, id: &str, actor: &str) -> Result<Bead, BeadError> {
        check_id(id)?;
        match self {
            Source::Files { dir } => {
                let mut bead = self.read(id)?;
                bead.status = "in_progress".to_string();
                bead.assignee = actor.to_string();
                let path = dir.join(format!("{id}.json"));
                let text =
                    serde_json::to_string_pretty(&bead).map_err(|err| BeadError::Answer {
                        command: path.display().to_string(),
                        detail: err.to_string(),
                    })?;
                std::fs::write(&path, text).map_err(|source| BeadError::Io { path, source })?;
            }
            Source::Dolt { repo, .. } => {
                let mut spec = Spec::new(
                    ["bd", "update", id, "--claim", "--actor", actor],
                    repo,
                    TIMEOUT,
                );
                spec.env = vec![("BEADS_ACTOR".to_string(), actor.to_string())];
                let out = capture(&spec)?;
                if !out.success() {
                    return Err(BeadError::Command {
                        command: spec.display(),
                        detail: out.combined().trim().to_string(),
                    });
                }
            }
        }
        let bead = self.read(id)?;
        if bead.status != "in_progress" || bead.assignee != actor {
            return Err(BeadError::ClaimNotVisible {
                id: id.to_string(),
                status: bead.status,
                assignee: bead.assignee,
            });
        }
        Ok(bead)
    }

    fn query(&self, sql: &str) -> Result<Vec<Value>, BeadError> {
        let Source::Dolt {
            host,
            port,
            user,
            database,
            ..
        } = self
        else {
            return Ok(Vec::new());
        };
        let port = port.to_string();
        let spec = Spec::new(
            [
                "dolt",
                "--host",
                host,
                "--port",
                &port,
                "--user",
                user,
                "--password",
                "",
                "--no-tls",
                "--use-db",
                database,
                "sql",
                "-r",
                "json",
                "-q",
                sql,
            ],
            Path::new("/"),
            TIMEOUT,
        );
        let command = format!("dolt sql on {host}:{port}/{database}");
        let out = capture(&spec)?;
        if !out.success() {
            return Err(BeadError::Command {
                command,
                detail: out.combined().trim().to_string(),
            });
        }
        // A query with no rows prints nothing at all.
        if out.stdout.trim().is_empty() {
            return Ok(Vec::new());
        }
        let answer: Value =
            serde_json::from_str(out.stdout.trim()).map_err(|err| BeadError::Answer {
                command: command.clone(),
                detail: err.to_string(),
            })?;
        Ok(answer
            .get("rows")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready() -> Bead {
        Bead {
            id: "toy-1".to_string(),
            title: "Add a thing".to_string(),
            acceptance_criteria: "- the thing exists".to_string(),
            status: "open".to_string(),
            issue_type: "task".to_string(),
            ..Bead::default()
        }
    }

    #[test]
    fn a_plain_open_bead_with_criteria_is_launchable() {
        assert_eq!(readiness(&ready(), "factory"), Vec::<String>::new());
    }

    #[test]
    fn each_obstacle_is_named() {
        let mut bead = ready();
        bead.status = "in_progress".to_string();
        bead.assignee = "someone".to_string();
        assert_eq!(readiness(&bead, "factory"), ["it is claimed by `someone`"]);

        let mut bead = ready();
        bead.status = "closed".to_string();
        assert_eq!(readiness(&bead, "factory"), ["its status is `closed`"]);

        let mut bead = ready();
        bead.acceptance_criteria = "  ".to_string();
        assert_eq!(
            readiness(&bead, "factory"),
            ["it has no acceptance criteria"]
        );

        let mut bead = ready();
        bead.open_blockers = vec!["toy-0".to_string()];
        assert_eq!(readiness(&bead, "factory"), ["it is blocked by toy-0"]);

        let mut bead = ready();
        bead.issue_type = "epic".to_string();
        assert_eq!(
            readiness(&bead, "factory"),
            ["it is a epic, not a piece of work"]
        );

        let mut bead = ready();
        bead.labels = vec!["hold:owner".to_string()];
        assert_eq!(readiness(&bead, "factory"), ["it is on hold"]);
    }

    #[test]
    fn our_own_claim_does_not_block_a_restart() {
        let mut bead = ready();
        bead.status = "in_progress".to_string();
        bead.assignee = "factory".to_string();
        assert_eq!(readiness(&bead, "factory"), Vec::<String>::new());
    }

    #[test]
    fn a_file_bead_is_read_and_claimed() {
        let dir = std::env::temp_dir().join(format!("factory-beads-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("toy-1.json"),
            serde_json::to_string(&ready()).unwrap(),
        )
        .unwrap();
        let source = Source::Files { dir: dir.clone() };
        assert_eq!(source.read("toy-1").unwrap(), ready());
        let claimed = source.claim("toy-1", "factory").unwrap();
        assert_eq!(claimed.status, "in_progress");
        assert_eq!(source.read("toy-1").unwrap().assignee, "factory");
        assert!(matches!(source.read("toy-2"), Err(BeadError::NotFound(_))));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_id_that_could_break_out_of_a_query_is_refused() {
        let source = Source::Files {
            dir: std::env::temp_dir(),
        };
        for id in ["", "a'b", "a b", "../x", "a;drop"] {
            assert!(matches!(source.read(id), Err(BeadError::BadId(_))), "{id}");
        }
    }
}
