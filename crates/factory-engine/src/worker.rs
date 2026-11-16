//! A worker: the process that runs the engine and the steps.

use std::str::FromStr;
use std::sync::Arc;

use temporalio_client::{
    Client, ClientOptions, Connection, ConnectionOptions, Url, WorkflowIdConflictPolicy,
    WorkflowStartOptions,
};
use temporalio_common::worker::{
    VersioningBehavior, WorkerDeploymentOptions, WorkerDeploymentVersion,
};
use temporalio_sdk::{Runtime, Worker, WorkerOptions};

use crate::accounts::{ACCOUNTS_ID, Accounts, AccountsInput};
use crate::describe;
use crate::engine::{Pipeline, PipelineInput};
use crate::issues::IssueSync;
use crate::steps::{Gate, StepRunner, Steps};
use crate::tiers::{INTAKE_RUN_SECS, Intake, IntakeInput, Project, Reach, intake_options};

/// Where a worker connects and what it calls itself.
#[derive(Clone, Debug)]
pub struct WorkerSettings {
    /// The server, such as `http://192.168.101.240:7233`.
    pub address: String,
    pub namespace: String,
    pub task_queue: String,
    /// The deployment this worker is one build of.
    pub deployment: String,
    /// This build. With `versioned`, a pipeline started on a build stays
    /// on it to its end, whatever build is current by then.
    pub build_id: String,
    /// Take part in worker versioning: the server gives this worker only
    /// the pipelines that belong to its build.
    pub versioned: bool,
}

/// Connect to the server.
pub async fn connect(address: &str, namespace: &str) -> Result<Client, String> {
    let url = Url::from_str(address).map_err(|err| format!("`{address}`: {err}"))?;
    let connection = Connection::connect(ConnectionOptions::new(url).build())
        .await
        .map_err(|err| format!("could not connect to {address}: {err}"))?;
    Client::new(connection, ClientOptions::new(namespace).build())
        .map_err(|err| format!("could not make a client: {err}"))
}

/// What the accounts workflow is started with: its ID, and what it says
/// about itself. One that is already running is left as it is.
pub fn accounts_options(task_queue: &str) -> WorkflowStartOptions {
    WorkflowStartOptions::new(task_queue.to_string(), ACCOUNTS_ID.to_string())
        .id_conflict_policy(WorkflowIdConflictPolicy::UseExisting)
        .static_summary(
            "Says which Claude account each agent step runs under, and holds steps while none is free"
                .to_string(),
        )
        .static_details(describe::accounts_static())
        .build()
}

/// What a pipeline is started with: its ID, and what it says about itself
/// before it has run. `title` is its bead's title, if that is known.
pub fn pipeline_options(
    task_queue: &str,
    id: &str,
    input: &PipelineInput,
    title: &str,
) -> WorkflowStartOptions {
    let about = describe::subject(&input.bead, &input.unit, title);
    let summary = if input.bead.is_empty() {
        format!("Unit {about}: take it through its flow")
    } else {
        format!(
            "Bead {about} of project `{}`: take it through its flow",
            input.project
        )
    };
    WorkflowStartOptions::new(task_queue.to_string(), id.to_string())
        .static_summary(describe::line(&summary))
        .static_details(describe::pipeline_static(
            &input.project,
            &input.bead,
            &input.unit,
            input.flow_name.as_deref(),
        ))
        .build()
}

/// See that the accounts workflow is running. One that is already running
/// is left as it is, with what it knows.
pub async fn ensure_accounts(
    client: &Client,
    task_queue: &str,
    check_secs: u64,
) -> Result<(), String> {
    let options = accounts_options(task_queue);
    let input = AccountsInput {
        check_secs,
        ..AccountsInput::default()
    };
    client
        .start_workflow(Accounts::run, input, options)
        .await
        .map(|_| ())
        .map_err(|err| format!("the accounts workflow could not be started: {err}"))
}

/// See that the tiers are on the engine: the intake, which listens and
/// watches, and the workflow of each project named. Those that are
/// running already are left as they are.
pub async fn ensure_tiers(
    client: &Client,
    reach: &Reach,
    projects: &[String],
) -> Result<(), String> {
    for project in projects {
        crate::tiers::ensure_project(client, reach, project).await?;
    }
    let input = IntakeInput {
        reach: reach.clone(),
        run_secs: INTAKE_RUN_SECS,
    };
    client
        .start_workflow(Intake::run, input, intake_options(&reach.task_queue))
        .await
        .map(|_| ())
        .map_err(|err| format!("the intake workflow could not be started: {err}"))
}

/// Run a worker until it is told to stop, or for ever.
///
/// It stops at a safe point when its runner says the worker is to go:
/// it takes on nothing new, lets every step and turn it is in the middle
/// of finish, and then returns. What it did not take on waits on the
/// server for the worker that starts next.
pub async fn run(settings: WorkerSettings, runner: Arc<dyn StepRunner>) -> Result<(), String> {
    let runtime = Runtime::from_current_tokio(Default::default())
        .map_err(|err| format!("the runtime could not be set up: {err}"))?;
    let client = connect(&settings.address, &settings.namespace).await?;
    let version = WorkerDeploymentVersion::builder()
        .deployment_name(settings.deployment.clone())
        .build_id(settings.build_id.clone())
        .build();
    let deployment = if settings.versioned {
        // A pipeline is hours long at most and is finished by the build
        // that started it: an engine change never lands in the middle of one.
        WorkerDeploymentOptions::new(version)
            .use_worker_versioning(true)
            .default_versioning_behavior(VersioningBehavior::Pinned)
            .build()
    } else {
        WorkerDeploymentOptions::new(version).build()
    };
    let gate = Arc::new(Gate::default());
    let options = WorkerOptions::new(settings.task_queue.clone())
        .deployment_options(deployment)
        .register_activities(Steps::with_gate(runner.clone(), gate.clone()))
        .register_workflow::<Pipeline>()
        .map_err(|err| format!("the engine could not be registered: {err}"))?
        .register_workflow::<Accounts>()
        .map_err(|err| format!("the accounts could not be registered: {err}"))?
        .register_workflow::<Project>()
        .map_err(|err| format!("the projects could not be registered: {err}"))?
        .register_workflow::<Intake>()
        .map_err(|err| format!("the intake could not be registered: {err}"))?
        .register_workflow::<IssueSync>()
        .map_err(|err| format!("the issue sweep could not be registered: {err}"))?
        .build();
    let mut worker = Worker::new(&runtime, client, options)
        .map_err(|err| format!("the worker could not be made: {err}"))?;
    let stop = worker.shutdown_handle();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            if !runner.draining() {
                continue;
            }
            // Nothing new is taken on from here.
            stop();
            while gate.busy() > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            // What only listens and watches ends too, and with it the
            // worker's run.
            gate.leave();
            return;
        }
    });
    worker
        .run()
        .await
        .map_err(|err| format!("the worker stopped: {err}"))
}
