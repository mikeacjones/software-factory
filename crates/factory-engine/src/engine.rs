//! The engine: one workflow that runs any flow.

use std::time::Duration;

use factory_flow::{Flow, Next, Position, StepKind};
use serde::{Deserialize, Serialize};
use temporalio_macros::{workflow, workflow_methods};
use temporalio_sdk::{
    ActivityOptions, ApplicationFailure, SyncWorkflowContext, WorkflowContext, WorkflowContextView,
    WorkflowResult,
};

use crate::steps::{StepCall, StepDone, Steps};

/// What a pipeline is started with.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PipelineInput {
    /// The unit the pipeline is for.
    pub unit: String,
    /// The flow definition, as the text of its file. It is part of the
    /// workflow from here on: the file may change and this pipeline will not.
    pub flow: String,
    /// How often an agent step says it is alive, and so how soon a step
    /// whose worker died is given to another. Seconds.
    pub beat_secs: u64,
}

/// Where a pipeline stands, for anyone who asks.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Standing {
    /// The step it is in, or was in when it stopped.
    pub step: String,
    /// Why it is parked, if it is. A parked pipeline waits to be continued.
    pub parked: Option<String>,
    /// When it is waiting for a usage limit to lift: for how long, in seconds.
    pub limited_secs: Option<u64>,
    /// The steps that have ended, in order, each with its outcome.
    pub done: Vec<(String, String)>,
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
        // Reading a definition is a pure function of its text, so it is the
        // same on every replay.
        let flow = Flow::parse(&input.flow).map_err(|err| refused(err.to_string()))?;
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
            let call = StepCall {
                unit: input.unit.clone(),
                step: step.id.clone(),
                round: position.round(),
                note: note.clone(),
                action: match &step.kind {
                    StepKind::Action { action } => Some(action.clone()),
                    _ => None,
                },
                session: session.take(),
                beat_secs: input.beat_secs.max(1),
            };
            let limit = Duration::from_secs(step.timeout_secs.unwrap_or(3600));
            let done: StepDone = match &step.kind {
                StepKind::Agent { .. } => {
                    // A step that stops saying it is alive has lost its
                    // worker, and is given to another.
                    let options = ActivityOptions::with_start_to_close_timeout(limit)
                        .heartbeat_timeout(Duration::from_secs(input.beat_secs.max(1) * 3))
                        .build();
                    ctx.execute_activity(Steps::agent, call, options).await?
                }
                _ => {
                    let options = ActivityOptions::with_start_to_close_timeout(limit).build();
                    ctx.execute_activity(Steps::plain, call, options).await?
                }
            };
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
                    return Ok(PipelineEnd {
                        ended: true,
                        done: ctx.state(|state| state.standing.done.clone()),
                    });
                }
                Next::Ask => format!("step `{}` needs the owner", step.id),
                Next::Park(reason) => reason,
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
            // routes it had used up are open again.
            position.used.clear();
            note = Some(go_on.note);
        }
    }

    /// Go on with a parked pipeline, from a step and with a note for it.
    #[signal]
    pub fn go_on(&mut self, _ctx: &mut SyncWorkflowContext<Self>, decision: GoOn) {
        self.go_on = Some(decision);
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
