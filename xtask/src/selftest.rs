//! `cargo xtask selftest`: proof that the check and gate commands say no when
//! they should. Each case runs a fixture that is wrong in one known way and
//! compares what the command reported with what it must report.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::Value;

use crate::check::{self, Stage};
use crate::error::{self, Error};
use crate::gate::{self, CheckStatus, GateStatus, Tier};

pub struct Outcome {
    pub name: String,
    /// `None` if the case held, otherwise what went wrong.
    pub failure: Option<String>,
}

fn outcome(name: impl Into<String>, failure: Option<String>) -> Outcome {
    Outcome {
        name: name.into(),
        failure,
    }
}

/// `cargo xtask check` passes a clean workspace, and fails one with a failing
/// test or a lint warning, in that stage and no other.
pub fn check_cases(root: &Path, offline: bool) -> Result<Vec<Outcome>, Error> {
    let cases: [(&str, &[Stage]); 3] = [
        ("passing", &[]),
        ("failing-test", &[Stage::Test]),
        ("failing-lint", &[Stage::Lint]),
    ];
    let mut outcomes = Vec::new();
    for (fixture, expected) in cases {
        let report = check::run(&check::Options {
            root: root.join("xtask/fixtures").join(fixture),
            offline,
            target_dir: Some(root.join("target/selftest").join(fixture)),
            quiet: true,
        })?;
        let failure = (report.failed != expected).then(|| {
            format!(
                "expected failed stages {expected:?}, got {:?}",
                report.failed
            )
        });
        outcomes.push(outcome(format!("check on fixture `{fixture}`"), failure));
    }
    Ok(outcomes)
}

struct GateCase {
    fixture: &'static str,
    tiers: &'static [Tier],
    status: GateStatus,
    /// The status each check must have, in manifest order.
    checks: &'static [CheckStatus],
}

const GATE_CASES: [GateCase; 6] = [
    GateCase {
        fixture: "passing",
        tiers: &[],
        status: GateStatus::Passed,
        checks: &[CheckStatus::Passed, CheckStatus::Passed],
    },
    GateCase {
        fixture: "empty",
        tiers: &[],
        status: GateStatus::Failed,
        checks: &[],
    },
    GateCase {
        fixture: "failing",
        tiers: &[],
        status: GateStatus::Failed,
        checks: &[CheckStatus::Passed, CheckStatus::Failed],
    },
    GateCase {
        fixture: "timeout",
        tiers: &[],
        status: GateStatus::Failed,
        checks: &[CheckStatus::TimedOut],
    },
    // The host check in this fixture would fail if it ran. With only the
    // hermetic tier selected it must be skipped, and the gate left incomplete.
    GateCase {
        fixture: "tiers",
        tiers: &[Tier::Hermetic],
        status: GateStatus::Incomplete,
        checks: &[CheckStatus::Passed, CheckStatus::Skipped],
    },
    GateCase {
        fixture: "tiers",
        tiers: &[],
        status: GateStatus::Failed,
        checks: &[CheckStatus::Passed, CheckStatus::Failed],
    },
];

/// The gate runner's rules, each proved by a fixture under `gates/selftest`,
/// and every report it wrote checked against the committed schema.
pub fn gate_cases(root: &Path) -> Result<Vec<Outcome>, Error> {
    let schema_path = root.join("gates/report.schema.json");
    let schema = read_json(&schema_path)?;
    let validator = jsonschema::validator_for(&schema).map_err(|err| Error::Schema {
        path: schema_path.clone(),
        reason: err.to_string(),
    })?;

    let mut outcomes = Vec::new();
    for case in &GATE_CASES {
        let id = format!("selftest/{}", case.fixture);
        let tiers: BTreeSet<Tier> = case.tiers.iter().copied().collect();
        let name = if tiers.is_empty() {
            format!("gate `{id}`")
        } else {
            let names: Vec<String> = tiers.iter().map(Tier::to_string).collect();
            format!("gate `{id}` with --tier {}", names.join(","))
        };
        let report = gate::run(&gate::Options {
            root: root.to_path_buf(),
            id: id.clone(),
            tiers,
            quiet: true,
        })?;

        let statuses: Vec<CheckStatus> = report.checks.iter().map(|c| c.status).collect();
        let mut failure = None;
        if report.status != case.status {
            failure = Some(format!(
                "expected {:?}, got {:?}",
                case.status, report.status
            ));
        } else if statuses != case.checks {
            failure = Some(format!(
                "expected checks {:?}, got {statuses:?}",
                case.checks
            ));
        }
        outcomes.push(outcome(name, failure));

        let written = read_json(&gate::report_path(root, &id))?;
        let errors: Vec<String> = validator
            .iter_errors(&written)
            .map(|err| format!("{} at {}", err, err.instance_path()))
            .collect();
        outcomes.push(outcome(
            format!("report of `{id}` matches the schema"),
            (!errors.is_empty()).then(|| errors.join("; ")),
        ));

        // The schema must be able to say no as well: a report with its status
        // removed, or with a status that does not exist, is not a report.
        let mut without_status = written.clone();
        if let Some(object) = without_status.as_object_mut() {
            object.remove("status");
        }
        let mut wrong_status = written.clone();
        if let Some(object) = wrong_status.as_object_mut() {
            object.insert("status".to_string(), Value::from("mostly"));
        }
        let rejected = !validator.is_valid(&without_status) && !validator.is_valid(&wrong_status);
        outcomes.push(outcome(
            format!("schema rejects a damaged report of `{id}`"),
            (!rejected).then(|| "a damaged report was accepted".to_string()),
        ));
    }

    outcomes.push(orphan_case(root)?);
    outcomes.push(list_case(root)?);
    Ok(outcomes)
}

/// The timeout fixture starts a background child and records its process ID.
/// After the gate has run, that child must be gone too.
fn orphan_case(root: &Path) -> Result<Outcome, Error> {
    let name = "a timed-out check leaves no process behind";
    let pid_path = root.join("target/gates/selftest/timeout/child.pid");
    let pid = fs::read_to_string(&pid_path).map_err(error::io(&pid_path))?;
    let pid = pid.trim();
    if pid.is_empty() || !pid.chars().all(|c| c.is_ascii_digit()) {
        return Ok(outcome(name, Some(format!("`{pid}` is not a process ID"))));
    }
    // `kill -0` sends nothing: it succeeds only if the process still exists.
    let alive = Command::new("/bin/kill")
        .args(["-0", pid])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|source| Error::Spawn {
            command: format!("/bin/kill -0 {pid}"),
            source,
        })?
        .success();
    Ok(outcome(
        name,
        alive.then(|| format!("process {pid} is still running")),
    ))
}

/// `--list` shows every gate with its checks and tiers.
fn list_case(root: &Path) -> Result<Outcome, Error> {
    let manifests = gate::list(root)?;
    let mut failure = None;
    if manifests.is_empty() {
        failure = Some("no gates listed".to_string());
    }
    let dir = root.join("gates");
    for entry in fs::read_dir(&dir).map_err(error::io(&dir))? {
        let path = entry.map_err(error::io(&dir))?.path();
        if path.extension().is_some_and(|ext| ext == "toml") {
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if !manifests.iter().any(|manifest| manifest.id == stem) {
                failure = Some(format!("gate `{stem}` is missing from the list"));
            }
        }
    }
    Ok(outcome("gate --list covers every manifest", failure))
}

fn read_json(path: &Path) -> Result<Value, Error> {
    let text = fs::read_to_string(path).map_err(error::io(path))?;
    serde_json::from_str(&text).map_err(|source| Error::Json {
        path: path.to_path_buf(),
        source,
    })
}
