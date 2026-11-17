//! Walk one unit through its flow, in the foreground: prepare the work, run
//! each agent and command step, publish.
//!
//! Agents propose and this code acts. It claims the bead, makes the worktree,
//! commits a checkpoint after every writing step, checks what each step
//! changed, pushes the branch and opens the pull request. Every decision and
//! action is appended to the unit's event log, and the unit's state is saved
//! after every step so a stopped run can be continued.
//!
//! This is the thin first cut of the driver: one unit, no ticks, no Redis.

pub mod goal;
pub mod report;
pub mod service;
pub mod thread;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use factory_beads::{Bead, Source, readiness};
use factory_brief::{
    Brief, ClassCount, Contract, Note, classes_for_builder, classes_for_tester, contract_of,
    findings_text, render, result_problems,
};
use factory_checks::{
    CommandResult, Verdict, copy_at_base, failure_signature, first_match, host_fault, run_command,
    run_command_with_grace, scope_violations, weakened_tests,
};
use factory_claude::accounts::{
    AccountError, Choice, Known, Refresh, TOKEN_URL, TokenSource, Tracker, USAGE_API, Vault,
    choose, read_usage, session_token,
};
use factory_claude::{Outcome, RunSpec, Session, Usage};
use factory_config::{BeadSource, ConfigDir, Factory, Project, read_env_file};
use factory_core::{EventLog, ProjectId, StepId, UnitId, now_unix};
use factory_flow::{
    ExitPolicy, Flow, Next, Position, Registry, SessionRule, Step, StepKind, Target, Workspace,
};
use factory_github::{
    CiVerdict, GitHub, MergeMethod, MergeRequest, ci_verdict, publication_problems, runs_to_rerun,
    without_replaced,
};
use factory_slack::{Slack, code_block, escape, link, quote, truncate};
use factory_supervise::{Control, tail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use thread::{Thread, ThreadState, is_stop};

#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    #[error(transparent)]
    Config(#[from] factory_config::ConfigError),
    #[error(transparent)]
    Flow(#[from] factory_flow::FlowError),
    #[error(transparent)]
    Bead(#[from] factory_beads::BeadError),
    #[error(transparent)]
    Git(#[from] factory_git::GitError),
    #[error(transparent)]
    Check(#[from] factory_checks::CheckError),
    #[error(transparent)]
    Supervise(#[from] factory_supervise::Error),
    #[error(transparent)]
    Log(#[from] factory_core::LogError),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path}: {source}")]
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("bead `{bead}` may not be launched:\n  - {}", reasons.join("\n  - "))]
    NotLaunchable { bead: String, reasons: Vec<String> },
    #[error(transparent)]
    Account(#[from] factory_claude::accounts::AccountError),
    #[error("{0}")]
    Preflight(String),
    #[error("unit `{0}` has already ended")]
    AlreadyEnded(String),
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> DriverError + '_ {
    move |source| DriverError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Running,
    /// Stopped, with everything kept for the owner.
    Parked(String),
    /// The flow reached its end.
    Ended,
}

/// What one step has used across all its runs.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StepUsage {
    pub runs: u32,
    pub wall_secs: u64,
    pub usage: Usage,
}

/// The session a step is in the middle of, kept so that a question to the
/// owner or a rejected result continues the same conversation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InStep {
    pub step: String,
    pub session: String,
}

/// The factory's record of one unit.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Unit {
    pub id: UnitId,
    pub project: ProjectId,
    pub bead: String,
    pub title: String,
    pub flow: String,
    pub branch: String,
    /// The commit the branch started from.
    pub base: String,
    /// The commit the last writing step left.
    pub checkpoint: String,
    pub worktree: PathBuf,
    pub position: Position,
    pub status: Status,
    /// The session each agent step last ran in.
    pub sessions: BTreeMap<String, String>,
    pub in_step: Option<InStep>,
    /// What sent the unit backwards, for the step that must act on it.
    pub feedback: Option<String>,
    /// The latest note each step produced for later steps.
    pub notes: BTreeMap<String, Note>,
    /// The latest copy of each artifact a command step produced.
    #[serde(default)]
    pub artifacts: BTreeMap<String, Artifact>,
    /// The latest typed result of each agent step.
    pub results: BTreeMap<String, Value>,
    pub usage: BTreeMap<String, StepUsage>,
    pub thread: Option<ThreadState>,
    /// Owner messages not yet handed to an agent step.
    pub owner_messages: Vec<String>,
    /// The owner's answer to the current step's question.
    pub answer: Option<String>,
    pub pull_request: Option<String>,
    #[serde(default)]
    pub pull_request_number: Option<u64>,
    /// The merge commit, once the pull request has landed.
    #[serde(default)]
    pub merged: Option<String>,
    /// The sub-goal worktree this unit's change is absorbed into, if it
    /// belongs to one.
    #[serde(default)]
    pub absorb_into: Option<PathBuf>,
    /// The commit this unit's change became on the sub-goal's branch.
    #[serde(default)]
    pub absorbed: Option<String>,
    /// The commit of the unit's branch that was last pushed to the remote,
    /// for a project that keeps its branches there as they change.
    #[serde(default)]
    pub synced: Option<String>,
    /// The commit last pushed to the unit's holding branch: where work in
    /// hand is kept while the unit's own branch has a pull request open.
    #[serde(default)]
    pub held: Option<String>,
    /// Whether this unit ships a sub-goal: its branch already holds one
    /// commit for each bead and is published as it stands.
    #[serde(default)]
    pub ship: bool,
    /// The design of the sub-goal the unit belongs to, as its director
    /// accepted it. Every agent step's first brief carries it.
    #[serde(default)]
    pub design: Option<String>,
    /// The beads closed when this unit lands. Empty means its own.
    #[serde(default)]
    pub closes: Vec<String>,
    /// The GitHub issues this unit's pull request must close: its bead's, or
    /// those of every bead of the sub-goal it ships.
    #[serde(default)]
    pub issues: Vec<u64>,
    /// The adversarial tester's regression tests that are now part of the
    /// unit's branch. Every one must pass before a fix is accepted.
    #[serde(default)]
    pub regressions: Vec<Regression>,
    /// What has happened on the unit, one line for each step that ended:
    /// what a fresh agent is told, since it shares no session with the rest.
    #[serde(default)]
    pub history: Vec<String>,
    /// The files and directories set aside for this unit while other beads
    /// of its sub-goal are built beside it. Empty when it runs alone.
    #[serde(default)]
    pub declared_paths: Vec<String>,
    /// Files it changed outside them.
    #[serde(default)]
    pub strayed: Vec<String>,
    /// Paths no step may change that a step changed all the same, each
    /// with the commit before that step: what the path has to be put back
    /// to. Empty once they are.
    #[serde(default)]
    pub trespassed: BTreeMap<String, String>,
    /// What whoever runs the bead's sub-goal has decided about it, in the
    /// order decided. Every agent step is shown them.
    #[serde(default)]
    pub rulings: Vec<String>,
    /// Files left with conflict markers when the sub-goal's branch was
    /// merged into this unit's, for the builder to resolve.
    #[serde(default)]
    pub conflicts: Vec<String>,
    /// How each command step last failed, until it passes again: a step that
    /// fails the same way once more parks, since another fix round is
    /// likely to go as the last one did.
    #[serde(default)]
    pub failures: BTreeMap<String, Failure>,
    /// Checks that failed on a commit and passed when run again on it. The
    /// change did not cause them, so no builder was sent, but something in
    /// the project or its tests did: whoever started the pipeline is handed
    /// each one when it ends, to file or to drop, as it is a finding.
    #[serde(default)]
    pub flakes: Vec<Flake>,
    /// Where the watch of the pull request's CI stands on the commit it
    /// last looked at: how often failed jobs were run again, and whether
    /// the failure was handed on.
    #[serde(default)]
    pub ci: Option<CiWatch>,
    pub runs: u32,
    pub started_at: u64,
}

/// A check that failed and then passed on the same commit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Flake {
    /// The step it happened in.
    pub step: String,
    /// What failed: a command of the project, or the CI checks by name.
    pub check: String,
    /// The commit it failed and passed on.
    pub commit: String,
    /// The failing lines, made comparable as a failure's signature is.
    pub signature: String,
    /// Where the failing output is kept.
    pub output: Option<PathBuf>,
}

impl Flake {
    /// The flake as a finding for whoever started the pipeline, in the
    /// shape a tester's or reviewer's has.
    pub fn finding(&self) -> Value {
        json!({
            "step": self.step,
            "severity": "flake",
            "category": "flaky-check",
            "title": format!(
                "`{}` failed on commit `{}` and passed when run again on it: the change did not cause it, so something in the project or its tests did. What failed: {}",
                self.check,
                short_commit(&self.commit),
                truncate(&self.signature.replace('\n', " | "), 400)
            ),
            "location": self
                .output
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
        })
    }
}

/// What a pipeline left for whoever started it to rule on: what its tester
/// and reviewer found that was not fixed, and the checks that failed for a
/// reason that was not the change's.
pub fn unit_findings(unit: &Unit) -> Vec<Value> {
    unit.results
        .iter()
        .flat_map(|(step, result)| {
            result
                .get("findings")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .map(move |finding| {
                    let mut finding = finding.clone();
                    finding["step"] = json!(step);
                    finding
                })
        })
        .chain(unit.flakes.iter().map(Flake::finding))
        .collect()
}

/// The watch of a pull request's CI on one commit.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiWatch {
    pub commit: String,
    /// How often the failed jobs have been run again on it.
    pub reruns: u32,
    /// One more than the project allows, for each time the flow came back
    /// to the watch with no new commit: nothing changed, so looking at the
    /// same failure again would tell nobody anything.
    #[serde(default)]
    pub granted: u32,
    /// The failed checks that were replaced by running them again, by URL.
    #[serde(default)]
    pub replaced: Vec<String>,
    /// The checks that failed before the jobs were last run again.
    #[serde(default)]
    pub failed: Vec<String>,
    /// The failing lines of that failure, and where its log is kept.
    #[serde(default)]
    pub signature: String,
    #[serde(default)]
    pub log: Option<PathBuf>,
    /// Whether the failure on this commit was handed on as red.
    #[serde(default)]
    pub reported: bool,
}

/// A command step's failure, made comparable with its next one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    /// [`factory_checks::failure_signature`] of its output.
    pub signature: String,
    /// The commit it failed on.
    pub commit: String,
}

/// Something a command step wrote for later steps, such as a directory of
/// reports. It is kept outside the worktree and describes one commit.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Artifact {
    pub path: PathBuf,
    /// The commit it describes.
    pub commit: String,
    pub step: String,
    /// The command that wrote it, as run.
    pub command: String,
    /// How the command ended: `passed`, or what went wrong.
    pub ended: String,
}

/// A test the adversarial tester left to prove a finding. It failed when the
/// factory ran it on the code the tester broke, and it stays in the branch.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Regression {
    /// The tester round that found it.
    pub round: u32,
    pub finding: String,
    /// Fails because of the finding, and must pass once it is fixed.
    pub command: String,
    /// The tests' names as written in the code, so that deleting a test
    /// cannot make its command pass.
    pub test_names: Vec<String>,
    /// The class of defect the tester gave the finding, as a key. Empty on
    /// a record written before findings had one.
    #[serde(default)]
    pub category: String,
}

/// What every agent of a sub-goal's pipelines is told of its design.
fn design_note(design: &str, ship: bool) -> Note {
    let lead = if ship {
        "This sub-goal was designed before any of its beads was built, and the goal's director accepted the design. The whole of it is on this branch now. Hold the branch to the design: a public name, an error set or a place for code that departs from it, and that no ruling in this brief allows, is a finding that must be fixed or ruled on."
    } else {
        "The sub-goal this bead belongs to was designed before any of its beads was built, and the goal's director accepted the design. It decides the public names, the error sets, the invariants and where code goes: work to it, and judge the bead's change against it. If the bead cannot be done within it, do not depart from it quietly: say so in your summary with the reason, so that whoever runs the sub-goal can rule."
    };
    Note {
        title: "The design of this sub-goal".to_string(),
        body: format!("{lead}\n\n{}", design.trim()),
    }
}

/// What a builder and a reviewer are told about the texts the project's code
/// is written to. `None` for any other step, and when the project names none.
fn standards_note(standards: &[String], tree: &Path, contract: Contract) -> Option<Note> {
    let lead = match contract {
        Contract::Change => {
            "The project's code is written to these texts, which are in the repository. Read them before you write: the reviewer judges your change against them."
        }
        Contract::Review => {
            "Judge whether the change is idiomatic and of the quality a maintainer would accept against these texts, which are in the repository. Read them before you read the diff. A finding of idiom or quality says which text it rests on and where, or says that the texts are silent and names the convention of the language that decides."
        }
        _ => return None,
    };
    if standards.is_empty() {
        return None;
    }
    let files: Vec<String> = standards
        .iter()
        .map(|path| {
            if tree.join(path).is_file() {
                format!("- `{path}`")
            } else {
                format!("- `{path}` (the project's record names it, and it is not in this tree)")
            }
        })
        .collect();
    Some(Note {
        title: "The project's written standards".to_string(),
        body: format!("{lead}\n\n{}", files.join("\n")),
    })
}

/// How often each class of defect has broken beads of a project, most
/// beads first: every critical or major finding a tester proved, read from
/// the records of the project's units. A record that cannot be read, or
/// that has another shape, is left out.
pub fn finding_classes(state_dir: &Path, project: &str) -> Vec<ClassCount> {
    let mut classes: BTreeMap<String, (usize, std::collections::BTreeSet<String>)> =
        BTreeMap::new();
    let units = std::fs::read_dir(state_dir.join("units"))
        .into_iter()
        .flatten()
        .flatten();
    for entry in units {
        let Ok(text) = std::fs::read_to_string(entry.path().join("unit.json")) else {
            continue;
        };
        let Ok(unit) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if unit.get("project").and_then(Value::as_str) != Some(project) {
            continue;
        }
        let bead = unit.get("bead").and_then(Value::as_str).unwrap_or("");
        let regressions = unit
            .get("regressions")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        for regression in regressions {
            let class = regression
                .get("category")
                .and_then(Value::as_str)
                .unwrap_or("");
            if factory_brief::is_class_key(class) {
                let (findings, beads) = classes.entry(class.to_string()).or_default();
                *findings += 1;
                beads.insert(bead.to_string());
            }
        }
    }
    let mut counts: Vec<ClassCount> = classes
        .into_iter()
        .map(|(class, (findings, beads))| ClassCount {
            class,
            findings,
            beads: beads.len(),
        })
        .collect();
    // The map gave them by name; a stable sort keeps that among equals.
    counts.sort_by_key(|count| std::cmp::Reverse((count.beads, count.findings)));
    counts
}

/// The check that answers a class of defect: a bead that builds a tool
/// which every later change runs, so that the class need not be carried
/// as words in a brief.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClassCheck {
    /// The bead that builds the check.
    pub bead: String,
    /// How many beads the class had broken when that bead landed. `None`
    /// while it has not landed.
    pub landed_at: Option<usize>,
}

impl ClassCheck {
    /// Whether the check has landed and the class has broken no bead since.
    pub fn holds(&self, beads: usize) -> bool {
        self.landed_at.is_some_and(|at| beads <= at)
    }

    /// How the check stands for a class that has broken `beads` beads, as
    /// a few words.
    pub fn words(&self, beads: usize) -> String {
        match self.landed_at {
            None => format!("check being built by `{}`", self.bead),
            Some(at) if beads <= at => format!("checked since `{}` landed", self.bead),
            Some(at) => format!(
                "check from `{}` landed and {} bead(s) broke after it",
                self.bead,
                beads - at
            ),
        }
    }
}

fn class_checks_path(state_dir: &Path, project: &str) -> PathBuf {
    state_dir.join("projects").join(project).join("checks.json")
}

/// The check that answers each class of defect of a project, by the
/// class's key. A record that cannot be read is no record.
pub fn class_checks(state_dir: &Path, project: &str) -> BTreeMap<String, ClassCheck> {
    std::fs::read_to_string(class_checks_path(state_dir, project))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn save_class_checks(
    state_dir: &Path,
    project: &str,
    checks: &BTreeMap<String, ClassCheck>,
) -> Result<(), DriverError> {
    let path = class_checks_path(state_dir, project);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io(parent))?;
    }
    let text = serde_json::to_string_pretty(checks).map_err(|source| DriverError::Json {
        path: path.clone(),
        source,
    })?;
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, text).map_err(io(&temp))?;
    std::fs::rename(&temp, &path).map_err(io(&path))
}

/// Record that a bead builds the check for a class of defect. A check
/// that is still being built keeps the class; one that landed and has
/// leaked since gives it up to the new bead.
pub fn answer_class(
    state_dir: &Path,
    project: &str,
    class: &str,
    bead: &str,
) -> Result<bool, DriverError> {
    let mut checks = class_checks(state_dir, project);
    if checks
        .get(class)
        .is_some_and(|check| check.landed_at.is_none())
    {
        return Ok(false);
    }
    checks.insert(
        class.to_string(),
        ClassCheck {
            bead: bead.to_string(),
            landed_at: None,
        },
    );
    save_class_checks(state_dir, project, &checks)?;
    Ok(true)
}

/// A bead has landed: every class whose check it builds is checked from
/// now, at the count of beads the class has broken so far. Returns those
/// classes.
pub fn check_landed(
    state_dir: &Path,
    project: &str,
    bead: &str,
) -> Result<Vec<String>, DriverError> {
    let mut checks = class_checks(state_dir, project);
    let counts = finding_classes(state_dir, project);
    let mut landed = Vec::new();
    for (class, check) in checks.iter_mut() {
        if check.bead == bead && check.landed_at.is_none() {
            check.landed_at = Some(
                counts
                    .iter()
                    .find(|count| &count.class == class)
                    .map_or(0, |count| count.beads),
            );
            landed.push(class.clone());
        }
    }
    if !landed.is_empty() {
        save_class_checks(state_dir, project, &checks)?;
    }
    Ok(landed)
}

/// The classes a builder still has to keep in mind: the ones no landed
/// check answers, and the ones that have broken a bead since theirs landed.
pub fn classes_unchecked(
    classes: Vec<ClassCount>,
    checks: &BTreeMap<String, ClassCheck>,
) -> Vec<ClassCount> {
    classes
        .into_iter()
        .filter(|count| {
            !checks
                .get(&count.class)
                .is_some_and(|check| check.holds(count.beads))
        })
        .collect()
}

/// The findings of a tester's result that must come with a regression test.
fn serious(result: &Value) -> Vec<&Value> {
    result
        .get("findings")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter(|finding| {
            matches!(
                finding.get("severity").and_then(Value::as_str),
                Some("critical" | "major")
            )
        })
        .collect()
}

/// How a step ended, as far as the flow is concerned.
enum StepEnd {
    Outcome(String),
    Park(String),
    /// It was waiting, and stopped waiting because the factory is about to
    /// restart. The step runs again when the unit is continued.
    Pause,
    /// It could not run because every account is at a usage limit until
    /// this time. Only when something else does the waiting: see
    /// [`Options::engine`].
    Limited(u64),
}

/// How one step of a unit ended, for whoever is walking the unit's flow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stepped {
    /// With an outcome for the flow to route.
    Outcome(String),
    /// The unit parked itself: something the flow has no route for.
    Parked(String),
    /// Every account is at a usage limit, for about this many seconds.
    Limited { secs: u64 },
    /// The factory is restarting. The step runs again.
    Paused,
}

/// The file whose presence asks the service to stop at a safe point: every
/// pipeline finishes the step it is in and starts no other, no agent is
/// woken, and the service then ends, to be started again by whatever keeps
/// it running. `factory service restart` makes it; the service removes it
/// when it starts.
pub fn drain_flag(state_dir: &Path) -> PathBuf {
    state_dir.join("drain")
}

/// Whether the service has been asked to stop at a safe point.
pub fn draining(state_dir: &Path) -> bool {
    drain_flag(state_dir).exists()
}

pub struct Options {
    /// Post to Slack. Off, the thread is written to the event log only.
    pub slack: bool,
    /// The pipeline to run instead of the project's own, when a goal names one.
    pub flow: Option<String>,
    /// The sub-goal this unit belongs to, if any.
    pub within: Option<Within>,
    /// Set when the agent that runs a sub-goal continues a pipeline that
    /// had parked or failed: what it wants the next step to know. The step
    /// it goes on from can then be left once more; routes used up elsewhere
    /// stay used up.
    pub continued: Option<String>,
    /// With `continued`: a decision about the bead that holds for every
    /// step from here on, and that each of them is shown.
    pub ruling: Option<String>,
    /// Stop between steps, with the unit left as running, when the service
    /// has been asked to stop at a safe point. For units the service runs:
    /// it continues them when it starts again.
    pub pausable: bool,
    /// Something else walks the unit's flow and calls for one step at a
    /// time: the engine. The unit then does no waiting of its own for a
    /// usage limit, and does not announce each time it is picked up.
    pub engine: bool,
    /// The account every run of this step is to use, when whoever walks
    /// the flow hands accounts out. The step then ends as limited if that
    /// account turns out to be at a limit, and picks no other itself.
    pub account: Option<String>,
}

/// Push a branch to the project's remote if its worktree is not at the
/// commit last pushed. `None` if the project does not keep its branches on
/// the remote, or there is nothing new. Otherwise the commit pushed, or why
/// it could not be. The write list is respected as it is for publishing.
fn push_if_moved(
    factory: &Factory,
    project: &Project,
    worktree: &Path,
    branch: &str,
    synced: Option<&str>,
) -> Option<Result<String, String>> {
    if !project.sync_branches {
        return None;
    }
    let head = factory_git::rev_parse(worktree, "HEAD").ok()?;
    if synced == Some(head.as_str()) {
        return None;
    }
    let push = || -> Result<(), String> {
        let url = factory_git::configured_remote_url(&project.repo, &project.remote)
            .map_err(|err| err.to_string())?;
        if let Some(github) = &project.github {
            let slug = factory_git::github_slug(&url).unwrap_or_default();
            github_client(factory, github)
                .check_allowed(&slug)
                .map_err(|err| format!("push to `{url}`: {err}"))?;
        }
        factory_git::push(worktree, &project.remote, branch).map_err(|err| err.to_string())
    };
    Some(push().map(|()| head))
}

/// Where a unit's work in hand is kept while its own branch has a pull
/// request open. Every push to that branch runs the project's whole CI, and
/// a commit made in the middle of a fix is not something to run it on.
pub fn holding_branch(branch: &str) -> String {
    format!("wip/{branch}")
}

/// Keep a worktree's work on the remote: on its own branch, or on its
/// holding branch once that branch has a pull request, which only
/// publishing may move. `synced` and `held` are the commits last pushed to
/// each. Returns the commit pushed and whether it went to the holding
/// branch, or `None` if there was nothing new.
fn keep_on_remote(
    factory: &Factory,
    project: &Project,
    worktree: &Path,
    branch: &str,
    (synced, held): (Option<&str>, Option<&str>),
    pull_request: bool,
) -> Option<Result<(String, bool), String>> {
    if !pull_request {
        return push_if_moved(factory, project, worktree, branch, synced)
            .map(|pushed| pushed.map(|commit| (commit, false)));
    }
    // What the pull request already shows needs no second copy.
    let head = factory_git::rev_parse(worktree, "HEAD").ok()?;
    if synced == Some(head.as_str()) {
        return None;
    }
    push_if_moved(factory, project, worktree, &holding_branch(branch), held)
        .map(|pushed| pushed.map(|commit| (commit, true)))
}

/// One absorb at a time, across every pipeline of the process: a sub-goal's
/// branch must not move between a unit reading where it is and adding to it.
static ABSORBING: Mutex<()> = Mutex::new(());

