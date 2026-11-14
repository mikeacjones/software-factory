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

pub mod thread;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use factory_beads::{Bead, Source, readiness};
use factory_brief::{Brief, Contract, Note, contract_of, findings_text, render, result_problems};
use factory_checks::{Verdict, run_command, scope_violations};
use factory_claude::{Outcome, RunSpec, Session, Usage};
use factory_config::{BeadSource, ConfigDir, Factory, Project, read_env_file};
use factory_core::{EventLog, ProjectId, StepId, UnitId, now_unix};
use factory_flow::{
    ExitPolicy, Flow, Next, Position, Registry, SessionRule, Step, StepKind, Workspace,
};
use factory_github::{GitHub, publication_problems};
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
    #[error("the installed Claude CLI is `{installed}`, but the factory is pinned to `{pinned}`")]
    CliVersion { installed: String, pinned: String },
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
    /// The latest typed result of each agent step.
    pub results: BTreeMap<String, Value>,
    pub usage: BTreeMap<String, StepUsage>,
    pub thread: Option<ThreadState>,
    /// Owner messages not yet handed to an agent step.
    pub owner_messages: Vec<String>,
    /// The owner's answer to the current step's question.
    pub answer: Option<String>,
    pub pull_request: Option<String>,
    pub runs: u32,
    pub started_at: u64,
}

/// How a step ended, as far as the flow is concerned.
enum StepEnd {
    Outcome(String),
    Park(String),
}

pub struct Options {
    /// Post to Slack. Off, the thread is written to the event log only.
    pub slack: bool,
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
    // A role the flow names must have a prompt before anything starts.
    for step in &flow.steps {
        if let StepKind::Agent { role, .. } = &step.kind {
            config.role_prompt(role)?;
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
        let project = config.project(project_id)?;
        let (flow, flow_text) = load_flow(&config, &factory, &project)?;
        preflight(&factory, &project)?;

        let source = bead_source(&project);
        let bead = source.read(bead_id)?;
        let reasons = readiness(&bead, &project.bead_actor);
        if !reasons.is_empty() {
            return Err(DriverError::NotLaunchable {
                bead: bead.id,
                reasons,
            });
        }

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

        let branch = factory_git::branch_name(&bead.id, &bead.title);
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
                results: BTreeMap::new(),
                usage: BTreeMap::new(),
                thread: None,
                owner_messages: Vec::new(),
                answer: None,
                pull_request: None,
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
        };
        driver.event(
            "unit.created",
            None,
            json!({
                "bead": driver.unit.bead, "flow": driver.unit.flow, "branch": driver.unit.branch,
            }),
        )?;
        driver.thread = driver.open_thread(options, None);
        driver.unit.thread = driver.thread.state().cloned();
        driver.save()?;

        // From here the unit exists; a failure parks it instead of losing it.
        if let Err(err) = driver.prepare(&source) {
            let reason = format!("could not prepare the unit: {err}");
            driver.park(&reason)?;
        }
        Ok(driver)
    }

