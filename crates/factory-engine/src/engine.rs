//! The engine: one workflow that runs any flow.

use std::time::Duration;

use factory_flow::{Flow, Next, Position, Step, StepKind};
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use temporalio_macros::{workflow, workflow_methods};
use temporalio_sdk::{
    ActivityOptions, ApplicationFailure, SignalWorkflowOptions, SyncWorkflowContext, TimerOptions,
    WorkflowContext, WorkflowContextView, WorkflowResult,
};

use crate::accounts::{AccountCall, Accounts, Lease, Limit};
use crate::describe::{self, Showing};
use crate::steps::{AdoptCall, ParkCall, PrepareCall, StepCall, StepDone, Steps};

/// How much longer than it was told to expect a pipeline waits for word
/// that an account is back before it asks again by itself, in seconds.
const ASK_AGAIN_SECS: u64 = 60;

/// What a pipeline is started with.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PipelineInput {
    /// The unit the pipeline is for, if it exists already. Empty for a
    /// bead that has no unit yet: one is made first, from `project` and
    /// `bead`, and the pipeline runs the flow that unit is pinned to.
    pub unit: String,
    /// The flow definition, as the text of its file. It is part of the
    /// workflow from here on: the file may change and this pipeline will not.
    pub flow: String,
    /// The project and the bead a unit is made for, when `unit` is empty.
    #[serde(default)]
    pub project: String,
    #[serde(default)]
    pub bead: String,
    /// The flow to run instead of the project's own, by name.
    #[serde(default)]
    pub flow_name: Option<String>,
    /// How often an agent step says it is alive, and so how soon a step
    /// whose worker died is given to another. Seconds.
    pub beat_secs: u64,
    /// The accounts workflow that says which account each agent step runs
    /// under, by its ID. Empty leaves the choice to whatever runs the step.
    #[serde(default)]
    pub accounts: String,
    /// The sub-goal the bead belongs to, when a goal runs it: one bead of
    /// the sub-goal, or the sub-goal itself being shipped.
    #[serde(default)]
    pub within: Option<factory_driver::Within>,
    /// Take `unit` up where its record says it stands, and go on as this
    /// says, instead of starting its flow from the top.
    #[serde(default)]
    pub adopt: Option<Adopt>,
    /// End when the unit parks, with the reason, and do not wait to be
    /// continued. For a pipeline a goal runs: what happens to a parked
    /// one is decided there, and going on is a new pipeline that takes
    /// the unit up.
    #[serde(default)]
    pub stop_at_park: bool,
}

/// How to go on with a unit that is taken up.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Adopt {
    /// The step to go on from. `None` goes on where the unit stands.
    pub from: Option<String>,
    /// What whoever continues it wants that step to know.
    pub note: Option<String>,
    /// A decision about the bead that every later step is shown.
    pub ruling: Option<String>,
}

/// Where a pipeline stands, for anyone who asks.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Standing {
    /// The unit the pipeline is for, once it has one.
    pub unit: String,
    /// The step it is in, or was in when it stopped.
    pub step: String,
    /// Why it is parked, if it is. A parked pipeline waits to be continued.
    pub parked: Option<String>,
    /// When it is waiting for a usage limit to lift: for how long, in seconds.
    pub limited_secs: Option<u64>,
    /// The steps that have ended, in order, each with its outcome.
    pub done: Vec<(String, String)>,
    /// Why the pipeline ended before its first step, if it did: the bead
    /// may not be worked on as it stands.
    #[serde(default)]
    pub refused: Option<String>,
}

/// How a pipeline ended.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipelineEnd {
    /// True if the flow reached its end. False if it was given up while
    /// parked, or parked and was told to end there.
    pub ended: bool,
    pub done: Vec<(String, String)>,
    /// The unit the pipeline was for.
    #[serde(default)]
    pub unit: String,
    /// Why it parked, if it ended there.
    #[serde(default)]
    pub parked: Option<String>,
}

