//! `cargo xtask gate <id>`: the single way a milestone is proved.
//!
//! A gate is a manifest, `gates/<id>.toml`, listing checks. Each check is a
//! command with a tier, a timeout and the claim it proves. The runner executes
//! them, writes `target/gates/<id>.json`, and says one of three things:
//!
//! - passed (exit 0): every check ran and passed
//! - failed (exit 1): a check failed or timed out, or the manifest has no checks
//! - incomplete (exit 2): nothing failed, but a tier was skipped
//!
//! An incomplete gate cannot close a milestone.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{self, Error};

pub const REPORT_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// No network and no model. Runs anywhere.
    Hermetic,
    /// Needs this Mac's real services, launchd jobs or worker slots.
    Host,
    /// Makes real Claude calls, on Haiku.
    Live,
    /// Needs something only the owner can supply.
    Owner,
}

impl Tier {
    pub fn parse(text: &str) -> Option<Tier> {
        match text {
            "hermetic" => Some(Tier::Hermetic),
            "host" => Some(Tier::Host),
            "live" => Some(Tier::Live),
            "owner" => Some(Tier::Owner),
            _ => None,
        }
    }
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Tier::Hermetic => "hermetic",
            Tier::Host => "host",
            Tier::Live => "live",
            Tier::Owner => "owner",
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub id: String,
    pub title: String,
    /// The milestone bead this gate closes.
    pub bead: Option<String>,
    #[serde(default, rename = "check")]
    pub checks: Vec<Check>,
    /// Tools whose versions the report records, beyond rustc and cargo.
    #[serde(default, rename = "tool")]
    pub tools: Vec<Tool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub id: String,
    pub tier: Tier,
    /// What passing this check proves, in one sentence.
    pub claim: String,
    /// The bead whose acceptance line this check is.
    pub bead: Option<String>,
    pub command: Vec<String>,
    pub timeout_secs: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tool {
    pub name: String,
    pub command: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GateStatus {
    Passed,
    Failed,
    Incomplete,
}

impl GateStatus {
    pub fn exit_code(self) -> u8 {
        match self {
            GateStatus::Passed => 0,
            GateStatus::Failed => 1,
            GateStatus::Incomplete => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Passed,
    Failed,
    TimedOut,
    Skipped,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub gate: String,
    pub title: String,
    pub bead: Option<String>,
    pub status: GateStatus,
    /// Why the gate has the status it has, when the checks alone do not say.
    pub reason: Option<String>,
    pub tiers_skipped: Vec<Tier>,
    pub git: GitState,
    pub tools: BTreeMap<String, String>,
    pub started_at_unix: u64,
    pub duration_ms: u64,
    pub checks: Vec<CheckReport>,
}

#[derive(Debug, Serialize)]
pub struct GitState {
    pub commit: Option<String>,
    /// Uncommitted changes were present, so the commit alone does not say what was tested.
    pub dirty: bool,
}

#[derive(Debug, Serialize)]
pub struct CheckReport {
    pub id: String,
    pub tier: Tier,
    pub bead: Option<String>,
    pub claim: String,
    pub command: Vec<String>,
    pub status: CheckStatus,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    /// The check's combined output, relative to the workspace root.
    pub log: Option<String>,
}

pub struct Options {
    /// The workspace root. Checks run here, and paths below are relative to it.
    pub root: PathBuf,
    /// A gate ID: the manifest's path under `gates/` without `.toml`.
    pub id: String,
    /// Run only these tiers. Empty means every tier.
    pub tiers: BTreeSet<Tier>,
    /// Do not print progress. Used by the self-test.
    pub quiet: bool,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.split('/').all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        })
}

pub fn manifest_path(root: &Path, id: &str) -> PathBuf {
    root.join("gates").join(format!("{id}.toml"))
}

pub fn report_path(root: &Path, id: &str) -> PathBuf {
    root.join("target/gates").join(format!("{id}.json"))
}

pub fn load(root: &Path, id: &str) -> Result<Manifest, Error> {
    if !valid_id(id) {
        return Err(Error::Usage(format!(
            "`{id}` is not a gate ID: use lowercase letters, digits and dashes"
        )));
    }
    let path = manifest_path(root, id);
    let text = fs::read_to_string(&path).map_err(error::io(&path))?;
    let manifest: Manifest = toml::from_str(&text).map_err(|err| Error::Manifest {
        path: path.clone(),
        reason: err.to_string(),
    })?;
    let invalid = |reason: String| Error::Manifest {
        path: path.clone(),
        reason,
    };

    let stem = id.rsplit('/').next().unwrap_or(id);
    if manifest.id != stem {
        return Err(invalid(format!(
            "id is `{}` but the file is named `{stem}.toml`",
            manifest.id
        )));
    }
    let mut seen = BTreeSet::new();
    for check in &manifest.checks {
        if !valid_id(&check.id) || check.id.contains('/') {
            return Err(invalid(format!("`{}` is not a check ID", check.id)));
        }
        if !seen.insert(check.id.as_str()) {
            return Err(invalid(format!("check `{}` is listed twice", check.id)));
        }
        if check.command.is_empty() {
            return Err(invalid(format!("check `{}` has no command", check.id)));
        }
        if check.timeout_secs == 0 {
            return Err(invalid(format!("check `{}` has no timeout", check.id)));
        }
        if check.claim.trim().is_empty() {
            return Err(invalid(format!("check `{}` has no claim", check.id)));
        }
    }
    Ok(manifest)
}

/// Run a gate and write its report. The report is returned whatever the gate's
/// status; an error means the gate could not be run at all.
pub fn run(options: &Options) -> Result<Report, Error> {
    let manifest = load(&options.root, &options.id)?;
    let started = Instant::now();
    let started_at_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let log_dir = options.root.join("target/gates").join(&options.id);
    fs::create_dir_all(&log_dir).map_err(error::io(&log_dir))?;

    let mut checks = Vec::new();
    for check in &manifest.checks {
        let selected = options.tiers.is_empty() || options.tiers.contains(&check.tier);
        let report = if selected {
            if !options.quiet {
                eprintln!("gate {}: [{}] {} ...", options.id, check.tier, check.id);
            }
            let report = run_check(&options.root, &options.id, check)?;
            if !options.quiet {
                eprintln!(
                    "gate {}: [{}] {} {:?} in {} ms",
                    options.id, check.tier, check.id, report.status, report.duration_ms
                );
            }
            report
        } else {
            CheckReport {
                id: check.id.clone(),
                tier: check.tier,
                bead: check.bead.clone(),
                claim: check.claim.clone(),
                command: check.command.clone(),
                status: CheckStatus::Skipped,
                exit_code: None,
                duration_ms: 0,
                log: None,
            }
        };
        checks.push(report);
    }

    let tiers_skipped: BTreeSet<Tier> = checks
        .iter()
        .filter(|c| c.status == CheckStatus::Skipped)
        .map(|c| c.tier)
        .collect();
    let any_failed = checks
        .iter()
        .any(|c| matches!(c.status, CheckStatus::Failed | CheckStatus::TimedOut));
    let (status, reason) = if checks.is_empty() {
        // A gate that checks nothing proves nothing.
        (
            GateStatus::Failed,
            Some("the manifest lists no checks".to_string()),
        )
    } else if any_failed {
        (GateStatus::Failed, None)
    } else if !tiers_skipped.is_empty() {
        let names: Vec<String> = tiers_skipped.iter().map(Tier::to_string).collect();
        (
            GateStatus::Incomplete,
            Some(format!("skipped tiers: {}", names.join(", "))),
        )
    } else {
        (GateStatus::Passed, None)
    };

    let report = Report {
        schema_version: REPORT_SCHEMA_VERSION,
        gate: options.id.clone(),
        title: manifest.title.clone(),
        bead: manifest.bead.clone(),
        status,
        reason,
        tiers_skipped: tiers_skipped.into_iter().collect(),
        git: git_state(&options.root),
        tools: tool_versions(&options.root, &manifest.tools),
        started_at_unix,
        duration_ms: millis(started.elapsed()),
        checks,
    };

    let path = report_path(&options.root, &options.id);
    let json = serde_json::to_string_pretty(&report).map_err(|source| Error::Json {
        path: path.clone(),
        source,
    })?;
    fs::write(&path, json + "\n").map_err(error::io(&path))?;
    Ok(report)
}

fn run_check(root: &Path, gate: &str, check: &Check) -> Result<CheckReport, Error> {
    let log_rel = format!("target/gates/{gate}/{}.log", check.id);
    let log_path = root.join(&log_rel);
    let log = File::create(&log_path).map_err(error::io(&log_path))?;
    let log_err = log.try_clone().map_err(error::io(&log_path))?;

    let started = Instant::now();
    let timeout = Duration::from_secs(check.timeout_secs);
    // The check gets its own process group, so a timeout can remove everything
    // it started and not only the process this runner spawned.
    let mut child = Command::new(&check.command[0])
        .args(&check.command[1..])
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log_err)
        .process_group(0)
        .spawn()
        .map_err(|source| Error::Spawn {
            command: check.command.join(" "),
            source,
        })?;

    let (status, exit_code) = loop {
        match child.try_wait().map_err(error::io(&log_path))? {
            Some(exit) if exit.success() => break (CheckStatus::Passed, exit.code()),
            Some(exit) => break (CheckStatus::Failed, exit.code()),
            None if started.elapsed() >= timeout => {
                kill_group(child.id());
                // The group kill can only be missed if `kill` itself could not
                // run; killing the child directly is the fallback.
                let _ = child.kill();
                let _ = child.wait();
                break (CheckStatus::TimedOut, None);
            }
            None => std::thread::sleep(Duration::from_millis(25)),
        }
    };

    Ok(CheckReport {
        id: check.id.clone(),
        tier: check.tier,
        bead: check.bead.clone(),
        claim: check.claim.clone(),
        command: check.command.clone(),
        status,
        exit_code,
        duration_ms: millis(started.elapsed()),
        log: Some(log_rel),
    })
}

/// SIGKILL a whole process group. Done through `kill` because this workspace
/// forbids unsafe code, which a direct system call would need.
fn kill_group(pgid: u32) {
    let _ = Command::new("/bin/kill")
        .args(["-KILL", "--", &format!("-{pgid}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn first_line(root: &Path, command: &[String]) -> Option<String> {
    let (program, args) = command.split_first()?;
    let output = Command::new(program)
        .args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines().next().map(|line| line.trim().to_string())
}

fn git_state(root: &Path) -> GitState {
    let run = |args: &[&str]| {
        let command: Vec<String> = std::iter::once("git")
            .chain(args.iter().copied())
            .map(str::to_string)
            .collect();
        Command::new(&command[0])
            .args(&command[1..])
            .current_dir(root)
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
    };
    GitState {
        commit: run(&["rev-parse", "HEAD"]),
        // If git cannot say, assume the worst.
        dirty: run(&["status", "--porcelain"]).is_none_or(|status| !status.is_empty()),
    }
}

fn tool_versions(root: &Path, extra: &[Tool]) -> BTreeMap<String, String> {
    let mut tools = BTreeMap::new();
    let mut record = |name: &str, command: &[String]| {
        let version = first_line(root, command).unwrap_or_else(|| "unavailable".to_string());
        tools.insert(name.to_string(), version);
    };
    for name in ["rustc", "cargo"] {
        record(name, &[name.to_string(), "--version".to_string()]);
    }
    for tool in extra {
        record(&tool.name, &tool.command);
    }
    tools
}

/// Every gate under `gates/` (not its subdirectories, which hold fixtures),
/// sorted by ID.
pub fn list(root: &Path) -> Result<Vec<Manifest>, Error> {
    let dir = root.join("gates");
    let mut ids = Vec::new();
    for entry in fs::read_dir(&dir).map_err(error::io(&dir))? {
        let path = entry.map_err(error::io(&dir))?.path();
        if path.extension().is_some_and(|ext| ext == "toml")
            && let Some(stem) = path.file_stem().and_then(|stem| stem.to_str())
        {
            ids.push(stem.to_string());
        }
    }
    ids.sort();
    ids.iter().map(|id| load(root, id)).collect()
}

pub fn print_list(manifests: &[Manifest]) {
    for manifest in manifests {
        let bead = manifest.bead.as_deref().unwrap_or("-");
        println!("{}  {}  ({bead})", manifest.id, manifest.title);
        if manifest.checks.is_empty() {
            println!("  (no checks: this gate fails)");
        }
        for check in &manifest.checks {
            let bead = check.bead.as_deref().unwrap_or("-");
            println!("  [{}] {}  ({bead})  {}", check.tier, check.id, check.claim);
        }
    }
}

pub fn print_summary(report: &Report) {
    for check in &report.checks {
        let detail = match (&check.status, &check.log) {
            (CheckStatus::Failed | CheckStatus::TimedOut, Some(log)) => format!("  see {log}"),
            _ => String::new(),
        };
        println!(
            "  [{}] {:<9} {}{detail}",
            check.tier,
            format!("{:?}", check.status).to_lowercase(),
            check.id
        );
    }
    let reason = report
        .reason
        .as_deref()
        .map(|reason| format!(" ({reason})"))
        .unwrap_or_default();
    println!(
        "gate {}: {}{reason}",
        report.gate,
        format!("{:?}", report.status).to_uppercase()
    );
    if report.git.dirty {
        println!(
            "  note: the working tree had uncommitted changes, so this report does not prove a commit"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_passed_gate_exits_zero() {
        assert_eq!(GateStatus::Passed.exit_code(), 0);
        assert_eq!(GateStatus::Failed.exit_code(), 1);
        assert_eq!(GateStatus::Incomplete.exit_code(), 2);
    }

    #[test]
    fn gate_ids_cannot_leave_the_gates_directory() {
        for id in ["m0", "m6-live", "selftest/empty"] {
            assert!(valid_id(id), "{id}");
        }
        for id in ["", "../m0", "/etc/passwd", "m0/", "M0", "a b", "m0.toml"] {
            assert!(!valid_id(id), "{id}");
        }
    }
}
