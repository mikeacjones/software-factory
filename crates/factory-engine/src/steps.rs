//! The steps of a flow, as activities.
//!
//! An agent step is the one that matters: a run of headless Claude can take
//! an hour, and the worker it runs under can die in the middle of it. The
//! activity heartbeats the run's session ID as soon as the run reports it,
//! and when Temporal gives the activity to another worker, that one reads
//! the ID back from the heartbeat and resumes the session. The work the run
//! had done is not done again.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use temporalio_macros::activities;
use temporalio_sdk::activities::{ActivityContext, ActivityError};

/// One step of one unit's flow, to be run.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepCall {
    pub unit: String,
    pub step: String,
    /// Which time the unit is in this step, counting from 1.
    pub round: u32,
    /// What whoever continued a parked pipeline wants this step to know.
    pub note: Option<String>,
    /// For an action step, which action.
    pub action: Option<String>,
    /// The session an earlier run of this step left when it stopped at a
    /// usage limit, to be resumed.
    pub session: Option<String>,
    /// How often an agent step is to say it is alive, in seconds.
    pub beat_secs: u64,
}

/// How a step ended.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepDone {
    /// The outcome the flow routes, such as `done`, `pass` or `broken`.
    pub outcome: String,
    /// Set instead of an outcome when the step could not run because every
    /// account is at a usage limit: how long until the first one is back.
    /// The engine waits that long, on a timer that needs nothing running,
    /// and runs the step again.
    pub limited_secs: Option<u64>,
    /// The session an agent run had, so that the run after a usage limit
    /// resumes it.
    pub session: Option<String>,
}

/// What does the work of a step. The engine and the activities know only
/// this much about it, which is what lets the same engine run a real
/// pipeline, or in a test a script.
pub trait StepRunner: Send + Sync + 'static {
    /// Run an agent step. `session` is the session of a run of this step
    /// that was cut off, to be resumed, or `None` to start one. `started`
    /// must be called with the run's session ID as soon as it is known.
    fn agent(
        &self,
        call: &StepCall,
        session: Option<String>,
        started: &dyn Fn(&str),
    ) -> Result<StepDone, String>;

    /// Run a command step or an action step.
    fn plain(&self, call: &StepCall) -> Result<StepDone, String>;
}

/// The activities of a worker, over whatever does the work.
pub struct Steps {
    runner: Arc<dyn StepRunner>,
}

impl Steps {
    pub fn new(runner: Arc<dyn StepRunner>) -> Steps {
        Steps { runner }
    }
}

fn failed(reason: String) -> ActivityError {
    ActivityError::from(std::io::Error::other(reason))
}

#[activities]
impl Steps {
    /// An agent step. Retried by Temporal if the worker dies, and then it
    /// resumes the session the cut-off run had.
    #[activity]
    pub async fn agent(
        self: Arc<Self>,
        ctx: ActivityContext,
        call: StepCall,
    ) -> Result<StepDone, ActivityError> {
        // A run cut off with its worker left its session in the last
        // heartbeat. One that stopped at a usage limit handed it back.
        let cut_off: Option<String> = ctx
            .heartbeat_details()
            .deserialize::<String>()?
            .filter(|id| !id.is_empty())
            .or_else(|| call.session.clone());
        let (said, mut heard) = tokio::sync::mpsc::unbounded_channel::<String>();
        let runner = self.runner.clone();
        let (work_call, resume) = (call.clone(), cut_off.clone());
        let mut work = tokio::task::spawn_blocking(move || {
            let started = |id: &str| {
                let _ = said.send(id.to_string());
            };
            runner.agent(&work_call, resume, &started)
        });
        let mut session = cut_off.unwrap_or_default();
        // The first heartbeat an activity sends goes to the server at once,
        // and the ones after it are held back for a while. So the first one
        // must be the one that carries the session: nothing is sent before
        // the run has said which session it is, or before the first beat is due.
        let every = Duration::from_secs(call.beat_secs.max(1));
        let mut beat = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
        loop {
            tokio::select! {
                done = &mut work => {
                    // A session reported just as the run ended.
                    while let Ok(id) = heard.try_recv() {
                        session = id;
                    }
                    return match done {
                        Ok(Ok(mut done)) => {
                            done.session = Some(session).filter(|id| !id.is_empty());
                            Ok(done)
                        }
                        Ok(Err(reason)) => Err(failed(reason)),
                        Err(join) => Err(failed(format!("the step's run was lost: {join}"))),
                    };
                }
                Some(id) = heard.recv() => {
                    session = id;
                    ctx.record_heartbeat(session.clone()).await?;
                }
                _ = beat.tick() => {
                    ctx.record_heartbeat(session.clone()).await?;
                }
            }
        }
    }

