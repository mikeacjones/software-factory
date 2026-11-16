//! Issues follow beads, on a schedule.
//!
//! The factory gives a bead a GitHub issue when it files the bead and when
//! a pipeline starts on it. A bead filed any other way, by the owner or by
//! another agent with `bd create`, would otherwise have none until some
//! goal took it up. A Temporal Schedule per project starts [`IssueSync`]
//! every so often, and its one activity sweeps the project's open beads:
//! a bead with no issue gets one, and each issue's type and priority
//! follow its bead's.
//!
//! The schedule is the worker's to keep. Each time a worker starts it sees
//! that every project that keeps issues has one, as it was meant to be: one
//! that is already right is left alone, paused or not, and one that is not
//! is made again.

use std::time::Duration;

use factory_driver::IssueSweep;
use serde::{Deserialize, Serialize};
use temporalio_client::grpc::WorkflowService;
use temporalio_client::schedules::{ScheduleError, ScheduleIntervalSpec};
use temporalio_client::tonic::{Code, IntoRequest};
use temporalio_client::{Client, NamespacedClient};
use temporalio_common::WorkflowDefinition;
use temporalio_common::data_converters::{SerializationContextData, WorkflowSerializationContext};
use temporalio_common::protos::temporal::api::common::v1::WorkflowType;
use temporalio_common::protos::temporal::api::enums::v1::ScheduleOverlapPolicy;
use temporalio_common::protos::temporal::api::schedule::v1::{
    IntervalSpec, Schedule, ScheduleAction, SchedulePolicies, ScheduleSpec, ScheduleState,
    schedule_action,
};
use temporalio_common::protos::temporal::api::sdk::v1::UserMetadata;
use temporalio_common::protos::temporal::api::taskqueue::v1::TaskQueue;
use temporalio_common::protos::temporal::api::workflow::v1::NewWorkflowExecutionInfo;
use temporalio_common::protos::temporal::api::workflowservice::v1::CreateScheduleRequest;
use temporalio_macros::{workflow, workflow_methods};
use temporalio_sdk::{ActivityOptions, ApplicationFailure, WorkflowContext, WorkflowResult};
use temporalio_workflow::RetryPolicy;

use crate::describe;
use crate::steps::{IssuesCall, Steps};

/// The ID of a project's schedule. The workflows it starts are named after
/// it, with the time each was started.
pub fn schedule_id(project: &str) -> String {
    format!("issues-{project}")
}

/// What each run of a project's sweep is started with.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueSyncInput {
    pub project: String,
    /// How often the sweep says it is alive, in seconds.
    pub beat_secs: u64,
}

/// One sweep of a project's issues.
#[workflow]
#[derive(Default)]
pub struct IssueSync {}

#[workflow_methods]
impl IssueSync {
    /// Sweep once and say what it did. A run in which some bead could not
    /// be brought into step fails, naming each, so that it stands out
    /// among the schedule's runs; the next run tries them again.
    #[run]
    pub async fn run(
        ctx: &mut WorkflowContext<Self>,
        input: IssueSyncInput,
    ) -> WorkflowResult<IssueSweep> {
        let beat = input.beat_secs.max(1);
        ctx.set_current_details(format!(
            "Sweeping the open beads of `{}` against their GitHub issues.",
            input.project
        ));
        // A worker that dies in the middle hands the sweep to another. Each
        // bead is read again just before its issue is made, so a sweep run
        // again does not make a second one.
        let options = ActivityOptions::with_start_to_close_timeout(Duration::from_secs(3600))
            .heartbeat_timeout(Duration::from_secs(beat * 3))
            .retry_policy(
                RetryPolicy::builder()
                    .maximum_attempts(3)
                    .maximum_interval(Duration::from_secs(60))
                    .build(),
            )
            .summary(describe::line(&format!(
                "Project `{}`: give each open bead without a GitHub issue one, and bring every issue's type and priority into step",
                input.project
            )))
            .build();
        let call = IssuesCall {
            project: input.project.clone(),
            beat_secs: beat,
        };
        let sweep = ctx.execute_activity(Steps::issues, call, options).await?;
        ctx.set_current_details(describe::issue_sweep_details(&input.project, &sweep));
        if !sweep.failed.is_empty() {
            return Err(
                ApplicationFailure::non_retryable(std::io::Error::other(format!(
                    "{} of the open beads of `{}` could not be brought into step: {}",
                    sweep.failed.len(),
                    input.project,
                    sweep.failed.join("; ")
                )))
                .into(),
            );
        }
        Ok(sweep)
    }
}

