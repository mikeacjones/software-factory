//! The tiers: a project's executive, its goals' directors and their
//! sub-goals' foremen, on the engine.
//!
//! What the tiers decide, and the record of every goal, are the
//! supervisor's, as a unit's record is the driver's: this does not decide
//! anything. It owns what the service's loop used to own. Each thing that
//! happens to a project, an owner's message, a pipeline that began or
//! ended, a time that came due, is a signal to the project's workflow,
//! which hands them to the supervisor one at a time, each as one activity,
//! and starts the pipelines the supervisor asks for as workflows of their
//! own. Nothing is kept in a process: a message that arrives while an
//! agent is busy, while no account is free or while the worker is being
//! replaced is in the workflow until it has been dealt with.
//!
//! One more workflow, the intake, keeps two things alive that bring the
//! outside in: the listener on Slack, and the watcher that tells each
//! project how its pipelines are doing.

use std::collections::VecDeque;
use std::time::Duration;

use factory_config::GoalDefinition;
use factory_driver::Within;
use factory_driver::goal::{Event, Turned};
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use temporalio_client::errors::WorkflowInteractionError;
use temporalio_client::{
    Client, WorkflowIdConflictPolicy, WorkflowSignalOptions, WorkflowStartOptions,
};
use temporalio_macros::{workflow, workflow_methods};
use temporalio_sdk::{
    ActivityOptions, ChildWorkflowOptions, ContinueAsNewOptions, ParentClosePolicy,
    SyncWorkflowContext, TimerOptions, WorkflowContext, WorkflowContextView, WorkflowResult,
};
use temporalio_workflow::{RetryPolicy, WorkflowIdReusePolicy};

use crate::describe;
use crate::engine::{Adopt, Pipeline, PipelineInput};
use crate::steps::{PumpCall, Steps, TurnCall};

/// What a project's workflow is called.
pub fn project_id(project: &str) -> String {
    format!("project-{project}")
}

/// The ID of the intake workflow. There is one in a namespace.
pub const INTAKE_ID: &str = "intake";

/// How many turns a project's workflow takes before it continues as a new
/// run, so that its history stays short.
const TURNS_PER_RUN: u32 = 150;

/// How many reports of pipelines a project remembers having been given,
/// to know one it is given twice.
const SEEN: usize = 400;

/// How long after a turn that failed the project looks again by itself,
/// and hands in again what a pipeline reported.
const AFTER_TROUBLE_SECS: u64 = 300;

/// How long the listener and the watcher run before the intake hands
/// over to a new run of itself, in seconds.
pub const INTAKE_RUN_SECS: u64 = 24 * 3600;

/// The least a project waits after a turn before it looks by itself
/// again, in seconds, doubled each time a look changed nothing, up to
/// [`IDLE_LOOK_MAX_SECS`]. Whatever was due when a turn was taken was
/// dealt with in it, so a time that is still in the past afterwards is
/// one no look will move.
const IDLE_LOOK_SECS: u64 = 30;
const IDLE_LOOK_MAX_SECS: u64 = 900;

/// How a project's workflow is reached, and started if it is not there.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reach {
    pub task_queue: String,
    /// How often a turn says it is alive, in seconds.
    pub beat_secs: u64,
    /// The accounts workflow its pipelines ask for their accounts.
    pub accounts: String,
}

/// What a project's workflow is started with by whoever finds it is not
/// there: its ID, and what it says about itself. One that is running is
/// left as it is.
pub fn project_options(task_queue: &str, project: &str) -> WorkflowStartOptions {
    WorkflowStartOptions::new(task_queue.to_string(), project_id(project))
        .id_conflict_policy(WorkflowIdConflictPolicy::UseExisting)
        .static_summary(describe::line(&format!(
            "Project `{project}`: its executive, directors and foremen take their turns here"
        )))
        .static_details(describe::project_static(project))
        .build()
}

/// See that a project's workflow is running.
pub async fn ensure_project(client: &Client, reach: &Reach, project: &str) -> Result<(), String> {
    let input = ProjectInput {
        project: project.to_string(),
        beat_secs: reach.beat_secs,
        accounts: reach.accounts.clone(),
        ..ProjectInput::default()
    };
    client
        .start_workflow(
            Project::run,
            input,
            project_options(&reach.task_queue, project),
        )
        .await
        .map(|_| ())
        .map_err(|err| format!("the workflow of project `{project}` could not be started: {err}"))
}