/// Whether `file` is one of `paths` or lies under one of them. Paths are
/// relative to the repository, with `/` between their parts.
pub fn under(file: &str, paths: &[String]) -> bool {
    paths.iter().any(|path| {
        let path = path.trim_end_matches('/');
        file == path
            || file
                .strip_prefix(path)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// How a unit relates to a goal's sub-goal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Within {
    /// One bead of the sub-goal: it branches from the sub-goal's tip and its
    /// flow ends by absorbing into the sub-goal's worktree.
    Bead {
        tip: String,
        worktree: PathBuf,
        /// Beads the sub-goal has absorbed: still open in the tracker, but
        /// done as far as this bead's blockers go.
        absorbed: Vec<String>,
        /// The files and directories set aside for it, when other beads of
        /// the sub-goal run beside it. Empty when it runs alone.
        paths: Vec<String>,
        /// The sub-goal's design as its director accepted it, when the
        /// goal has a designer.
        #[serde(default)]
        design: Option<String>,
    },
    /// The sub-goal itself, being shipped as one pull request.
    Ship(Ship),
}

/// A sub-goal's branch, ready to ship.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ship {
    /// Names the unit, as a bead's ID would.
    pub id: String,
    pub title: String,
    pub description: String,
    pub acceptance_criteria: String,
    pub worktree: PathBuf,
    pub branch: String,
    /// The commit of the default branch the sub-goal started from.
    pub base: String,
    /// The beads whose work it holds, closed when it lands.
    pub closes: Vec<String>,
    /// The GitHub issues of those beads, which its pull request must close.
    pub issues: Vec<u64>,
    /// The sub-goal's design as its director accepted it, when the goal
    /// has a designer.
    #[serde(default)]
    pub design: Option<String>,
}

pub struct Driver {
    config: ConfigDir,
    factory: Factory,
    project: Project,
    flow: Flow,
    bead: Bead,
    unit: Unit,
    dir: PathBuf,
    log: EventLog,
    thread: Thread,
    pausable: bool,
    paused: bool,
    engine: bool,
    account: Option<String>,
}

/// The flow a project names, checked against everything it may refer to.
pub fn load_flow(
    config: &ConfigDir,
    factory: &Factory,
    project: &Project,
) -> Result<(Flow, String), DriverError> {
    let text = config.flow_text(&project.flow)?;
    let flow = Flow::parse(&text)?;
    let mut models: std::collections::BTreeSet<String> =
        factory.allowed_models.iter().cloned().collect();
    models.extend(factory.model_override.values().cloned());
    flow.check(&Registry {
        roles: factory_brief::role_registry(),
        models,
        commands: project.commands.keys().cloned().collect(),
        scopes: project.scopes.keys().cloned().collect(),
    })?;
    // A role the flow names must have a prompt before anything starts, and a
    // command must write an artifact exactly when its step says it produces one.
    for step in &flow.steps {
        match &step.kind {
            StepKind::Agent { role, .. } => {
                config.role_prompt(role)?;
            }
            StepKind::Command { command, .. } => {
                let wants_out = project.commands[command].wants_out();
                if step.produces.is_some() && !wants_out {
                    return Err(DriverError::Preflight(format!(
                        "step `{}` produces an artifact, but the command `{command}` has no `{{out}}` in its argv to say where it goes",
                        step.id
                    )));
                }
                if step.produces.is_none() && wants_out {
                    return Err(DriverError::Preflight(format!(
                        "the command `{command}` writes to `{{out}}`, but step `{}` does not say what it produces",
                        step.id
                    )));
                }
            }
            StepKind::Commands { members } => {
                for member in members {
                    if project.commands[&member.command].wants_out() {
                        return Err(DriverError::Preflight(format!(
                            "the command `{}` writes to `{{out}}`, so it needs a step of its own to say what it produces, not a place in the group `{}`",
                            member.command, step.id
                        )));
                    }
                }
            }
            StepKind::Action { .. } => {}
        }
    }
    Ok((flow, text))
}

pub fn bead_source(project: &Project) -> Source {
    match &project.beads {
        BeadSource::Dolt {
            host,
            port,
            user,
            database,
        } => Source::Dolt {
            host: host.clone(),
            port: *port,
            user: user.clone(),
            database: database.clone(),
            repo: project.repo.clone(),
        },
        BeadSource::Files { dir } => Source::Files { dir: dir.clone() },
    }
}

/// One command a step runs, as the factory judges it: a command step's
/// own, or one of a group's.
struct Check<'a> {
    step: &'a Step,
    /// What its notes, its failures and its output files are kept under,
    /// and what the thread calls it: the step's ID for a command step, and
    /// `<step>.<command>` for a command of a group.
    key: String,
    name: &'a str,
    policy: ExitPolicy,
    produces: Option<&'a String>,
    when_changed: &'a [String],
}

/// A command of a group, while the group runs.
struct GroupRun {
    key: String,
    name: String,
    command: factory_config::CommandSpec,
    argv: Vec<String>,
    result: Option<CommandResult>,
}

/// A command step's run that failed, as the factory looks into it.
struct Failed<'a> {
    /// What it is kept under: see [`Check::key`].
    key: &'a str,
    name: &'a str,
    command: &'a factory_config::CommandSpec,
    argv: &'a [String],
    result: &'a CommandResult,
    /// How it failed, as the thread says it.
    how: &'a str,
    /// Where its output was kept.
    output: &'a Path,
}

/// What looking into a failure came to.
enum Triaged {
    /// The step ends here, not at a builder: passed as a flake, or parked.
    End(StepEnd),
    /// The failure goes to the builder, with what was found on the way.
    Found(Vec<String>),
}

/// What a run on the base said.
enum AtBase {
    /// It failed there too: the park reason.
    FailsToo(String),
    /// Something for the builder to know; the failure stays the change's.
    Found(String),
}

fn verdict_text(verdict: &Verdict) -> String {
    match verdict {
        Verdict::Passed => "passed".to_string(),
        Verdict::Failed { code } => format!(
            "failed (exit code {})",
            code.map_or("none".to_string(), |c| c.to_string())
        ),
        Verdict::TimedOut => "timed out".to_string(),
        Verdict::CouldNotRun(why) => format!("could not run ({why})"),
    }
}

fn short_commit(commit: &str) -> &str {
    &commit[..commit.len().min(10)]
}

fn duration_text(seconds: u64) -> String {
    match (seconds / 3600, (seconds % 3600) / 60, seconds % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, s) => format!("{m}m {s}s"),
        (h, m, _) => format!("{h}h {m}m"),
    }
}

fn tokens_text(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1e6)
    } else if tokens >= 1000 {
        format!("{:.1}k", tokens as f64 / 1e3)
    } else {
        tokens.to_string()
    }
}

