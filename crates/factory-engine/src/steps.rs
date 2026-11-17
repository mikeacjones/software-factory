//! The steps of a flow, as activities.
//!
//! An agent step is the one that matters: a run of headless Claude can take
//! an hour, and the worker it runs under can die in the middle of it. The
//! activity heartbeats the run's session ID as soon as the run reports it,
//! and when Temporal gives the activity to another worker, that one reads
//! the ID back from the heartbeat and resumes the session. The work the run
//! had done is not done again.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use factory_driver::goal::{Event, RunningPipeline, Turned};
use factory_driver::{IssueSweep, Within};
use factory_flow::Position;
use factory_slack::socket::Incoming;
use serde::{Deserialize, Serialize};
use temporalio_client::errors::WorkflowInteractionError;
use temporalio_client::{
    WorkflowDescribeOptions, WorkflowExecuteUpdateOptions, WorkflowExecutionStatus,
    WorkflowGetResultOptions, WorkflowQueryOptions, WorkflowSignalOptions,
};
use temporalio_macros::activities;
use temporalio_sdk::ApplicationFailure;
use temporalio_sdk::activities::{ActivityContext, ActivityError};

use crate::accounts::{AccountCall, Accounts, Ask, CheckCall, Lease, Picture};
use crate::engine::Pipeline;
use crate::tiers::{self, Reach};

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
    /// The account an agent step is to run under, when the accounts
    /// workflow hands them out. `None` leaves the choice to the runner.
    #[serde(default)]
    pub account: Option<String>,
    /// How often each bounded route of the flow has been taken so far,
    /// keyed `step:outcome`, for the unit's own record.
    #[serde(default)]
    pub used: BTreeMap<String, u32>,
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
    /// Set instead of an outcome when the step parked the unit itself: for
    /// something the flow has no route for, such as the owner saying stop.
    pub parked: Option<String>,
}

/// A unit to make ready before its first step: a bead of a project.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrepareCall {
    pub project: String,
    pub bead: String,
    /// The flow to run instead of the project's own, by name.
    pub flow_name: Option<String>,
    /// The sub-goal the unit belongs to, if it is one bead of a sub-goal
    /// or the sub-goal itself being shipped.
    #[serde(default)]
    pub within: Option<Within>,
}

/// A unit that exists already, to be taken up where its record says it
/// stands: one that parked or failed and is continued, or one something
/// else was walking.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdoptCall {
    pub unit: String,
    /// The step to go on from. `None` goes on where it stands.
    pub from: Option<String>,
    /// What whoever continues it wants that step to know.
    pub note: Option<String>,
    /// A decision about the bead that every later step is shown.
    pub ruling: Option<String>,
    /// The flow to run instead of the project's own, by name.
    pub flow_name: Option<String>,
}

/// A unit taken up: its flow, and where it stands in it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Adopted {
    pub flow: String,
    pub position: Position,
    pub project: String,
    pub bead: String,
    pub title: String,
}

/// A unit made ready: what it is called, and the flow it is pinned to.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Prepared {
    pub unit: String,
    /// The text of the unit's flow definition, as it was when the unit was
    /// made. This is what the engine runs.
    pub flow: String,
    /// The bead's title, for what the pipeline says about itself.
    #[serde(default)]
    pub title: String,
}

/// A unit the engine has parked, and why.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParkCall {
    pub unit: String,
    pub reason: String,
}

/// One thing that happened to a project, for its tiers to deal with.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TurnCall {
    pub project: String,
    pub event: Event,
    /// How often the turn says it is alive, in seconds.
    pub beat_secs: u64,
}

/// A sweep of a project's open beads against its GitHub issues.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssuesCall {
    pub project: String,
    /// How often the sweep says it is alive, in seconds.
    pub beat_secs: u64,
}

/// What the listener and the watcher are started with.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PumpCall {
    /// How long to go on for, in seconds, before handing over to a run
    /// that starts afresh.
    pub secs: u64,
    /// How often to say it is alive, in seconds.
    pub beat_secs: u64,
    /// How a project's workflow is reached, and started if it is not there.
    pub reach: Reach,
}