/// Tell a project's workflow that something happened. If the workflow is
/// not there, it is started first: nothing said to a project is lost
/// because someone ended its workflow.
pub async fn tell(
    client: &Client,
    reach: &Reach,
    project: &str,
    event: Event,
) -> Result<(), String> {
    let mut last = String::new();
    for attempt in 0..3 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let told = client
            .get_workflow_handle::<Project>(project_id(project))
            .signal(
                Project::event,
                event.clone(),
                WorkflowSignalOptions::default(),
            )
            .await;
        match told {
            Ok(()) => return Ok(()),
            Err(WorkflowInteractionError::NotFound(_)) => {
                last = match ensure_project(client, reach, project).await {
                    Ok(()) => format!("the workflow of project `{project}` was not there"),
                    Err(reason) => reason,
                };
            }
            Err(err) => last = format!("project `{project}` could not be told: {err}"),
        }
    }
    Err(last)
}

/// Have the running tiers take up a goal definition of a project that was
/// put in force while they run, so that no worker has to be restarted for
/// it. A worker starts the workflow only of a project that has
/// definitions, so it is started here if it is not there; one that is
/// running is left as it is, and reads the definitions in force at its
/// next turn. A definition with a schedule has the project look now: its
/// workflow learns when it is next due only from a turn, and no turn has
/// seen this schedule yet. Returns whether the project was told to look.
pub async fn take_up(
    client: &Client,
    reach: &Reach,
    project: &str,
    definition: &GoalDefinition,
) -> Result<bool, String> {
    ensure_project(client, reach, project).await?;
    let scheduled = definition.schedule.is_some();
    if scheduled {
        tell(client, reach, project, Event::Tick).await?;
    }
    Ok(scheduled)
}

/// What a project's workflow is started with, and what it hands to its
/// next run when it continues as new.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ProjectInput {
    pub project: String,
    /// How often a running turn says it is alive, in seconds.
    pub beat_secs: u64,
    /// The accounts workflow its pipelines ask for their accounts.
    #[serde(default)]
    pub accounts: String,
    /// What has happened and has not been dealt with yet, in order.
    #[serde(default)]
    pub queue: Vec<Event>,
    /// What is to be handed in again, and when.
    #[serde(default)]
    pub later: Vec<(u64, Event)>,
    /// The reports of pipelines it has been given already.
    #[serde(default)]
    pub seen: Vec<String>,
    /// When the supervisor next has something to do by itself.
    #[serde(default)]
    pub due_at: Option<u64>,
    /// How many turns the project has taken, over all its runs.
    #[serde(default)]
    pub turns: u64,
}

/// Where a project's workflow stands, for anyone who asks.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectStanding {
    pub project: String,
    /// What it is doing now, in words.
    pub doing: String,
    /// How many things wait to be dealt with.
    pub waiting: usize,
    pub turns: u64,
    /// What the last turns did, newest last.
    pub lately: Vec<String>,
    /// Why the last turn that failed did, if one has since the run began.
    pub trouble: Option<String>,
    pub due_at: Option<u64>,
}

/// A pipeline the supervisor asked for.
#[derive(Clone, Debug, Deserialize)]
struct Launch {
    id: String,
    project: String,
    bead: String,
    #[serde(default)]
    ship: bool,
    #[serde(default)]
    flow_name: Option<String>,
    #[serde(default)]
    within: Option<Within>,
    #[serde(default)]
    unit: Option<String>,
    #[serde(default)]
    adopt: Option<Adopt>,
}

/// A project's tiers.
#[workflow]
#[derive(Default)]
pub struct Project {
    queue: VecDeque<Event>,
    seen: VecDeque<String>,
    standing: ProjectStanding,
}