    /// Pick up a unit that was stopped or parked.
    pub fn resume(
        config: ConfigDir,
        unit_id: &str,
        options: &Options,
    ) -> Result<Driver, DriverError> {
        let factory = config.factory()?;
        let dir = factory.state_dir.join("units").join(unit_id);
        let state = dir.join("unit.json");
        let text = std::fs::read_to_string(&state).map_err(io(&state))?;
        let mut unit: Unit = serde_json::from_str(&text).map_err(|source| DriverError::Json {
            path: state.clone(),
            source,
        })?;
        if unit.status == Status::Ended {
            return Err(DriverError::AlreadyEnded(unit_id.to_string()));
        }
        let project = config.project(unit.project.as_str())?;
        // A unit runs on the flow it started with, not on what the file says now.
        let pinned = dir.join("flow.toml");
        let flow = Flow::parse(&std::fs::read_to_string(&pinned).map_err(io(&pinned))?)?;
        preflight(&factory, &project)?;
        let bead = bead_source(&project).read(&unit.bead)?;
        let log = EventLog::new(
            dir.join("events.jsonl"),
            project.id.clone(),
            unit.id.clone(),
        );
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
        };
        driver.thread = driver.open_thread(options, saved_thread);
        driver.event(
            "unit.resumed",
            None,
            json!({ "step": driver.unit.position.step }),
        )?;
        driver.thread.say(&format!(
            ":arrow_forward: Continuing from step *{}*.",
            driver.unit.position.step
        ));
        driver.save()?;
        Ok(driver)
    }

    pub fn unit(&self) -> &Unit {
        &self.unit
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
        let slack = Slack::new(token.clone());
        match saved {
            Some(state) => Thread::resume(slack, state, owners, self.header(), self.log.clone()),
            None => Thread::open(slack, channel, owners, self.header(), self.log.clone()),
        }
    }

    fn event(&self, kind: &str, step: Option<&str>, data: Value) -> Result<(), DriverError> {
        let step = step.map(StepId::new);
        Ok(self.log.append(kind, step.as_ref(), data)?)
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

        let repo = self.project.repo.clone();
        factory_git::fetch(&repo, &self.project.remote)?;
        let start = format!("{}/{}", self.project.remote, self.project.default_branch);
        let base = factory_git::rev_parse(&repo, &start)?;
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
            let step_id = self.unit.position.step.clone();
            let step = self
                .flow
                .step(&step_id)
                .ok_or_else(|| factory_flow::FlowError::UnknownStep(step_id.clone()))?
                .clone();
            let round = self.unit.position.round();
            self.event("step.started", Some(&step_id), json!({ "round": round }))?;
            let started = Instant::now();
            let end = match &step.kind {
                StepKind::Agent { .. } => self.agent_step(&step)?,
                StepKind::Command { .. } => self.command_step(&step)?,
                StepKind::Action { .. } => self.publish_step(&step)?,
            };
            let used = self.unit.usage.entry(step_id.clone()).or_default();
            used.wall_secs += started.elapsed().as_secs();
            if !matches!(step.kind, StepKind::Agent { .. }) {
                used.runs += 1;
            }
            let outcome = match end {
                StepEnd::Park(reason) => return self.park(&reason),
                StepEnd::Outcome(outcome) => outcome,
            };
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
                Next::End => {
                    self.unit.status = Status::Ended;
                    self.event(
                        "unit.ended",
                        None,
                        json!({ "pull_request": self.unit.pull_request }),
                    )?;
                    let report = usage_report(&self.unit, &self.flow);
                    let done = match &self.unit.pull_request {
                        Some(url) => format!(":tada: *Done.* Pull request: {}", link(url, url)),
                        None => format!(
                            ":tada: *Done.* Branch `{}` is pushed.",
                            escape(&self.unit.branch)
                        ),
                    };
                    self.thread.say(&format!(
                        "{done}\nThe bead stays claimed by `{}` until the pull request merges.\n{}",
                        escape(&self.project.bead_actor),
                        code_block(&report)
                    ));
                    self.thread.status(&match &self.unit.pull_request {
                        Some(url) => {
                            format!(":white_check_mark: pull request opened: {}", link(url, url))
                        }
                        None => ":white_check_mark: done".to_string(),
                    });
                    self.save()?;
                    return Ok(Status::Ended);
                }
            }
        }
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
        let poll = Duration::from_secs(settings.as_ref().map_or(10, |s| s.poll_secs).max(2));
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

    /// Wait out a usage limit. Returns why the unit should park, if it should.
    fn wait_for_limit(
        &mut self,
        step: &str,
        resets_at: Option<u64>,
    ) -> Result<Option<String>, DriverError> {
        let Some(resets_at) = resets_at else {
            return Ok(Some(
                "the account is at a usage limit and the CLI gave no reset time".to_string(),
            ));
        };
        let wait = resets_at.saturating_sub(now_unix()) + 60;
        self.event(
            "account.limited",
            Some(step),
            json!({ "resets_at": resets_at, "wait_secs": wait }),
        )?;
        if wait > 6 * 3600 {
            return Ok(Some(format!(
                "the account is at a usage limit that resets in {}",
                duration_text(wait)
            )));
        }
        self.thread.say(&format!(
            ":hourglass: *Usage limit.* Step *{step}* is paused and its session kept. Resuming in {}.",
            duration_text(wait)
        ));
        self.thread.status(&format!(
            ":hourglass: usage limit, resuming *{step}* in {}",
            duration_text(wait)
        ));
        let until = Instant::now() + Duration::from_secs(wait);
        while Instant::now() < until {
            if self.hear_owner() {
                return Ok(Some("the owner said stop".to_string()));
            }
            std::thread::sleep(Duration::from_secs(15));
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

        let own_session = self
            .unit
            .in_step
            .as_ref()
            .filter(|current| current.step == step.id)
            .map(|current| current.session.clone());
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
        let notes: Vec<Note> = if continuing {
            Vec::new()
        } else {
            self.flow
                .steps
                .iter()
                .filter(|other| other.id != step.id)
                .filter_map(|other| self.unit.notes.get(&other.id).cloned())
                .collect()
        };
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
            .max(2);

        let mut rejected: Vec<String> = Vec::new();
        let mut answer = self.unit.answer.take();
        let mut interrupted = false;
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
            };
            self.event("run.started", Some(&step.id), json!({
                "run": self.unit.runs, "role": role, "model": model, "effort": effort,
                "session": match &session { Session::Fresh => json!("fresh"), Session::Resume(id) => json!(id) },
                "cwd": cwd, "log": log_path,
            }))?;
            self.thread.status(&format!(
                ":arrows_counterclockwise: *{}* · round {round} · {role} on `{model}`",
                step.id
            ));

            let started = Instant::now();
            let mut heard: Vec<String> = Vec::new();
            let finished = {
                let thread = &mut self.thread;
                let step_id = step.id.clone();
                let mut last_poll = Instant::now();
                let mut last_status = Instant::now();
                factory_claude::run(&spec, &log_path, &mut |transcript| {
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
            // Said while the step ran: for the next agent step.
            self.unit.owner_messages.extend(heard);
            let usage = finished.transcript.usage();
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
                Outcome::Limited { resets_at, .. } => {
                    limit_waits += 1;
                    if limit_waits > 4 {
                        return Ok(StepEnd::Park(
                            "the account hit its usage limit five times in one step".to_string(),
                        ));
                    }
                    if let Some(reason) = self.wait_for_limit(&step.id, resets_at)? {
                        return Ok(StepEnd::Park(reason));
                    }
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
                let allowed: Vec<String> = writes
                    .iter()
                    .flat_map(|scope| self.project.scopes.get(scope).cloned().unwrap_or_default())
                    .collect();
                let violations =
                    scope_violations(&changed, &allowed, &self.project.forbidden_paths)?;
                self.unit.checkpoint = head.clone();
                self.event("checkpoint", Some(&step.id), json!({
                    "commit": head, "committed": committed, "files": changed,
                    "violations": violations.iter().map(|v| json!({"path": v.path, "reason": v.reason})).collect::<Vec<_>>(),
                }))?;
                self.save()?;
                if !violations.is_empty() {
                    let list: Vec<String> = violations
                        .iter()
                        .map(|violation| format!("{} ({})", violation.path, violation.reason))
                        .collect();
                    return Ok(StepEnd::Park(format!(
                        "step `{}` changed files it may not change:\n{}",
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
                if contract == Contract::Probe {
                    let summary = result.get("summary").and_then(Value::as_str).unwrap_or("");
                    let mut body = format!("Outcome: `{outcome}`.\n\n{summary}\n");
                    let findings = findings_text(&result);
                    if !findings.is_empty() {
                        body.push_str(&format!(
                            "\nFindings, each reproduced by the tester:\n\n{findings}"
                        ));
                    }
                    if let Some(path) = &patch_path {
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
                self.unit.feedback = Some(text);
            }
            (Contract::Change, "done") => self.unit.feedback = None,
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
        let command = self.project.commands[name].clone();
        let shown = command.argv.join(" ");
        self.thread.status(&format!(
            ":arrows_counterclockwise: *{}* · running `{}`",
            step.id,
            escape(&shown)
        ));
        self.unit.runs += 1;
        let result = run_command(
            &command.argv,
            &self.unit.worktree,
            Duration::from_secs(command.timeout_secs),
        );
        let output_path = self.artifact(&format!("{}-output.txt", step.id), &result.output)?;
        let secs = result.duration.as_secs();
        self.event(
            "command.ran",
            Some(&step.id),
            json!({
                "command": name, "argv": command.argv, "verdict": format!("{:?}", result.verdict),
                "secs": secs, "output": output_path, "commit": self.unit.checkpoint,
            }),
        )?;

        let (passed, how) = match (&result.verdict, policy) {
            (Verdict::CouldNotRun(reason), _) => {
                // The host's fault, not the code's: no fix round can help.
                return Ok(StepEnd::Park(format!(
                    "the command `{name}` could not run: {reason}"
                )));
            }
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
        let short = &self.unit.checkpoint[..self.unit.checkpoint.len().min(10)];
        self.unit.notes.insert(
            step.id.clone(),
            Note {
                title: format!("Check `{name}`"),
                body: format!(
                    "`{shown}` {how} on commit `{short}` (took {}).",
                    duration_text(secs)
                ),
            },
        );
        if passed {
            self.thread.say(&format!(
                ":white_check_mark: *{}* · `{}` {how} · {}",
                step.id,
                escape(&shown),
                duration_text(secs)
            ));
            return Ok(StepEnd::Outcome("pass".to_string()));
        }
        self.unit.feedback = Some(format!(
            "The command `{name}` (`{shown}`) {how}, run by the factory in your worktree on commit `{short}`.\n\nThe end of its output:\n\n```\n{}\n```\n\nThe full output is in `{}`.",
            tail(&result.output, 150),
            output_path.display()
        ));
        self.thread.say(&format!(
            ":x: *{}* · `{}` {how} · {}\n{}",
            step.id,
            escape(&shown),
            duration_text(secs),
            code_block(&truncate(&tail(&result.output, 25), 2400))
        ));
        Ok(StepEnd::Outcome("fail".to_string()))
    }

    /// The `publish` action: one final commit, a push, and the pull request.
    fn publish_step(&mut self, step: &Step) -> Result<StepEnd, DriverError> {
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
        self.thread.status(&format!(
            ":arrows_counterclockwise: *{}* · commit, push, pull request",
            step.id
        ));

        if factory_git::commit_count(&worktree, &self.unit.base, "HEAD")? == 0 {
            return Ok(StepEnd::Park(
                "there is nothing to publish: the branch has no changes".to_string(),
            ));
        }
        // The checkpoints become one commit. The content must not change.
        let tree = factory_git::tree_of(&worktree, "HEAD")?;
        let commit = factory_git::squash(&worktree, &self.unit.base, &message)?;
        if factory_git::tree_of(&worktree, &commit)? != tree {
            return Ok(StepEnd::Park(
                "the final commit does not hold what the checks passed on".to_string(),
            ));
        }
        self.unit.checkpoint = commit.clone();
        self.event(
            "published.commit",
            Some(&step.id),
            json!({ "commit": commit }),
        )?;

        let url = factory_git::remote_url(&self.project.repo, &self.project.remote)?;
        if let Some(github) = &self.project.github {
            let slug = factory_git::github_slug(&url).unwrap_or_default();
            GitHub::new(github.write.clone())
                .check_allowed(&slug)
                .map_err(|err| DriverError::Preflight(format!("push to `{url}`: {err}")))?;
        }
        factory_git::push(&worktree, &self.project.remote, &self.unit.branch)?;
        self.event(
            "published.pushed",
            Some(&step.id),
            json!({ "remote": url, "branch": self.unit.branch }),
        )?;

        let mut said = format!(
            ":rocket: *{}* · one commit `{}` pushed to `{}`",
            step.id,
            &commit[..commit.len().min(10)],
            escape(&self.unit.branch)
        );
        if let Some(github) = self.project.github.clone() {
            let pull_request = GitHub::new(github.write.clone())
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
            self.unit.pull_request = Some(pull_request.url);
        }
        self.thread.say(&said);
        self.save()?;
        Ok(StepEnd::Outcome("done".to_string()))
    }
}

/// Checks before a unit is prepared or continued: the pinned CLI and the repo.
fn preflight(factory: &Factory, project: &Project) -> Result<(), DriverError> {
    let installed = factory_claude::installed_version(&factory.claude_bin)?;
    if installed != factory.cli_version {
        return Err(DriverError::CliVersion {
            installed,
            pinned: factory.cli_version.clone(),
        });
    }
    if !project.repo.join(".git").exists() {
        return Err(DriverError::Preflight(format!(
            "{} is not a git checkout",
            project.repo.display()
        )));
    }
    if let Some(github) = &project.github
        && !GitHub::new(github.write.clone()).authenticated(&project.repo)
    {
        return Err(DriverError::Preflight("`gh` is not logged in".to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