/// The account's usage windows, such as `5h 23% · 7d 44%`.
fn windows_text(usage: &Usage) -> String {
    let Some(limit) = &usage.rate_limit else {
        return String::new();
    };
    limit
        .windows
        .iter()
        .map(|(name, window)| {
            let short = match name.as_str() {
                "five_hour" => "5h",
                "seven_day" => "7d",
                other => other,
            };
            format!("{short} {:.0}%", window.utilization * 100.0)
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// A table of what each step used, for the end of a run.
pub fn usage_report(unit: &Unit, flow: &Flow) -> String {
    let mut lines = vec![format!(
        "{:<10} {:>4} {:>9} {:>6} {:>9} {:>9} {:>9}",
        "step", "runs", "time", "turns", "out tok", "cache rd", "est. $"
    )];
    let mut total = StepUsage::default();
    let mut models: BTreeMap<String, f64> = BTreeMap::new();
    for step in &flow.steps {
        let Some(used) = unit.usage.get(&step.id) else {
            continue;
        };
        let cache_read: u64 = used
            .usage
            .models
            .values()
            .map(|m| m.cache_read_tokens)
            .sum();
        let is_agent = matches!(step.kind, StepKind::Agent { .. });
        lines.push(format!(
            "{:<10} {:>4} {:>9} {:>6} {:>9} {:>9} {:>9}",
            step.id,
            used.runs,
            duration_text(used.wall_secs),
            if is_agent {
                used.usage.turns.to_string()
            } else {
                "-".to_string()
            },
            if is_agent {
                tokens_text(used.usage.output_tokens())
            } else {
                "-".to_string()
            },
            if is_agent {
                tokens_text(cache_read)
            } else {
                "-".to_string()
            },
            if is_agent {
                format!("{:.2}", used.usage.cost_usd)
            } else {
                "-".to_string()
            },
        ));
        total.runs += used.runs;
        total.wall_secs += used.wall_secs;
        total.usage.add(&used.usage);
        for (model, usage) in &used.usage.models {
            *models.entry(model.clone()).or_insert(0.0) += usage.cost_usd;
        }
    }
    let cache_read: u64 = total
        .usage
        .models
        .values()
        .map(|m| m.cache_read_tokens)
        .sum();
    lines.push(format!(
        "{:<10} {:>4} {:>9} {:>6} {:>9} {:>9} {:>9}",
        "total",
        total.runs,
        duration_text(total.wall_secs),
        total.usage.turns,
        tokens_text(total.usage.output_tokens()),
        tokens_text(cache_read),
        format!("{:.2}", total.usage.cost_usd),
    ));
    for (model, cost) in &models {
        lines.push(format!("model {model}: est. ${cost:.2}"));
    }
    let windows = windows_text(&total.usage);
    if !windows.is_empty() {
        lines.push(format!("account windows now: {windows}"));
    }
    lines.push(
        "est. $ is the list price of the tokens, not what the subscription is billed.".to_string(),
    );
    lines.join("\n")
}

impl Driver {
    /// Prepare a new unit for a bead: check it may be launched, claim it, and
    /// give it a worktree. Nothing here involves a model.
    pub fn start(
        config: ConfigDir,
        project_id: &str,
        bead_id: &str,
        options: &Options,
    ) -> Result<Driver, DriverError> {
        let factory = config.factory()?;
        let mut project = config.project(project_id)?;
        if let Some(flow) = &options.flow {
            project.flow = flow.clone();
        }
        let (flow, flow_text) = load_flow(&config, &factory, &project)?;
        preflight(&factory, &project)?;

        let source = bead_source(&project);
        let ship = match &options.within {
            Some(Within::Ship(ship)) => Some(ship.clone()),
            _ => None,
        };
        let bead = match &ship {
            // A sub-goal is not a bead in the tracker: it stands in for one.
            Some(ship) => Bead {
                id: ship.id.clone(),
                title: ship.title.clone(),
                description: ship.description.clone(),
                acceptance_criteria: ship.acceptance_criteria.clone(),
                status: "open".to_string(),
                issue_type: "feature".to_string(),
                ..Bead::default()
            },
            None => {
                let mut bead = source.read(bead_id)?;
                if let Some(Within::Bead { absorbed, .. }) = &options.within {
                    bead.open_blockers
                        .retain(|blocker| !absorbed.contains(blocker));
                }
                let reasons = readiness(&bead, &project.bead_actor);
                if !reasons.is_empty() {
                    return Err(DriverError::NotLaunchable {
                        bead: bead.id,
                        reasons,
                    });
                }
                bead
            }
        };

        let started_at = now_unix();
        let unit_id = UnitId::new(format!("{}-{started_at}", bead.id));
        let dir = factory.state_dir.join("units").join(unit_id.as_str());
        for sub in ["runs", "artifacts"] {
            let path = dir.join(sub);
            std::fs::create_dir_all(&path).map_err(io(&path))?;
        }
        let pinned = dir.join("flow.toml");
        std::fs::write(&pinned, &flow_text).map_err(io(&pinned))?;
        let log = EventLog::new(
            dir.join("events.jsonl"),
            project.id.clone(),
            unit_id.clone(),
        );

        let branch = factory_git::branch_name(
            &project.branch_prefix(&bead.issue_type),
            project.bead_short(&bead.id),
            &bead.title,
        );
        let worktree = factory.state_dir.join("worktrees").join(unit_id.as_str());
        let mut driver = Driver {
            thread: Thread::disabled(log.clone()),
            unit: Unit {
                id: unit_id,
                project: project.id.clone(),
                bead: bead.id.clone(),
                title: bead.title.clone(),
                flow: flow.id.clone(),
                branch,
                base: String::new(),
                checkpoint: String::new(),
                worktree,
                position: Position::start(&flow),
                status: Status::Running,
                sessions: BTreeMap::new(),
                in_step: None,
                feedback: None,
                notes: BTreeMap::new(),
                artifacts: BTreeMap::new(),
                results: BTreeMap::new(),
                usage: BTreeMap::new(),
                thread: None,
                owner_messages: Vec::new(),
                answer: None,
                pull_request: None,
                pull_request_number: None,
                merged: None,
                absorb_into: None,
                absorbed: None,
                synced: None,
                held: None,
                ship: false,
                design: None,
                closes: Vec::new(),
                issues: Vec::new(),
                regressions: Vec::new(),
                history: Vec::new(),
                declared_paths: Vec::new(),
                strayed: Vec::new(),
                trespassed: BTreeMap::new(),
                rulings: Vec::new(),
                failures: BTreeMap::new(),
                flakes: Vec::new(),
                ci: None,
                conflicts: Vec::new(),
                runs: 0,
                started_at,
            },
            config,
            factory,
            project,
            flow,
            bead,
            dir,
            log,
            pausable: options.pausable,
            paused: false,
            engine: options.engine,
            account: options.account.clone(),
        };
        driver.event(
            "unit.created",
            None,
            json!({
                "bead": driver.unit.bead, "flow": driver.unit.flow, "branch": driver.unit.branch,
            }),
        )?;
        match &options.within {
            Some(Within::Bead {
                tip,
                worktree,
                paths,
                design,
                ..
            }) => {
                driver.unit.design = design.clone();
                driver.unit.base = tip.clone();
                driver.unit.absorb_into = Some(worktree.clone());
                if !paths.is_empty() {
                    driver.unit.declared_paths = paths.clone();
                    driver.unit.history.push(format!(
                        "This bead is being built at the same time as others of its sub-goal, each in a worktree of its own. The paths set aside for this one: {}. Keep your changes inside them wherever the bead allows. A file you change outside them may be one another bead is changing too, and the two will then have to be merged.",
                        paths.join(", ")
                    ));
                }
            }
            Some(Within::Ship(ship)) => {
                driver.unit.ship = true;
                driver.unit.design = ship.design.clone();
                driver.unit.issues = ship.issues.clone();
                driver.unit.closes = ship.closes.clone();
                driver.unit.worktree = ship.worktree.clone();
                driver.unit.branch = ship.branch.clone();
                driver.unit.base = ship.base.clone();
                driver.unit.checkpoint = factory_git::rev_parse(&ship.worktree, "HEAD")?;
                // Kept beside the unit, since the tracker has no such bead.
                let path = driver.dir.join("bead.json");
                let text = serde_json::to_string_pretty(&driver.bead).map_err(|source| {
                    DriverError::Json {
                        path: path.clone(),
                        source,
                    }
                })?;
                std::fs::write(&path, text).map_err(io(&path))?;
            }
            None => {}
        }
        driver.thread = driver.open_thread(options, None);
        driver.unit.thread = driver.thread.state().cloned();
        driver.save()?;
        if driver.unit.ship {
            let commits =
                factory_git::commit_count(&driver.unit.worktree, &driver.unit.base, "HEAD")?;
            driver.thread.say(&format!(
                ":package: *Shipping* · branch `{}` holds {commits} commit(s) for {} bead(s). One pull request, squashed when it lands.",
                escape(&driver.unit.branch),
                driver.unit.closes.len()
            ));
            return Ok(driver);
        }

        // From here the unit exists; a failure parks it instead of losing it.
        if let Err(err) = driver.prepare(&source) {
            let reason = format!("could not prepare the unit: {err}");
            driver.park(&reason)?;
        }
        Ok(driver)
    }

    /// Pick up a unit that was stopped or parked.
    ///
    /// `from` is the owner's override: put the unit at that step of the
    /// project's flow as it is now, whatever flow the unit started with and
    /// even if it had ended. It is how a unit is moved onto a changed flow.
    pub fn resume(
        config: ConfigDir,
        unit_id: &str,
        options: &Options,
        from: Option<&str>,
    ) -> Result<Driver, DriverError> {
        let factory = config.factory()?;
        let dir = factory.state_dir.join("units").join(unit_id);
        let state = dir.join("unit.json");
        let text = std::fs::read_to_string(&state).map_err(io(&state))?;
        let mut unit: Unit = serde_json::from_str(&text).map_err(|source| DriverError::Json {
            path: state.clone(),
            source,
        })?;
        if unit.status == Status::Ended && from.is_none() {
            return Err(DriverError::AlreadyEnded(unit_id.to_string()));
        }
        let project = config.project(unit.project.as_str())?;
        let pinned = dir.join("flow.toml");
        let flow = match from {
            // A unit runs on the flow it started with, not on what the file says now.
            None => Flow::parse(&std::fs::read_to_string(&pinned).map_err(io(&pinned))?)?,
            Some(step) => {
                // The current flow is the goal's own pipeline, when a goal
                // runs this unit, and otherwise the project's.
                let mut with_flow = project.clone();
                if let Some(flow) = &options.flow {
                    with_flow.flow = flow.clone();
                }
                let (flow, text) = load_flow(&config, &factory, &with_flow)?;
                if flow.step(step).is_none() {
                    return Err(factory_flow::FlowError::UnknownStep(step.to_string()).into());
                }
                std::fs::write(&pinned, text).map_err(io(&pinned))?;
                unit.flow = flow.id.clone();
                unit.position.step = step.to_string();
                *unit.position.rounds.entry(step.to_string()).or_insert(0) += 1;
                unit.in_step = None;
                // Whoever moves a unit by hand has a reason to try again: a
                // failure seen before is no reason to stop this time.
                unit.failures.clear();
                flow
            }
        };
        preflight(&factory, &project)?;
        let bead = match std::fs::read_to_string(dir.join("bead.json")) {
            // A unit that ships a sub-goal carries its own stand-in for a bead.
            Ok(text) => serde_json::from_str(&text).map_err(|source| DriverError::Json {
                path: dir.join("bead.json"),
                source,
            })?,
            Err(_) => bead_source(&project).read(&unit.bead)?,
        };
        let log = EventLog::new(
            dir.join("events.jsonl"),
            project.id.clone(),
            unit.id.clone(),
        );
        if let Some(note) = &options.continued {
            // Whoever runs the sub-goal judged this pipeline worth going on
            // with: the step it continues from can be left once more, and
            // is told why. A route used up elsewhere stays used up, so the
            // next time the tester breaks the change, say, it comes
            // straight back to be looked at.
            unit.position.reopen(&flow);
            if let Some(ruling) = options.ruling.as_deref().map(str::trim)
                && !ruling.is_empty()
                && !unit.rulings.iter().any(|known| known == ruling)
            {
                unit.rulings.push(ruling.to_string());
            }
            let stopped = match &unit.status {
                Status::Parked(reason) => format!("It had parked: {reason}\n\n"),
                _ => String::new(),
            };
            let before = unit
                .feedback
                .take()
                .map(|feedback| format!("{feedback}\n\n"))
                .unwrap_or_default();
            unit.feedback = Some(format!(
                "{before}This pipeline stopped and has been continued by the agent that runs its sub-goal. {stopped}What it says to you:\n\n{note}"
            ));
        }
        unit.status = Status::Running;
        let saved_thread = unit.thread.clone();
        let mut driver = Driver {
            thread: Thread::disabled(log.clone()),
            config,
            factory,
            project,
            flow,
            bead,
            unit,
            dir,
            log,
            pausable: options.pausable,
            paused: false,
            engine: options.engine,
            account: options.account.clone(),
        };
        driver.thread = driver.open_thread(options, saved_thread);
        // Picked up once per step by the engine: that is not news.
        if !options.engine {
            driver.event(
                if from.is_some() {
                    "unit.redirected"
                } else {
                    "unit.resumed"
                },
                None,
                json!({ "step": driver.unit.position.step, "flow": driver.unit.flow }),
            )?;
            driver.thread.say(&format!(
                ":arrow_forward: Continuing from step *{}*.",
                driver.unit.position.step
            ));
        }
        driver.save()?;
        Ok(driver)
    }

    pub fn unit(&self) -> &Unit {
        &self.unit
    }

    /// A unit's record as it stands, whatever state the unit is in.
    pub fn read_unit(config: &ConfigDir, unit_id: &str) -> Result<Unit, DriverError> {
        let factory = config.factory()?;
        let state = factory
            .state_dir
            .join("units")
            .join(unit_id)
            .join("unit.json");
        let text = std::fs::read_to_string(&state).map_err(io(&state))?;
        serde_json::from_str(&text).map_err(|source| DriverError::Json {
            path: state,
            source,
        })
    }

    /// Whether `run` returned because the service is stopping at a safe
    /// point. The unit is still running as far as its record goes, and is
    /// continued from the step it was about to start.
    pub fn paused(&self) -> bool {
        self.paused
    }

    fn pause_wanted(&self) -> bool {
        self.pausable && draining(&self.factory.state_dir)
    }

    fn pause(&mut self) -> Result<Status, DriverError> {
        let step = self.unit.position.step.clone();
        self.event("unit.paused", Some(&step), json!({}))?;
        self.thread.say(&format!(
            ":double_vertical_bar: Paused before *{step}*: the factory is restarting. It continues by itself."
        ));
        self.paused = true;
        self.save()?;
        Ok(Status::Running)
    }

    pub fn flow(&self) -> &Flow {
        &self.flow
    }

    pub fn event_log(&self) -> &Path {
        self.log.path()
    }

    fn header(&self) -> String {
        format!(
            ":factory: *{}* · {}\n_Run `{}` · flow `{}` · reply here to steer, `stop` ends the run_",
            escape(&self.unit.bead),
            escape(&self.unit.title),
            self.unit.id,
            self.unit.flow,
        )
    }

    fn open_thread(&self, options: &Options, saved: Option<ThreadState>) -> Thread {
        let (Some(settings), Some(channel), true) = (
            &self.factory.slack,
            &self.project.slack_channel,
            options.slack,
        ) else {
            return Thread::disabled(self.log.clone());
        };
        let env = match read_env_file(&settings.env_file) {
            Ok(env) => env,
            Err(err) => {
                let _ = self.log.append(
                    "slack.failed",
                    None,
                    json!({ "doing": "read tokens", "error": err.to_string() }),
                );
                return Thread::disabled(self.log.clone());
            }
        };
        let Some(token) = env.get("SLACK_BOT_TOKEN") else {
            let _ = self.log.append(
                "slack.failed",
                None,
                json!({ "doing": "read tokens", "error": "no SLACK_BOT_TOKEN" }),
            );
            return Thread::disabled(self.log.clone());
        };
        let owners: Vec<String> = env
            .get("SLACK_OWNER_USER_ID")
            .map(|ids| {
                ids.split(',')
                    .map(|id| id.trim().to_string())
                    .filter(|id| !id.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        // SLACK_API_URL exists for tests, which point it at a local stand-in.
        let slack = match env.get("SLACK_API_URL") {
            Some(url) => Slack::with_base_url(token.clone(), url.clone()),
            None => Slack::new(token.clone()),
        };
        match saved {
            Some(state) => Thread::resume(slack, state, owners, self.header(), self.log.clone()),
            None => Thread::open(slack, channel, owners, self.header(), self.log.clone()),
        }
    }

    fn event(&self, kind: &str, step: Option<&str>, data: Value) -> Result<(), DriverError> {
        let step = step.map(StepId::new);
        Ok(self.log.append(kind, step.as_ref(), data)?)
    }

    /// Have the classifier say what kind of failure `output` shows, and
    /// keep what it said in the unit's log. It is a shadow: nothing is
    /// routed by it, and a key that is missing, a refusal or a reading
    /// that is late is recorded and holds nothing up. Without a `[judge]`
    /// table in the factory's settings nothing is sent anywhere.
    fn judge(&self, step: &str, what: Value, preface: &str, output: &str, patterns: &[String]) {
        let Some(settings) = &self.factory.judge else {
            return;
        };
        let mut data = what;
        let failed = |why: String| json!({ "why": why });
        let key = factory_config::read_env_file(&settings.env_file)
            .map_err(|error| error.to_string())
            .and_then(|env| {
                env.get(&settings.key_env)
                    .filter(|key| !key.trim().is_empty())
                    .cloned()
                    .ok_or_else(|| {
                        format!(
                            "{} has no {}",
                            settings.env_file.display(),
                            settings.key_env
                        )
                    })
            });
        let home = std::env::var("HOME").ok();
        let reading = key.and_then(|key| {
            let excerpt = factory_checks::judge::failure_excerpt(output, patterns, home.as_deref());
            factory_checks::judge::Judge::new(
                &settings.url,
                &settings.model,
                key,
                Duration::from_secs(settings.timeout_secs),
            )
            .kind(preface, &excerpt)
            .map_err(|error| error.to_string())
        });
        let (kind, said) = match reading {
            Ok(judged) => (
                "failure.judged",
                json!({
                    "kind": judged.kind, "confidence": judged.confidence,
                    "probabilities": judged.probabilities.into_iter().collect::<BTreeMap<String, f64>>(),
                    "model": judged.model, "millis": judged.millis,
                }),
            ),
            Err(why) => ("failure.judge_failed", failed(why)),
        };
        if let (Some(data), Some(said)) = (data.as_object_mut(), said.as_object()) {
            data.extend(said.clone());
        }
        // The log is where it goes; if that cannot be written the step
        // will say so itself.
        let _ = self.event(kind, Some(step), data);
    }

    fn save(&mut self) -> Result<(), DriverError> {
        self.unit.thread = self.thread.state().cloned().or(self.unit.thread.take());
        let path = self.dir.join("unit.json");
        let text =
            serde_json::to_string_pretty(&self.unit).map_err(|source| DriverError::Json {
                path: path.clone(),
                source,
            })?;
        // Written whole, then moved into place: a crash leaves the old file.
        let temp = self.dir.join("unit.json.tmp");
        std::fs::write(&temp, text).map_err(io(&temp))?;
        std::fs::rename(&temp, &path).map_err(io(&path))
    }

    fn artifact(&self, name: &str, content: &str) -> Result<PathBuf, DriverError> {
        let path = self
            .dir
            .join("artifacts")
            .join(format!("{:03}-{name}", self.unit.runs));
        std::fs::write(&path, content).map_err(io(&path))?;
        Ok(path)
    }

    /// The deterministic steps before any agent: claim, fetch, worktree, setup.
    fn prepare(&mut self, source: &Source) -> Result<(), DriverError> {
        self.thread
            .status(":gear: preparing: claim, worktree, setup");
        source.claim(&self.unit.bead, &self.project.bead_actor)?;
        self.event(
            "bead.claimed",
            None,
            json!({ "actor": self.project.bead_actor }),
        )?;
        // The bead's GitHub issue, which the pull request will close. A bead
        // that has none gets one now.
        if let Some(url) = ensure_issue(&self.factory, &self.project, source, &self.bead)? {
            if self.bead.external_ref != url {
                self.event("issue.created", None, json!({ "url": url }))?;
            }
            self.bead.external_ref = url;
            // Its type and priority on GitHub follow the bead's, each time
            // work starts on it. A failure here does not hold the work up.
            match sync_issue_fields(&self.factory, &self.project, &self.bead) {
                Ok(changed) if changed.is_empty() => {}
                Ok(changed) => self.event("issue.fields", None, json!({ "set": changed }))?,
                Err(err) => self.event(
                    "issue.fields_failed",
                    None,
                    json!({ "error": err.to_string() }),
                )?,
            }
        }
        if let Some(github) = &self.project.github
            && let Some(number) =
                factory_github::issue_number(&self.bead.external_ref, &github.repo)
        {
            self.unit.issues = vec![number];
        }

        let repo = self.project.repo.clone();
        let base = if self.unit.base.is_empty() {
            factory_git::fetch(&repo, &self.project.remote)?;
            let start = format!("{}/{}", self.project.remote, self.project.default_branch);
            factory_git::rev_parse(&repo, &start)?
        } else {
            // A bead of a sub-goal starts from the sub-goal's tip.
            self.unit.base.clone()
        };
        if factory_git::branch_exists(&repo, &self.unit.branch)? {
            return Err(DriverError::Preflight(format!(
                "the branch `{}` already exists in {}: an earlier unit for this bead was not cleaned up",
                self.unit.branch,
                repo.display()
            )));
        }
        if let Some(parent) = self.unit.worktree.parent() {
            std::fs::create_dir_all(parent).map_err(io(parent))?;
        }
        factory_git::worktree_add(&repo, &self.unit.worktree, &self.unit.branch, &base)?;
        self.unit.base = base.clone();
        self.unit.checkpoint = base.clone();
        self.event(
            "worktree.created",
            None,
            json!({
                "path": self.unit.worktree, "branch": self.unit.branch, "base": base,
            }),
        )?;
        self.save()?;

        let worktree = self.unit.worktree.clone();
        self.run_setup(&worktree)?;
        self.thread.say(&format!(
            ":gear: *Prepared* · bead claimed as `{}` · branch `{}` from `{}` at `{}`",
            escape(&self.project.bead_actor),
            escape(&self.unit.branch),
            escape(&self.project.default_branch),
            &base[..base.len().min(10)],
        ));
        Ok(())
    }

    /// Run the project's setup commands in a new worktree.
    fn run_setup(&mut self, worktree: &Path) -> Result<(), DriverError> {
        for name in self.project.setup.clone() {
            let command = &self.project.commands[&name];
            let result = run_command(
                &command.argv,
                worktree,
                Duration::from_secs(command.timeout_secs),
            );
            self.event(
                "setup.ran",
                None,
                json!({
                    "command": name, "worktree": worktree,
                    "verdict": format!("{:?}", result.verdict), "secs": result.duration.as_secs(),
                }),
            )?;
            if result.verdict != Verdict::Passed {
                return Err(DriverError::Preflight(format!(
                    "setup command `{name}` did not pass ({:?}):\n{}",
                    result.verdict,
                    tail(&result.output, 20)
                )));
            }
        }
        Ok(())
    }

    fn park(&mut self, reason: &str) -> Result<Status, DriverError> {
        self.unit.status = Status::Parked(reason.to_string());
        self.event(
            "unit.parked",
            Some(&self.unit.position.step.clone()),
            json!({ "reason": reason }),
        )?;
        self.thread.say(&format!(
            ":double_vertical_bar: *Parked* at step *{}*\n{}\nEverything is kept. Continue with `factory run --unit {}`.\n{}",
            self.unit.position.step,
            quote(&truncate(reason, 1500)),
            self.unit.id,
            code_block(&usage_report(&self.unit, &self.flow)),
        ));
        self.thread.status(&format!(
            ":double_vertical_bar: parked at *{}*: {}",
            self.unit.position.step,
            escape(&truncate(reason.lines().next().unwrap_or(""), 200))
        ));
        self.save()?;
        Ok(self.unit.status.clone())
    }

    /// Hand new owner messages to the unit. Returns true if the owner said stop.
    fn hear_owner(&mut self) -> bool {
        let mut stop = false;
        for message in self.thread.poll() {
            if is_stop(&message) {
                stop = true;
            } else {
                self.unit.owner_messages.push(message);
            }
        }
        stop
    }

    /// Walk the unit through its flow until it ends or parks.
    pub fn run(&mut self) -> Result<Status, DriverError> {
        if let Status::Parked(_) = self.unit.status {
            return Ok(self.unit.status.clone());
        }
        loop {
            if self.hear_owner() {
                return self.park("the owner said stop");
            }
            if self.pause_wanted() {
                return self.pause();
            }
            let step_id = self.unit.position.step.clone();
            let (step, round, started, end) = self.perform(&step_id)?;
            let outcome = match end {
                StepEnd::Park(reason) => return self.park(&reason),
                StepEnd::Pause => return self.pause(),
                // Only a unit walked by the engine leaves the waiting to it.
                StepEnd::Limited(_) => {
                    return self.park("every account is at a usage limit");
                }
                StepEnd::Outcome(outcome) => outcome,
            };
            self.remember(&step, round, &outcome);
            let next = self.unit.position.advance(&self.flow, &outcome)?;
            self.event(
                "step.ended",
                Some(&step_id),
                json!({
                    "round": round, "outcome": outcome, "next": format!("{next:?}"),
                    "secs": started.elapsed().as_secs(),
                }),
            )?;
            self.save()?;
            match next {
                Next::Step(_) => {}
                Next::Ask => match self.ask_owner(&step_id)? {
                    Some(answer) => {
                        self.unit.answer = Some(answer);
                        self.save()?;
                    }
                    None => {
                        return self.park(&format!(
                            "step `{step_id}` asked the owner a question and got no answer:\n{}",
                            self.question(&step_id)
                        ));
                    }
                },
                Next::Park(reason) => {
                    let detail = self.unit.feedback.clone().unwrap_or_default();
                    return self.park(&format!("{reason}\n{}", truncate(&detail, 1200)));
                }
                Next::End => return self.end_unit(),
            }
        }
    }

    /// Run one step of the flow, whichever the caller names, and account
    /// for the time it took. Returns the step, which round of it this was,
    /// when it started, and how it ended.
    fn perform(&mut self, step_id: &str) -> Result<(Step, u32, Instant, StepEnd), DriverError> {
        let step = self
            .flow
            .step(step_id)
            .ok_or_else(|| factory_flow::FlowError::UnknownStep(step_id.to_string()))?
            .clone();
        let round = self.unit.position.round();
        self.event("step.started", Some(step_id), json!({ "round": round }))?;
        let started = Instant::now();
        let end = match &step.kind {
            StepKind::Agent { .. } => self.agent_step(&step)?,
            StepKind::Command { .. } => self.command_step(&step)?,
            StepKind::Commands { .. } => self.commands_step(&step)?,
            StepKind::Action { action } => match action.as_str() {
                "await-ci" => self.await_ci_step(&step)?,
                "land" => self.land_step(&step)?,
                "sync" => self.sync_step(&step)?,
                _ => self.publish_step(&step)?,
            },
        };
        let used = self.unit.usage.entry(step_id.to_string()).or_default();
        used.wall_secs += started.elapsed().as_secs();
        if !matches!(step.kind, StepKind::Agent { .. }) {
            used.runs += 1;
        }
        // Whatever the step left on the branch goes to the remote.
        self.sync_branch(step_id);
        Ok((step, round, started, end))
    }

    /// Push the unit's branch to the remote if it has moved, for a project
    /// that keeps its branches there. A failure is recorded and holds
    /// nothing up: the work is still here.
    fn sync_branch(&mut self, step: &str) {
        // A unit that was absorbed or landed has no branch left to keep.
        if self.unit.absorbed.is_some()
            || self.unit.merged.is_some()
            || !self.unit.worktree.exists()
        {
            return;
        }
        let pushed = keep_on_remote(
            &self.factory,
            &self.project,
            &self.unit.worktree,
            &self.unit.branch,
            (self.unit.synced.as_deref(), self.unit.held.as_deref()),
            self.unit.pull_request.is_some(),
        );
        let _ = match pushed {
            None => Ok(()),
            Some(Ok((commit, held))) => {
                let branch = if held {
                    self.unit.held = Some(commit.clone());
                    holding_branch(&self.unit.branch)
                } else {
                    self.unit.synced = Some(commit.clone());
                    self.unit.branch.clone()
                };
                self.event(
                    "branch.synced",
                    Some(step),
                    json!({ "branch": branch, "commit": commit, "when": "after the step" }),
                )
            }
            Some(Err(error)) => self.event(
                "branch.sync_failed",
                Some(step),
                json!({ "branch": self.unit.branch, "error": error }),
            ),
        };
    }

    /// Remove the unit's holding branch from the remote, if it has one. A
    /// failure leaves a branch behind and holds nothing up.
    fn drop_holding(&mut self, step: &str) {
        if self.unit.held.take().is_none() {
            return;
        }
        let branch = holding_branch(&self.unit.branch);
        let gone =
            factory_git::delete_remote_branch(&self.project.repo, &self.project.remote, &branch);
        let _ = self.event(
            "branch.unheld",
            Some(step),
            json!({ "branch": branch, "error": gone.err().map(|err| err.to_string()) }),
        );
    }

    /// Add a step that ended to what a fresh agent is told has happened.
    fn remember(&mut self, step: &Step, round: u32, outcome: &str) {
        let step_id = &step.id;
        let mut line = format!("{step_id} (round {round}): {outcome}");
        if matches!(step.kind, StepKind::Agent { .. })
            && let Some(findings) = self
                .unit
                .results
                .get(step_id)
                .and_then(|result| result.get("findings"))
                .and_then(Value::as_array)
                .filter(|findings| !findings.is_empty())
        {
            let titles: Vec<String> = findings
                .iter()
                .take(8)
                .map(|finding| {
                    format!(
                        "[{}] {}",
                        finding
                            .get("severity")
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                        truncate(
                            finding.get("title").and_then(Value::as_str).unwrap_or(""),
                            160
                        )
                    )
                })
                .collect();
            line.push_str(&format!(". Findings: {}", titles.join("; ")));
        }
        self.unit.history.push(line);
    }

    /// The flow reached its end: record it and say so.
    fn end_unit(&mut self) -> Result<Status, DriverError> {
        self.unit.status = Status::Ended;
        self.event(
            "unit.ended",
            None,
            json!({ "pull_request": self.unit.pull_request }),
        )?;
        let report = usage_report(&self.unit, &self.flow);
        let (done, status) = match (&self.unit.merged, &self.unit.pull_request) {
            (Some(commit), Some(url)) => (
                format!(
                    ":tada: *Landed.* {} is merged into `{}` as `{}`, and the bead is closed.",
                    link(url, "The pull request"),
                    escape(&self.project.default_branch),
                    &commit[..commit.len().min(10)]
                ),
                format!(":white_check_mark: landed: {}", link(url, url)),
            ),
            (_, Some(url)) => (
                format!(
                    ":tada: *Done.* Pull request: {}\nThe bead stays claimed by `{}` until it merges.",
                    link(url, url),
                    escape(&self.project.bead_actor)
                ),
                format!(":white_check_mark: pull request opened: {}", link(url, url)),
            ),
            _ => (
                format!(
                    ":tada: *Done.* Branch `{}` is pushed.",
                    escape(&self.unit.branch)
                ),
                ":white_check_mark: done".to_string(),
            ),
        };
        self.thread.say(&format!("{done}\n{}", code_block(&report)));
        self.thread.status(&status);
        self.save()?;
        Ok(Status::Ended)
    }

    /// Run exactly one step, the one named, as its `round`th time, and say
    /// how it ended. For a unit whose flow something else walks: where the
    /// outcome leads is the caller's to decide, and nothing is advanced
    /// here. The unit's record says which step it was last given.
    pub fn run_step(&mut self, step_id: &str, round: u32) -> Result<Stepped, DriverError> {
        self.run_step_at(step_id, round, None)
    }

    /// The same, and with how often each bounded route has been taken so
    /// far, as whoever walks the flow counts it. The unit's record then
    /// says where it stands in full, and something else can take the unit
    /// up from the record alone.
    pub fn run_step_at(
        &mut self,
        step_id: &str,
        round: u32,
        used: Option<&BTreeMap<String, u32>>,
    ) -> Result<Stepped, DriverError> {
        if let Some(used) = used {
            self.unit.position.used = used.clone();
        }
        self.unit.position.step = step_id.to_string();
        self.unit
            .position
            .rounds
            .insert(step_id.to_string(), round.max(1));
        self.unit.status = Status::Running;
        let (step, round, started, end) = self.perform(step_id)?;
        let stepped = match end {
            StepEnd::Park(reason) => {
                self.park(&reason)?;
                Stepped::Parked(reason)
            }
            StepEnd::Pause => Stepped::Paused,
            StepEnd::Limited(until) => Stepped::Limited {
                secs: until.saturating_sub(now_unix()).max(1),
            },
            StepEnd::Outcome(outcome) => {
                self.remember(&step, round, &outcome);
                self.event(
                    "step.ended",
                    Some(step_id),
                    json!({
                        "round": round, "outcome": outcome, "next": "the engine decides",
                        "secs": started.elapsed().as_secs(),
                    }),
                )?;
                Stepped::Outcome(outcome)
            }
        };
        self.save()?;
        Ok(stepped)
    }

    /// Record that whoever walks the flow has parked the unit, with the
    /// reason, and say so in its thread.
    pub fn record_park(&mut self, reason: &str) -> Result<(), DriverError> {
        let detail = self.unit.feedback.clone().unwrap_or_default();
        self.park(&format!("{reason}\n{}", truncate(&detail, 1200)))
            .map(|_| ())
    }

    /// Record that the flow reached its end.
    pub fn finish(&mut self) -> Result<(), DriverError> {
        self.end_unit().map(|_| ())
    }

    fn question(&self, step: &str) -> String {
        self.unit
            .results
            .get(step)
            .and_then(|result| result.get("question"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    }

    /// Put a step's question to the owner and wait for the reply.
    fn ask_owner(&mut self, step: &str) -> Result<Option<String>, DriverError> {
        let question = self.question(step);
        self.event("owner.asked", Some(step), json!({ "question": question }))?;
        if !self.thread.live() {
            return Ok(None);
        }
        self.thread.say(&format!(
            ":question: {} step *{step}* needs an answer before it can go on:\n{}\n_Reply in this thread._",
            self.thread.owner_mentions(),
            quote(&question)
        ));
        self.thread
            .status(&format!(":question: *{step}* is waiting for your answer"));
        let settings = self.factory.slack.clone();
        let poll = Duration::from_secs(settings.as_ref().map_or(10, |s| s.poll_secs).max(1));
        let limit = Duration::from_secs(settings.as_ref().map_or(43_200, |s| s.answer_wait_secs));
        let started = Instant::now();
        // Anything said before the question was asked is steering, not an answer.
        let earlier = std::mem::take(&mut self.unit.owner_messages);
        let answer = loop {
            if self.hear_owner() {
                break None;
            }
            if !self.unit.owner_messages.is_empty() {
                break Some(std::mem::take(&mut self.unit.owner_messages).join("\n"));
            }
            if started.elapsed() >= limit {
                break None;
            }
            std::thread::sleep(poll);
        };
        self.unit.owner_messages = earlier;
        if let Some(answer) = &answer {
            self.event("owner.answered", Some(step), json!({ "answer": answer }))?;
        }
        Ok(answer)
    }

    fn model(&self, model: &str) -> String {
        let map = &self.factory.model_override;
        map.get(model)
            .or_else(|| map.get("*"))
            .cloned()
            .unwrap_or_else(|| model.to_string())
    }

    fn record_usage(&mut self, step: &str, usage: &Usage) {
        let used = self.unit.usage.entry(step.to_string()).or_default();
        used.runs += 1;
        used.usage.add(usage);
    }

    /// Every account is at a limit: wait for the first to come back. The
    /// step's session is kept. Returns how the step ends if the wait was cut
    /// short: the owner said stop, or the factory is restarting.
    fn wait_for_accounts(
        &mut self,
        step: &str,
        until: u64,
    ) -> Result<Option<StepEnd>, DriverError> {
        let wait = until.saturating_sub(now_unix());
        self.event(
            "accounts.limited",
            Some(step),
            json!({ "until": until, "wait_secs": wait }),
        )?;
        let clock = format!(
            "{:02}:{:02} UTC",
            (until % 86_400) / 3600,
            (until % 3600) / 60
        );
        self.thread.say(&format!(
            ":hourglass: *Every account is at a usage limit.* Step *{step}* is paused and its session kept. It resumes at {clock}, in {}.",
            duration_text(wait)
        ));
        self.thread.status(&format!(
            ":hourglass: usage limits: resuming *{step}* at {clock}"
        ));
        while now_unix() < until {
            if self.hear_owner() {
                return Ok(Some(StepEnd::Park("the owner said stop".to_string())));
            }
            if self.pause_wanted() {
                return Ok(Some(StepEnd::Pause));
            }
            std::thread::sleep(Duration::from_secs(
                until.saturating_sub(now_unix()).clamp(1, 15),
            ));
        }
        Ok(None)
    }

    fn agent_step(&mut self, step: &Step) -> Result<StepEnd, DriverError> {
        let StepKind::Agent {
            role,
            model,
            effort,
            session: session_rule,
            workspace,
            writes,
        } = &step.kind
        else {
            return Ok(StepEnd::Park("not an agent step".to_string()));
        };
        let Some(contract) = contract_of(role) else {
            return Ok(StepEnd::Park(format!("role `{role}` has no contract")));
        };
        let outcomes = self.flow.outcomes(&step.id)?;
        let schema = factory_brief::schema(contract, &outcomes);
        let model = self.model(model);
        let round = self.unit.position.round();
        // A step can be given more effort in its later rounds.
        let effort = match step.effort_by_round.len() {
            0 => effort.clone(),
            rounds => {
                step.effort_by_round[(round as usize).saturating_sub(1).min(rounds - 1)].clone()
            }
        };
        let system_prompt = self.config.role_prompt(role)?;
        let repo = self.project.repo.clone();

        let cwd = match workspace {
            Workspace::Unit => self.unit.worktree.clone(),
            Workspace::Scratch => {
                let dir = self
                    .factory
                    .state_dir
                    .join("worktrees")
                    .join(format!("{}-{}-{round}", self.unit.id, step.id));
                // Kept across a question to the owner, so it may be there already.
                if !dir.exists() {
                    factory_git::worktree_add_detached(&repo, &dir, &self.unit.checkpoint)?;
                    if let Err(err) = self.run_setup(&dir) {
                        return Ok(StepEnd::Park(format!(
                            "could not set up the scratch worktree for step `{}`: {err}",
                            step.id
                        )));
                    }
                }
                dir
            }
        };

        // A run of this step that was cut off when whatever was watching it
        // died left its session's ID behind. That session has the work it
        // had done: it is resumed, not started over.
        let in_flight = self.dir.join(format!("in-flight-{}.session", step.id));
        let cut_off = std::fs::read_to_string(&in_flight)
            .ok()
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty());
        if let Some(id) = &cut_off {
            self.event("run.cut_off", Some(&step.id), json!({ "session": id }))?;
            self.thread.say(&format!(
                ":adhesive_bandage: *{}* was cut off in the middle of a run. It resumes that session and carries on.",
                step.id
            ));
        }
        let own_session = cut_off.clone().or_else(|| {
            self.unit
                .in_step
                .as_ref()
                .filter(|current| current.step == step.id)
                .map(|current| current.session.clone())
        });
        let mut briefed = own_session.is_some();
        let mut session = match (own_session, session_rule) {
            (Some(id), _) => Session::Resume(id),
            (None, SessionRule::Fresh) => Session::Fresh,
            (None, SessionRule::Continue(earlier)) => match self.unit.sessions.get(earlier) {
                Some(id) => Session::Resume(id.clone()),
                None => {
                    return Ok(StepEnd::Park(format!(
                        "step `{}` continues the session of `{earlier}`, which has not run",
                        step.id
                    )));
                }
            },
        };
        let continuing = matches!(session_rule, SessionRule::Continue(_));
        let diff_stat = factory_git::diff_stat(&cwd, &self.unit.base, "HEAD").unwrap_or_default();
        // A fresh session is given what earlier steps produced, in flow order.
        // A continued one already has its own history, and gets only feedback.
        // The publisher is the exception: it continues the builder's session,
        // which never saw the factory's checks or the reports, and the text it
        // writes must describe them truthfully.
        let notes: Vec<Note> = if continuing && contract != Contract::Publication {
            Vec::new()
        } else {
            self.flow
                .steps
                .iter()
                .filter(|other| other.id != step.id)
                .filter_map(|other| self.unit.notes.get(&other.id).cloned())
                .collect()
        };
        let mut notes = notes;
        if !continuing && !self.unit.history.is_empty() {
            // A fresh session shares nothing with the steps before it.
            notes.insert(
                0,
                Note {
                    title: "What has happened on this bead so far".to_string(),
                    body: format!(
                        "{}\n\nEach round of changes is a commit on this branch (`git log {}..HEAD`). Regression tests left by earlier rounds of adversarial testing are part of the code.",
                        self.unit
                            .history
                            .iter()
                            .map(|line| format!("- {line}"))
                            .collect::<Vec<_>>()
                            .join("\n"),
                        self.unit.base
                    ),
                },
            );
        }
        if contract == Contract::Publication
            && self.unit.absorb_into.is_none()
            && !self.unit.issues.is_empty()
        {
            notes.push(Note {
                title: "GitHub issues this pull request closes".to_string(),
                body: format!(
                    "Write these in the pull-request body, where it says what the pull request closes, each exactly like this so that GitHub closes the issue when it merges: {}. The factory refuses a body that leaves one out.",
                    self.unit
                        .issues
                        .iter()
                        .map(|number| format!("`Closes #{number}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            });
        }
        if self.unit.ship {
            // Nobody in this pipeline built the change: say what it holds.
            let commits =
                factory_git::log_messages(&cwd, &self.unit.base, "HEAD").unwrap_or_default();
            notes.insert(
                0,
                Note {
                    title: "The commits on this branch, one for each bead".to_string(),
                    body: format!(
                        "{}\n\nThe beads: {}.",
                        truncate(commits.trim(), 12_000),
                        self.unit.closes.join(", ")
                    ),
                },
            );
        }
        // What testers proved on earlier beads of the project: a builder is
        // told before it writes anything, and a tester so that it names a
        // class as the others did.
        if !continuing && matches!(contract, Contract::Change | Contract::Probe) {
            let classes = finding_classes(&self.factory.state_dir, self.unit.project.as_str());
            let note = match contract {
                // A class whose check has landed is no longer the builder's
                // to remember: the check runs on its change. It is told
                // again only if the class has broken a bead since.
                Contract::Change => classes_for_builder(&classes_unchecked(
                    classes,
                    &class_checks(&self.factory.state_dir, self.unit.project.as_str()),
                )),
                _ => classes_for_tester(&classes),
            };
            notes.extend(note);
        }
        // What was decided for the whole sub-goal before any bead of it
        // was built.
        if let Some(design) = self.unit.design.as_ref().filter(|_| !continuing) {
            notes.insert(0, design_note(design, self.unit.ship));
        }
        // The texts the project's code is written to.
        if !continuing {
            notes.extend(standards_note(&self.project.standards, &cwd, contract));
        }
        for name in &step.needs {
            match self.artifact_note(name) {
                Ok(note) => notes.push(note),
                Err(reason) => {
                    // A unit continued from this step may have begun on a
                    // flow that did not make it, or have new commits since
                    // it was made. Whoever continues it is told where to.
                    let maker = self
                        .flow
                        .steps
                        .iter()
                        .find(|other| other.produces.as_deref() == Some(name.as_str()))
                        .map(|other| {
                            format!(
                                ". Step `{}` makes it: continue the unit from there, or from a step before it",
                                other.id
                            )
                        })
                        .unwrap_or_default();
                    return Ok(StepEnd::Park(format!(
                        "step `{}` needs `{name}`: {reason}{maker}",
                        step.id
                    )));
                }
            }
        }
        let feedback = self.unit.feedback.clone();
        let timeout = Duration::from_secs(
            step.timeout_secs
                .unwrap_or(self.project.limits.step_timeout_secs),
        );
        let poll_secs = self
            .factory
            .slack
            .as_ref()
            .map_or(10, |s| s.poll_secs)
            .max(1);

        let mut rejected: Vec<String> = Vec::new();
        let mut answer = self.unit.answer.take();
        let mut interrupted = cut_off.is_some();
        let (mut attempts, mut limit_waits, mut retries) = (0, 0, 0);

        self.thread.say(&format!(
            ":arrow_forward: *{}* · round {round} · {role} on `{model}` ({effort})",
            step.id
        ));

        let (result, session_id) = loop {
            attempts += 1;
            let owner_messages = std::mem::take(&mut self.unit.owner_messages);
            let prompt = if interrupted && briefed && rejected.is_empty() && answer.is_none() {
                "You were interrupted before you finished this step. Check the state of your working directory, carry on from where you stopped, and return the step's result.".to_string()
            } else {
                render(&Brief {
                    project: &self.project,
                    bead: &self.bead,
                    step,
                    round,
                    base: &self.unit.base,
                    diff_stat: &diff_stat,
                    continuing,
                    feedback: feedback.as_deref(),
                    notes: &notes,
                    owner_messages: &owner_messages,
                    rulings: &self.unit.rulings,
                    answer: answer.as_deref(),
                    rejected: &rejected,
                })
            };
            self.unit.runs += 1;
            self.artifact(&format!("{}-brief.md", step.id), &prompt)?;
            let log_path = self
                .dir
                .join("runs")
                .join(format!("{:03}-{}.jsonl", self.unit.runs, step.id));
            // The account is chosen afresh for every run: the one whose
            // session resets soonest, or a wait when all are at a limit.
            let (account, oauth_token) = loop {
                if let Some(id) = self.account.clone() {
                    // Chosen by what walks the flow, which knows every
                    // account's limits and who else is waiting for one.
                    let token = account_token(&self.factory, &id)?;
                    break (id, token);
                }
                match pick_account(&self.factory, &model)? {
                    AccountPick::Use { id, token } => break (id, token),
                    // Under the engine the wait is a timer of the server's,
                    // and the step is called again when it fires.
                    AccountPick::Wait { until } if self.engine => {
                        self.event(
                            "accounts.limited",
                            Some(&step.id),
                            json!({ "until": until, "waits": "the engine" }),
                        )?;
                        return Ok(StepEnd::Limited(until));
                    }
                    AccountPick::Wait { until } => {
                        if let Some(end) = self.wait_for_accounts(&step.id, until)? {
                            return Ok(end);
                        }
                    }
                }
            };
            let spec = RunSpec {
                claude_bin: self.factory.claude_bin.clone(),
                model: model.clone(),
                effort: effort.clone(),
                session: session.clone(),
                system_prompt: system_prompt.clone(),
                schema: schema.clone(),
                cwd: cwd.clone(),
                prompt,
                budget_usd: self.project.limits.step_budget_usd,
                timeout,
                oauth_token,
                session_file: Some(in_flight.clone()),
            };
            self.event("run.started", Some(&step.id), json!({
                "run": self.unit.runs, "role": role, "model": model, "effort": effort,
                "account": account,
                "session": match &session { Session::Fresh => json!("fresh"), Session::Resume(id) => json!(id) },
                "cwd": cwd, "log": log_path,
            }))?;
            self.thread.status(&format!(
                ":arrows_counterclockwise: *{}* · round {round} · {role} on `{model}` · account `{account}`",
                step.id
            ));

            let started = Instant::now();
            let mut heard: Vec<String> = Vec::new();
            // An agent that works in the unit's own worktree may commit as
            // it goes. Each commit goes to the remote while it works.
            let keeps = (*workspace == Workspace::Unit).then(|| {
                (
                    self.factory.clone(),
                    self.project.clone(),
                    self.unit.worktree.clone(),
                    self.unit.branch.clone(),
                    self.log.clone(),
                )
            });
            let mut synced = self.unit.synced.clone();
            let mut held = self.unit.held.clone();
            let pull_request = self.unit.pull_request.is_some();
            let finished = {
                let thread = &mut self.thread;
                let step_id = step.id.clone();
                let mut last_poll = Instant::now();
                let mut last_status = Instant::now();
                let mut last_sync = Instant::now();
                factory_claude::run(&spec, &log_path, &mut |transcript| {
                    if let Some((factory, project, worktree, branch, log)) = &keeps
                        && last_sync.elapsed() >= Duration::from_secs(2)
                    {
                        last_sync = Instant::now();
                        let step = StepId::new(step_id.clone());
                        let kept = keep_on_remote(
                            factory,
                            project,
                            worktree,
                            branch,
                            (synced.as_deref(), held.as_deref()),
                            pull_request,
                        );
                        match kept {
                            None => {}
                            Some(Ok((commit, holding))) => {
                                let pushed_to = if holding {
                                    holding_branch(branch)
                                } else {
                                    branch.clone()
                                };
                                let _ = log.append(
                                    "branch.synced",
                                    Some(&step),
                                    json!({ "branch": pushed_to, "commit": commit, "when": "during the run" }),
                                );
                                if holding {
                                    held = Some(commit);
                                } else {
                                    synced = Some(commit);
                                }
                            }
                            Some(Err(error)) => {
                                let _ = log.append(
                                    "branch.sync_failed",
                                    Some(&step),
                                    json!({ "branch": branch, "error": error }),
                                );
                                // Not again for the same commit every two seconds.
                                last_sync = Instant::now() + Duration::from_secs(60);
                            }
                        }
                    }
                    if last_poll.elapsed() >= Duration::from_secs(poll_secs) {
                        last_poll = Instant::now();
                        for message in thread.poll() {
                            if is_stop(&message) {
                                return Control::Kill("the owner said stop".to_string());
                            }
                            heard.push(message);
                        }
                    }
                    if last_status.elapsed() >= Duration::from_secs(60) {
                        last_status = Instant::now();
                        thread.status(&format!(
                            ":arrows_counterclockwise: *{step_id}* · round {round} · {} · {} tool calls · {}",
                            duration_text(started.elapsed().as_secs()),
                            transcript.tool_calls,
                            escape(transcript.last_activity.as_deref().unwrap_or("thinking")),
                        ));
                    }
                    Control::Continue
                })?
            };
            self.unit.synced = synced;
            self.unit.held = held;
            // The run is over, however it ended: nothing is in flight.
            let _ = std::fs::remove_file(&in_flight);
            // Said while the step ran: for the next agent step.
            self.unit.owner_messages.extend(heard);
            let usage = finished.transcript.usage();
            observe_account(&self.factory, &account, usage.rate_limit.as_ref(), &model);
            self.record_usage(&step.id, &usage);
            if let Some(id) = &finished.transcript.session_id {
                self.unit.in_step = Some(InStep {
                    step: step.id.clone(),
                    session: id.clone(),
                });
                session = Session::Resume(id.clone());
                briefed = true;
            }
            let unexpected: Vec<&String> = usage
                .models
                .keys()
                .filter(|used| !self.factory.allowed_models.contains(used))
                .collect();
            self.event(
                "run.ended",
                Some(&step.id),
                json!({
                    "run": self.unit.runs, "outcome": finished.outcome.name(),
                    "session": finished.transcript.session_id, "exit_code": finished.exit.code,
                    "secs": finished.exit.duration.as_secs(), "usage": usage,
                    "tool_calls": finished.transcript.tool_calls,
                    "unknown_stream_lines": finished.transcript.unknown,
                    "unexpected_models": unexpected,
                }),
            )?;
            self.save()?;

            let reject = |driver: &mut Driver, problems: Vec<String>| -> Result<(), DriverError> {
                driver.event(
                    "result.rejected",
                    Some(&step.id),
                    json!({ "problems": problems }),
                )?;
                driver.thread.say(&format!(
                    ":warning: *{}* returned a result that code rejected; sending it back.\n{}",
                    step.id,
                    quote(&truncate(&problems.join("\n"), 800))
                ));
                Ok(())
            };
            match finished.outcome {
                Outcome::Completed(result) => {
                    let mut problems = result_problems(contract, &outcomes, &result);
                    let verdict = result.get("outcome").and_then(Value::as_str).unwrap_or("");
                    if problems.is_empty() && contract == Contract::Probe && verdict == "broken" {
                        // A finding is proved by a test that fails, not by a claim.
                        problems.extend(self.verify_reproductions(&cwd, &result)?);
                    }
                    if problems.is_empty()
                        && contract == Contract::Change
                        && verdict == "done"
                        && *workspace == Workspace::Unit
                    {
                        // A fix is not done while a regression test still fails.
                        problems.extend(self.check_regressions(&cwd)?);
                    }
                    if problems.is_empty()
                        && contract == Contract::Change
                        && verdict == "done"
                        && *workspace == Workspace::Unit
                        && self.unit.ship
                    {
                        // A sub-goal's commits are published as they are.
                        // One this builder made that the project's check on
                        // commits refuses can still be reworded by it now;
                        // at the publish step nobody can.
                        problems.extend(self.check_ship_commits(step, &cwd)?);
                    }
                    if contract == Contract::Publication
                        && self.unit.absorb_into.is_none()
                        && self
                            .project
                            .github
                            .as_ref()
                            .is_some_and(|github| github.issues)
                    {
                        // The pull request must close the issues of the beads it lands.
                        let body = result.get("pr_body").and_then(Value::as_str).unwrap_or("");
                        for number in factory_github::unclosed_issues(body, &self.unit.issues) {
                            problems.push(format!(
                                "`pr_body` does not close issue #{number}: write `Closes #{number}` in it, so that GitHub closes the issue when the pull request merges"
                            ));
                        }
                    }
                    if contract == Contract::Publication {
                        let own = self
                            .project
                            .github
                            .as_ref()
                            .map(|github| github.repo.as_str());
                        for (field, max) in [
                            ("commit_message", 8000),
                            ("pr_title", 200),
                            ("pr_body", 30_000),
                            ("merge_summary", 2000),
                        ] {
                            let text = result.get(field).and_then(Value::as_str).unwrap_or("");
                            problems.extend(
                                publication_problems(text, own, max)
                                    .into_iter()
                                    .map(|problem| format!("`{field}`: {problem}")),
                            );
                        }
                    }
                    if problems.is_empty() {
                        let id = finished.transcript.session_id.clone().unwrap_or_default();
                        self.announce_agent_end(
                            step,
                            &result,
                            &usage,
                            finished.exit.duration.as_secs(),
                        );
                        break (result, id);
                    }
                    if attempts >= 3 {
                        return Ok(StepEnd::Park(format!(
                            "step `{}` returned a result that code rejected three times:\n{}",
                            step.id,
                            problems.join("\n")
                        )));
                    }
                    reject(self, problems.clone())?;
                    rejected = problems;
                    answer = None;
                }
                Outcome::InvalidResult(why) => {
                    if attempts >= 3 {
                        return Ok(StepEnd::Park(format!(
                            "step `{}` did not return a typed result: {why}",
                            step.id
                        )));
                    }
                    reject(self, vec![why.clone()])?;
                    rejected = vec![format!("{why}. End by returning the structured result.")];
                    answer = None;
                }
                Outcome::Limited { window, resets_at } => {
                    // The tracker has already taken the account out until its
                    // reset. The next turn of this loop picks another, or
                    // waits, and continues this same session.
                    limit_waits += 1;
                    self.event(
                        "account.limited",
                        Some(&step.id),
                        json!({
                            "account": account, "window": window, "resets_at": resets_at,
                        }),
                    )?;
                    if self.account.is_some() {
                        // The account was handed to this step. Whoever
                        // handed it out is told, and names another or
                        // waits; the session is kept for the next run.
                        return Ok(StepEnd::Limited(
                            resets_at.unwrap_or_else(|| now_unix() + 15 * 60),
                        ));
                    }
                    if limit_waits > 8 {
                        return Ok(StepEnd::Park(
                            "the step was refused at a usage limit nine times".to_string(),
                        ));
                    }
                    self.thread.say(&format!(
                        ":hourglass: Account `{account}` is at its {} limit. Step *{}* continues its session under another account, or waits if there is none.",
                        window.as_deref().unwrap_or("usage").replace('_', "-"),
                        step.id
                    ));
                    attempts -= 1;
                    interrupted = true;
                }
                Outcome::ApiError { status, message } => {
                    retries += 1;
                    if retries > 2 {
                        return Ok(StepEnd::Park(format!(
                            "step `{}` failed three times with an API error ({status:?}): {message}",
                            step.id
                        )));
                    }
                    self.thread.say(&format!(
                        ":warning: *{}* hit an API error ({}); retrying in a minute.",
                        step.id,
                        escape(&truncate(&message, 200))
                    ));
                    std::thread::sleep(Duration::from_secs(60));
                    attempts -= 1;
                    interrupted = true;
                }
                Outcome::Crashed { code } => {
                    retries += 1;
                    if retries > 1 {
                        return Ok(StepEnd::Park(format!(
                            "the worker for step `{}` ended twice without a result (exit code {code:?}); see {}",
                            step.id,
                            log_path.display()
                        )));
                    }
                    attempts -= 1;
                    interrupted = true;
                }
                Outcome::AuthError(message) => {
                    return Ok(StepEnd::Park(format!(
                        "Claude refused the login: {message}"
                    )));
                }
                Outcome::BudgetExceeded => {
                    return Ok(StepEnd::Park(format!(
                        "step `{}` spent its budget cap",
                        step.id
                    )));
                }
                Outcome::TimedOut => {
                    return Ok(StepEnd::Park(format!(
                        "step `{}` was still running after {} and was killed",
                        step.id,
                        duration_text(timeout.as_secs())
                    )));
                }
                Outcome::Killed(reason) => return Ok(StepEnd::Park(reason)),
            }
        };

        let outcome = result
            .get("outcome")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        self.unit.sessions.insert(step.id.clone(), session_id);
        self.unit.results.insert(step.id.clone(), result.clone());
        self.artifact(
            &format!("{}-result.json", step.id),
            &serde_json::to_string_pretty(&result).unwrap_or_default(),
        )?;
        let asking = outcome == "needs_owner";
        if !asking {
            self.unit.in_step = None;
        }

        match workspace {
            Workspace::Unit => {
                let before = self.unit.checkpoint.clone();
                let (head, committed) = factory_git::checkpoint(
                    &cwd,
                    &format!("checkpoint: {} round {round} ({})", step.id, self.unit.bead),
                )?;
                let changed = factory_git::changed_files(&cwd, &before, &head)?;
                if before != self.unit.base && !self.project.test_patterns.is_empty() {
                    // A later round of changes: did it make a test easier to pass?
                    let diff = factory_git::diff_zero_context(&cwd, &before, &head)?;
                    let removed = weakened_tests(&diff, &self.project.test_patterns)?;
                    if !removed.is_empty() {
                        self.event(
                            "tests.removed",
                            Some(&step.id),
                            json!({ "lines": removed, "from": before, "to": head }),
                        )?;
                        self.unit.history.push(format!(
                            "{} (round {round}) removed lines that look like tests or assertions; check that no test was weakened: {}",
                            step.id,
                            truncate(&removed.join(" | "), 1500)
                        ));
                    }
                }
                // Beads built side by side were each given paths. Leaving
                // them is not refused, since the bead may need to, but it is
                // recorded: whoever set the paths should know.
                let strays: Vec<String> = changed
                    .iter()
                    .filter(|file| {
                        !self.unit.declared_paths.is_empty()
                            && !under(file, &self.unit.declared_paths)
                            && !self.unit.strayed.contains(file)
                    })
                    .cloned()
                    .collect();
                if !strays.is_empty() {
                    self.event(
                        "paths.strayed",
                        Some(&step.id),
                        json!({ "files": strays, "declared": self.unit.declared_paths }),
                    )?;
                    self.unit.strayed.extend(strays);
                }
                let allowed: Vec<String> = writes
                    .iter()
                    .flat_map(|scope| self.project.scopes.get(scope).cloned().unwrap_or_default())
                    .collect();
                let mut violations =
                    scope_violations(&changed, &allowed, &self.project.forbidden_paths)?;
                // A path no step may change is in the wrong until it is as
                // it was before the step that changed it, and not after.
                // Putting it back is a change to it too, and is the one
                // change that is wanted: it must not park the unit again.
                for trespass in
                    scope_violations(&changed, &["**".to_string()], &self.project.forbidden_paths)?
                {
                    self.unit
                        .trespassed
                        .entry(trespass.path)
                        .or_insert_with(|| before.clone());
                }
                let mut since: BTreeMap<String, Vec<String>> = BTreeMap::new();
                let mut restored: Vec<String> = Vec::new();
                for (path, clean) in &self.unit.trespassed {
                    if !since.contains_key(clean) {
                        let files = factory_git::changed_files(&cwd, clean, &head)?;
                        since.insert(clean.clone(), files);
                    }
                    if !since.get(clean).is_some_and(|files| files.contains(path)) {
                        restored.push(path.clone());
                    }
                }
                for path in &restored {
                    self.unit.trespassed.remove(path);
                }
                violations.retain(|violation| !restored.contains(&violation.path));
                // One that an earlier step changed, and that is still not
                // put back, is not let through because this step left it
                // alone.
                for path in self.unit.trespassed.keys() {
                    if !violations.iter().any(|violation| &violation.path == path) {
                        violations.push(factory_checks::Violation {
                            path: path.clone(),
                            reason: "no step may change this path, and an earlier step's change to it is still there".to_string(),
                        });
                    }
                }
                self.unit.checkpoint = head.clone();
                self.event("checkpoint", Some(&step.id), json!({
                    "commit": head, "committed": committed, "files": changed,
                    "violations": violations.iter().map(|v| json!({"path": v.path, "reason": v.reason})).collect::<Vec<_>>(),
                    "restored": restored,
                }))?;
                self.save()?;
                if !violations.is_empty() {
                    let list: Vec<String> = violations
                        .iter()
                        .map(|violation| format!("{} ({})", violation.path, violation.reason))
                        .collect();
                    return Ok(StepEnd::Park(format!(
                        "step `{}` changed files it may not change:\n{}\nA path no step may change has to be put back exactly as it was; a step that does that is not stopped for it.",
                        step.id,
                        list.join("\n")
                    )));
                }
            }
            Workspace::Scratch if !asking => {
                // Whatever the step wrote is kept as a patch, then thrown away.
                let patch =
                    factory_git::patch_since(&cwd, &self.unit.checkpoint).unwrap_or_default();
                let patch_path = if patch.trim().is_empty() {
                    None
                } else {
                    Some(self.artifact(&format!("{}-probes.patch", step.id), &patch)?)
                };
                factory_git::worktree_remove(&repo, &cwd)?;
                self.event(
                    "scratch.discarded",
                    Some(&step.id),
                    json!({ "patch": patch_path }),
                )?;
                // What a tester that broke the change left behind is its
                // regression tests: they join the unit's branch, failing.
                let mut adopted = false;
                if contract == Contract::Probe
                    && outcome == "broken"
                    && let Some(path) = &patch_path
                {
                    if let Err(err) = self.adopt_regressions(step, round, path, &result) {
                        return Ok(StepEnd::Park(format!(
                            "the tester's regression tests could not be added to the branch: {err}"
                        )));
                    }
                    adopted = true;
                }
                if contract == Contract::Probe {
                    let summary = result.get("summary").and_then(Value::as_str).unwrap_or("");
                    let mut body = format!("Outcome: `{outcome}`.\n\n{summary}\n");
                    let findings = findings_text(&result);
                    if !findings.is_empty() {
                        body.push_str(&format!(
                            "\nFindings, each reproduced by the tester:\n\n{findings}"
                        ));
                    }
                    if adopted {
                        body.push_str("\nThe tester's regression tests for these findings are now part of this branch.\n");
                    } else if let Some(path) = &patch_path {
                        body.push_str(&format!(
                            "\nThe tester's probe files are a patch against the checkpoint it tested: `{}`. Apply it with `git apply` to run the reproductions; do not keep the probes unless they belong in the test suite.\n",
                            path.display()
                        ));
                    }
                    self.unit.notes.insert(
                        step.id.clone(),
                        Note {
                            title: format!(
                                "Adversarial testing (step `{}`, round {round})",
                                step.id
                            ),
                            body,
                        },
                    );
                }
            }
            Workspace::Scratch => {}
        }
        if contract == Contract::Review && !asking {
            let summary = result.get("summary").and_then(Value::as_str).unwrap_or("");
            self.unit.notes.insert(
                step.id.clone(),
                Note {
                    title: format!("Review (step `{}`, round {round})", step.id),
                    body: format!(
                        "Outcome: `{outcome}`.\n\n{summary}\n\n{}",
                        findings_text(&result)
                    ),
                },
            );
        }

        match (contract, outcome.as_str()) {
            (Contract::Probe, "broken") | (Contract::Review, "rework") => {
                let who = if contract == Contract::Probe {
                    "The adversarial tester broke the change"
                } else {
                    "The reviewer asked for rework"
                };
                let mut text = format!(
                    "{who} (step `{}`, round {round}).\n\n{}\n\n{}",
                    step.id,
                    result.get("summary").and_then(Value::as_str).unwrap_or(""),
                    findings_text(&result)
                );
                if let Some(note) = self.unit.notes.get(&step.id)
                    && let Some(line) = note.body.lines().find(|line| line.contains("probe files"))
                {
                    text.push_str(&format!("\n{line}\n"));
                }
                let pending: Vec<&Regression> = self
                    .unit
                    .regressions
                    .iter()
                    .filter(|regression| contract == Contract::Probe && regression.round == round)
                    .collect();
                if !pending.is_empty() {
                    text.push_str("\nThe tester left regression tests, and the factory has added them to your worktree as a commit. Each of these commands fails now. The factory runs every regression test of this bead, these and the earlier ones, before it accepts your result, and it does not accept it while one fails. Do not weaken or remove them: a test that is itself wrong is something to say in your summary.\n\n");
                    for regression in pending {
                        text.push_str(&format!(
                            "- `{}` ({})\n",
                            regression.command, regression.finding
                        ));
                    }
                }
                let breaks = self
                    .unit
                    .regressions
                    .iter()
                    .map(|regression| regression.round)
                    .collect::<std::collections::BTreeSet<_>>()
                    .len();
                if contract == Contract::Probe && breaks >= 2 {
                    text.push_str(&format!(
                        "\nThis is the {breaks}th round in which the tester has broken this change. Fixing each report as it came has not fixed the design. Before you change any code: write down the invariant this mechanism must keep; list the orderings, interleavings and states it must hold in, starting with every one reported so far; and check the code against each. Then fix the cause. Say in your summary what the invariant is and which cases you checked.\n"
                    ));
                }
                self.unit.feedback = Some(text);
            }
            (Contract::Change, "done") | (Contract::Publication, "ready") => {
                self.unit.feedback = None
            }
            _ => {}
        }
        self.save()?;
        Ok(StepEnd::Outcome(outcome))
    }

    fn announce_agent_end(&mut self, step: &Step, result: &Value, usage: &Usage, secs: u64) {
        let outcome = result.get("outcome").and_then(Value::as_str).unwrap_or("");
        let icon = match outcome {
            "done" | "held" | "approve" | "ready" | "already_done" => ":white_check_mark:",
            "broken" | "rework" => ":hammer:",
            "needs_owner" => ":question:",
            _ => ":grey_question:",
        };
        let mut facts = vec![
            duration_text(secs),
            format!("{} turns", usage.turns),
            format!("{} out", tokens_text(usage.output_tokens())),
            format!("est. ${:.2}", usage.cost_usd),
        ];
        let windows = windows_text(usage);
        if !windows.is_empty() {
            facts.push(windows);
        }
        let mut text = format!(
            "{icon} *{}* → `{outcome}` · {}\n{}",
            step.id,
            facts.join(" · "),
            quote(&truncate(
                result.get("summary").and_then(Value::as_str).unwrap_or(""),
                1200
            )),
        );
        let findings: Vec<String> = result
            .get("findings")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .map(|finding| {
                let field = |key: &str| finding.get(key).and_then(Value::as_str).unwrap_or("");
                format!(
                    "• *{}* {} (`{}`)",
                    field("severity"),
                    escape(field("title")),
                    escape(field("location"))
                )
            })
            .collect();
        if !findings.is_empty() {
            text.push_str(&format!("\n{}", truncate(&findings.join("\n"), 1500)));
        }
        self.thread.say(&text);
        let reply = result
            .get("reply_to_owner")
            .and_then(Value::as_str)
            .unwrap_or("");
        if !reply.trim().is_empty() {
            self.thread.say(&format!(
                ":speech_balloon: *{}* replies:\n{}",
                step.id,
                quote(&truncate(reply, 2500))
            ));
        }
    }

    fn command_step(&mut self, step: &Step) -> Result<StepEnd, DriverError> {
        let StepKind::Command {
            command: name,
            policy,
        } = &step.kind
        else {
            return Ok(StepEnd::Park("not a command step".to_string()));
        };
        let check = Check {
            step,
            key: step.id.clone(),
            name,
            policy: *policy,
            produces: step.produces.as_ref(),
            when_changed: &step.when_changed,
        };
        if self.not_needed(&check)? {
            return Ok(StepEnd::Outcome("pass".to_string()));
        }
        let command = self.project.commands[name].clone();
        let (argv, out_dir) = self.command_argv(&check, &command)?;
        self.thread.status(&format!(
            ":arrows_counterclockwise: *{}* · running `{}`",
            step.id,
            escape(&command.display())
        ));
        let result = run_command_with_grace(
            &argv,
            &self.unit.worktree,
            Duration::from_secs(command.timeout_secs),
            Duration::from_secs(command.stop_grace_secs),
        );
        self.judge_command(&check, &command, &argv, out_dir.as_ref(), &result)
    }

    /// Run a group of commands at the same time, wait for every one, and
    /// judge them together. The step passes when each passed. It parks when
    /// one of them parks, since no fix round can help with that one, and
    /// fails otherwise, with every failure in what the builder is given. A
    /// command that changes the worktree runs first, alone, so that no
    /// other reads a file it is writing.
    fn commands_step(&mut self, step: &Step) -> Result<StepEnd, DriverError> {
        let StepKind::Commands { members } = &step.kind else {
            return Ok(StepEnd::Park("not a group of commands".to_string()));
        };
        let mut runs: Vec<GroupRun> = Vec::new();
        let mut unrun = 0;
        for member in members {
            let check = Check {
                step,
                key: format!("{}.{}", step.id, member.command),
                name: &member.command,
                policy: ExitPolicy::MustPass,
                produces: None,
                when_changed: &member.when_changed,
            };
            if self.not_needed(&check)? {
                unrun += 1;
                continue;
            }
            let command = self.project.commands[&member.command].clone();
            let (argv, _) = self.command_argv(&check, &command)?;
            runs.push(GroupRun {
                key: check.key,
                name: member.command.clone(),
                command,
                argv,
                result: None,
            });
        }
        let worktree = self.unit.worktree.clone();
        let run = |one: &GroupRun| {
            run_command_with_grace(
                &one.argv,
                &worktree,
                Duration::from_secs(one.command.timeout_secs),
                Duration::from_secs(one.command.stop_grace_secs),
            )
        };
        for one in runs.iter_mut().filter(|one| one.command.edits) {
            self.thread.status(&format!(
                ":arrows_counterclockwise: *{}* · running `{}` first and alone: it changes the worktree",
                step.id,
                escape(&one.command.display())
            ));
            one.result = Some(run(one));
        }
        let together: Vec<String> = runs
            .iter()
            .filter(|one| one.result.is_none())
            .map(|one| format!("`{}`", escape(&one.command.display())))
            .collect();
        if !together.is_empty() {
            self.thread.status(&format!(
                ":arrows_counterclockwise: *{}* · running {} at the same time",
                step.id,
                together.join(", ")
            ));
        }
        let results: Vec<(usize, CommandResult)> = std::thread::scope(|scope| {
            let started = Instant::now();
            let handles: Vec<_> = runs
                .iter()
                .enumerate()
                .filter(|(_, one)| one.result.is_none())
                .map(|(index, one)| (index, scope.spawn(move || run(one))))
                .collect();
            handles
                .into_iter()
                .map(|(index, handle)| {
                    let result = handle.join().unwrap_or_else(|_| CommandResult {
                        verdict: Verdict::CouldNotRun(
                            "the thread that ran it panicked".to_string(),
                        ),
                        output: String::new(),
                        duration: started.elapsed(),
                    });
                    (index, result)
                })
                .collect()
        });
        for (index, result) in results {
            runs[index].result = Some(result);
        }
        // Each is judged as a step of its own would be, in the order the
        // flow lists them, and only then are they put together.
        let mut parked = Vec::new();
        let mut failed = Vec::new();
        let mut lines = Vec::new();
        for one in &runs {
            let check = Check {
                step,
                key: one.key.clone(),
                name: &one.name,
                policy: ExitPolicy::MustPass,
                produces: None,
                when_changed: &[],
            };
            let Some(result) = &one.result else { continue };
            self.unit.feedback = None;
            let end = self.judge_command(&check, &one.command, &one.argv, None, result)?;
            if let Some(note) = self.unit.notes.remove(&one.key) {
                lines.push(format!("- {}", note.body));
            }
            match end {
                StepEnd::Outcome(outcome) if outcome == "pass" => {}
                StepEnd::Outcome(_) => {
                    let text = self.unit.feedback.take().unwrap_or_default();
                    failed.push((one.name.clone(), text));
                }
                StepEnd::Park(reason) => parked.push(reason),
                other => return Ok(other),
            }
        }
        if unrun > 0 {
            lines.push(format!(
                "- {unrun} more of them did not need to run: the change touches nothing they depend on."
            ));
        }
        self.unit.notes.insert(
            step.id.clone(),
            Note {
                title: format!(
                    "Checks {}, run at the same time",
                    members
                        .iter()
                        .map(|member| format!("`{}`", member.command))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                body: lines.join("\n"),
            },
        );
        self.unit.feedback = match failed.as_slice() {
            [] => None,
            [(_, text)] => Some(text.clone()),
            all => Some(format!(
                "{} of the checks the factory ran at the same time failed: {}. Each is below. Fix every one before you return.\n\n{}",
                all.len(),
                all.iter()
                    .map(|(name, _)| format!("`{name}`"))
                    .collect::<Vec<_>>()
                    .join(", "),
                all.iter()
                    .map(|(name, text)| format!("## `{name}`\n\n{text}"))
                    .collect::<Vec<_>>()
                    .join("\n\n")
            )),
        };
        if !parked.is_empty() {
            // What failed besides stays in the feedback, for whoever goes on.
            return Ok(StepEnd::Park(parked.join("\n\n")));
        }
        Ok(StepEnd::Outcome(
            if failed.is_empty() { "pass" } else { "fail" }.to_string(),
        ))
    }

    /// Whether a command depends only on paths the change does not touch,
    /// so that it passes unrun. Says so where people look when it does.
    fn not_needed(&mut self, check: &Check) -> Result<bool, DriverError> {
        if check.when_changed.is_empty() {
            return Ok(false);
        }
        let globs: Vec<String> = check
            .when_changed
            .iter()
            .flat_map(|scope| self.project.scopes.get(scope).cloned().unwrap_or_default())
            .collect();
        let changed = factory_git::changed_files(&self.unit.worktree, &self.unit.base, "HEAD")?;
        let outside = scope_violations(&changed, &globs, &[])?;
        if outside.len() != changed.len() {
            return Ok(false);
        }
        self.event(
            "command.skipped",
            Some(&check.step.id),
            json!({ "command": check.name, "when_changed": check.when_changed }),
        )?;
        self.thread.say(&format!(
            ":fast_forward: *{}* · not needed: the change touches nothing in {}",
            check.key,
            check.when_changed.join(", ")
        ));
        Ok(true)
    }

    /// The argv a command runs with, and the directory made for the
    /// artifact it produces, if it produces one.
    fn command_argv(
        &mut self,
        check: &Check,
        command: &factory_config::CommandSpec,
    ) -> Result<(Vec<String>, Option<PathBuf>), DriverError> {
        self.unit.runs += 1;
        // An artifact gets a directory of its own, outside the worktree.
        let out_dir = match check.produces {
            Some(artifact) => {
                let dir = self
                    .dir
                    .join("artifacts")
                    .join(format!("{:03}-{artifact}", self.unit.runs));
                std::fs::create_dir_all(&dir).map_err(io(&dir))?;
                Some(dir)
            }
            None => None,
        };
        let argv: Vec<String> = command
            .argv
            .iter()
            .map(|arg| {
                let arg = arg
                    .replace("{base}", &self.unit.base)
                    .replace("{config}", &self.config.0.to_string_lossy());
                match &out_dir {
                    Some(dir) => arg.replace("{out}", &dir.to_string_lossy()),
                    None => arg,
                }
            })
            .collect();
        Ok((argv, out_dir))
    }

    /// Judge a command that ran: keep its output, record what it says,
    /// look into a failure, and say how the check ended. A failure leaves
    /// what the builder is to be given in the unit's feedback.
    fn judge_command(
        &mut self,
        check: &Check,
        command: &factory_config::CommandSpec,
        argv: &[String],
        out_dir: Option<&PathBuf>,
        result: &CommandResult,
    ) -> Result<StepEnd, DriverError> {
        let name = check.name;
        // What people are shown: the command as configured, not the long
        // paths. What the builder is given is the command it can run.
        let shown = command.display();
        let runnable = command.runnable(&self.config.0);
        let output_path = self.artifact(&format!("{}-output.txt", check.key), &result.output)?;
        let secs = result.duration.as_secs();
        self.event(
            "command.ran",
            Some(&check.step.id),
            json!({
                "command": name, "argv": argv, "verdict": format!("{:?}", result.verdict),
                "secs": secs, "output": output_path, "commit": self.unit.checkpoint,
            }),
        )?;

        let (passed, how) = match (&result.verdict, &check.policy) {
            (Verdict::CouldNotRun(reason), _) => {
                // The host's fault, not the code's: no fix round can help.
                return Ok(StepEnd::Park(format!(
                    "the command `{name}` could not run: {reason}"
                )));
            }
            // A report that was cut short is not a report.
            (Verdict::TimedOut, ExitPolicy::ReportOnly) => (
                false,
                format!(
                    "was still running after {} and was killed",
                    duration_text(command.timeout_secs)
                ),
            ),
            (_, ExitPolicy::ReportOnly) => (true, "reported".to_string()),
            (Verdict::Passed, ExitPolicy::MustPass) => (true, "passed".to_string()),
            (Verdict::Passed, ExitPolicy::MustFail) => {
                (false, "passed, but it had to fail".to_string())
            }
            (Verdict::Failed { .. }, ExitPolicy::MustFail) => {
                (true, "failed, as it had to".to_string())
            }
            (Verdict::Failed { code }, ExitPolicy::MustPass) => (
                false,
                format!(
                    "failed (exit code {})",
                    code.map_or("none".to_string(), |c| c.to_string())
                ),
            ),
            (Verdict::TimedOut, _) => (
                false,
                format!(
                    "was still running after {} and was killed",
                    duration_text(command.timeout_secs)
                ),
            ),
        };
        let short = self.unit.checkpoint[..self.unit.checkpoint.len().min(10)].to_string();
        let short = short.as_str();
        if let (Some(artifact), Some(dir), true) = (check.produces, out_dir, passed) {
            let ended = match &result.verdict {
                Verdict::Passed => "passed".to_string(),
                Verdict::Failed { code } => format!(
                    "exited {}",
                    code.map_or("with no code".to_string(), |c| c.to_string())
                ),
                other => format!("{other:?}"),
            };
            self.unit.artifacts.insert(
                artifact.clone(),
                Artifact {
                    path: dir.clone(),
                    commit: self.unit.checkpoint.clone(),
                    step: check.step.id.clone(),
                    command: shown.clone(),
                    ended: ended.clone(),
                },
            );
            self.event(
                "artifact.produced",
                Some(&check.step.id),
                json!({ "artifact": artifact, "path": dir, "commit": self.unit.checkpoint, "ended": ended }),
            )?;
            let summary = command
                .summary
                .as_ref()
                .and_then(|file| std::fs::read_to_string(dir.join(file)).ok())
                .unwrap_or_default();
            if let Some(line) = first_match(&summary, &command.fail_when)? {
                // Advisory as a whole, but this line is one that must not pass.
                self.unit.feedback = Some(format!(
                    "The project's reports (`{shown}`), written by tools on commit `{short}`, show a result that must not pass:\n\n    {}\n\nThe summary:\n\n```\n{}\n```\n\nThe reports are in `{}`: read the file for that check, fix what it shows, and run the project's commands before you return.",
                    line.trim(),
                    truncate(summary.trim_end(), 4000),
                    dir.display()
                ));
                self.event(
                    "report.blocked",
                    Some(&check.step.id),
                    json!({ "artifact": artifact, "line": line }),
                )?;
                self.thread.say(&format!(
                    ":x: *{}* · the reports show a result that must not pass: `{}`",
                    check.key,
                    escape(line.trim())
                ));
                self.save()?;
                return Ok(StepEnd::Outcome("fail".to_string()));
            }
            self.thread.say(&format!(
                ":page_facing_up: *{}* · `{artifact}` written on `{short}` ({ended}) · {}\n{}",
                check.key,
                duration_text(secs),
                if summary.trim().is_empty() {
                    String::new()
                } else {
                    code_block(&truncate(&summary, 2400))
                }
            ));
            self.save()?;
            return Ok(StepEnd::Outcome("pass".to_string()));
        }
        self.unit.notes.insert(
            check.key.clone(),
            Note {
                title: format!("Check `{name}`"),
                body: format!(
                    "`{shown}` {how} on commit `{short}` (took {}).",
                    duration_text(secs)
                ),
            },
        );
        if passed {
            self.unit.failures.remove(&check.key);
            self.thread.say(&format!(
                ":white_check_mark: *{}* · `{}` {how} · {}",
                check.key,
                escape(&shown),
                duration_text(secs)
            ));
            return Ok(StepEnd::Outcome("pass".to_string()));
        }
        let mut found = Vec::new();
        if matches!(check.policy, ExitPolicy::MustPass)
            && matches!(result.verdict, Verdict::Failed { .. } | Verdict::TimedOut)
        {
            let failed = Failed {
                key: &check.key,
                name,
                command,
                argv,
                result,
                how: &how,
                output: &output_path,
            };
            match self.triage(check.step, &failed)? {
                Triaged::End(end) => return Ok(end),
                Triaged::Found(lines) => found = lines,
            }
        }
        let found = if found.is_empty() {
            String::new()
        } else {
            format!(
                "\n\nWhat the factory found out before sending you:\n\n{}",
                found
                    .iter()
                    .map(|line| format!("- {line}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        };
        self.unit.feedback = Some(format!(
            "The command `{name}` (`{runnable}`) {how}, run by the factory in your worktree on commit `{short}`.\n\nThe end of its output:\n\n```\n{}\n```\n\nThe full output is in `{}`.{found}",
            tail(&result.output, 150),
            output_path.display()
        ));
        self.thread.say(&format!(
            ":x: *{}* · `{}` {how} · {}\n{}",
            check.key,
            escape(&shown),
            duration_text(secs),
            code_block(&truncate(&tail(&result.output, 25), 2400))
        ));
        Ok(StepEnd::Outcome("fail".to_string()))
    }

    /// Find out, before a builder is sent, whether a failed check is the
    /// change's: the project's word that the host was at fault, a run once
    /// more on the change, a run on the code as it was at the base, and
    /// whether it failed the same way as the last time. Each but the first
    /// and the last only where the project's command turns it on.
    fn triage(&mut self, step: &Step, failed: &Failed) -> Result<Triaged, DriverError> {
        let name = failed.name;
        let command = failed.command;
        let code = match failed.result.verdict {
            Verdict::Failed { code } => code,
            _ => None,
        };
        self.judge(
            &step.id,
            json!({ "command": name, "output": failed.output }),
            if matches!(failed.result.verdict, Verdict::TimedOut) {
                "The command was stopped because it reached its time limit."
            } else {
                ""
            },
            &failed.result.output,
            &command.failure_lines,
        );
        if let Some(why) = host_fault(
            &failed.result.output,
            code,
            &command.could_not_run_when,
            &command.could_not_run_exit,
        )? {
            self.event(
                "command.host_fault",
                Some(&step.id),
                json!({ "command": name, "why": why }),
            )?;
            return Ok(Triaged::End(StepEnd::Park(format!(
                "the command `{name}` {} and could not run properly: {why}. The host is at fault, not the change, so no builder was sent. Its output is in `{}`.",
                failed.how,
                failed.output.display()
            ))));
        }
        let mut found = Vec::new();
        if command.triage.rerun {
            self.thread.status(&format!(
                ":arrows_counterclockwise: *{}* · `{}` {}: running it once more",
                failed.key,
                escape(&command.display()),
                failed.how
            ));
            let again = run_command_with_grace(
                failed.argv,
                &self.unit.worktree,
                Duration::from_secs(command.timeout_secs),
                Duration::from_secs(command.stop_grace_secs),
            );
            let path = self.artifact(&format!("{}-rerun-output.txt", failed.key), &again.output)?;
            self.event(
                "command.rerun",
                Some(&step.id),
                json!({
                    "command": name, "verdict": format!("{:?}", again.verdict),
                    "secs": again.duration.as_secs(), "output": path,
                }),
            )?;
            if again.verdict == Verdict::Passed {
                self.event(
                    "command.flaky",
                    Some(&step.id),
                    json!({ "command": name, "first": failed.output, "rerun": path }),
                )?;
                // Not the change's, and still somebody's: whoever started
                // the pipeline is handed it when the pipeline ends.
                self.unit.flakes.push(Flake {
                    step: step.id.clone(),
                    check: name.to_string(),
                    commit: self.unit.checkpoint.clone(),
                    signature: failure_signature(&failed.result.output, &command.failure_lines)?,
                    output: Some(failed.output.to_path_buf()),
                });
                self.unit.failures.remove(failed.key);
                self.unit.notes.insert(
                    failed.key.to_string(),
                    Note {
                        title: format!("Check `{name}`"),
                        body: format!(
                            "`{}` {} on commit `{}` and passed when run again: a flake. Both outputs are kept (`{}`, `{}`).",
                            command.display(),
                            failed.how,
                            short_commit(&self.unit.checkpoint),
                            failed.output.display(),
                            path.display()
                        ),
                    },
                );
                self.thread.say(&format!(
                    ":recycle: *{}* · `{}` {} and then passed when run again: a flake, not the change's, recorded and passed",
                    failed.key,
                    escape(&command.display()),
                    failed.how
                ));
                return Ok(Triaged::End(StepEnd::Outcome("pass".to_string())));
            }
            found.push(format!(
                "Run once more on the change, it {} again (`{}`).",
                verdict_text(&again.verdict),
                path.display()
            ));
        }
        if command.triage.base {
            match self.run_at_base(step, failed)? {
                AtBase::FailsToo(reason) => return Ok(Triaged::End(StepEnd::Park(reason))),
                AtBase::Found(line) => found.push(line),
            }
        }
        let signature = failure_signature(&failed.result.output, &command.failure_lines)?;
        if let Some(previous) = self.unit.failures.get(failed.key)
            && previous.signature == signature
        {
            let reason = format!(
                "the command `{name}` {} the same way as it did on commit `{}`, after the fix round between: another round is likely to go as that one did, so none was sent. What failed, both times:\n\n{}\n\nThe output is in `{}`.",
                failed.how,
                short_commit(&previous.commit),
                truncate(&signature, 1500),
                failed.output.display()
            );
            self.event(
                "command.repeated",
                Some(&step.id),
                json!({ "command": name, "commit": self.unit.checkpoint, "previous": previous.commit }),
            )?;
            return Ok(Triaged::End(StepEnd::Park(reason)));
        }
        self.unit.failures.insert(
            failed.key.to_string(),
            Failure {
                signature,
                commit: self.unit.checkpoint.clone(),
            },
        );
        Ok(Triaged::Found(found))
    }

    /// Run a failed command on a copy of the worktree with every file the
    /// change touched put back as it is at the unit's base.
    fn run_at_base(&mut self, step: &Step, failed: &Failed) -> Result<AtBase, DriverError> {
        let name = failed.name;
        let command = failed.command;
        let base = self.unit.base.clone();
        let short_base = short_commit(&base);
        self.thread.status(&format!(
            ":arrows_counterclockwise: *{}* · `{}` {}: running it on the base `{short_base}`",
            failed.key,
            escape(&command.display()),
            failed.how
        ));
        let dest = self.dir.join(format!("at-base-{:03}", self.unit.runs));
        let result = match copy_at_base(&self.unit.worktree, &base, &dest) {
            Ok(_) => run_command_with_grace(
                failed.argv,
                &dest,
                Duration::from_secs(command.timeout_secs),
                Duration::from_secs(command.stop_grace_secs),
            ),
            Err(err) => {
                let _ = std::fs::remove_dir_all(&dest);
                self.event(
                    "command.base",
                    Some(&step.id),
                    json!({ "command": name, "base": base, "error": err.to_string() }),
                )?;
                return Ok(AtBase::Found(format!(
                    "It could not be run on the base `{short_base}`: {err}."
                )));
            }
        };
        let _ = std::fs::remove_dir_all(&dest);
        let _ = std::fs::remove_file(dest.with_extension("index"));
        let path = self.artifact(&format!("{}-base-output.txt", failed.key), &result.output)?;
        self.event(
            "command.base",
            Some(&step.id),
            json!({
                "command": name, "base": base, "verdict": format!("{:?}", result.verdict),
                "secs": result.duration.as_secs(), "output": path,
            }),
        )?;
        let code = match result.verdict {
            Verdict::Passed => {
                return Ok(AtBase::Found(format!(
                    "On the base `{short_base}`, with every file the change touched put back, it passed: the failure comes with the change (`{}`).",
                    path.display()
                )));
            }
            Verdict::CouldNotRun(ref why) => {
                return Ok(AtBase::Found(format!(
                    "On the base `{short_base}` it could not run ({why}), so nothing is known from there."
                )));
            }
            Verdict::Failed { code } => code,
            Verdict::TimedOut => None,
        };
        if let Some(why) = host_fault(
            &result.output,
            code,
            &command.could_not_run_when,
            &command.could_not_run_exit,
        )? {
            return Ok(AtBase::Found(format!(
                "On the base `{short_base}` it could not run properly ({why}), so nothing is known from there."
            )));
        }
        if !command.failure_lines.is_empty() {
            let here = failure_signature(&failed.result.output, &command.failure_lines)?;
            let there = failure_signature(&result.output, &command.failure_lines)?;
            if !here
                .lines()
                .any(|line| there.lines().any(|other| other == line))
            {
                return Ok(AtBase::Found(format!(
                    "On the base `{short_base}` it {} too, but on other things: what fails with the change does not fail there, so it is the change's (`{}`).",
                    verdict_text(&result.verdict),
                    path.display()
                )));
            }
        }
        Ok(AtBase::FailsToo(format!(
            "the command `{name}` {} on the change, and {} on the base `{short_base}` as well, with every file the change touched put back: the failure is not the change's, so no builder was sent. The change's output is in `{}`, the base's in `{}`. The end of the base's:\n\n```\n{}\n```",
            failed.how,
            verdict_text(&result.verdict),
            failed.output.display(),
            path.display(),
            tail(&result.output, 30)
        )))
    }

    /// An action could not do its job. Routed as `failed` if the flow routes
    /// that, and parked otherwise.
    fn action_failed(&mut self, step: &Step, reason: String) -> Result<StepEnd, DriverError> {
        self.event("action.failed", Some(&step.id), json!({ "reason": reason }))?;
        self.unit.feedback = Some(reason.clone());
        self.thread.say(&format!(
            ":x: *{}* · {}",
            step.id,
            escape(&truncate(&reason, 1500))
        ));
        self.save()?;
        Ok(if step.on.contains_key("failed") {
            StepEnd::Outcome("failed".to_string())
        } else {
            StepEnd::Park(reason)
        })
    }

    fn pull_request_number(&self) -> Option<u64> {
        self.unit.pull_request_number.or_else(|| {
            self.unit
                .pull_request
                .as_deref()
                .and_then(|url| url.rsplit('/').next())
                .and_then(|last| last.parse().ok())
        })
    }

    /// Run the project's publish checks on a sub-goal's branch as a fix
    /// left it. Those checks read commits, and a ship publishes its commits
    /// unchanged, so what they refuse is said to the builder that made the
    /// commit, in the session that made it. A check that cannot run says
    /// nothing here: the publish step runs it again and parks on that.
    fn check_ship_commits(&mut self, step: &Step, cwd: &Path) -> Result<Vec<String>, DriverError> {
        let mut problems = Vec::new();
        for name in self.project.publish_checks.clone() {
            let Some(command) = self.project.commands.get(&name).cloned() else {
                continue;
            };
            let result = run_command(
                &command.argv,
                cwd,
                Duration::from_secs(command.timeout_secs),
            );
            self.event(
                "publish.check",
                Some(&step.id),
                json!({
                    "command": name, "verdict": format!("{:?}", result.verdict),
                    "when": "before the fix is accepted",
                }),
            )?;
            if matches!(result.verdict, Verdict::Failed { .. } | Verdict::TimedOut) {
                problems.push(format!(
                    "the project's check on what is published, `{name}` (`{}`), fails on this branch as you left it. The commits of this branch are published as they are, so a commit you made in this run has to pass it, and no later step can reword one. The end of its output:\n\n```\n{}\n```\n\nPut it right in the commits you made in this run: reword the last one with `git commit --amend`, or an earlier one of yours with a rebase over your own commits only. Leave every commit that was on the branch before you started exactly as it is, and change no file for this",
                    command.argv.join(" "),
                    tail(&result.output, 40)
                ));
            }
        }
        Ok(problems)
    }

    /// Whether the failed jobs of a red CI would be run again: the project
    /// allows it, they have not been run again that often on this commit,
    /// and each failure is a job of a workflow run, which is all that can
    /// be run again.
    fn may_rerun_ci(
        &self,
        settings: &factory_config::GitHub,
        failed: &[factory_github::Check],
        watch: &CiWatch,
    ) -> bool {
        settings.ci_reruns > 0
            && watch.reruns < settings.ci_reruns + watch.granted
            && runs_to_rerun(failed).is_some()
    }

    /// Have the failed jobs of a red CI run again on the same commit, if the
    /// project allows it and they have not been run again that often yet.
    /// No model is asked whether the failure looks like the change's: jobs
    /// that pass on the same commit answer that. True when they are running
    /// again, and the watch goes on.
    fn rerun_ci(
        &mut self,
        step: &Step,
        github: &GitHub,
        settings: &factory_config::GitHub,
        failed: &[factory_github::Check],
        watch: &mut CiWatch,
    ) -> Result<bool, DriverError> {
        if !self.may_rerun_ci(settings, failed, watch) {
            return Ok(false);
        }
        let Some(runs) = runs_to_rerun(failed) else {
            return Ok(false);
        };
        let repo_dir = self.project.repo.clone();
        let names: Vec<String> = failed.iter().map(|check| check.name.clone()).collect();
        // The failing log first: once the jobs run again it is the old
        // attempt's, and it is the evidence if they pass.
        let log = failed
            .iter()
            .find_map(|check| github.failed_log(&repo_dir, &settings.repo, check));
        let mut log_path = watch.log.clone();
        let mut signature = watch.signature.clone();
        if let Some(log) = &log {
            self.unit.runs += 1;
            log_path = Some(self.artifact(&format!("{}-failed-before-rerun.log", step.id), log)?);
            signature = failure_signature(log, &settings.ci_failure_lines)?;
        }
        for run in &runs {
            if let Err(err) = github.rerun_failed(&repo_dir, &settings.repo, run) {
                // The failure stands as it is, and goes where it always went.
                self.event(
                    "ci.rerun_refused",
                    Some(&step.id),
                    json!({ "run": run, "error": err.to_string() }),
                )?;
                return Ok(false);
            }
        }
        watch.reruns += 1;
        watch.replaced = failed
            .iter()
            .filter_map(|check| check.url.clone())
            .collect();
        for name in &names {
            if !watch.failed.contains(name) {
                watch.failed.push(name.clone());
            }
        }
        watch.signature = signature;
        watch.log = log_path.clone();
        self.unit.ci = Some(watch.clone());
        self.event(
            "ci.rerun",
            Some(&step.id),
            json!({
                "failed": names, "runs": runs, "times": watch.reruns,
                "commit": watch.commit, "log": log_path,
            }),
        )?;
        self.thread.say(&format!(
            ":arrows_counterclockwise: *{}* · {} failed on `{}`: running the failed jobs again on the same commit ({} of {}), before any builder is sent. A pass makes it a flake, which is recorded and filed; another failure goes to the builder.",
            step.id,
            escape(&names.join(", ")),
            short_commit(&watch.commit),
            watch.reruns,
            settings.ci_reruns + watch.granted
        ));
        self.save()?;
        Ok(true)
    }

    /// The `await-ci` action: watch the pull request's checks by polling, with
    /// no model involved. A failed check ends the wait as `red` once the
    /// checks still running have finished, or have been waited for long
    /// enough, with every failure and its log as feedback for the step that
    /// fixes them. `green` needs every check passed, and to still be so a
    /// little later.
    fn await_ci_step(&mut self, step: &Step) -> Result<StepEnd, DriverError> {
        let (Some(settings), Some(number)) =
            (self.project.github.clone(), self.pull_request_number())
        else {
            return Ok(StepEnd::Park(format!(
                "step `{}` waits for CI, but the unit has no pull request",
                step.id
            )));
        };
        let github = github_client(&self.factory, &settings);
        let repo_dir = self.project.repo.clone();
        let poll = Duration::from_secs(settings.ci_poll_secs.max(1));
        let settle = Duration::from_secs(settings.ci_settle_secs);
        let timeout = Duration::from_secs(settings.ci_timeout_secs);
        let red_wait = Duration::from_secs(settings.ci_red_wait_secs);
        let started = Instant::now();
        // What the time limit is counted from: the start, or the last time
        // failed jobs were run again, since those have to run in full.
        let mut clock = started;
        let mut red_since: Option<Instant> = None;
        let mut green_since: Option<Instant> = None;
        let mut last_status: Option<Instant> = None;
        let mut unreadable = 0;
        let mut waiting_on: Vec<String> = Vec::new();
        let short = self.unit.checkpoint[..self.unit.checkpoint.len().min(10)].to_string();
        // Where the watch stands on this commit: kept in the unit's record,
        // so that a watch taken up again after a restart does not run the
        // jobs again as if for the first time.
        let mut watch = self
            .unit
            .ci
            .clone()
            .filter(|watch| watch.commit == self.unit.checkpoint)
            .unwrap_or_else(|| CiWatch {
                commit: self.unit.checkpoint.clone(),
                ..CiWatch::default()
            });
        if settings.ci_reruns > 0 && watch.reported {
            // The failure on this commit was handed on, and the flow is
            // back with no new commit: a fix that found nothing of the
            // branch's to change, or someone who continued the unit from
            // here. The same failure is not news, so the jobs run again.
            watch.reported = false;
            watch.granted += 1;
            self.unit.ci = Some(watch.clone());
            self.save()?;
        }
        self.thread.say(&format!(
            ":eyes: *{}* · watching GitHub CI on `{short}`. No model runs while it waits: a failure goes back to the builder once the checks still running have finished, all green goes on.",
            step.id
        ));

        loop {
            if self.hear_owner() {
                return Ok(StepEnd::Park("the owner said stop".to_string()));
            }
            let status = match github.pull_request_status(&repo_dir, &settings.repo, number) {
                Ok(status) => {
                    unreadable = 0;
                    status
                }
                Err(err) => {
                    // GitHub being down is not the change's fault: keep looking.
                    unreadable += 1;
                    self.event(
                        "ci.unreadable",
                        Some(&step.id),
                        json!({ "error": err.to_string(), "times": unreadable }),
                    )?;
                    if started.elapsed() >= timeout {
                        return Ok(StepEnd::Park(format!(
                            "GitHub could not be read for the pull request's checks: {err}"
                        )));
                    }
                    if self.pause_wanted() {
                        return Ok(StepEnd::Pause);
                    }
                    std::thread::sleep(poll);
                    continue;
                }
            };
            if status.state == "MERGED" {
                // Someone merged it by hand. There is nothing left to wait for.
                self.event("ci.merged_elsewhere", Some(&step.id), json!({}))?;
                return Ok(StepEnd::Outcome("green".to_string()));
            }
            if status.state != "OPEN" {
                return Ok(StepEnd::Park(format!(
                    "the pull request is {}, so there is nothing to land",
                    status.state.to_lowercase()
                )));
            }
            if status.head != self.unit.checkpoint {
                return Ok(StepEnd::Park(format!(
                    "the pull request's head is `{}`, not the commit the factory published (`{}`): something else pushed to the branch",
                    status.head, self.unit.checkpoint
                )));
            }

            // A failed job that was run again is waited for until GitHub
            // shows the job that replaced it.
            let checks = without_replaced(&status.checks, &watch.replaced);
            match ci_verdict(&checks, &settings.required_checks) {
                CiVerdict::Red { failed }
                    if clock.elapsed() < timeout
                        && (!factory_github::red_is_final(
                            &checks,
                            red_since.map_or(Duration::ZERO, |since| since.elapsed()),
                            red_wait,
                        ) || (self.may_rerun_ci(&settings, &failed, &watch)
                            && checks.iter().any(|check| {
                                check.state == factory_github::CheckState::Pending
                            }))) =>
                {
                    // Red already, with checks still running. Another may
                    // fail for another reason: the builder is handed all
                    // of it at once. And a run's failed jobs can only be
                    // run again once the run has ended, so while that is
                    // what comes next the wait is not cut short.
                    let running: Vec<String> = checks
                        .iter()
                        .filter(|check| check.state == factory_github::CheckState::Pending)
                        .map(|check| check.name.clone())
                        .collect();
                    if red_since.is_none() {
                        red_since = Some(Instant::now());
                        self.event(
                            "ci.failing",
                            Some(&step.id),
                            json!({
                                "failed": failed.iter().map(|check| check.name.clone()).collect::<Vec<_>>(),
                                "running": running, "secs": started.elapsed().as_secs(),
                            }),
                        )?;
                        self.thread.say(&format!(
                            ":x: *{}* · CI has failed after {} on `{}`. {} other check(s) are still running: waiting up to {} for them, so the builder is handed every failure at once.",
                            step.id,
                            duration_text(started.elapsed().as_secs()),
                            escape(&failed[0].name),
                            running.len(),
                            duration_text(red_wait.as_secs())
                        ));
                    }
                    green_since = None;
                    waiting_on = running;
                    if last_status.is_none_or(|at| at.elapsed() >= Duration::from_secs(60)) {
                        last_status = Some(Instant::now());
                        self.thread.status(&format!(
                            ":x: *{}* · {} failed · {} · waiting on {} still running",
                            step.id,
                            failed.len(),
                            duration_text(started.elapsed().as_secs()),
                            waiting_on.len()
                        ));
                    }
                }
                CiVerdict::Red { failed }
                    if self.rerun_ci(step, &github, &settings, &failed, &mut watch)? =>
                {
                    // The failed jobs are running again on the same commit:
                    // the watch starts over for them.
                    clock = Instant::now();
                    red_since = None;
                    green_since = None;
                    last_status = None;
                }
                CiVerdict::Red { failed } => {
                    let names: Vec<String> = failed
                        .iter()
                        .map(|check| match &check.url {
                            Some(url) => format!("- `{}`: {url}", check.name),
                            None => format!("- `{}`", check.name),
                        })
                        .collect();
                    // The logs of the first few: the same defect often fails
                    // every row of a matrix, and three are enough to tell.
                    let logs: Vec<(String, String)> = failed
                        .iter()
                        .filter_map(|check| {
                            github
                                .failed_log(&repo_dir, &settings.repo, check)
                                .map(|log| (check.name.clone(), log))
                        })
                        .take(3)
                        .collect();
                    let log = logs.first().cloned();
                    if let Some((_, text)) = &log {
                        // GitHub puts the job, the step and the time before
                        // every line; the classifier is sent the line.
                        let plain: Vec<&str> = text
                            .lines()
                            .map(|line| {
                                let line = line.rsplit('\t').next().unwrap_or(line);
                                match line.split_once(' ') {
                                    Some((stamp, rest))
                                        if stamp.ends_with('Z')
                                            && stamp.starts_with(|c: char| c.is_ascii_digit()) =>
                                    {
                                        rest
                                    }
                                    _ => line,
                                }
                            })
                            .collect();
                        self.judge(
                            &step.id,
                            json!({ "ci": failed.iter().map(|check| check.name.clone()).collect::<Vec<_>>() }),
                            &format!(
                                "Failed CI jobs: {}.",
                                failed
                                    .iter()
                                    .map(|check| check.name.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ),
                            &plain.join("\n"),
                            &settings.ci_failure_lines,
                        );
                    }
                    let mut feedback = format!(
                        "GitHub CI failed on the pull request, on commit `{short}`. The factory's own checks passed on this machine, so look for what differs there: the platform, the optimisation mode, timing.\n\nFailed checks:\n{}\n",
                        names.join("\n")
                    );
                    let mut log_path = None;
                    if let Some((name, log)) = &log {
                        self.unit.runs += 1;
                        let path = self.artifact(&format!("{}-failed.log", step.id), log)?;
                        feedback.push_str(&format!(
                            "\nThe end of the failing log of `{name}`:\n\n```\n{}\n```\n\nThe full failing log is in `{}`.\n",
                            tail(log, 150),
                            path.display()
                        ));
                        log_path = Some(path);
                    }
                    for (index, (name, log)) in logs.iter().enumerate().skip(1) {
                        let path =
                            self.artifact(&format!("{}-failed-{}.log", step.id, index + 1), log)?;
                        feedback.push_str(&format!(
                            "\nThe end of the failing log of `{name}`:\n\n```\n{}\n```\n\nThe full failing log is in `{}`.\n",
                            tail(log, 60),
                            path.display()
                        ));
                    }
                    if failed.len() > 1 {
                        feedback.push_str("\nMore than one check failed. They may share one cause or have several: account for each before you return.\n");
                    }
                    if let Some(note) = &settings.ci_failure_note {
                        feedback.push_str(&format!(
                            "\n{}\n",
                            note.trim()
                                .replace("{config}", &self.config.0.to_string_lossy())
                        ));
                    }
                    if watch.reruns > 0 {
                        feedback.push_str(&format!(
                            "\nWhat the factory found out before sending you:\n\n- The failed jobs were run again on this commit, {} time(s), and failed each time.\n",
                            watch.reruns
                        ));
                    }
                    feedback.push_str("\nReproduce the failure if you can, fix its cause, and run the project's commands before you return.");
                    // Where the step this goes to may say so, a failure that
                    // is not the branch's is not mended in the branch and
                    // not put to the owner.
                    let may_leave = step
                        .on
                        .get("red")
                        .and_then(|route| match &route.to {
                            Target::Step(id) => self.flow.outcomes(id).ok(),
                            _ => None,
                        })
                        .is_some_and(|outcomes| outcomes.iter().any(|o| o == "already_done"));
                    if may_leave && settings.ci_reruns > 0 {
                        feedback.push_str(" If what failed is not this branch's doing (a test the branch does not touch, which also fails now and then on the default branch or on another pull request), change nothing and return `already_done` with what you found: the factory runs the failed jobs again by itself, and the defect goes to whoever runs the goal, to be filed and fixed at its cause. Do not make a commit only to start CI again, and do not ask the owner whether to run it again.");
                    }
                    self.unit.feedback = Some(feedback);
                    watch.reported = true;
                    self.unit.ci = Some(watch.clone());
                    self.event(
                        "ci.red",
                        Some(&step.id),
                        json!({
                            "failed": failed.iter().map(|check| check.name.clone()).collect::<Vec<_>>(),
                            "log": log_path, "secs": started.elapsed().as_secs(),
                            "reruns": watch.reruns,
                        }),
                    )?;
                    let shown = log
                        .as_ref()
                        .map(|(_, log)| code_block(&truncate(&tail(log, 20), 2200)))
                        .unwrap_or_default();
                    self.thread.say(&format!(
                        ":x: *{}* · CI failed after {} · handing it back to the builder\n{}\n{shown}",
                        step.id,
                        duration_text(started.elapsed().as_secs()),
                        escape(&names.join("\n"))
                    ));
                    self.save()?;
                    return Ok(StepEnd::Outcome("red".to_string()));
                }
                CiVerdict::Green { passed } => match green_since {
                    // Seen green twice, far enough apart: no check appeared late.
                    Some(since) if since.elapsed() >= settle => {
                        if watch.reruns > 0 && !watch.failed.is_empty() {
                            // Failed and then passed on one commit: not
                            // the change's, and still somebody's. Whoever
                            // started the pipeline is handed it when the
                            // pipeline ends.
                            let check = format!("CI: {}", watch.failed.join(", "));
                            self.event(
                                "ci.flaky",
                                Some(&step.id),
                                json!({
                                    "failed": watch.failed, "reruns": watch.reruns,
                                    "commit": watch.commit, "log": watch.log,
                                }),
                            )?;
                            self.thread.say(&format!(
                                ":recycle: *{}* · {} failed on `{short}` and passed when run again: a flake, not the change's. It is recorded, and goes to whoever runs this work to be filed.",
                                step.id,
                                escape(&watch.failed.join(", "))
                            ));
                            self.unit.flakes.push(Flake {
                                step: step.id.clone(),
                                check,
                                commit: watch.commit.clone(),
                                signature: watch.signature.clone(),
                                output: watch.log.clone(),
                            });
                            watch.failed.clear();
                            self.unit.ci = Some(watch.clone());
                            self.save()?;
                        }
                        self.event(
                            "ci.green",
                            Some(&step.id),
                            json!({ "checks": passed, "secs": started.elapsed().as_secs() }),
                        )?;
                        self.thread.say(&format!(
                            ":white_check_mark: *{}* · all {passed} checks passed on `{short}` · {}",
                            step.id,
                            duration_text(started.elapsed().as_secs())
                        ));
                        return Ok(StepEnd::Outcome("green".to_string()));
                    }
                    Some(_) => {}
                    None => green_since = Some(Instant::now()),
                },
                CiVerdict::Pending {
                    passed,
                    total,
                    waiting,
                } => {
                    green_since = None;
                    waiting_on = waiting;
                    if last_status.is_none_or(|at| at.elapsed() >= Duration::from_secs(60)) {
                        last_status = Some(Instant::now());
                        let mut names = waiting_on.clone();
                        let more = names.len().saturating_sub(3);
                        names.truncate(3);
                        self.thread.status(&format!(
                            ":hourglass_flowing_sand: *{}* · {passed}/{total} checks passed · {} · waiting on {}{}",
                            step.id,
                            duration_text(started.elapsed().as_secs()),
                            escape(&names.join(", ")),
                            if more > 0 { format!(" and {more} more") } else { String::new() }
                        ));
                    }
                }
            }
            if clock.elapsed() >= timeout {
                return Ok(StepEnd::Park(format!(
                    "CI had not finished after {}; still waiting on: {}",
                    duration_text(timeout.as_secs()),
                    waiting_on.join(", ")
                )));
            }
            // Waiting for CI can take hours, and is safe to take up again.
            if self.pause_wanted() {
                return Ok(StepEnd::Pause);
            }
            std::thread::sleep(poll);
        }
    }

    /// The `land` action: merge the pull request, confirm the merge commit is
    /// on the default branch, close the bead and clean up.
    fn land_step(&mut self, step: &Step) -> Result<StepEnd, DriverError> {
        match self.land(step) {
            Ok(end) => Ok(end),
            Err(err) => self.action_failed(step, format!("landing failed: {err}")),
        }
    }

    fn land(&mut self, step: &Step) -> Result<StepEnd, DriverError> {
        let (Some(settings), Some(number)) =
            (self.project.github.clone(), self.pull_request_number())
        else {
            return Ok(StepEnd::Park(format!(
                "step `{}` lands a pull request, but the unit has none",
                step.id
            )));
        };
        let github = github_client(&self.factory, &settings);
        let repo_dir = self.project.repo.clone();
        let read = |github: &GitHub| {
            github
                .pull_request_status(&repo_dir, &settings.repo, number)
                .map_err(|err| DriverError::Preflight(err.to_string()))
        };
        self.thread.status(&format!(
            ":arrows_counterclockwise: *{}* · merging",
            step.id
        ));

        let mut status = read(&github)?;
        if status.state == "OPEN" {
            if status.head != self.unit.checkpoint {
                return Ok(StepEnd::Park(format!(
                    "the pull request's head is `{}`, not the commit the factory published (`{}`)",
                    status.head, self.unit.checkpoint
                )));
            }
            // GitHub works out whether a pull request can merge when asked.
            for _ in 0..10 {
                if status.mergeable != "UNKNOWN" {
                    break;
                }
                std::thread::sleep(Duration::from_secs(3));
                status = read(&github)?;
            }
            if status.mergeable != "MERGEABLE" {
                return self.action_failed(
                    step,
                    format!(
                        "the pull request cannot be merged as it stands (GitHub says `{}`): the default branch has probably changed the same files since the unit started",
                        status.mergeable
                    ),
                );
            }
            if !matches!(
                ci_verdict(&status.checks, &settings.required_checks),
                CiVerdict::Green { .. }
            ) {
                return self.action_failed(
                    step,
                    "the pull request's checks are no longer all green".to_string(),
                );
            }
            let title = self
                .publication()
                .and_then(|text| {
                    text.get("pr_title")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_else(|| self.unit.title.clone());
            let subject = settings.merge_subject.as_ref().map(|template| {
                template
                    .replace("{bead_short}", self.project.bead_short(&self.unit.bead))
                    .replace("{bead}", &self.unit.bead)
                    .replace("{title}", &title)
                    .replace("{number}", &number.to_string())
            });
            let method = match settings.merge_method {
                factory_config::MergeMethod::Merge => MergeMethod::Merge,
                factory_config::MergeMethod::Squash => MergeMethod::Squash,
                factory_config::MergeMethod::Rebase => MergeMethod::Rebase,
            };
            let body = settings.merge_body.as_ref().map(|template| {
                let passed = match ci_verdict(&status.checks, &settings.required_checks) {
                    CiVerdict::Green { passed } => passed,
                    _ => status.checks.len(),
                };
                self.merge_body(template, &title, number, passed)
            });
            github
                .merge(
                    &repo_dir,
                    &settings.repo,
                    &MergeRequest {
                        number,
                        method,
                        head: &status.head,
                        subject: subject.as_deref(),
                        body: body.as_deref(),
                    },
                )
                .map_err(|err| DriverError::Preflight(err.to_string()))?;
            self.event(
                "landing.merge_requested",
                Some(&step.id),
                json!({ "head": status.head, "subject": subject, "method": format!("{method:?}") }),
            )?;
            for _ in 0..20 {
                status = read(&github)?;
                if status.state == "MERGED" {
                    break;
                }
                std::thread::sleep(Duration::from_secs(3));
            }
        }
        if status.state != "MERGED" {
            return self.action_failed(
                step,
                format!(
                    "GitHub reports the pull request as {}, not merged",
                    status.state.to_lowercase()
                ),
            );
        }
        let Some(merge_commit) = status.merge_commit.clone() else {
            return self.action_failed(
                step,
                "GitHub gave no merge commit for the merged pull request".to_string(),
            );
        };

        // A bead is closed only when its change is on the default branch, as
        // git sees it and not only as GitHub says.
        factory_git::fetch(&repo_dir, &self.project.remote)?;
        let default = format!("{}/{}", self.project.remote, self.project.default_branch);
        if !factory_git::is_ancestor(&repo_dir, &merge_commit, &default)? {
            return self.action_failed(
                step,
                format!("the merge commit `{merge_commit}` is not on `{default}`"),
            );
        }
        self.unit.merged = Some(merge_commit.clone());
        self.event(
            "landed",
            Some(&step.id),
            json!({ "merge_commit": merge_commit }),
        )?;
        self.save()?;

        let url = self.unit.pull_request.clone().unwrap_or_default();
        // A unit that ships a sub-goal closes every bead whose work it held.
        let closes = if self.unit.closes.is_empty() {
            vec![self.unit.bead.clone()]
        } else {
            self.unit.closes.clone()
        };
        for bead in &closes {
            bead_source(&self.project).close(
                bead,
                &format!("Merged {url} as {merge_commit}; every CI check passed"),
                &self.project.bead_actor,
            )?;
            self.event("bead.closed", Some(&step.id), json!({ "bead": bead }))?;
            // A bead that builds the check for a class of defect: the
            // class is checked from here on.
            let classes = check_landed(&self.factory.state_dir, self.project.id.as_str(), bead)?;
            if !classes.is_empty() {
                self.event(
                    "check.landed",
                    Some(&step.id),
                    json!({ "bead": bead, "classes": classes }),
                )?;
            }
        }

        // Tidying up. A failure here leaves litter, not a wrong result.
        let mut litter = Vec::new();
        if self.unit.worktree.exists()
            && let Err(err) = factory_git::worktree_remove(&repo_dir, &self.unit.worktree)
        {
            litter.push(err.to_string());
        }
        if let Err(err) = factory_git::delete_branch(&repo_dir, &self.unit.branch) {
            litter.push(err.to_string());
        }
        if settings.delete_branch_after_merge
            && let Err(err) = factory_git::delete_remote_branch(
                &repo_dir,
                &self.project.remote,
                &self.unit.branch,
            )
        {
            litter.push(err.to_string());
        }
        self.drop_holding(&step.id);
        self.event("cleanup", Some(&step.id), json!({ "problems": litter }))?;
        self.save()?;
        Ok(StepEnd::Outcome("done".to_string()))
    }

    /// The body of the merge commit: the publisher's paragraph on what the
    /// branch adds, then the factory's own account of what passed.
    fn merge_body(&self, template: &str, title: &str, number: u64, ci_passed: usize) -> String {
        let summary = self
            .publication()
            .and_then(|text| {
                text.get("merge_summary")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .filter(|summary| !summary.trim().is_empty())
            .unwrap_or_else(|| format!("{title}."));
        // Every must-pass command of the flow that ran: the unit could not
        // have reached the landing unless each passed on the published code.
        let mut commands: Vec<String> = Vec::new();
        for step in &self.flow.steps {
            if !self.unit.usage.contains_key(&step.id) {
                continue;
            }
            let named: Vec<&String> = match &step.kind {
                StepKind::Command {
                    command,
                    policy: ExitPolicy::MustPass,
                } => vec![command],
                StepKind::Commands { members } => {
                    members.iter().map(|member| &member.command).collect()
                }
                _ => Vec::new(),
            };
            for command in named {
                let Some(spec) = self.project.commands.get(command) else {
                    continue;
                };
                let shown = spec
                    .display()
                    .replace("{base}", &self.project.default_branch);
                if !commands.contains(&shown) {
                    commands.push(shown);
                }
            }
        }
        let short = &self.unit.checkpoint[..self.unit.checkpoint.len().min(10)];
        let testing = format!(
            "Testing: on {}-{} the factory ran {}, and each passed on the published commit ({short}); all {ci_passed} CI checks passed on pull request {number}'s head.",
            std::env::consts::ARCH,
            std::env::consts::OS,
            commands.join(", "),
        );
        // The published commit's own message, for a squash: its body, and
        // the block of trailers it ends with.
        let message = self
            .publication()
            .and_then(|text| {
                text.get("commit_message")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_default();
        let (commit_body, commit_trailers) = split_message(&message);
        let filled = template
            .replace("{summary}", &wrap(&summary, 72))
            .replace("{commit_body}", &commit_body)
            .replace("{commit_trailers}", &commit_trailers)
            .replace("{testing}", &wrap(&testing, 72))
            .replace("{bead_short}", self.project.bead_short(&self.unit.bead))
            .replace("{bead}", &self.unit.bead);
        // A part that came out empty must not leave a hole in the message.
        filled
            .split("\n\n")
            .map(str::trim)
            .filter(|paragraph| !paragraph.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// What a step that needs an artifact is told about it.
    fn artifact_note(&self, name: &str) -> Result<Note, String> {
        let artifact = self
            .unit
            .artifacts
            .get(name)
            .ok_or_else(|| "it has not been produced".to_string())?;
        if artifact.commit != self.unit.checkpoint {
            return Err(format!(
                "it describes commit `{}`, but the code is now at `{}`",
                artifact.commit, self.unit.checkpoint
            ));
        }
        let spec = self
            .flow
            .step(&artifact.step)
            .and_then(|step| match &step.kind {
                StepKind::Command { command, .. } => self.project.commands.get(command),
                _ => None,
            });
        let short = &artifact.commit[..artifact.commit.len().min(10)];
        let mut body = format!(
            "Written by `{}`, with no model involved, on commit `{short}`: the commit you are looking at. The command {}.\n\nThey are in `{}`.",
            artifact.command,
            if artifact.ended == "passed" {
                "exited 0".to_string()
            } else {
                artifact.ended.clone()
            },
            artifact.path.display()
        );
        if let Some(note) = spec.and_then(|spec| spec.note.as_ref()) {
            body.push_str(&format!("\n\n{}", note.trim()));
        }
        if let Some(file) = spec.and_then(|spec| spec.summary.as_ref()) {
            match std::fs::read_to_string(artifact.path.join(file)) {
                Ok(text) => body.push_str(&format!(
                    "\n\n`{file}`:\n\n```\n{}\n```",
                    truncate(text.trim_end(), 6000)
                )),
                Err(err) => return Err(format!("its `{file}` cannot be read: {err}")),
            }
        }
        Ok(Note {
            title: format!("Reports: `{name}`"),
            body,
        })
    }

    /// The latest result of a step that works to the publication contract.
    fn publication(&self) -> Option<Value> {
        self.flow
            .steps
            .iter()
            .rev()
            .filter(|other| matches!(&other.kind, StepKind::Agent { role, .. } if contract_of(role) == Some(Contract::Publication)))
            .find_map(|other| self.unit.results.get(&other.id))
            .cloned()
    }

    /// Run a reproduction or regression command the way the project would.
    fn run_proof(&self, command: &str, cwd: &Path) -> Verdict {
        run_command(
            &["sh".to_string(), "-c".to_string(), command.to_string()],
            cwd,
            Duration::from_secs(self.project.limits.reproduction_timeout_secs),
        )
        .verdict
    }

    /// Check a tester's findings by running them. What it left in its
    /// worktree must only add lines, and each critical or major finding's
    /// command must fail there: a command that passes, or cannot run, proves
    /// nothing. A hang counts as failing once the time allowed runs out.
    fn verify_reproductions(
        &self,
        scratch: &Path,
        result: &Value,
    ) -> Result<Vec<String>, DriverError> {
        let mut problems = Vec::new();
        let findings = serious(result);
        if findings.is_empty() {
            return Ok(problems);
        }
        let left = factory_git::numstat_since(scratch, &self.unit.checkpoint)?;
        if left.is_empty() {
            problems.push("you left no regression test in the worktree: a critical or major finding needs a test in the project's own suite that fails because of it. Add it and leave it there; remove everything else you wrote".to_string());
        }
        let rewrote: Vec<&str> = left
            .iter()
            .filter(|(_, removed, _)| *removed > 0)
            .map(|(_, _, path)| path.as_str())
            .collect();
        if !rewrote.is_empty() {
            problems.push(format!(
                "what you leave joins the branch as regression tests, so it may only add lines; you changed or removed existing lines in: {}. Restore them and add your tests beside the code",
                rewrote.join(", ")
            ));
        }
        for finding in findings {
            let title = finding.get("title").and_then(Value::as_str).unwrap_or("");
            let command = finding
                .pointer("/reproduction/command")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            let names = finding
                .get("test_names")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            if names == 0 {
                problems.push(format!(
                    "the finding `{title}` names no regression test in `test_names`"
                ));
            }
            match self.run_proof(command, scratch) {
                Verdict::Failed { .. } | Verdict::TimedOut => {}
                Verdict::Passed => problems.push(format!(
                    "the factory ran the reproduction of `{title}` (`{command}`) in your worktree and it passed: a finding needs a command that fails because of it. Fix the test or drop the finding"
                )),
                Verdict::CouldNotRun(reason) => problems.push(format!(
                    "the reproduction of `{title}` (`{command}`) could not be run: {reason}"
                )),
            }
        }
        Ok(problems)
    }

    /// Add the tester's regression tests to the unit's branch as a commit of
    /// their own, and remember the command that proves each finding.
    fn adopt_regressions(
        &mut self,
        step: &Step,
        round: u32,
        patch: &Path,
        result: &Value,
    ) -> Result<(), DriverError> {
        let worktree = self.unit.worktree.clone();
        factory_git::apply_patch(&worktree, patch)?;
        let before = self.unit.checkpoint.clone();
        let (head, _) = factory_git::checkpoint(
            &worktree,
            &format!("tests: adversarial round {round} ({})", self.unit.bead),
        )?;
        let files = factory_git::changed_files(&worktree, &before, &head)?;
        let forbidden =
            scope_violations(&files, &["**".to_string()], &self.project.forbidden_paths)?;
        if !forbidden.is_empty() {
            return Err(DriverError::Preflight(format!(
                "they touch paths no step may change: {}",
                forbidden
                    .iter()
                    .map(|v| v.path.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        self.unit.checkpoint = head.clone();
        for finding in serious(result) {
            let text = |pointer: &str| {
                finding
                    .pointer(pointer)
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string()
            };
            self.unit.regressions.push(Regression {
                round,
                finding: text("/title"),
                command: text("/reproduction/command"),
                test_names: finding
                    .get("test_names")
                    .and_then(Value::as_array)
                    .map(|names| {
                        names
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default(),
                category: text("/category"),
            });
        }
        self.event(
            "regressions.added",
            Some(&step.id),
            json!({ "commit": head, "files": files, "round": round,
                    "commands": self.unit.regressions.iter().filter(|r| r.round == round).map(|r| r.command.clone()).collect::<Vec<_>>() }),
        )?;
        self.thread.say(&format!(
            ":test_tube: *{}* · the tester's regression tests joined the branch as `{}` ({} file(s)). The fix is not accepted until they pass.",
            step.id,
            &head[..head.len().min(10)],
            files.len()
        ));
        self.save()
    }

    /// Run every regression test of the unit. A fix is refused while one
    /// fails, or while a test it should run has disappeared from the code.
    fn check_regressions(&self, worktree: &Path) -> Result<Vec<String>, DriverError> {
        let mut problems = Vec::new();
        let mut ran = std::collections::BTreeSet::new();
        for regression in &self.unit.regressions {
            for name in &regression.test_names {
                if !factory_git::tree_contains(worktree, name)? {
                    problems.push(format!(
                        "the regression test `{name}` (for `{}`) is no longer in the code: restore it",
                        regression.finding
                    ));
                }
            }
            if regression.command.is_empty() || !ran.insert(regression.command.clone()) {
                continue;
            }
            match self.run_proof(&regression.command, worktree) {
                Verdict::Passed => {}
                verdict => problems.push(format!(
                    "the regression test for `{}` still fails ({verdict:?}) when the factory runs `{}` in your worktree",
                    regression.finding, regression.command
                )),
            }
        }
        Ok(problems)
    }

    /// The `absorb` action: the unit's one commit joins the branch of the
    /// sub-goal it belongs to. Nothing is pushed, no pull request is opened
    /// and the bead stays open: all of that happens once, when the sub-goal
    /// ships.
    fn absorb(&mut self, step: &Step, commit: &str) -> Result<StepEnd, DriverError> {
        let Some(target) = self.unit.absorb_into.clone() else {
            return Ok(StepEnd::Park(format!(
                "step `{}` absorbs into a sub-goal, but this unit belongs to none",
                step.id
            )));
        };
        // The unit branched from the sub-goal's tip, so this only moves the
        // tip forward. If something else moved it, nothing is merged here.
        factory_git::fast_forward(&target, commit)?;
        self.unit.absorbed = Some(commit.to_string());
        self.event(
            "absorbed",
            Some(&step.id),
            json!({ "commit": commit, "into": target }),
        )?;
        // The bead's work is now on the sub-goal's branch. That branch goes
        // to the remote, and only when it is there does the bead's own
        // branch leave it: until then it is the only copy off this machine.
        if let Ok(subgoal) = factory_git::current_branch(&target) {
            match push_if_moved(&self.factory, &self.project, &target, &subgoal, None) {
                None => {}
                Some(Ok(pushed)) => {
                    self.event(
                        "subgoal.synced",
                        Some(&step.id),
                        json!({ "branch": subgoal, "commit": pushed }),
                    )?;
                    if self.unit.synced.take().is_some() {
                        let gone = factory_git::delete_remote_branch(
                            &self.project.repo,
                            &self.project.remote,
                            &self.unit.branch,
                        );
                        self.event(
                            "branch.unsynced",
                            Some(&step.id),
                            json!({ "branch": self.unit.branch, "error": gone.err().map(|err| err.to_string()) }),
                        )?;
                    }
                }
                Some(Err(error)) => self.event(
                    "subgoal.sync_failed",
                    Some(&step.id),
                    json!({ "branch": subgoal, "error": error }),
                )?,
            }
        }
        let repo = self.project.repo.clone();
        let mut litter = Vec::new();
        if let Err(err) = factory_git::worktree_remove(&repo, &self.unit.worktree) {
            litter.push(err.to_string());
        }
        if let Err(err) = factory_git::delete_branch(&repo, &self.unit.branch) {
            litter.push(err.to_string());
        }
        self.event("cleanup", Some(&step.id), json!({ "problems": litter }))?;
        self.thread.say(&format!(
            ":inbox_tray: *{}* · absorbed into the sub-goal's branch as `{}`. The bead stays open until the sub-goal lands.",
            step.id,
            &commit[..commit.len().min(10)]
        ));
        self.save()?;
        Ok(StepEnd::Outcome("done".to_string()))
    }

    /// Conflicts handed to a builder must be gone before anything else
    /// happens to the branch. Returns how the step ends while one is left.
    fn unresolved(&mut self, step: &Step) -> Result<Option<StepEnd>, DriverError> {
        let left = factory_git::with_conflict_markers(&self.unit.worktree, &self.unit.conflicts);
        if left.is_empty() {
            self.unit.conflicts.clear();
            return Ok(None);
        }
        let reason = format!(
            "These files still hold conflict markers from the merge into your branch: {}. Resolve each one so that both changes do what they were for, and remove the markers.",
            left.join(", ")
        );
        self.send_back(step, "conflict", reason).map(Some)
    }

    /// Before a unit is absorbed: if another bead of its sub-goal was
    /// absorbed since this one branched, merge the sub-goal's branch into
    /// the unit's. Returns how the step ends when the unit is not ready to
    /// be absorbed yet, and `None` when it is.
    fn reconcile(&mut self, step: &Step) -> Result<Option<StepEnd>, DriverError> {
        let Some(target) = self.unit.absorb_into.clone() else {
            return Ok(None);
        };
        if let Some(end) = self.unresolved(step)? {
            return Ok(Some(end));
        }
        let tip = factory_git::rev_parse(&target, "HEAD")?;
        if tip == self.unit.base {
            return Ok(None);
        }
        self.take_in(step, &tip, true).map(Some)
    }

    /// The `sync` action: bring the unit's branch up to date with the
    /// project's default branch as the remote has it now. A sub-goal that
    /// ships after other work landed takes that work in before anything is
    /// verified, so that what is checked is what will land.
    fn sync_step(&mut self, step: &Step) -> Result<StepEnd, DriverError> {
        let synced = (|| {
            if let Some(end) = self.unresolved(step)? {
                return Ok(end);
            }
            factory_git::fetch(&self.project.repo, &self.project.remote)?;
            let worktree = self.unit.worktree.clone();
            let target = factory_git::rev_parse(
                &worktree,
                &format!("{}/{}", self.project.remote, self.project.default_branch),
            )?;
            if factory_git::is_ancestor(&worktree, &target, "HEAD")? {
                return Ok(StepEnd::Outcome("current".to_string()));
            }
            self.take_in(step, &target, false)
        })();
        match synced {
            Ok(end) => Ok(end),
            Err(err) => {
                let err: DriverError = err;
                self.event(
                    "sync.failed",
                    Some(&step.id),
                    json!({ "error": err.to_string() }),
                )?;
                Ok(StepEnd::Park(format!(
                    "the branch could not be brought up to date with the default branch: {err}"
                )))
            }
        }
    }

    /// Merge `tip` into the unit's branch and make it the unit's new base:
    /// from here the unit's own change is what it adds to `tip`. `sibling`
    /// says whose work `tip` holds: another bead of the same sub-goal, or
    /// whatever landed on the default branch. A clean merge ends the step
    /// as `moved` and one with conflicts as `conflict`.
    fn take_in(&mut self, step: &Step, tip: &str, sibling: bool) -> Result<StepEnd, DriverError> {
        let worktree = self.unit.worktree.clone();
        let (theirs, into, headline) = if sibling {
            (
                "the sub-goal's branch",
                "another bead of the sub-goal was absorbed first",
                "Another bead of the sub-goal was absorbed first",
            )
        } else {
            (
                "the default branch",
                "other work landed on the default branch first",
                "Other work landed on the default branch first",
            )
        };
        let conflicted = factory_git::merge_keeping_conflicts(
            &worktree,
            tip,
            &format!("checkpoint: merge {theirs} ({})", self.unit.bead),
        )?;
        let head = factory_git::rev_parse(&worktree, "HEAD")?;
        let from = std::mem::replace(&mut self.unit.base, tip.to_string());
        self.unit.checkpoint = head.clone();
        // On a new base a failure seen before says nothing about the next.
        self.unit.failures.clear();
        self.event(
            "tip.merged",
            Some(&step.id),
            json!({ "from": from, "onto": tip, "commit": head, "conflicts": conflicted, "sibling": sibling }),
        )?;
        let short = |commit: &str| commit[..commit.len().min(10)].to_string();
        if conflicted.is_empty() {
            self.unit.history.push(format!(
                "{}: {into}, so {theirs} ({}) was merged into this one. It merged cleanly, and the checks run on the two together.",
                step.id,
                short(tip)
            ));
            self.save()?;
            return self.send_back(
                step,
                "moved",
                format!(
                    "{headline}. It merged cleanly into this branch, and the checks have to pass on the two together."
                ),
            );
        }
        self.unit.conflicts = conflicted.clone();
        let reason = format!(
            "While this branch was being worked on, {into}. {} has been merged into yours, and these files conflicted: {}.\n\nEach holds git's conflict markers (`<<<<<<<`, `=======`, `>>>>>>>`), committed as a checkpoint: there is no merge in progress and nothing to abort or continue. Resolve every one so that both changes do what they were for: this branch's, and the other work's (`git log -p {}..{}` shows what it did). Do not drop the other work to make this branch's fit. Then make sure the result builds and its tests pass.",
            if sibling {
                "The sub-goal's branch"
            } else {
                "The default branch"
            },
            conflicted.join(", "),
            short(&from),
            short(tip)
        );
        self.send_back(step, "conflict", reason)
    }

    /// End an action with an outcome that leads back into the flow, with
    /// what the step it leads to needs to know. A flow that does not route
    /// the outcome parks the unit instead.
    fn send_back(
        &mut self,
        step: &Step,
        outcome: &str,
        reason: String,
    ) -> Result<StepEnd, DriverError> {
        self.thread.say(&format!(
            ":twisted_rightwards_arrows: *{}* · {}",
            step.id,
            escape(&truncate(&reason, 600))
        ));
        if !step.on.contains_key(outcome) {
            self.save()?;
            return Ok(StepEnd::Park(format!(
                "{reason}\n\nThe flow has no `{outcome}` route from `{}`, so the unit stops here.",
                step.id
            )));
        }
        if outcome == "conflict" {
            self.unit.feedback = Some(reason);
        }
        self.save()?;
        Ok(StepEnd::Outcome(outcome.to_string()))
    }

    /// The `publish` action: one final commit, a push, and the pull request.
    fn publish_step(&mut self, step: &Step) -> Result<StepEnd, DriverError> {
        // A unit joining a sub-goal's branch does so alone.
        let absorbing = matches!(&step.kind, StepKind::Action { action } if action == "absorb");
        let _alone = absorbing.then(|| ABSORBING.lock().unwrap_or_else(PoisonError::into_inner));
        match self.publish(step) {
            Ok(end) => Ok(end),
            Err(err) => {
                let reason = format!("publishing failed: {err}");
                self.event(
                    "publish.failed",
                    Some(&step.id),
                    json!({ "error": err.to_string() }),
                )?;
                self.unit.feedback = Some(reason.clone());
                self.thread.say(&format!(
                    ":x: *{}* · {}",
                    step.id,
                    escape(&truncate(&reason, 1500))
                ));
                if step.on.contains_key("failed") {
                    Ok(StepEnd::Outcome("failed".to_string()))
                } else {
                    Ok(StepEnd::Park(reason))
                }
            }
        }
    }

    fn publish(&mut self, step: &Step) -> Result<StepEnd, DriverError> {
        // The text comes from the latest step that works to the publication contract.
        let text = self
            .flow
            .steps
            .iter()
            .rev()
            .filter(|other| matches!(&other.kind, StepKind::Agent { role, .. } if contract_of(role) == Some(Contract::Publication)))
            .find_map(|other| self.unit.results.get(&other.id))
            .cloned();
        let Some(text) = text else {
            return Ok(StepEnd::Park(
                "nothing to publish with: no step has written the commit message".to_string(),
            ));
        };
        let field = |key: &str| {
            text.get(key)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        let (message, title, body) = (field("commit_message"), field("pr_title"), field("pr_body"));
        let worktree = self.unit.worktree.clone();
        let waits = if matches!(&step.kind, StepKind::Action { action } if action == "absorb") {
            self.reconcile(step)?
        } else {
            self.unresolved(step)?
        };
        if let Some(end) = waits {
            return Ok(end);
        }
        self.thread.status(&format!(
            ":arrows_counterclockwise: *{}* · commit, push, pull request",
            step.id
        ));

        if factory_git::commit_count(&worktree, &self.unit.base, "HEAD")? == 0 {
            return Ok(StepEnd::Park(
                "there is nothing to publish: the branch has no changes".to_string(),
            ));
        }
        let commit = if self.unit.ship {
            // A sub-goal's branch already holds one commit for each bead.
            // They are published as they are and squashed when the pull
            // request lands, with the message written for the whole.
            factory_git::rev_parse(&worktree, "HEAD")?
        } else {
            // The checkpoints become one commit. The content must not change.
            let tree = factory_git::tree_of(&worktree, "HEAD")?;
            let commit = factory_git::squash(&worktree, &self.unit.base, &message)?;
            if factory_git::tree_of(&worktree, &commit)? != tree {
                return Ok(StepEnd::Park(
                    "the final commit does not hold what the checks passed on".to_string(),
                ));
            }
            commit
        };
        self.unit.checkpoint = commit.clone();
        self.event(
            "published.commit",
            Some(&step.id),
            json!({ "commit": commit }),
        )?;
        self.save()?;

        // Checks that read the commit message can only run now that it exists.
        for name in self.project.publish_checks.clone() {
            let command = self.project.commands[&name].clone();
            let shown = command.argv.join(" ");
            let result = run_command(
                &command.argv,
                &worktree,
                Duration::from_secs(command.timeout_secs),
            );
            self.event(
                "publish.check",
                Some(&step.id),
                json!({
                    "command": name, "verdict": format!("{:?}", result.verdict), "commit": commit,
                }),
            )?;
            match result.verdict {
                Verdict::Passed => {}
                Verdict::CouldNotRun(reason) => {
                    return Ok(StepEnd::Park(format!(
                        "the publish check `{name}` could not run: {reason}"
                    )));
                }
                Verdict::Failed { .. } | Verdict::TimedOut => {
                    let reason = if self.unit.ship {
                        // A sub-goal's commits are published as they are:
                        // no text a publisher writes changes one of them.
                        // What the check refuses is put right in the branch.
                        format!(
                            "The commits of this branch are published as they are, and the project's publish check `{name}` (`{shown}`) refuses them, so nothing was pushed.\n\nThe end of its output:\n\n```\n{}\n```\n\nPut right what it names, and nothing else. Where it is a commit's message, reword that commit and change no file: `git commit --amend` for the last one; for an earlier one, rebase from its parent and reword that one alone. Every other commit keeps its message, and the files end exactly as they are now (`git diff` between the old and the new tip must show nothing). This is the one case in which you rewrite commits; the factory pushes the branch itself, so do not push. Where the check wants something added, such as a changelog entry, add it in a new commit. This is yours to do: do not ask the owner to do it.",
                            tail(&result.output, 60)
                        )
                    } else {
                        format!(
                            "The final commit did not pass the project's publish check `{name}` (`{shown}`), so nothing was pushed. The check reads the commit the factory makes from your commit message.\n\nThe end of its output:\n\n```\n{}\n```\n\nWrite the commit message and pull-request text again so that the check passes.",
                            tail(&result.output, 60)
                        )
                    };
                    self.unit.feedback = Some(reason.clone());
                    self.thread.say(&format!(
                        ":x: *{}* · {} failed `{}`; nothing was pushed\n{}",
                        step.id,
                        if self.unit.ship {
                            "the branch's commits"
                        } else {
                            "the final commit"
                        },
                        escape(&shown),
                        code_block(&truncate(&tail(&result.output, 15), 2000))
                    ));
                    self.save()?;
                    return Ok(if step.on.contains_key("failed") {
                        StepEnd::Outcome("failed".to_string())
                    } else {
                        StepEnd::Park(reason)
                    });
                }
            }
        }

        if matches!(&step.kind, StepKind::Action { action } if action == "absorb") {
            return self.absorb(step, &commit);
        }

        // The URL as the project configured it: that is what the write list is about.
        let url = factory_git::configured_remote_url(&self.project.repo, &self.project.remote)?;
        if let Some(github) = &self.project.github {
            let slug = factory_git::github_slug(&url).unwrap_or_default();
            github_client(&self.factory, github)
                .check_allowed(&slug)
                .map_err(|err| DriverError::Preflight(format!("push to `{url}`: {err}")))?;
        }
        factory_git::push(&worktree, &self.project.remote, &self.unit.branch)?;
        self.unit.synced = Some(commit.clone());
        self.event(
            "published.pushed",
            Some(&step.id),
            json!({ "remote": url, "branch": self.unit.branch }),
        )?;
        // The work that was held aside is in what was just pushed.
        self.drop_holding(&step.id);

        let mut said = format!(
            ":rocket: *{}* · one commit `{}` pushed to `{}`",
            step.id,
            &commit[..commit.len().min(10)],
            escape(&self.unit.branch)
        );
        if let Some(github) = self.project.github.clone() {
            let pull_request = github_client(&self.factory, &github)
                .publish_pull_request(
                    &worktree,
                    &github.repo,
                    &self.unit.branch,
                    &self.project.default_branch,
                    &title,
                    &body,
                )
                .map_err(|err| DriverError::Preflight(err.to_string()))?;
            self.event("published.pull_request", Some(&step.id), json!({
                "number": pull_request.number, "url": pull_request.url, "created": pull_request.created,
            }))?;
            said.push_str(&format!(
                "\n{} pull request {}",
                if pull_request.created {
                    "Opened"
                } else {
                    "Updated"
                },
                link(
                    &pull_request.url,
                    &format!("#{} {title}", pull_request.number)
                )
            ));
            self.unit.pull_request_number = Some(pull_request.number);
            self.unit.pull_request = Some(pull_request.url);
        }
        self.thread.say(&said);
        self.save()?;
        Ok(StepEnd::Outcome("done".to_string()))
    }
}

/// A commit message without its subject, as its body and its final block
/// of trailers (`Key: value` lines). Either may be empty.
fn split_message(message: &str) -> (String, String) {
    let mut paragraphs: Vec<&str> = message
        .trim()
        .split("\n\n")
        .map(str::trim)
        .filter(|paragraph| !paragraph.is_empty())
        .collect();
    if !paragraphs.is_empty() {
        paragraphs.remove(0);
    }
    let is_trailer = |line: &str| {
        line.split_once(": ").is_some_and(|(key, _)| {
            !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
    };
    let trailers = match paragraphs.last() {
        Some(last) if last.lines().all(is_trailer) => paragraphs.pop().unwrap_or_default(),
        _ => "",
    };
    (paragraphs.join("\n\n"), trailers.to_string())
}

/// Wrap prose at `width` columns, keeping its paragraphs.
fn wrap(text: &str, width: usize) -> String {
    text.split("\n\n")
        .map(|paragraph| {
            let mut lines: Vec<String> = Vec::new();
            let mut line = String::new();
            for word in paragraph.split_whitespace() {
                if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
                    lines.push(std::mem::take(&mut line));
                }
                if !line.is_empty() {
                    line.push(' ');
                }
                line.push_str(word);
            }
            lines.push(line);
            lines.join("\n")
        })
        .filter(|paragraph| !paragraph.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The account a run should use, or how long to wait for one.
pub enum AccountPick {
    Use {
        id: String,
        /// `None` for the CLI's own login.
        token: Option<String>,
    },
    Wait {
        until: u64,
    },
}

/// A picture of an account older than this is refreshed from the usage
/// endpoint before a choice is made, if the account has a login to ask with.
const STALE_SECS: u64 = 30 * 60;

/// What the factory knows about its accounts, after asking the usage
/// endpoint about each one it has not heard from for `max_age_secs` and has
/// a login to ask with. No model call is made.
///
/// A limit a run hit stands until it ends, whatever the endpoint says,
/// unless `forget_limits`: that is for when the owner says the picture is
/// wrong, such as after a usage reset, and the endpoint is then believed.
pub fn refresh_accounts(
    factory: &Factory,
    max_age_secs: u64,
    forget_limits: bool,
) -> BTreeMap<String, factory_claude::accounts::AccountState> {
    let tracker = Tracker::new(&factory.state_dir);
    let now = now_unix();
    let states = tracker.load();
    for account in &factory.accounts {
        let Some(service) = &account.usage_keychain else {
            continue;
        };
        let known = states.get(&account.id).cloned().unwrap_or_default();
        let age = now.saturating_sub(known.updated_at.max(known.usage_checked_at));
        if max_age_secs > 0 && age <= max_age_secs {
            continue;
        }
        let refresh = account.usage_refresh.then_some(Refresh {
            token_url: TOKEN_URL,
            safe_dir: &factory.state_dir,
            lock_dir: account.usage_lock_dir.as_deref(),
            lock_tries: factory_claude::accounts::LOCK_TRIES,
        });
        let whose = Known {
            identity: known.identity.as_deref(),
            login: known.identity_login,
        };
        let vault = Vault::keychain(service);
        let read = read_usage(&account.id, &vault, USAGE_API, refresh, whose, now);
        if let Err(err) = &read {
            // Never fatal: runs report the same windows. But a login that
            // could not be refreshed is something the owner must hear about.
            factory_core::note!("accounts: {err}");
        }
        let _ = tracker.update(&account.id, |state| {
            match read {
                Ok(mut fresh) => {
                    // Limits a run hit and that have not ended still stand.
                    for (scope, until) in &state.blocked {
                        if *until > now && !forget_limits {
                            fresh.blocked.entry(scope.clone()).or_insert(*until);
                        }
                    }
                    // The login is this account's. If its week is not the
                    // one the runs reported, the runs are another account's.
                    fresh.concern = state
                        .week_moved(fresh.seven_day.as_ref(), now)
                        .or(state.concern.take());
                    *state = fresh;
                }
                // The stored login is someone else's: say so until the
                // owner has put it right or accepted it.
                Err(AccountError::Identity { reason, .. }) => state.concern = Some(reason),
                // No windows this time, but the login was confirmed.
                Err(AccountError::Unanswered {
                    identity: Some(identity),
                    identity_login,
                    ..
                }) => {
                    state.identity = Some(identity);
                    state.identity_login = identity_login;
                }
                Err(_) => {}
            }
            state.usage_checked_at = now;
        });
    }
    tracker.load()
}

/// The token a run under an account is given: `None` for the CLI's own
/// login. It is read where the run happens and goes nowhere else.
pub fn account_token(factory: &Factory, id: &str) -> Result<Option<String>, DriverError> {
    let Some(account) = factory.accounts.iter().find(|account| account.id == id) else {
        return Err(DriverError::Preflight(format!(
            "the factory has no account `{id}`"
        )));
    };
    if let Some(path) = &account.token_env_file {
        // Read on every run, so a token the owner replaces is used by the
        // next one. Nothing else is tried: an account that quietly became
        // the CLI's login would run under whoever is logged in there.
        let name = account.token_env.as_deref().unwrap_or_default();
        let values = factory_config::read_env_file(path)?;
        return match values
            .get(name)
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
        {
            Some(token) => Ok(Some(token.to_string())),
            None => Err(AccountError::Token {
                account: id.to_string(),
                reason: format!("{} has no value for {name}", path.display()),
            }
            .into()),
        };
    }
    Ok(session_token(
        id,
        TokenSource {
            file: account.token_file.as_deref(),
            env: account.token_env.as_deref(),
            keychain: account.token_keychain.as_deref(),
        },
    )?)
}

/// Choose the account for a run on `model`. The accounts' windows are
/// learned from every run; one the factory has not heard from lately is
/// asked about once, without a model call.
pub fn pick_account(factory: &Factory, model: &str) -> Result<AccountPick, DriverError> {
    let states = refresh_accounts(factory, STALE_SECS, false);
    let ids: Vec<String> = factory.accounts.iter().map(|a| a.id.clone()).collect();
    Ok(match choose(&ids, &states, model, now_unix()) {
        Choice::Wait { until } => AccountPick::Wait {
            until: until + factory.limit_grace_secs,
        },
        Choice::Use(id) => {
            let token = account_token(factory, &id)?;
            AccountPick::Use { id, token }
        }
    })
}

/// Record what a run's rate-limit event said about the account it ran under.
pub fn observe_account(
    factory: &Factory,
    account: &str,
    limit: Option<&factory_claude::RateLimit>,
    model: &str,
) {
    if let Some(limit) = limit {
        let _ = Tracker::new(&factory.state_dir)
            .update(account, |state| state.observe(limit, model, now_unix()));
    }
}

/// Bring a bead's GitHub issue into step with the bead in the repository's
/// own issue type and priority field, where the project names them. Labels
/// are set when the issue is created; these are what GitHub itself sorts
/// and filters by. Returns what was changed: nothing if the issue already
/// agreed, the project names neither, or the bead has no issue.
pub fn sync_issue_fields(
    factory: &Factory,
    project: &Project,
    bead: &Bead,
) -> Result<Vec<String>, DriverError> {
    let Some(settings) = project.github.as_ref().filter(|github| github.issues) else {
        return Ok(Vec::new());
    };
    let Some(number) = factory_github::issue_number(&bead.external_ref, &settings.repo) else {
        return Ok(Vec::new());
    };
    let (issue_type, select) = settings.issue_fields(&bead.issue_type, bead.priority);
    github_client(factory, settings)
        .set_issue_fields(
            &project.repo,
            &settings.repo,
            number,
            &factory_github::IssueFields { issue_type, select },
        )
        .map_err(|err| DriverError::Preflight(err.to_string()))
}

/// The URL of a bead's GitHub issue, creating the issue if the project keeps
/// issues in step with beads and the bead has none. `None` if the project
/// does not keep them.
pub fn ensure_issue(
    factory: &Factory,
    project: &Project,
    source: &Source,
    bead: &Bead,
) -> Result<Option<String>, DriverError> {
    let Some(settings) = project.github.as_ref().filter(|github| github.issues) else {
        return Ok(None);
    };
    if !bead.external_ref.trim().is_empty() {
        return Ok(Some(bead.external_ref.clone()));
    }
    let priority = match bead.priority {
        0 => "critical",
        1 => "high",
        2 => "medium",
        3 => "low",
        _ => "none",
    };
    let labels: Vec<String> = settings
        .issue_labels
        .iter()
        .map(|label| {
            label
                .replace("{type}", &bead.issue_type)
                .replace("{priority}", priority)
        })
        .collect();
    let mut body = bead.description.trim().to_string();
    if !bead.acceptance_criteria.trim().is_empty() {
        body.push_str(&format!(
            "\n\n## Acceptance criteria\n\n{}",
            bead.acceptance_criteria.trim()
        ));
    }
    body.push_str(&format!("\n\nTracked as bead `{}`.", bead.id));
    let (_, url) = github_client(factory, settings)
        .create_issue(&project.repo, &settings.repo, &bead.title, &body, &labels)
        .map_err(|err| DriverError::Preflight(err.to_string()))?;
    source.link(&bead.id, &url, &project.bead_actor)?;
    Ok(Some(url))
}

/// How old a bead must be before a sweep gives it an issue, in seconds.
/// The factory gives a bead it files its issue a moment after filing it,
/// and a sweep that came in between would give it a second.
pub const ISSUE_SWEEP_MIN_AGE_SECS: u64 = 600;

/// What one sweep of a project's issues did. Each entry names its bead.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueSweep {
    /// Whether the project keeps issues in step with its beads at all.
    pub kept: bool,
    /// How many open beads were looked at.
    pub open: usize,
    /// Beads given an issue, each as `<bead> <url>`.
    pub created: Vec<String>,
    /// Issues whose type or priority was set, each as `<bead>: <what>`.
    pub changed: Vec<String>,
    /// Beads with no issue left for a later sweep, each with why.
    pub left: Vec<String>,
    /// What went wrong, each as `<bead>: <error>`.
    pub failed: Vec<String>,
}

/// Bring every open bead of a project into step with GitHub: a bead with
/// no issue gets one, made as [`ensure_issue`] makes it, and each issue's
/// type and priority follow its bead's, as [`sync_issue_fields`] sets
/// them. A bead younger than `min_age_secs`, or one the factory has
/// claimed, gets no issue here: whoever filed or claimed it is making
/// one. What fails for one bead is recorded and the sweep goes on.
pub fn sweep_issues(
    factory: &Factory,
    project: &Project,
    min_age_secs: u64,
) -> Result<IssueSweep, DriverError> {
    let mut sweep = IssueSweep::default();
    if !project.github.as_ref().is_some_and(|github| github.issues) {
        return Ok(sweep);
    }
    sweep.kept = true;
    let source = bead_source(project);
    let open = source.list_open()?;
    // Without the ages no bead can be shown to be old enough, so none is
    // given an issue; the fields are still brought into step.
    let ages = source.ages().unwrap_or_else(|err| {
        sweep.failed.push(format!("(ages): {err}"));
        Default::default()
    });
    sweep.open = open.len();
    for summary in open {
        let id = summary.id;
        let mut bead = match source.read(&id) {
            Ok(bead) => bead,
            Err(err) => {
                sweep.failed.push(format!("{id}: {err}"));
                continue;
            }
        };
        if bead.external_ref.trim().is_empty() {
            if bead.status == "in_progress" && bead.assignee == project.bead_actor {
                sweep
                    .left
                    .push(format!("{id}: its pipeline gives it its issue"));
                continue;
            }
            match ages.get(&id) {
                Some(age) if *age >= min_age_secs => {}
                Some(age) => {
                    sweep.left.push(format!("{id}: filed {age}s ago"));
                    continue;
                }
                None => {
                    sweep.left.push(format!("{id}: its age is not known"));
                    continue;
                }
            }
            // As it stands now: someone may have given it an issue since.
            bead = match source.read(&id) {
                Ok(bead) => bead,
                Err(err) => {
                    sweep.failed.push(format!("{id}: {err}"));
                    continue;
                }
            };
            if bead.external_ref.trim().is_empty() {
                match ensure_issue(factory, project, &source, &bead) {
                    Ok(Some(url)) => {
                        sweep.created.push(format!("{id} {url}"));
                        bead.external_ref = url;
                    }
                    Ok(None) => continue,
                    Err(err) => {
                        sweep.failed.push(format!("{id}: {err}"));
                        continue;
                    }
                }
            }
        }
        match sync_issue_fields(factory, project, &bead) {
            Ok(set) if set.is_empty() => {}
            Ok(set) => sweep.changed.push(format!("{id}: {}", set.join(", "))),
            Err(err) => sweep.failed.push(format!("{id}: {err}")),
        }
    }
    Ok(sweep)
}

pub(crate) fn github_client(factory: &Factory, settings: &factory_config::GitHub) -> GitHub {
    let mut github = GitHub::new(settings.write.clone());
    github.gh_bin = factory.gh_bin.clone();
    github
}

/// Checks before a unit is prepared or continued: that the Claude CLI runs
/// and the repo is there. The CLI's version is not pinned: it updates
/// itself, and a run on a newer one goes ahead (its stream lines the
/// factory does not know are counted, not fatal).
fn preflight(factory: &Factory, project: &Project) -> Result<(), DriverError> {
    factory_claude::installed_version(&factory.claude_bin)?;
    if !project.repo.join(".git").exists() {
        return Err(DriverError::Preflight(format!(
            "{} is not a git checkout",
            project.repo.display()
        )));
    }
    if let Some(github) = &project.github
        && !github_client(factory, github).authenticated(&project.repo)
    {
        return Err(DriverError::Preflight("`gh` is not logged in".to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_class_leaves_the_builders_brief_when_its_check_lands_and_returns_if_it_breaks_again() {
        let state = std::env::temp_dir().join(format!("factory-checks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&state);
        let broke = |bead: &str, class: &str| {
            let dir = state.join("units").join(format!("{bead}-1"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("unit.json"),
                json!({"project": "toy", "bead": bead, "regressions": [{"category": class}]})
                    .to_string(),
            )
            .unwrap();
        };
        broke("toy-1", "error-path-cleanup");
        broke("toy-2", "error-path-cleanup");
        broke("toy-3", "double-call");
        let listed = || -> Vec<String> {
            classes_unchecked(finding_classes(&state, "toy"), &class_checks(&state, "toy"))
                .into_iter()
                .map(|count| count.class)
                .collect()
        };
        let words = |class: &str| {
            let beads = finding_classes(&state, "toy")
                .into_iter()
                .find(|count| count.class == class)
                .map_or(0, |count| count.beads);
            class_checks(&state, "toy")[class].words(beads)
        };
        // With no check, a builder is told every class.
        assert!(class_checks(&state, "toy").is_empty());
        assert_eq!(listed(), ["error-path-cleanup", "double-call"]);

        // A bead that builds the check is filed: the class is still told,
        // and a second bead for it does not take its place.
        assert!(answer_class(&state, "toy", "error-path-cleanup", "toy-20").unwrap());
        assert!(!answer_class(&state, "toy", "error-path-cleanup", "toy-21").unwrap());
        assert_eq!(listed(), ["error-path-cleanup", "double-call"]);
        assert_eq!(words("error-path-cleanup"), "check being built by `toy-20`");
        // Another bead landing changes nothing.
        assert_eq!(
            check_landed(&state, "toy", "toy-21").unwrap(),
            Vec::<String>::new()
        );

        // It lands: the check runs on every change from here, and the
        // class is no longer something a builder is asked to remember.
        assert_eq!(
            check_landed(&state, "toy", "toy-20").unwrap(),
            ["error-path-cleanup"]
        );
        assert_eq!(listed(), ["double-call"]);
        assert_eq!(words("error-path-cleanup"), "checked since `toy-20` landed");
        assert_eq!(
            check_landed(&state, "toy", "toy-20").unwrap(),
            Vec::<String>::new()
        );

        // The class breaks another bead all the same: the check leaked,
        // builders are told again, and a new bead may answer it.
        broke("toy-4", "error-path-cleanup");
        assert_eq!(listed(), ["error-path-cleanup", "double-call"]);
        assert_eq!(
            words("error-path-cleanup"),
            "check from `toy-20` landed and 1 bead(s) broke after it"
        );
        assert!(answer_class(&state, "toy", "error-path-cleanup", "toy-30").unwrap());
        assert_eq!(words("error-path-cleanup"), "check being built by `toy-30`");
        // Another project's classes are its own.
        assert!(class_checks(&state, "other").is_empty());
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn an_account_can_take_its_token_from_a_file_of_variables_and_from_nowhere_else() {
        let dir = std::env::temp_dir().join(format!("factory-env-token-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let secrets = dir.join("secrets.env");
        std::fs::write(
            dir.join("factory.toml"),
            format!(
                "state_dir = \"/tmp/s\"\nallowed_models = [\"m\"]\n[[account]]\nid = \"a\"\ntoken_env_file = \"{0}\"\ntoken_env = \"A_TOKEN\"\n[[account]]\nid = \"b\"\ntoken_env_file = \"{0}\"\ntoken_env = \"B_TOKEN\"\n",
                secrets.display()
            ),
        )
        .unwrap();
        let factory = ConfigDir(dir.clone()).factory().unwrap();
        // As a shell would source it: comments, quotes, an empty value.
        std::fs::write(&secrets, "# tokens\nA_TOKEN=\"tok-a\"\nB_TOKEN=\n").unwrap();
        assert_eq!(
            account_token(&factory, "a").unwrap().as_deref(),
            Some("tok-a")
        );
        // An empty value is not the CLI's own login: it is an error.
        let empty = account_token(&factory, "b").unwrap_err().to_string();
        assert!(
            empty.contains("account `b`") && empty.contains("no value for B_TOKEN"),
            "{empty}"
        );
        // The file is read each time, so a replaced token is the next run's.
        std::fs::write(&secrets, "A_TOKEN=tok-a2\nB_TOKEN=tok-b\n").unwrap();
        assert_eq!(
            account_token(&factory, "a").unwrap().as_deref(),
            Some("tok-a2")
        );
        assert_eq!(
            account_token(&factory, "b").unwrap().as_deref(),
            Some("tok-b")
        );
        // Without the file there is no token, and no falling back.
        std::fs::remove_file(&secrets).unwrap();
        assert!(account_token(&factory, "a").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_builder_and_a_reviewer_are_named_the_projects_standards_and_nobody_else_is() {
        let dir = std::env::temp_dir().join(format!("factory-standards-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("STYLE.md"), "Free what you allocate.").unwrap();
        let named = ["STYLE.md".to_string(), "docs/gone.md".to_string()];
        let review = standards_note(&named, &dir, Contract::Review).unwrap();
        assert_eq!(review.title, "The project's written standards");
        assert!(
            review
                .body
                .starts_with("Judge whether the change is idiomatic")
        );
        assert!(
            review.body.ends_with(
                "- `STYLE.md`\n- `docs/gone.md` (the project's record names it, and it is not in this tree)"
            ),
            "{}",
            review.body
        );
        let build = standards_note(&named, &dir, Contract::Change).unwrap();
        assert!(
            build
                .body
                .starts_with("The project's code is written to these texts")
        );
        assert_eq!(standards_note(&named, &dir, Contract::Probe), None);
        assert_eq!(standards_note(&named, &dir, Contract::Publication), None);
        assert_eq!(standards_note(&[], &dir, Contract::Review), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_commit_message_splits_into_body_and_trailers() {
        let message = "Add a thing\n\nWhy it was needed.\n\nHow it works: in two parts.\n\nBead: toy-1\nChangelog: none (internal)\n";
        assert_eq!(
            split_message(message),
            (
                "Why it was needed.\n\nHow it works: in two parts.".to_string(),
                "Bead: toy-1\nChangelog: none (internal)".to_string()
            )
        );
        // Prose that happens to contain a colon is not a trailer block.
        assert_eq!(
            split_message("Subject\n\nNote: this one\nruns over two lines."),
            (
                "Note: this one\nruns over two lines.".to_string(),
                String::new()
            )
        );
        assert_eq!(
            split_message("Subject only"),
            (String::new(), String::new())
        );
    }

    #[test]
    fn prose_is_wrapped_and_keeps_its_paragraphs() {
        let text = "one two three four five six\n\nseven  eight\nnine";
        assert_eq!(
            wrap(text, 14),
            "one two three\nfour five six\n\nseven eight\nnine"
        );
        assert!(
            wrap(&"word ".repeat(40), 72)
                .lines()
                .all(|line| line.len() <= 72)
        );
    }

    #[test]
    fn durations_and_token_counts_read_naturally() {
        assert_eq!(duration_text(42), "42s");
        assert_eq!(duration_text(125), "2m 5s");
        assert_eq!(duration_text(3725), "1h 2m");
        assert_eq!(tokens_text(950), "950");
        assert_eq!(tokens_text(12_340), "12.3k");
        assert_eq!(tokens_text(2_500_000), "2.5M");
    }
}