/// The schedule a project's sweep is to have.
async fn wanted(
    client: &Client,
    task_queue: &str,
    project: &str,
    every_secs: u64,
    beat_secs: u64,
) -> Result<Schedule, String> {
    let converter = client.data_converter();
    let context = SerializationContextData::Workflow(WorkflowSerializationContext::new());
    let input = IssueSyncInput {
        project: project.to_string(),
        beat_secs,
    };
    let encode = |what: &str, err: &dyn std::fmt::Display| format!("the schedule's {what}: {err}");
    let input = converter
        .to_payloads(&context, &input)
        .await
        .map_err(|err| encode("input", &err))?;
    let summary = converter
        .to_payload(
            &context,
            &describe::line(&format!(
                "Project `{project}`: give each open bead without a GitHub issue one, and bring every issue's type and priority into step"
            )),
        )
        .await
        .map_err(|err| encode("summary", &err))?;
    let details = converter
        .to_payload(&context, &describe::issue_sync_static(project, every_secs))
        .await
        .map_err(|err| encode("details", &err))?;
    // The client turns a duration into the server's own type only by way
    // of an interval.
    let proto =
        |secs: u64| IntervalSpec::from(ScheduleIntervalSpec::from(Duration::from_secs(secs)));
    let interval = proto(every_secs.max(1));
    // The server holds a catch-up window to ten seconds at least.
    let catchup_window = proto(every_secs.max(60)).interval;
    let run_limit = proto(2 * 3600).interval;
    Ok(Schedule {
        spec: Some(ScheduleSpec {
            interval: vec![interval],
            ..ScheduleSpec::default()
        }),
        action: Some(ScheduleAction {
            action: Some(schedule_action::Action::StartWorkflow(
                NewWorkflowExecutionInfo {
                    workflow_id: schedule_id(project),
                    workflow_type: Some(WorkflowType {
                        name: IssueSync::run.name().to_string(),
                    }),
                    task_queue: Some(TaskQueue {
                        name: task_queue.to_string(),
                        ..TaskQueue::default()
                    }),
                    input: Some(
                        temporalio_common::protos::temporal::api::common::v1::Payloads {
                            payloads: input,
                        },
                    ),
                    // A sweep that hangs would otherwise hold back every
                    // run after it, since a run is skipped while one goes.
                    workflow_execution_timeout: run_limit,
                    user_metadata: Some(UserMetadata {
                        summary: Some(summary),
                        details: Some(details),
                    }),
                    ..NewWorkflowExecutionInfo::default()
                },
            )),
        }),
        policies: Some(SchedulePolicies {
            overlap_policy: ScheduleOverlapPolicy::Skip as i32,
            // A worker away for a while makes up one sweep, not one per
            // interval it missed.
            catchup_window,
            ..SchedulePolicies::default()
        }),
        state: Some(ScheduleState {
            notes: format!(
                "Kept by the factory's worker, which makes it again when it is not as below: every {} on queue `{task_queue}`.",
                describe::span(every_secs.max(1))
            ),
            ..ScheduleState::default()
        }),
    })
}

/// What makes a schedule do what it does, as the server keeps it: the
/// server fills in some fields of its own, such as the kind of a task
/// queue, and those are not compared. Its state, such as being paused, is
/// the owner's and is not compared either.
type Shape = (
    Vec<i64>,
    String,
    String,
    Vec<Vec<u8>>,
    Option<Vec<u8>>,
    i32,
    Option<i64>,
    Option<i64>,
);

fn shape(schedule: &Schedule) -> Option<Shape> {
    let intervals = schedule
        .spec
        .as_ref()?
        .interval
        .iter()
        .map(|spec| spec.interval.as_ref().map_or(0, |every| every.seconds))
        .collect();
    let Some(schedule_action::Action::StartWorkflow(start)) =
        schedule.action.as_ref()?.action.as_ref()
    else {
        return None;
    };
    let policies = schedule.policies.unwrap_or_default();
    Some((
        intervals,
        start.workflow_type.as_ref()?.name.clone(),
        start.task_queue.as_ref()?.name.clone(),
        start
            .input
            .as_ref()
            .map(|input| {
                input
                    .payloads
                    .iter()
                    .map(|payload| payload.data.clone())
                    .collect()
            })
            .unwrap_or_default(),
        start
            .user_metadata
            .as_ref()
            .and_then(|meta| meta.summary.as_ref())
            .map(|summary| summary.data.clone()),
        policies.overlap_policy,
        policies.catchup_window.map(|window| window.seconds),
        start
            .workflow_execution_timeout
            .as_ref()
            .map(|limit| limit.seconds),
    ))
}

/// See that a project has its sweep's schedule, as this worker would make
/// it. Returns whether it was made or made again.
pub async fn ensure_schedule(
    client: &Client,
    task_queue: &str,
    project: &str,
    every_secs: u64,
    beat_secs: u64,
) -> Result<bool, String> {
    let id = schedule_id(project);
    let schedule = wanted(client, task_queue, project, every_secs, beat_secs).await?;
    let handle = client.get_schedule_handle(id.clone());
    match handle.describe(Default::default()).await {
        Ok(description) => {
            if description
                .raw()
                .schedule
                .as_ref()
                .is_some_and(|there| shape(there).is_some() && shape(there) == shape(&schedule))
            {
                return Ok(false);
            }
            handle
                .delete(Default::default())
                .await
                .map_err(|err| format!("the schedule `{id}` could not be replaced: {err}"))?;
        }
        Err(ScheduleError::Rpc(status)) if status.code() == Code::NotFound => {}
        Err(err) => return Err(format!("the schedule `{id}` could not be read: {err}")),
    }
    let request = CreateScheduleRequest {
        namespace: client.namespace(),
        schedule_id: id.clone(),
        schedule: Some(schedule),
        identity: client.identity(),
        // The server takes a create with a request ID it has seen for a
        // retry of that one, and makes nothing: one made again needs its own.
        request_id: format!(
            "{id}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_nanos())
        ),
        ..CreateScheduleRequest::default()
    };
    WorkflowService::create_schedule(&mut client.clone(), request.into_request())
        .await
        .map_err(|err| format!("the schedule `{id}` could not be made: {err}"))?;
    Ok(true)
}
