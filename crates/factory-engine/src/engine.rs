//! The engine: one workflow that runs any flow.

use std::time::Duration;

use factory_flow::{Flow, Next, Position, StepKind};
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use temporalio_macros::{workflow, workflow_methods};
use temporalio_sdk::{
    ActivityOptions, ApplicationFailure, SignalWorkflowOptions, SyncWorkflowContext,
    WorkflowContext, WorkflowContextView, WorkflowResult,
};

use crate::accounts::{AccountCall, Accounts, Lease, Limit};
use crate::steps::{ParkCall, PrepareCall, StepCall, StepDone, Steps};

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
    /// True if the flow reached its end. False if it was given up while parked.
    pub ended: bool,
    pub done: Vec<(String, String)>,
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
        let brief = |limit: u64| {
            ActivityOptions::with_start_to_close_timeout(Duration::from_secs(limit))
                .heartbeat_timeout(Duration::from_secs(input.beat_secs.max(1) * 3))
                .build()
        };
        // A bead with no unit yet gets one: its worktree and branch, and
        // its own copy of the flow, which is what this pipeline then runs.
        let (unit, definition) = if input.unit.is_empty() {
            let call = PrepareCall {
                project: input.project.clone(),
                bead: input.bead.clone(),
                flow_name: input.flow_name.clone(),
            };
            let options =
                ActivityOptions::with_start_to_close_timeout(Duration::from_secs(1800)).build();
            match ctx.execute_activity(Steps::prepare, call, options).await {
                Ok(prepared) => (prepared.unit, prepared.flow),
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
                    return Err(refused(reason).into());
                }
            }
        } else {
            (input.unit.clone(), input.flow.clone())
        };
        ctx.state_mut(|state| state.standing.unit = unit.clone());
        // Reading a definition is a pure function of its text, so it is the
        // same on every replay.
        let flow = Flow::parse(&definition).map_err(|err| refused(err.to_string()))?;
        let mut position = Position::start(&flow);
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
                        let options =
                            ActivityOptions::with_start_to_close_timeout(Duration::from_secs(60))
                                .build();
                        match ctx.execute_activity(Steps::account, ask, options).await? {
                            Lease::Use { account } => break Some(account),
                            Lease::Wait { secs } => {
                                // Nothing runs while it waits. The accounts
                                // workflow says when to ask again; the timer
                                // is only in case that word never comes.
                                ctx.state_mut(|state| state.standing.limited_secs = Some(secs));
                                let mut timer =
                                    ctx.timer(Duration::from_secs(secs + ASK_AGAIN_SECS)).fuse();
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
            };
            // The step's own time limit is kept by whatever runs it. This
            // one is only the last resort. What notices a step that has
            // lost its worker is that it stops saying it is alive: it is
            // then given to another.
            let limit = step.timeout_secs.unwrap_or(0).max(6 * 3600) + 600;
            let done: StepDone = match &step.kind {
                StepKind::Agent { .. } => {
                    ctx.execute_activity(Steps::agent, call, brief(limit))
                        .await?
                }
                _ => {
                    ctx.execute_activity(Steps::plain, call, brief(limit))
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
                ctx.external_workflow(input.accounts.clone(), None)
                    .signal(Accounts::limited, limit, SignalWorkflowOptions::default())
                    .await
                    .map_err(|err| refused(format!("the accounts could not be told: {err}")))?;
                session = done.session;
                continue;
            }
            if let Some(secs) = done.limited_secs {
                // Every account is at a limit. Nothing needs to run while
                // the factory waits: the timer is the server's.
                ctx.state_mut(|state| state.standing.limited_secs = Some(secs));
                ctx.timer(Duration::from_secs(secs)).await;
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
                        ctx.execute_activity(Steps::finish, unit.clone(), brief(600))
                            .await?;
                        return Ok(PipelineEnd {
                            ended: true,
                            done: ctx.state(|state| state.standing.done.clone()),
                        });
                    }
                    Next::Ask => format!("step `{}` needs the owner", step.id),
                    Next::Park(reason) => reason,
                };
                let call = ParkCall {
                    unit: unit.clone(),
                    reason: reason.clone(),
                };
                ctx.execute_activity(Steps::park, call, brief(600)).await?;
                reason
            };
            // Parked: it waits, for as long as it takes and with nothing
            // running, until someone continues it or gives it up.
            ctx.state_mut(|state| state.standing.parked = Some(reason));
            ctx.wait_condition(|state| state.go_on.is_some() || state.given_up)
                .await?;
            let Some(go_on) = ctx.state_mut(|state| state.go_on.take()) else {
                return Ok(PipelineEnd {
                    ended: false,
                    done: ctx.state(|state| state.standing.done.clone()),
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