/// How long a pipeline a goal's record says is running may be unknown to
/// the server before its goal is told it never started, in seconds. The
/// record is written in the turn that decides to start it, and the
/// pipeline is started when that turn is over. A turn in which an agent
/// runs can take far longer than this, so what the watcher says here can
/// be out of date by the time the project hears it: see
/// `said_never_started`.
const NEVER_STARTED_SECS: u64 = 120;

/// What the watcher says of a pipeline the server does not know.
const NEVER_STARTED: &str = "The engine has no pipeline for it: it was never started.";

/// The pipeline an event says was never started, when that is what the
/// event says.
fn said_never_started(event: &Event) -> Option<&str> {
    match event {
        Event::Ended {
            word, rest, name, ..
        } if word == "failed" && rest == NEVER_STARTED => Some(name),
        _ => None,
    }
}

/// How long the watcher takes a project to have heard what it told it,
/// before it tells it again, in seconds.
const TOLD_AGAIN_SECS: u64 = 600;

/// Why a unit was not made ready.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NotPrepared {
    /// The bead may not be worked on as it stands, as when it has no
    /// acceptance criteria or someone else has it. Trying again would
    /// change nothing: the pipeline ends, and says why.
    Refused(String),
    /// Something went wrong that may not go wrong the next time.
    Failed(String),
}

/// What does the work of a step. The engine and the activities know only
/// this much about it, which is what lets the same engine run a real
/// pipeline, or in a test a script.
pub trait StepRunner: Send + Sync + 'static {
    /// Make a unit ready for its first step. A runner with nothing to make
    /// ready has no use for this.
    fn prepare(&self, call: &PrepareCall) -> Result<Prepared, NotPrepared> {
        Err(NotPrepared::Refused(format!(
            "this runner cannot prepare a unit for `{}`",
            call.bead
        )))
    }

    /// Run an agent step. `session` is the session of a run of this step
    /// that was cut off, to be resumed, or `None` to start one. `started`
    /// must be called with the run's session ID as soon as it is known.
    fn agent(
        &self,
        call: &StepCall,
        session: Option<String>,
        started: Started,
    ) -> Result<StepDone, String>;

    /// Run a command step or an action step.
    fn plain(&self, call: &StepCall) -> Result<StepDone, String>;

    /// Take up a unit that exists already.
    fn adopt(&self, call: &AdoptCall) -> Result<Adopted, NotPrepared> {
        Err(NotPrepared::Refused(format!(
            "this runner cannot take up the unit `{}`",
            call.unit
        )))
    }

    /// The engine has parked the unit: record it where people will look.
    fn park(&self, _call: &ParkCall) -> Result<(), String> {
        Ok(())
    }

    /// The unit's flow reached its end.
    fn finish(&self, _unit: &str) -> Result<(), String> {
        Ok(())
    }

    /// What every account has left, asked afresh as `call` says. Only
    /// names and numbers: a token stays where it is kept.
    fn accounts(&self, _call: &CheckCall) -> Result<Picture, String> {
        Err("this runner knows no accounts".to_string())
    }

    /// Hand one thing that happened to a project's tiers: its executive,
    /// its goals' directors and their foremen decide, and what they
    /// decided is on their records when this returns. What comes back is
    /// what is left for the engine to do.
    fn turn(&self, project: &str, _event: &Event) -> Result<Turned, String> {
        Err(format!("this runner has no tiers for `{project}`"))
    }

    /// Bring a project's GitHub issues into step with its open beads: a
    /// bead with none is given one, and each issue's type and priority
    /// follow its bead's.
    fn issues(&self, project: &str) -> Result<IssueSweep, String> {
        Err(format!("this runner keeps no issues for `{project}`"))
    }

    /// Listen for what the owner does, until `stop` is set: each thing is
    /// handed to `heard` with the project it is for, as soon as it
    /// arrives. First of all, what [`StepRunner::keep`] kept.
    fn listen(&self, _heard: &dyn Fn(String, Incoming), stop: &AtomicBool) -> Result<(), String> {
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(())
    }

    /// Keep something the owner did that could not be handed on, for the
    /// next time someone listens.
    fn keep(&self, _incoming: &Incoming) {}

    /// The pipelines the goals' records say are running.
    fn running(&self) -> Result<Vec<RunningPipeline>, String> {
        Ok(Vec::new())
    }

    /// Whether the worker has been asked to stop at a safe point: it then
    /// takes on nothing new, and leaves when what it is doing is done.
    fn draining(&self) -> bool {
        false
    }
}

