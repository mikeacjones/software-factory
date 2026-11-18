//! What the engine's tests share: a local Temporal server, a directory with
//! the scripted step, and the workers started against them.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use factory_engine::accounts::{ACCOUNTS_ID, Accounts, AccountsInput, Board, Picture};
use factory_engine::engine::PipelineInput;
use factory_engine::worker::{accounts_options, pipeline_options};
use temporalio_client::{
    Client, UntypedWorkflow, WorkflowFetchHistoryOptions, WorkflowQueryOptions,
    WorkflowStartOptions,
};
use temporalio_sdk::testing::{
    EphemeralExe, LocalServer, LocalWorkflowEnvironmentOptions, WorkflowEnvironment,
};

pub fn temporal_cli() -> Option<String> {
    std::env::var("PATH").ok()?.split(':').find_map(|dir| {
        let path = Path::new(dir).join("temporal");
        path.is_file().then(|| path.to_string_lossy().into_owned())
    })
}

/// A local server, a directory with the scripted step, and the workers
/// started against them, which die with the rig.
pub struct Rig {
    pub env: Option<WorkflowEnvironment<LocalServer>>,
    pub address: String,
    pub dir: PathBuf,
    pub queue: String,
    pub workers: Vec<Child>,
}

/// A rig dropped without `stop`, as when its test fails part way, still
/// ends its workers. Left running, they go on dialling their server's port,
/// and once another test's fake server is given that port they talk to it.
impl Drop for Rig {
    fn drop(&mut self) {
        for worker in &mut self.workers {
            let _ = worker.kill();
            let _ = worker.wait();
        }
    }
}