fn unix(time: Option<std::time::SystemTime>) -> u64 {
    time.and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

/// What a report of a pipeline is known by, if the event is one: the
/// watcher tells a project again until the goal's record shows it heard.
/// The pipeline's own name is part of it, since a unit that parked and
/// was continued ends more than once.
fn report_key(event: &Event) -> Option<String> {
    match event {
        Event::Began { name, unit, .. } => Some(format!("began {name} {unit}")),
        Event::Ended { name, word, .. } => Some(format!("ended {name} {word}")),
        _ => None,
    }
}

impl Project {
    /// Put something that happened where it waits its turn, unless it is
    /// a report of a pipeline this project has been given already.
    fn take(&mut self, event: Event) {
        if let Some(key) = report_key(&event) {
            if self.seen.contains(&key) {
                return;
            }
            self.seen.push_back(key);
            while self.seen.len() > SEEN {
                self.seen.pop_front();
            }
        }
        self.queue.push_back(event);
        self.standing.waiting = self.queue.len();
    }

    fn details(&self) -> String {
        let standing = &self.standing;
        let mut text = format!(
            "The executive, directors and foremen of project `{}` take their turns here, one at a time.\n\n**Now:** {}\n\n",
            standing.project, standing.doing
        );
        text.push_str(&format!(
            "{} turn(s) taken · {} thing(s) waiting to be dealt with",
            standing.turns, standing.waiting
        ));
        if let Some(at) = standing.due_at {
            text.push_str(&format!(" · next due by itself at {}", describe::clock(at)));
        }
        text.push_str("\n\n");
        if let Some(trouble) = &standing.trouble {
            text.push_str(&format!(
                "**The last turn that failed:** {}\n\n",
                describe::cut(trouble, 2_000)
            ));
        }
        if !standing.lately.is_empty() {
            text.push_str("**Lately:**\n");
            for line in &standing.lately {
                text.push_str(&format!("- {line}\n"));
            }
        }
        describe::cut(text.trim_end(), describe::DETAILS_MAX)
    }
}

/// Say where the project stands, for someone reading it on the server.
fn show(ctx: &WorkflowContext<Project>, doing: String) {
    let details = ctx.state_mut(|state| {
        state.standing.doing = doing;
        state.standing.waiting = state.queue.len();
        state.details()
    });
    ctx.set_current_details(details);
}

#[workflow_methods]
impl Project {
    #[run]
    pub async fn run(ctx: &mut WorkflowContext<Self>, input: ProjectInput) -> WorkflowResult<()> {
        let beat = input.beat_secs.max(1);
        ctx.state_mut(|state| {
            // What was signalled before this run's first task is already
            // in the queue: what the last run handed on goes before it.
            for event in input.queue.iter().rev() {
                state.queue.push_front(event.clone());
            }
            for key in input.seen.iter().rev() {
                state.seen.push_front(key.clone());
            }
            state.standing.project = input.project.clone();
            state.standing.turns = input.turns;
            state.standing.due_at = input.due_at;
            // A project that is new here first picks up what was running
            // before: pipelines the records say are running are asked for
            // again, and whoever was owed an event is woken. It goes
            // before anything that was signalled meanwhile.
            if input.turns == 0 {
                state.queue.push_front(Event::Resume);
            }
        });
        let mut later = input.later.clone();
        let mut due_at = input.due_at;
        let mut turns_here = 0u32;
        // How many looks in a row found the same thing still due.
        let mut idle_looks = 0u32;
        loop {
            let now = unix(ctx.workflow_time());
            // What was put off until now is handed in again.
            let (ready, wait): (Vec<_>, Vec<_>) = later.into_iter().partition(|(at, _)| *at <= now);
            later = wait;
            ctx.state_mut(|state| {
                for (_, event) in ready {
                    state.queue.push_back(event);
                }
            });
            let event = match ctx.state_mut(|state| state.queue.pop_front()) {
                Some(event) => event,
                None if due_at.is_some_and(|at| at <= now) => Event::Tick,
                None => {
                    // Nothing to do. It waits, with nothing running, for
                    // something to happen or for a time to come.
                    let wake = later.iter().map(|(at, _)| *at).chain(due_at).min();
                    show(
                        ctx,
                        match wake {
                            Some(at) => format!(
                                "waiting for something to happen, or for {}",
                                describe::clock(at)
                            ),
                            None => "waiting for something to happen".to_string(),
                        },
                    );
                    let mut heard = ctx.wait_condition(|state| !state.queue.is_empty()).fuse();
                    match wake {
                        Some(at) => {
                            let secs = at.saturating_sub(now).max(1);
                            let options = TimerOptions::builder(Duration::from_secs(secs))
                                .summary(describe::line(&format!(
                                    "Project `{}` has nothing to do for {}: at {} an agent is tried again, a note is dealt with, or a message that waited for an account is answered",
                                    input.project,
                                    describe::span(secs),
                                    describe::clock(at)
                                )))
                                .build();
                            let mut timer = ctx.timer(options).fuse();
                            futures::select_biased! {
                                heard = heard => heard?,
                                _ = timer => {}
                            }
                        }
                        None => heard.await?,
                    }
                    continue;
                }
            };
            let what = describe::event(&event);
            show(ctx, what.clone());
            // If the worker dies in the middle, the server hands the turn
            // to another, for as long as it takes. What the tiers
            // themselves could not do comes back as the turn's trouble.
            let options =
                ActivityOptions::with_start_to_close_timeout(Duration::from_secs(6 * 3600))
                    .heartbeat_timeout(Duration::from_secs(beat * 3))
                    .retry_policy(
                        RetryPolicy::builder()
                            .maximum_interval(Duration::from_secs(60))
                            .build(),
                    )
                    .summary(describe::line(&format!(
                        "Project `{}`: {what}",
                        input.project
                    )))
                    .build();
            let call = TurnCall {
                project: input.project.clone(),
                event: event.clone(),
                beat_secs: beat,
            };
            // A turn that could not be taken at all is the project's
            // trouble, not its end: it goes on with the next thing.
            let turned = match ctx.execute_activity(Steps::turn, call, options).await {
                Ok(turned) => turned,
                Err(err) => Turned {
                    due_at,
                    trouble: Some(format!("the turn could not be taken: {err}")),
                    ..Turned::default()
                },
            };
            let now = unix(ctx.workflow_time());
            let mut started = Vec::new();
            let mut unread = None;
            for launch in &turned.launches {
                let launch: Launch = match serde_json::from_value(launch.clone()) {
                    Ok(launch) => launch,
                    Err(err) => {
                        unread = Some(format!("a pipeline to start could not be read: {err}"));
                        continue;
                    }
                };
                let taken_up = launch.unit.is_some();
                let pipeline = PipelineInput {
                    unit: launch.unit.clone().unwrap_or_default(),
                    project: launch.project.clone(),
                    bead: launch.bead.clone(),
                    flow_name: launch.flow_name.clone(),
                    beat_secs: beat,
                    accounts: input.accounts.clone(),
                    within: launch.within.clone(),
                    adopt: taken_up.then(|| launch.adopt.clone().unwrap_or_default()),
                    stop_at_park: true,
                    ..PipelineInput::default()
                };
                let about = describe::subject(&launch.bead, "", "");
                let summary = if launch.ship {
                    format!(
                        "Sub-goal {about} of project `{}`: ship it as one pull request",
                        launch.project
                    )
                } else if taken_up {
                    format!(
                        "Bead {about} of project `{}`: go on with its unit",
                        launch.project
                    )
                } else {
                    format!(
                        "Bead {about} of project `{}`: take it through its flow",
                        launch.project
                    )
                };
                let options = ChildWorkflowOptions::builder()
                    .workflow_id(launch.id.clone())
                    // It goes on whatever becomes of this run: the
                    // project hears of it from the watcher.
                    .parent_close_policy(ParentClosePolicy::Abandon)
                    .id_reuse_policy(WorkflowIdReusePolicy::RejectDuplicate)
                    .static_summary(describe::line(&summary))
                    .static_details(describe::pipeline_static(
                        &launch.project,
                        &launch.bead,
                        launch.unit.as_deref().unwrap_or(""),
                        launch.flow_name.as_deref(),
                    ))
                    .build();
                // The turn left out those the server already has. One
                // started under the name since then is the one asked
                // for all the same, so a refusal here is no trouble.
                let _ = ctx
                    .start_child_workflow(Pipeline::run, pipeline, options)
                    .await;
                started.push(launch.bead.clone());
            }
            // A look that leaves something overdue changed nothing about
            // it: the next one comes later each time, never at once.
            let overdue = turned.due_at.is_some_and(|at| at <= now);
            idle_looks = match (&event, overdue) {
                (Event::Tick, true) => idle_looks.saturating_add(1),
                _ => 0,
            };
            let least = (IDLE_LOOK_SECS << idle_looks.min(5)).min(IDLE_LOOK_MAX_SECS);
            due_at = turned.due_at.map(|at| at.max(now + least));
            let trouble = turned.trouble.clone().or(unread);
            if let Some(at) = turned.again_at {
                later.push((at.max(now + 1), event));
            } else if trouble.is_some() && report_key(&event).is_some() {
                // What a pipeline reported is never dropped: a goal that
                // did not hear its pipeline end would wait for ever.
                later.push((now + AFTER_TROUBLE_SECS, event));
            }
            if trouble.is_some() {
                // What stands on the goals' records is looked at again.
                due_at = Some(due_at.map_or(now + AFTER_TROUBLE_SECS, |at| {
                    at.min(now + AFTER_TROUBLE_SECS)
                }));
            }
            ctx.state_mut(|state| {
                let mut line = format!("{} {what}", describe::clock(now));
                if !started.is_empty() {
                    line.push_str(&format!(" → started `{}`", started.join("`, `")));
                }
                if turned.again_at.is_some() {
                    line.push_str(" → put off: no account was free");
                }
                if let Some(trouble) = &trouble {
                    line.push_str(&format!(" → FAILED: {trouble}"));
                    state.standing.trouble = Some(format!("{what}: {trouble}"));
                }
                state.standing.lately.push(describe::cut(&line, 300));
            });
            turns_here += 1;
            ctx.state_mut(|state| {
                state.standing.turns += 1;
                state.standing.due_at = due_at;
                let extra = state.standing.lately.len().saturating_sub(25);
                state.standing.lately.drain(..extra);
            });
            if turns_here >= TURNS_PER_RUN || ctx.continue_as_new_suggested() {
                let next = ctx.state_mut(|state| ProjectInput {
                    project: input.project.clone(),
                    beat_secs: input.beat_secs,
                    accounts: input.accounts.clone(),
                    queue: state.queue.drain(..).collect(),
                    later: later.clone(),
                    seen: state.seen.iter().cloned().collect(),
                    due_at,
                    turns: state.standing.turns,
                });
                ctx.continue_as_new(next, ContinueAsNewOptions::default())?;
            }
        }
    }

    /// Something happened. It is dealt with in its turn.
    #[signal]
    pub fn event(&mut self, _ctx: &mut SyncWorkflowContext<Self>, event: Event) {
        self.take(event);
    }

    /// Where the project stands.
    #[query]
    pub fn standing(&self, _ctx: &WorkflowContextView) -> ProjectStanding {
        ProjectStanding {
            waiting: self.queue.len(),
            ..self.standing.clone()
        }
    }
}

/// What the intake is started with.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct IntakeInput {
    pub reach: Reach,
    /// How long the listener and the watcher run before the intake
    /// continues as a new run, in seconds.
    pub run_secs: u64,
}