/// What a worker is in the middle of, for stopping it at a safe point.
#[derive(Default)]
pub struct Gate {
    busy: AtomicUsize,
    leaving: AtomicBool,
}

impl Gate {
    /// How many steps and turns are running in this worker right now.
    pub fn busy(&self) -> usize {
        self.busy.load(Ordering::SeqCst)
    }

    /// Have what only listens and watches end: the worker is leaving.
    pub fn leave(&self) {
        self.leaving.store(true, Ordering::SeqCst);
    }

    pub fn leaving(&self) -> bool {
        self.leaving.load(Ordering::SeqCst)
    }

    fn enter(self: &Arc<Self>) -> Busy {
        self.busy.fetch_add(1, Ordering::SeqCst);
        Busy(self.clone())
    }
}

/// One step or turn in flight.
struct Busy(Arc<Gate>);

impl Drop for Busy {
    fn drop(&mut self) {
        self.0.busy.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The pipelines among those a turn asked for that the server has no
/// workflow for yet. One that is there under the name is the one asked
/// for, since the name is made from the goal's record of it: a project
/// whose workflow starts afresh asks again for every pipeline its goals
/// have running. Asking the server to start it a second time would only
/// be refused, and stand in the project's history as a failure that is
/// none. The watcher tells the project what becomes of it either way.
pub async fn not_yet_there(
    client: &temporalio_client::Client,
    launches: Vec<serde_json::Value>,
) -> Vec<serde_json::Value> {
    let mut wanted = Vec::with_capacity(launches.len());
    for launch in launches {
        let there = match launch["id"].as_str() {
            Some(name) => client
                .get_workflow_handle::<Pipeline>(name.to_string())
                .describe(WorkflowDescribeOptions::default())
                .await
                .is_ok(),
            None => false,
        };
        // When the server could not be asked the pipeline is asked for
        // all the same: at worst that is refused, as it was before.
        if !there {
            wanted.push(launch);
        }
    }
    wanted
}

/// Sets a flag when it goes, however the code that holds it is left.
struct SetOnDrop(Arc<AtomicBool>);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// How a runner says which session an agent run is, from whatever thread
/// learns it.
pub type Started = Arc<dyn Fn(&str) + Send + Sync>;

/// The activities of a worker, over whatever does the work.
pub struct Steps {
    runner: Arc<dyn StepRunner>,
    gate: Arc<Gate>,
}

impl Steps {
    pub fn new(runner: Arc<dyn StepRunner>) -> Steps {
        Steps::with_gate(runner, Arc::new(Gate::default()))
    }

    /// The activities of a worker that can be stopped at a safe point:
    /// `gate` is how it knows what is in flight.
    pub fn with_gate(runner: Arc<dyn StepRunner>, gate: Arc<Gate>) -> Steps {
        Steps { runner, gate }
    }

    /// Tell a project of one of its pipelines, unless it was told the
    /// same a little while ago.
    async fn report(
        &self,
        client: &temporalio_client::Client,
        reach: &Reach,
        told: &mut HashMap<String, Instant>,
        project: &str,
        event: Event,
    ) {
        let key = format!("{project} {event:?}");
        if told
            .get(&key)
            .is_some_and(|at| at.elapsed() < Duration::from_secs(TOLD_AGAIN_SECS))
        {
            return;
        }
        // One that could not be told is told on the next look.
        if tiers::tell(client, reach, project, event).await.is_ok() {
            told.insert(key, Instant::now());
        }
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
        let _busy = self.gate.enter();
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
            let started: Started = Arc::new(move |id: &str| {
                let _ = said.send(id.to_string());
            });
            runner.agent(&work_call, resume, started)
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

    /// A command step or an action step. It says it is alive while it
    /// runs, so that one whose worker died is noticed and run again.
    #[activity]
    pub async fn plain(
        self: Arc<Self>,
        ctx: ActivityContext,
        call: StepCall,
    ) -> Result<StepDone, ActivityError> {
        let _busy = self.gate.enter();
        let runner = self.runner.clone();
        let every = Duration::from_secs(call.beat_secs.max(1));
        let mut work = tokio::task::spawn_blocking(move || runner.plain(&call));
        let mut beat = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
        loop {
            tokio::select! {
                done = &mut work => {
                    return match done {
                        Ok(Ok(done)) => Ok(done),
                        Ok(Err(reason)) => Err(failed(reason)),
                        Err(join) => Err(failed(format!("the step's run was lost: {join}"))),
                    };
                }
                _ = beat.tick() => {
                    ctx.record_heartbeat(String::new()).await?;
                }
            }
        }
    }

    /// Make a unit ready before its first step.
    #[activity]
    pub async fn prepare(
        self: Arc<Self>,
        _ctx: ActivityContext,
        call: PrepareCall,
    ) -> Result<Prepared, ActivityError> {
        let _busy = self.gate.enter();
        let runner = self.runner.clone();
        match tokio::task::spawn_blocking(move || runner.prepare(&call)).await {
            Ok(Ok(prepared)) => Ok(prepared),
            // Temporal would otherwise try this again for ever, and the
            // pipeline would stand still without saying why.
            Ok(Err(NotPrepared::Refused(reason))) => Err(ActivityError::from(
                ApplicationFailure::non_retryable(std::io::Error::other(reason)),
            )),
            Ok(Err(NotPrepared::Failed(reason))) => Err(failed(reason)),
            Err(join) => Err(failed(format!("preparing the unit was lost: {join}"))),
        }
    }

    /// Take up a unit that exists already.
    #[activity]
    pub async fn adopt(
        self: Arc<Self>,
        _ctx: ActivityContext,
        call: AdoptCall,
    ) -> Result<Adopted, ActivityError> {
        let _busy = self.gate.enter();
        let runner = self.runner.clone();
        match tokio::task::spawn_blocking(move || runner.adopt(&call)).await {
            Ok(Ok(adopted)) => Ok(adopted),
            Ok(Err(NotPrepared::Refused(reason))) => Err(ActivityError::from(
                ApplicationFailure::non_retryable(std::io::Error::other(reason)),
            )),
            Ok(Err(NotPrepared::Failed(reason))) => Err(failed(reason)),
            Err(join) => Err(failed(format!("taking up the unit was lost: {join}"))),
        }
    }

    /// Record that the engine has parked a unit.
    #[activity]
    pub async fn park(
        self: Arc<Self>,
        _ctx: ActivityContext,
        call: ParkCall,
    ) -> Result<bool, ActivityError> {
        let _busy = self.gate.enter();
        let runner = self.runner.clone();
        match tokio::task::spawn_blocking(move || runner.park(&call)).await {
            Ok(Ok(())) => Ok(true),
            Ok(Err(reason)) => Err(failed(reason)),
            Err(join) => Err(failed(format!("recording the park was lost: {join}"))),
        }
    }

    /// Ask what every account has left.
    #[activity]
    pub async fn accounts(
        self: Arc<Self>,
        _ctx: ActivityContext,
        call: CheckCall,
    ) -> Result<Picture, ActivityError> {
        let runner = self.runner.clone();
        match tokio::task::spawn_blocking(move || runner.accounts(&call)).await {
            Ok(Ok(picture)) => Ok(picture),
            Ok(Err(reason)) => Err(failed(reason)),
            Err(join) => Err(failed(format!("checking the accounts was lost: {join}"))),
        }
    }

    /// Ask the accounts workflow which account a step is to run under. A
    /// workflow cannot ask another this way itself, so it is done here.
    #[activity]
    pub async fn account(
        self: Arc<Self>,
        ctx: ActivityContext,
        call: AccountCall,
    ) -> Result<Lease, ActivityError> {
        let ask = Ask {
            who: call.who,
            model: call.model,
        };
        ctx.client()
            .get_workflow_handle::<Accounts>(call.accounts)
            .execute_update(
                Accounts::acquire,
                ask,
                WorkflowExecuteUpdateOptions::default(),
            )
            .await
            .map_err(|err| failed(format!("the accounts could not be asked: {err}")))
    }

    /// Record that a unit's flow reached its end.
    #[activity]
    pub async fn finish(
        self: Arc<Self>,
        _ctx: ActivityContext,
        unit: String,
    ) -> Result<bool, ActivityError> {
        let _busy = self.gate.enter();
        let runner = self.runner.clone();
        match tokio::task::spawn_blocking(move || runner.finish(&unit)).await {
            Ok(Ok(())) => Ok(true),
            Ok(Err(reason)) => Err(failed(reason)),
            Err(join) => Err(failed(format!("finishing the unit was lost: {join}"))),
        }
    }

    /// A sweep of a project's issues. It says it is alive while it runs,
    /// so that one whose worker died is noticed and run again.
    #[activity]
    pub async fn issues(
        self: Arc<Self>,
        ctx: ActivityContext,
        call: IssuesCall,
    ) -> Result<IssueSweep, ActivityError> {
        let _busy = self.gate.enter();
        let runner = self.runner.clone();
        let every = Duration::from_secs(call.beat_secs.max(1));
        let mut work = tokio::task::spawn_blocking(move || runner.issues(&call.project));
        let mut beat = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
        loop {
            tokio::select! {
                done = &mut work => {
                    return match done {
                        Ok(Ok(sweep)) => Ok(sweep),
                        Ok(Err(reason)) => Err(failed(reason)),
                        Err(join) => Err(failed(format!("the sweep was lost: {join}"))),
                    };
                }
                _ = beat.tick() => {
                    ctx.record_heartbeat(String::new()).await?;
                }
            }
        }
    }

    /// One turn of a project's tiers. It says it is alive while an agent
    /// of theirs runs, so that one whose worker died is noticed and the
    /// turn taken again.
    #[activity]
    pub async fn turn(
        self: Arc<Self>,
        ctx: ActivityContext,
        call: TurnCall,
    ) -> Result<Turned, ActivityError> {
        let _busy = self.gate.enter();
        let runner = self.runner.clone();
        let every = Duration::from_secs(call.beat_secs.max(1));
        // The watcher says a pipeline never started when the server has not
        // known it for a while. The project hears that only when its turn
        // comes, and the turn before it may be the one that started the
        // pipeline. One the server knows by now did start: its goal is not
        // told otherwise, the records are only looked at, and the watcher
        // tells the project how the pipeline really ends.
        let mut event = call.event;
        if let Some(name) = said_never_started(&event) {
            let there = ctx
                .client()
                .get_workflow_handle::<Pipeline>(name.to_string())
                .describe(WorkflowDescribeOptions::default())
                .await
                .is_ok();
            if there {
                event = Event::Tick;
            }
        }
        let mut work = tokio::task::spawn_blocking(move || runner.turn(&call.project, &event));
        let mut beat = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
        loop {
            tokio::select! {
                done = &mut work => {
                    return match done {
                        Ok(Ok(mut turned)) => {
                            turned.launches =
                                not_yet_there(&ctx.client(), turned.launches).await;
                            // What the turn left for other projects is
                            // signalled to each one's workflow. What
                            // cannot be handed on is kept as the listener
                            // keeps it, where it can be, and said.
                            for (project, event) in std::mem::take(&mut turned.handed) {
                                let told = ctx
                                    .client()
                                    .get_workflow_handle::<tiers::Project>(tiers::project_id(
                                        &project,
                                    ))
                                    .signal(
                                        tiers::Project::event,
                                        event.clone(),
                                        WorkflowSignalOptions::default(),
                                    )
                                    .await;
                                if let Err(err) = told {
                                    if let Event::Slack(incoming) = &event {
                                        self.runner.keep(incoming);
                                    }
                                    turned.trouble.get_or_insert(format!(
                                        "project `{project}` could not be handed what the owner approved: {err}"
                                    ));
                                }
                            }
                            Ok(turned)
                        }
                        // The tiers could not be asked at all. Whoever
                        // handed the event in decides what becomes of it.
                        Ok(Err(reason)) => Ok(Turned {
                            trouble: Some(reason),
                            ..Turned::default()
                        }),
                        Err(join) => Err(failed(format!("the turn was lost: {join}"))),
                    };
                }
                _ = beat.tick() => {
                    ctx.record_heartbeat(String::new()).await?;
                }
            }
        }
    }

    /// Listen for what the owner does, and signal each thing to its
    /// project's workflow as soon as it arrives. It runs for as long as
    /// it is told to, or until the worker leaves.
    #[activity]
    pub async fn listen(
        self: Arc<Self>,
        ctx: ActivityContext,
        call: PumpCall,
    ) -> Result<bool, ActivityError> {
        let client = ctx.client();
        let stop = Arc::new(AtomicBool::new(false));
        // The listener stops however this is left.
        let _stopped = SetOnDrop(stop.clone());
        let (said, mut heard) = tokio::sync::mpsc::unbounded_channel::<(String, Incoming)>();
        let (runner, flag) = (self.runner.clone(), stop.clone());
        let mut work = tokio::task::spawn_blocking(move || {
            runner.listen(
                &|project, incoming| {
                    let _ = said.send((project, incoming));
                },
                &flag,
            )
        });
        let began = Instant::now();
        let every = Duration::from_secs(call.beat_secs.max(1));
        let mut beat = tokio::time::interval(every);
        // The first thing that could not be handed on ends the run: it
        // is kept, and the run that takes this one's place hands it on.
        let mut undelivered: Option<String> = None;
        loop {
            tokio::select! {
                Some((project, incoming)) = heard.recv() => {
                    if undelivered.is_some() {
                        self.runner.keep(&incoming);
                    } else if let Err(reason) =
                        tiers::tell(&client, &call.reach, &project, Event::Slack(incoming.clone())).await
                    {
                        self.runner.keep(&incoming);
                        undelivered = Some(reason);
                        stop.store(true, Ordering::Relaxed);
                    }
                }
                done = &mut work => {
                    // What arrived as the listener stopped.
                    while let Ok((project, incoming)) = heard.try_recv() {
                        if undelivered.is_some()
                            || tiers::tell(&client, &call.reach, &project, Event::Slack(incoming.clone()))
                                .await
                                .is_err()
                        {
                            self.runner.keep(&incoming);
                        }
                    }
                    return match (done, undelivered) {
                        (_, Some(reason)) => Err(failed(format!(
                            "something the owner did could not be handed to its project, and is kept: {reason}"
                        ))),
                        (Ok(Ok(())), None) => Ok(true),
                        (Ok(Err(reason)), None) => Err(failed(reason)),
                        (Err(join), None) => Err(failed(format!("the listener was lost: {join}"))),
                    };
                }
                _ = beat.tick() => {
                    ctx.record_heartbeat(String::new()).await?;
                    if ctx.is_cancelled()
                        || self.gate.leaving()
                        || began.elapsed() >= Duration::from_secs(call.secs)
                    {
                        stop.store(true, Ordering::Relaxed);
                    }
                }
            }
        }
    }

    /// Watch the pipelines the goals' records say are running, and tell
    /// each one's project when it has a unit and when it has ended,
    /// however it ended. It runs for as long as it is told to, or until
    /// the worker leaves.
    #[activity]
    pub async fn watch(
        self: Arc<Self>,
        ctx: ActivityContext,
        call: PumpCall,
    ) -> Result<bool, ActivityError> {
        let client = ctx.client();
        let began = Instant::now();
        // What each project was told, and when: it hears a thing again
        // only if its record still does not show it a while later.
        let mut told: HashMap<String, Instant> = HashMap::new();
        // Pipelines the server does not know, and since when.
        let mut missing: HashMap<String, Instant> = HashMap::new();
        loop {
            if ctx.is_cancelled()
                || self.gate.leaving()
                || began.elapsed() >= Duration::from_secs(call.secs)
            {
                return Ok(true);
            }
            let runner = self.runner.clone();
            let running = match tokio::task::spawn_blocking(move || runner.running()).await {
                Ok(Ok(running)) => running,
                Ok(Err(reason)) => return Err(failed(reason)),
                Err(join) => return Err(failed(format!("the records could not be read: {join}"))),
            };
            missing.retain(|name, _| running.iter().any(|pipeline| &pipeline.name == name));
            for pipeline in running {
                let handle = client.get_workflow_handle::<Pipeline>(pipeline.name.clone());
                let ended = |unit: Option<String>, word: &str, rest: String| Event::Ended {
                    goal: pipeline.goal.clone(),
                    bead: pipeline.bead.clone(),
                    ship: pipeline.ship,
                    unit: unit
                        .filter(|unit| !unit.is_empty())
                        .or(pipeline.unit.clone()),
                    word: word.to_string(),
                    rest,
                    name: pipeline.name.clone(),
                };
                let status = match handle.describe(WorkflowDescribeOptions::default()).await {
                    Ok(description) => description.status(),
                    Err(WorkflowInteractionError::NotFound(_)) => {
                        let since = *missing
                            .entry(pipeline.name.clone())
                            .or_insert_with(Instant::now);
                        if since.elapsed() >= Duration::from_secs(NEVER_STARTED_SECS) {
                            let event = ended(None, "failed", NEVER_STARTED.to_string());
                            self.report(&client, &call.reach, &mut told, &pipeline.project, event)
                                .await;
                        }
                        continue;
                    }
                    // The server could not be asked: the next look asks again.
                    Err(_) => continue,
                };
                missing.remove(&pipeline.name);
                let standing = || async {
                    handle
                        .query(Pipeline::standing, (), WorkflowQueryOptions::default())
                        .await
                        .ok()
                };
                let event = match status {
                    WorkflowExecutionStatus::Running | WorkflowExecutionStatus::Paused => {
                        if pipeline.unit.is_some() {
                            continue;
                        }
                        match standing().await {
                            Some(standing) if !standing.unit.is_empty() => Event::Began {
                                goal: pipeline.goal.clone(),
                                bead: pipeline.bead.clone(),
                                unit: standing.unit,
                                name: pipeline.name.clone(),
                            },
                            _ => continue,
                        }
                    }
                    WorkflowExecutionStatus::Completed => {
                        match handle.get_result(WorkflowGetResultOptions::default()).await {
                            Ok(end) => match serde_json::to_string(&end) {
                                Ok(rest) => ended(Some(end.unit.clone()), "end", rest),
                                Err(_) => continue,
                            },
                            Err(_) => continue,
                        }
                    }
                    WorkflowExecutionStatus::Unspecified
                    | WorkflowExecutionStatus::ContinuedAsNew
                    | WorkflowExecutionStatus::Unknown => continue,
                    // It failed, or someone ended it by hand.
                    closed => {
                        let standing = standing().await;
                        let unit = standing.as_ref().map(|standing| standing.unit.clone());
                        match standing.and_then(|standing| standing.refused) {
                            Some(reason) => ended(unit, "refused", reason.replace('\n', " ")),
                            None => {
                                let why = match handle
                                    .get_result(WorkflowGetResultOptions::default())
                                    .await
                                {
                                    Err(err) => err.to_string().replace('\n', " "),
                                    Ok(_) => format!("it is closed as {closed:?}"),
                                };
                                ended(unit, "failed", why)
                            }
                        }
                    }
                };
                self.report(&client, &call.reach, &mut told, &pipeline.project, event)
                    .await;
            }
            ctx.record_heartbeat(String::new()).await?;
            tokio::time::sleep(Duration::from_secs(call.beat_secs.clamp(1, 3))).await;
        }
    }
}

/// A runner that does each step by running a command, which is how a
/// worker will hand a step to the factory, and how a test scripts one.
///
/// The command is told what to do in its environment (`FACTORY_KIND`,
/// `FACTORY_UNIT`, `FACTORY_STEP`, `FACTORY_ROUND`, `FACTORY_NOTE`,
/// `FACTORY_ACTION`, `FACTORY_SESSION` when a cut-off run is to be resumed,
/// and `FACTORY_ACCOUNT` when the step was handed an account), and answers
/// on its standard output, a line at a time: `session <id>` as soon as an
/// agent run knows its session, then `outcome <word>`, or `limited
/// <seconds>` if the run stopped at a usage limit. Asked for the accounts
/// (`FACTORY_KIND=accounts`), it answers `accounts <picture as JSON>`.
pub struct ExecRunner {
    pub argv: Vec<String>,
}

impl ExecRunner {
    fn run(
        &self,
        kind: &str,
        call: &StepCall,
        session: Option<String>,
        started: Option<Started>,
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
            .env("FACTORY_ACCOUNT", call.account.as_deref().unwrap_or(""))
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
                    Some(("session", id)) => {
                        if let Some(started) = &started {
                            started(id.trim());
                        }
                    }
                    Some(("park", reason)) => done.parked = Some(reason.trim().to_string()),
                    Some(("outcome", word)) => done.outcome = word.trim().to_string(),
                    Some(("limited", secs)) => done.limited_secs = secs.trim().parse().ok(),
                    _ => {}
                }
            }
        }
        let status = child
            .wait()
            .map_err(|err| format!("`{program}` could not be waited for: {err}"))?;
        if done.outcome.is_empty() && done.limited_secs.is_none() && done.parked.is_none() {
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
        started: Started,
    ) -> Result<StepDone, String> {
        self.run("agent", call, session, Some(started))
    }

    fn plain(&self, call: &StepCall) -> Result<StepDone, String> {
        let kind = if call.action.is_some() {
            "action"
        } else {
            "command"
        };
        self.run(kind, call, None, None)
    }

    fn issues(&self, project: &str) -> Result<IssueSweep, String> {
        let (program, args) = self.argv.split_first().ok_or("the step command is empty")?;
        let out = std::process::Command::new(program)
            .args(args)
            .env("FACTORY_KIND", "issues")
            .env("FACTORY_PROJECT", project)
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|err| format!("`{program}` could not be run: {err}"))?;
        let said = String::from_utf8_lossy(&out.stdout);
        let sweep = said
            .lines()
            .find_map(|line| line.strip_prefix("issues "))
            .ok_or("the command did not say what the sweep did")?;
        serde_json::from_str(sweep).map_err(|err| format!("the sweep's result: {err}"))
    }