impl Rig {
    pub async fn start(name: &str) -> Option<Rig> {
        let Some(cli) = temporal_cli() else {
            eprintln!("no `temporal` CLI on the PATH: skipped");
            return None;
        };
        // Something else on the machine may be starting and stopping
        // servers of its own, and one of ours can then fail to come up.
        // That says nothing about the engine: it is started again.
        let mut tries = 0;
        let (env, port) = loop {
            let port = std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            let options = LocalWorkflowEnvironmentOptions::builder()
                .server_executable(EphemeralExe::ExistingPath(cli.clone()))
                .port(port)
                .startup_timeout(Duration::from_secs(30))
                .build();
            tries += 1;
            match WorkflowEnvironment::start_local(options).await {
                Ok(env) => break (env, port),
                Err(err) if tries < 3 => eprintln!("the test server did not start: {err}"),
                Err(err) => panic!("the test server did not start, three times: {err}"),
            }
        };
        let dir =
            std::env::temp_dir().join(format!("factory-engine-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("state")).unwrap();
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/step.sh"),
            dir.join("step.sh"),
        )
        .unwrap();
        Some(Rig {
            env: Some(env),
            address: format!("http://127.0.0.1:{port}"),
            dir,
            queue: format!("factory-{name}"),
            workers: Vec::new(),
        })
    }

    pub fn client(&self) -> Client {
        self.env.as_ref().unwrap().client().clone()
    }

    pub fn scenario(&self, words: &str) {
        std::fs::write(self.dir.join("state/scenario"), words).unwrap();
    }

    /// Start a worker process. Returns its place among the rig's workers.
    pub fn worker(&mut self, build: &str, mode: &str) -> usize {
        let child = Command::new(env!("CARGO_BIN_EXE_factory-engine-worker"))
            .args([
                self.address.as_str(),
                "default",
                self.queue.as_str(),
                "factory-test",
                build,
                mode,
            ])
            .arg(self.dir.join("step.sh"))
            .env("FACTORY_BUILD", build)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        self.workers.push(child);
        self.workers.len() - 1
    }

    /// Kill a worker where it stands, as a crash would.
    pub fn kill(&mut self, index: usize) {
        let _ = self.workers[index].kill();
        let _ = self.workers[index].wait();
    }

    pub fn input(&self, unit: &str, flow: &str) -> PipelineInput {
        PipelineInput {
            unit: unit.to_string(),
            flow: flow.to_string(),
            beat_secs: 1,
            ..PipelineInput::default()
        }
    }

    /// What the scripted accounts have left, for the next check to find.
    pub fn picture(&self, picture: &Picture) {
        std::fs::write(
            self.dir.join("state/picture.json"),
            serde_json::to_string(picture).unwrap(),
        )
        .unwrap();
    }

    /// Start the accounts workflow.
    pub async fn accounts(&self, check_secs: u64, checks_per_run: u32) {
        let input = AccountsInput {
            check_secs,
            checks_per_run,
            ..AccountsInput::default()
        };
        self.client()
            .start_workflow(Accounts::run, input, accounts_options(&self.queue))
            .await
            .unwrap();
    }

    pub async fn board(&self) -> Board {
        self.client()
            .get_workflow_handle::<Accounts>(ACCOUNTS_ID.to_string())
            .query(Accounts::board, (), WorkflowQueryOptions::default())
            .await
            .unwrap()
    }

    /// A pipeline whose agent steps ask the accounts workflow for an account.
    pub fn input_under_accounts(&self, unit: &str, flow: &str) -> PipelineInput {
        PipelineInput {
            accounts: ACCOUNTS_ID.to_string(),
            ..self.input(unit, flow)
        }
    }

    /// What a pipeline is started with, as the factory starts one: with
    /// what it says about itself.
    pub fn options(&self, id: &str, input: &PipelineInput) -> WorkflowStartOptions {
        pipeline_options(&self.queue, id, input, "A bead of the toy project")
    }

    /// Fail unless everything in a workflow's history says what it is
    /// for: the workflow itself, and every activity, timer and signal it
    /// started. Someone reading it on the server has only that to go by.
    pub async fn assert_described(&self, workflow: &str) {
        self.described(workflow, true).await;
    }

    /// The same, for a workflow a test started bare: only what the
    /// workflow started itself is looked at.
    pub async fn assert_steps_described(&self, workflow: &str) {
        self.described(workflow, false).await;
    }

    async fn described(&self, workflow: &str, from_its_start: bool) {
        let json = self
            .client()
            .get_workflow_handle::<UntypedWorkflow>(workflow.to_string())
            .fetch_history(WorkflowFetchHistoryOptions::default())
            .to_json()
            .await
            .unwrap();
        let history: serde_json::Value = serde_json::from_slice(&json).unwrap();
        let events = history["events"].as_array().unwrap();
        let mut seen = 0;
        for event in events {
            // The kind of an event is the name of its attributes.
            let kind = event["attributes"]
                .as_object()
                .and_then(|attributes| attributes.keys().next().cloned())
                .unwrap_or_default();
            let start = kind.starts_with("WorkflowExecutionStarted");
            let started = (start && from_its_start)
                || [
                    "ActivityTaskScheduled",
                    "TimerStarted",
                    "SignalExternalWorkflowExecutionInitiated",
                ]
                .iter()
                .any(|name| kind.starts_with(name));
            if !started {
                continue;
            }
            seen += 1;
            let summary = &event["user_metadata"]["summary"]["data"];
            assert!(
                summary.as_str().is_some_and(|data| !data.is_empty())
                    || summary.as_array().is_some_and(|data| !data.is_empty()),
                "`{workflow}` started something that does not say what it is for: {event}"
            );
        }
        assert!(seen > 1, "`{workflow}` started nothing: {history}");
    }

    /// How many times the accounts were checked.
    pub fn checks(&self) -> usize {
        std::fs::read_to_string(self.dir.join("state/checks"))
            .unwrap_or_default()
            .lines()
            .count()
    }

    /// What the step script was asked to do, in order, without the times.
    pub fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.join("state/calls"))
            .unwrap_or_default()
            .lines()
            .map(|line| {
                line.split_once(' ')
                    .map_or(line, |(_, rest)| rest)
                    .to_string()
            })
            .collect()
    }

    pub fn wait_for(&self, what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !done() {
            assert!(Instant::now() < deadline, "{what}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Run the `temporal` CLI against the rig's server. Returns whether it
    /// succeeded and what it printed.
    pub fn temporal(&self, args: &[&str]) -> (bool, String) {
        let out = Command::new(temporal_cli().unwrap())
            .args(args)
            .args(["--address", self.address.trim_start_matches("http://")])
            .output()
            .unwrap();
        (
            out.status.success(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
        )
    }

    /// Make a build the current one of the test's deployment, once the
    /// server has heard from a worker of that build.
    pub fn make_current(&self, build: &str) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let (ok, said) = self.temporal(&[
                "worker",
                "deployment",
                "set-current-version",
                "--deployment-name",
                "factory-test",
                "--build-id",
                build,
                "--yes",
            ]);
            if ok {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "build {build} never became current: {said}"
            );
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    pub async fn stop(mut self) {
        for worker in &mut self.workers {
            let _ = worker.kill();
            let _ = worker.wait();
        }
        if let Ok(pid) = std::fs::read_to_string(self.dir.join("state/hung.pid")) {
            let _ = Command::new("/bin/kill")
                .args(["-KILL", pid.trim()])
                .status();
        }
        if let Some(env) = self.env.take() {
            env.shutdown().await.unwrap();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