/// What the intake workflow is started with: its ID, and what it says
/// about itself. One that is already running is left as it is.
pub fn intake_options(task_queue: &str) -> WorkflowStartOptions {
    WorkflowStartOptions::new(task_queue.to_string(), INTAKE_ID.to_string())
        .id_conflict_policy(WorkflowIdConflictPolicy::UseExisting)
        .static_summary(
            "Listens on Slack for the owner and watches every goal's pipelines, and tells each project's workflow what it hears and sees"
                .to_string(),
        )
        .static_details(describe::intake_static())
        .build()
}

/// The intake: what brings the outside in.
#[workflow]
#[derive(Default)]
pub struct Intake {}

#[workflow_methods]
impl Intake {
    #[run]
    pub async fn run(ctx: &mut WorkflowContext<Self>, input: IntakeInput) -> WorkflowResult<()> {
        // They say they are alive more often than a turn does: a
        // listener that is gone is a factory that does not hear.
        let beat = input.reach.beat_secs.clamp(1, 5);
        ctx.set_current_details(format!(
            "Listening on Slack for the owner, and watching the pipelines the goals' records say are running.\n\nWhat is heard or seen is signalled to the project's workflow (`project-<id>`), where it waits its turn. This run started at {} and hands over to a new one after {}.",
            describe::clock(unix(ctx.workflow_time())),
            describe::span(input.run_secs),
        ));
        let pump = |what: &str| {
            ActivityOptions::with_start_to_close_timeout(Duration::from_secs(input.run_secs + 600))
                .heartbeat_timeout(Duration::from_secs(beat * 4))
                // Tried again for as long as it takes, and soon.
                .retry_policy(
                    RetryPolicy::builder()
                        .maximum_interval(Duration::from_secs(15))
                        .build(),
                )
                .summary(describe::line(what))
                .build()
        };
        let call = PumpCall {
            secs: input.run_secs,
            beat_secs: beat,
            reach: input.reach.clone(),
        };
        let mut listen = ctx
            .execute_activity(
                Steps::listen,
                call.clone(),
                pump("Listen on Slack: each message or button press of the owner's is signalled to its project's workflow"),
            )
            .fuse();
        let mut watch = ctx
            .execute_activity(
                Steps::watch,
                call,
                pump("Watch the pipelines the goals' records say are running: tell each one's project when it has a unit and when it has ended"),
            )
            .fuse();
        // When either has run its time, or has ended for a reason the
        // server will not try again, a new run takes over and starts both
        // afresh. The one still running finds out with its next sign of
        // life, and ends.
        let ended_well = futures::select_biased! {
            heard = listen => heard.is_ok(),
            seen = watch => seen.is_ok(),
        };
        if !ended_well {
            let pause = TimerOptions::builder(Duration::from_secs(30))
                .summary(
                    "The listener or the watcher ended in a failure the server does not try again: wait a little, then start both afresh"
                        .to_string(),
                )
                .build();
            ctx.timer(pause).await;
        }
        match ctx.continue_as_new(input, ContinueAsNewOptions::default())? {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ended(name: &str, word: &str) -> Event {
        Event::Ended {
            goal: "g".to_string(),
            bead: "b".to_string(),
            ship: false,
            unit: Some("u".to_string()),
            word: word.to_string(),
            rest: String::new(),
            name: name.to_string(),
        }
    }

    #[test]
    fn a_report_is_known_by_its_pipeline_so_a_continued_unit_can_end_twice() {
        // The watcher tells a project of the same ending until the goal's
        // record shows it: that is one report.
        assert_eq!(
            report_key(&ended("pipeline-g-b-1-0", "end")),
            report_key(&ended("pipeline-g-b-1-0", "end"))
        );
        // The unit parked, was continued by a new pipeline, and that one
        // ended too: the same goal, bead, unit and word, and another report.
        assert_ne!(
            report_key(&ended("pipeline-g-b-1-0", "end")),
            report_key(&ended("pipeline-g-b-1-1", "end"))
        );
        // What the owner does is never taken for a repeat.
        assert_eq!(report_key(&Event::Tick), None);
        assert_eq!(
            report_key(&Event::Open {
                project: "p".to_string(),
                text: "t".to_string()
            }),
            None
        );
    }

    #[test]
    fn a_project_told_the_same_report_twice_queues_it_once() {
        let mut project = Project::default();
        for event in [
            ended("pipeline-g-b-1-0", "end"),
            ended("pipeline-g-b-1-0", "end"),
            ended("pipeline-g-b-1-1", "end"),
            Event::Tick,
            Event::Tick,
        ] {
            project.take(event);
        }
        // Both endings once each, and both looks.
        assert_eq!(project.queue.len(), 4);
        assert_eq!(project.standing.waiting, 4);
        // It remembers only so many: the oldest are forgotten first.
        for n in 0..SEEN + 10 {
            project.take(ended(&format!("pipeline-g-b-{n}-9"), "end"));
        }
        assert_eq!(project.seen.len(), SEEN);
    }
}