    fn accounts(&self, call: &CheckCall) -> Result<Picture, String> {
        let (program, args) = self.argv.split_first().ok_or("the step command is empty")?;
        let out = std::process::Command::new(program)
            .args(args)
            .env("FACTORY_KIND", "accounts")
            .env("FACTORY_FORGET", if call.forget_limits { "1" } else { "" })
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|err| format!("`{program}` could not be run: {err}"))?;
        let said = String::from_utf8_lossy(&out.stdout);
        let picture = said
            .lines()
            .find_map(|line| line.strip_prefix("accounts "))
            .ok_or("the command did not say what the accounts have left")?;
        serde_json::from_str(picture).map_err(|err| format!("the accounts' picture: {err}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ended(word: &str, rest: &str) -> Event {
        Event::Ended {
            goal: "g1".to_string(),
            bead: "testing-envs".to_string(),
            ship: true,
            unit: None,
            word: word.to_string(),
            rest: rest.to_string(),
            name: "pipeline-g1-testing-envs-1-0".to_string(),
        }
    }

    #[test]
    fn only_the_watchers_own_report_is_checked_against_the_server() {
        // What the watcher says of a pipeline the server does not know
        // names the pipeline to ask the server about.
        assert_eq!(
            said_never_started(&ended("failed", NEVER_STARTED)),
            Some("pipeline-g1-testing-envs-1-0")
        );
        // A pipeline that failed for a reason of its own did run, and one
        // that ended or was refused says so itself: none is asked about.
        assert_eq!(
            said_never_started(&ended("failed", "the worker was lost")),
            None
        );
        assert_eq!(said_never_started(&ended("end", NEVER_STARTED)), None);
        assert_eq!(said_never_started(&ended("refused", "no criteria")), None);
        assert_eq!(said_never_started(&Event::Tick), None);
    }
}