    /// A command step or an action step.
    #[activity]
    pub async fn plain(
        self: Arc<Self>,
        _ctx: ActivityContext,
        call: StepCall,
    ) -> Result<StepDone, ActivityError> {
        let runner = self.runner.clone();
        match tokio::task::spawn_blocking(move || runner.plain(&call)).await {
            Ok(Ok(done)) => Ok(done),
            Ok(Err(reason)) => Err(failed(reason)),
            Err(join) => Err(failed(format!("the step's run was lost: {join}"))),
        }
    }
}

/// A runner that does each step by running a command, which is how a
/// worker will hand a step to the factory, and how a test scripts one.
///
/// The command is told what to do in its environment (`FACTORY_KIND`,
/// `FACTORY_UNIT`, `FACTORY_STEP`, `FACTORY_ROUND`, `FACTORY_NOTE`,
/// `FACTORY_ACTION`, and `FACTORY_SESSION` when a cut-off run is to be
/// resumed), and answers on its standard output, a line at a time:
/// `session <id>` as soon as an agent run knows its session, then
/// `outcome <word>`, or `limited <seconds>` if every account is at a limit.
pub struct ExecRunner {
    pub argv: Vec<String>,
}

impl ExecRunner {
    fn run(
        &self,
        kind: &str,
        call: &StepCall,
        session: Option<String>,
        started: &dyn Fn(&str),
    ) -> Result<StepDone, String> {
        use std::io::BufRead;
        let (program, args) = self.argv.split_first().ok_or("the step command is empty")?;
        let mut command = std::process::Command::new(program);
        command
            .args(args)
            .env("FACTORY_KIND", kind)
            .env("FACTORY_UNIT", &call.unit)
            .env("FACTORY_STEP", &call.step)
            .env("FACTORY_ROUND", call.round.to_string())
            .env("FACTORY_NOTE", call.note.as_deref().unwrap_or(""))
            .env("FACTORY_ACTION", call.action.as_deref().unwrap_or(""))
            .env("FACTORY_SESSION", session.as_deref().unwrap_or(""))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|err| format!("`{program}` could not be started: {err}"))?;
        let mut done = StepDone::default();
        if let Some(stdout) = child.stdout.take() {
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                match line.split_once(' ') {
                    Some(("session", id)) => started(id.trim()),
                    Some(("outcome", word)) => done.outcome = word.trim().to_string(),
                    Some(("limited", secs)) => done.limited_secs = secs.trim().parse().ok(),
                    _ => {}
                }
            }
        }
        let status = child
            .wait()
            .map_err(|err| format!("`{program}` could not be waited for: {err}"))?;
        if done.outcome.is_empty() && done.limited_secs.is_none() {
            return Err(format!(
                "step `{}` ended ({status}) without saying how",
                call.step
            ));
        }
        Ok(done)
    }
}

impl StepRunner for ExecRunner {
    fn agent(
        &self,
        call: &StepCall,
        session: Option<String>,
        started: &dyn Fn(&str),
    ) -> Result<StepDone, String> {
        self.run("agent", call, session, started)
    }

    fn plain(&self, call: &StepCall) -> Result<StepDone, String> {
        let kind = if call.action.is_some() {
            "action"
        } else {
            "command"
        };
        self.run(kind, call, None, &|_| {})
    }
}