/// A decision to go on with a parked pipeline.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GoOn {
    /// The step to continue from.
    pub from: String,
    /// What that step should know.
    pub note: String,
}

/// The engine. Its state is what it can be asked and told between steps.
#[workflow]
#[derive(Default)]
pub struct Pipeline {
    standing: Standing,
    go_on: Option<GoOn>,
    given_up: bool,
    /// The accounts workflow has said an account is back.
    account_ready: bool,
    /// What it says about itself to someone reading it on the server.
    showing: Showing,
}

/// The options of an activity that is over quickly, with what it is for.
fn quick(secs: u64, summary: String) -> ActivityOptions {
    ActivityOptions::with_start_to_close_timeout(Duration::from_secs(secs))
        .summary(describe::line(&summary))
        .build()
}

/// A timer, with what it waits for.
fn timer(secs: u64, summary: String) -> TimerOptions {
    TimerOptions::builder(Duration::from_secs(secs))
        .summary(describe::line(&summary))
        .build()
}

/// Say where the pipeline stands, for someone reading it on the server.
fn show(ctx: &WorkflowContext<Pipeline>, doing: String) {
    let details = ctx.state_mut(|state| {
        state.showing.doing = doing;
        describe::pipeline_details(&state.showing, &state.standing)
    });
    ctx.set_current_details(details);
}

/// A step and its round, as the details name them.
fn at(step: &Step, round: u32) -> String {
    if round > 1 {
        format!("step `{}`, round {round}", step.id)
    } else {
        format!("step `{}`", step.id)
    }
}

