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
use crate::engine::Pipeline;
use crate::steps::{StepRunner, Steps};

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

/// See that the accounts workflow is running. One that is already running
/// is left as it is, with what it knows.
pub async fn ensure_accounts(
    client: &Client,
    task_queue: &str,
    check_secs: u64,
) -> Result<(), String> {
    let options = WorkflowStartOptions::new(task_queue.to_string(), ACCOUNTS_ID.to_string())
        .id_conflict_policy(WorkflowIdConflictPolicy::UseExisting)
        .build();
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

/// Run a worker until it is told to stop, or for ever.
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
    let options = WorkerOptions::new(settings.task_queue.clone())
        .deployment_options(deployment)
        .register_activities(Steps::new(runner))
        .register_workflow::<Pipeline>()
        .map_err(|err| format!("the engine could not be registered: {err}"))?
        .register_workflow::<Accounts>()
        .map_err(|err| format!("the accounts could not be registered: {err}"))?
        .build();
    let mut worker = Worker::new(&runtime, client, options)
        .map_err(|err| format!("the worker could not be made: {err}"))?;
    worker
        .run()
        .await
        .map_err(|err| format!("the worker stopped: {err}"))
}