fn unix(time: Option<std::time::SystemTime>) -> u64 {
    time.and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

/// A definition the engine cannot run. Trying again would not help.
fn refused(reason: String) -> ApplicationFailure {
    ApplicationFailure::non_retryable(std::io::Error::other(reason))
}

#[workflow_methods]
impl Pipeline {
    #[run]
    pub async fn run(
        ctx: &mut WorkflowContext<Self>,
        input: PipelineInput,
    ) -> WorkflowResult<PipelineEnd> {
        let brief = |limit: u64, summary: String| {
            ActivityOptions::with_start_to_close_timeout(Duration::from_secs(limit))
                .heartbeat_timeout(Duration::from_secs(input.beat_secs.max(1) * 3))
                .summary(describe::line(&summary))
                .build()
        };
        ctx.state_mut(|state| {
            state.showing.project = input.project.clone();
            state.showing.bead = input.bead.clone();
        });
        // A bead with no unit yet gets one: its worktree and branch, and
        // its own copy of the flow, which is what this pipeline then runs.
        let mut taken_up = None;
        let (unit, definition, title) = if let Some(adopt) = &input.adopt {
            show(
                ctx,
                "taking the unit up where its record says it stands".to_string(),
            );
            let call = AdoptCall {
                unit: input.unit.clone(),
                from: adopt.from.clone(),
                note: adopt.note.clone(),
                ruling: adopt.ruling.clone(),
                flow_name: input.flow_name.clone(),
            };
            let how = match &adopt.from {
                Some(step) => format!("go on from step `{step}`"),
                None => "go on where it stands".to_string(),
            };
            let options = quick(
                600,
                format!(
                    "Take up unit `{}` from its record and {how}{}",
                    input.unit,
                    adopt
                        .note
                        .as_ref()
                        .map(|note| format!(": {note}"))
                        .unwrap_or_default()
                ),
            );
            match ctx.execute_activity(Steps::adopt, call, options).await {
                Ok(adopted) => {
                    ctx.state_mut(|state| {
                        state.showing.project = adopted.project.clone();
                        state.showing.bead = adopted.bead.clone();
                    });
                    taken_up = Some((adopted.position, adopted.bead));
                    (input.unit.clone(), adopted.flow, adopted.title)
                }
                Err(err) => {
                    let reason = err
                        .failure()
                        .and_then(|failure| failure.cause.as_ref())
                        .map(|cause| cause.message.clone())
                        .unwrap_or_else(|| err.to_string());
                    ctx.state_mut(|state| {
                        state.standing.unit = input.unit.clone();
                        state.standing.refused = Some(reason.clone());
                    });
                    show(ctx, String::new());
                    return Err(refused(reason).into());
                }
            }
        } else if input.unit.is_empty() {
            show(
                ctx,
                "making the bead ready: claiming it, giving it a worktree and a branch".to_string(),
            );
            let call = PrepareCall {
                project: input.project.clone(),
                bead: input.bead.clone(),
                flow_name: input.flow_name.clone(),
                within: input.within.clone(),
            };
            let options = quick(
                1800,
                format!(
                    "Make bead `{}` of `{}` ready: claim it, give it a worktree and a branch, pin its flow",
                    input.bead, input.project
                ),
            );
            match ctx.execute_activity(Steps::prepare, call, options).await {
                Ok(prepared) => (prepared.unit, prepared.flow, prepared.title),
                // Only a refusal comes back: anything else is tried again.
                // It is kept where someone who asks can read it, and the
                // pipeline ends.
                Err(err) => {
                    // What the driver said is under the activity's own
                    // failure, as its cause.
                    let reason = err
                        .failure()
                        .and_then(|failure| failure.cause.as_ref())
                        .map(|cause| cause.message.clone())
                        .unwrap_or_else(|| err.to_string());
                    ctx.state_mut(|state| state.standing.refused = Some(reason.clone()));
                    show(ctx, String::new());
                    return Err(refused(reason).into());
                }
            }
        } else {
            (input.unit.clone(), input.flow.clone(), String::new())
        };
        let (start, bead) = match taken_up {
            Some((position, bead)) => (Some(position), bead),
            None => (None, input.bead.clone()),
        };
        let about = describe::subject(&bead, &unit, &title);
        ctx.state_mut(|state| {
            state.standing.unit = unit.clone();
            state.showing.title = title.clone();
        });
        // Reading a definition is a pure function of its text, so it is the
        // same on every replay.
        let flow = Flow::parse(&definition).map_err(|err| refused(err.to_string()))?;
        ctx.state_mut(|state| state.showing.flow = flow.id.clone());
        let mut position = match start {
            Some(position) if flow.step(&position.step).is_some() => position,
            Some(position) => {
                return Err(refused(format!(
                    "unit `{unit}` stands at step `{}`, which its flow does not have",
                    position.step
                ))
                .into());
            }
            None => Position::start(&flow),
        };
        let mut note: Option<String> = None;
        // The session of a run that stopped at a usage limit.
        let mut session: Option<String> = None;
        loop {
            let step = flow
                .step(&position.step)
                .ok_or_else(|| refused(format!("the flow has no step `{}`", position.step)))?
                .clone();
            ctx.state_mut(|state| {
                state.standing.step = step.id.clone();
                state.standing.parked = None;
            });
            // An agent step runs under an account, and which one is not
            // its to choose: it asks, and waits here if there is none.
            let account = match &step.kind {
                StepKind::Agent { model, .. } if !input.accounts.is_empty() => {
                    let who = ctx.info().workflow_id().to_string();
                    loop {
                        ctx.state_mut(|state| state.account_ready = false);
                        let ask = AccountCall {
                            accounts: input.accounts.clone(),
                            who: who.clone(),
                            model: model.clone(),
                        };
                        show(
                            ctx,
                            format!(
                                "{}: asking which account it runs under",
                                at(&step, position.round())
                            ),
                        );
                        let options = quick(
                            60,
                            format!(
                                "Ask which account step `{}` ({model}) of {about} runs under",
                                step.id
                            ),
                        );
                        match ctx.execute_activity(Steps::account, ask, options).await? {
                            Lease::Use { account } => break Some(account),
                            Lease::Wait { secs } => {
                                // Nothing runs while it waits. The accounts
                                // workflow says when to ask again; the timer
                                // is only in case that word never comes.
                                ctx.state_mut(|state| state.standing.limited_secs = Some(secs));
                                let until = describe::clock(unix(ctx.workflow_time()) + secs);
                                show(
                                    ctx,
                                    format!(
                                        "{}: waiting for an account. None is free for {model}; the first is expected back in {}, at {until}",
                                        at(&step, position.round()),
                                        describe::span(secs)
                                    ),
                                );
                                let mut timer = ctx
                                    .timer(timer(
                                        secs + ASK_AGAIN_SECS,
                                        format!(
                                            "No account is free for step `{}` of {about}: ask again by {until} if the accounts workflow has not said one is back",
                                            step.id
                                        ),
                                    ))
                                    .fuse();
                                let mut told =
                                    ctx.wait_condition(|state| state.account_ready).fuse();
                                futures::select_biased! {
                                    heard = told => heard?,
                                    _ = timer => {}
                                }
                                ctx.state_mut(|state| state.standing.limited_secs = None);
                            }
                        }
                    }
                }
                _ => None,
            };
            let call = StepCall {
                unit: unit.clone(),
                step: step.id.clone(),
                round: position.round(),
                note: note.clone(),
                action: match &step.kind {
                    StepKind::Action { action } => Some(action.clone()),
                    _ => None,
                },
                session: session.take(),
                beat_secs: input.beat_secs.max(1),
                account: account.clone(),
                used: position.used.clone(),
            };
            // The step's own time limit is kept by whatever runs it. This
            // one is only the last resort. What notices a step that has
            // lost its worker is that it stops saying it is alive: it is
            // then given to another.
            let limit = step.timeout_secs.unwrap_or(0).max(6 * 3600) + 600;
            let summary = describe::step(&step, position.round(), account.as_deref(), &about);
            show(
                ctx,
                format!(
                    "{}: {}{}",
                    at(&step, position.round()),
                    describe::step_kind(&step),
                    account
                        .as_ref()
                        .map(|account| format!(", under account `{account}`"))
                        .unwrap_or_default()
                ),
            );
            let done: StepDone = match &step.kind {
                StepKind::Agent { .. } => {
                    ctx.execute_activity(Steps::agent, call, brief(limit, summary))
                        .await?
                }
                _ => {
                    ctx.execute_activity(Steps::plain, call, brief(limit, summary))
                        .await?
                }
            };
            if let (Some(secs), Some(account), StepKind::Agent { model, .. }) =
                (done.limited_secs, &account, &step.kind)
            {
                // The account it was handed is at a limit. The accounts
                // workflow is told, and the step asks it again: for
                // another account, or to wait. Its session is kept.
                let limit = Limit {
                    account: account.clone(),
                    model: model.clone(),
                    secs,
                };
                let options = SignalWorkflowOptions::builder()
                    .summary(describe::line(&format!(
                        "Account `{account}` stopped step `{}` of {about} at a usage limit on {model} that ends in {}",
                        step.id,
                        describe::span(secs)
                    )))
                    .build();
                ctx.external_workflow(input.accounts.clone(), None)
                    .signal(Accounts::limited, limit, options)
                    .await
                    .map_err(|err| refused(format!("the accounts could not be told: {err}")))?;
                session = done.session;
                continue;
            }
            if let Some(secs) = done.limited_secs {
                // Every account is at a limit. Nothing needs to run while
                // the factory waits: the timer is the server's.
                ctx.state_mut(|state| state.standing.limited_secs = Some(secs));
                let until = describe::clock(unix(ctx.workflow_time()) + secs);
                show(
                    ctx,
                    format!(
                        "{}: every account is at a usage limit. It goes on in its session in {}, at {until}",
                        at(&step, position.round()),
                        describe::span(secs)
                    ),
                );
                ctx.timer(timer(
                    secs,
                    format!(
                        "Every account is at a usage limit: step `{}` of {about} goes on in its session at {until}",
                        step.id
                    ),
                ))
                .await;
                ctx.state_mut(|state| state.standing.limited_secs = None);
                // The same step again, in the session it had.
                session = done.session;
                continue;
            }
            note = None;
            let reason = if let Some(reason) = done.parked {
                // The step parked the unit itself, and has recorded it.
                reason
            } else {
                ctx.state_mut(|state| {
                    state
                        .standing
                        .done
                        .push((step.id.clone(), done.outcome.clone()));
                });
                let next = match position.advance(&flow, &done.outcome) {
                    Ok(next) => next,
                    Err(err) => Next::Park(err.to_string()),
                };
                let reason = match next {
                    Next::Step(_) => continue,
                    Next::End => {
                        show(
                            ctx,
                            "ending the unit: every step of its flow is done".to_string(),
                        );
                        let summary = format!(
                            "End the unit of {about}: every step of flow `{}` is done",
                            flow.id
                        );
                        ctx.execute_activity(Steps::finish, unit.clone(), brief(600, summary))
                            .await?;
                        show(ctx, "ended: every step of its flow is done".to_string());
                        return Ok(PipelineEnd {
                            ended: true,
                            done: ctx.state(|state| state.standing.done.clone()),
                            unit: unit.clone(),
                            parked: None,
                        });
                    }
                    Next::Ask => format!("step `{}` needs the owner", step.id),
                    Next::Park(reason) => reason,
                };
                let call = ParkCall {
                    unit: unit.clone(),
                    reason: reason.clone(),
                };
                let summary = format!(
                    "Park {about} at step `{}` and tell whoever watches it: {}",
                    step.id,
                    reason.lines().next().unwrap_or_default()
                );
                ctx.execute_activity(Steps::park, call, brief(600, summary))
                    .await?;
                reason
            };
            // Parked: it waits, for as long as it takes and with nothing
            // running, until someone continues it or gives it up.
            ctx.state_mut(|state| {
                state.standing.parked = Some(reason.clone());
                state.showing.left_parked = input.stop_at_park;
            });
            show(ctx, String::new());
            if input.stop_at_park {
                // Whoever started it decides what happens to it now.
                return Ok(PipelineEnd {
                    ended: false,
                    done: ctx.state(|state| state.standing.done.clone()),
                    unit: unit.clone(),
                    parked: Some(reason),
                });
            }
            ctx.wait_condition(|state| state.go_on.is_some() || state.given_up)
                .await?;
            let Some(go_on) = ctx.state_mut(|state| state.go_on.take()) else {
                ctx.state_mut(|state| state.showing.given_up = true);
                show(ctx, String::new());
                return Ok(PipelineEnd {
                    ended: false,
                    done: ctx.state(|state| state.standing.done.clone()),
                    unit: unit.clone(),
                    parked: None,
                });
            };
            if flow.step(&go_on.from).is_none() {
                return Err(refused(format!("the flow has no step `{}`", go_on.from)).into());
            }
            position.step = go_on.from.clone();
            *position.rounds.entry(go_on.from).or_insert(0) += 1;
            // Whoever continued it judged it worth going on with: the
            // step it goes on from can be left once more. A route used up
            // elsewhere stays used up.
            position.reopen(&flow);
            note = Some(go_on.note);
        }
    }

    /// Go on with a parked pipeline, from a step and with a note for it.
    #[signal]
    pub fn go_on(&mut self, _ctx: &mut SyncWorkflowContext<Self>, decision: GoOn) {
        self.go_on = Some(decision);
    }

    /// An account is back: a pipeline waiting for one asks again.
    #[signal]
    pub fn account_ready(&mut self, _ctx: &mut SyncWorkflowContext<Self>, _nothing: ()) {
        self.account_ready = true;
    }

    /// Give a parked pipeline up.
    #[signal]
    pub fn give_up(&mut self, _ctx: &mut SyncWorkflowContext<Self>, _nothing: ()) {
        self.given_up = true;
    }

    /// Where the pipeline stands.
    #[query]
    pub fn standing(&self, _ctx: &WorkflowContextView) -> Standing {
        self.standing.clone()
    }
}
