//! A real pipeline under the engine: the factory's own driver does every
//! step, on a real repository with a bare remote, with scripted stand-ins
//! for the `claude` CLI, and the engine on a local Temporal server walks
//! the flow. A test that finds no `temporal` CLI says so and passes.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[path = "../../factory-driver/tests/common/world.rs"]
mod world;

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use factory_engine::accounts::{ACCOUNTS_ID, Accounts};
use factory_engine::engine::{Adopt, GoOn, Pipeline, PipelineInput};
use temporalio_client::{
    Client, WorkflowFetchHistoryOptions, WorkflowGetResultOptions, WorkflowQueryOptions,
    WorkflowSignalOptions, WorkflowStartOptions,
};
use temporalio_sdk::testing::{
    EphemeralExe, LocalServer, LocalWorkflowEnvironmentOptions, WorkflowEnvironment,
};
use world::{World, run};

fn temporal_cli() -> Option<String> {
    std::env::var("PATH").ok()?.split(':').find_map(|dir| {
        let path = Path::new(dir).join("temporal");
        path.is_file().then(|| path.to_string_lossy().into_owned())
    })
}

/// A project in a box, a local server, and a worker of the real engine
/// command pointed at both.
struct Shop {
    world: World,
    env: Option<WorkflowEnvironment<LocalServer>>,
    worker: std::sync::Mutex<Child>,
}

/// A shop dropped without `close`, as when its test fails part way, still
/// ends its worker, which would otherwise go on dialling its server's port.
impl Drop for Shop {
    fn drop(&mut self) {
        let mut worker = self
            .worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = worker.kill();
        let _ = worker.wait();
    }
}

impl Shop {
    async fn open(name: &str, test_command: &str, scenario: &str) -> Option<Shop> {
        Shop::open_after(name, test_command, scenario, |_| {}).await
    }

    /// Open a shop whose world `prepare` has changed first, before the
    /// worker reads its config.
    async fn open_after(
        name: &str,
        test_command: &str,
        scenario: &str,
        prepare: impl FnOnce(&World),
    ) -> Option<Shop> {
        Shop::open_with(name, test_command, scenario, prepare, "").await
    }

    /// The same, with more said in the factory's `[temporal]` table.
    async fn open_with(
        name: &str,
        test_command: &str,
        scenario: &str,
        prepare: impl FnOnce(&World),
        temporal: &str,
    ) -> Option<Shop> {
        let Some(cli) = temporal_cli() else {
            eprintln!("no `temporal` CLI on the PATH: skipped");
            return None;
        };
        let world = World::new(name, test_command);
        prepare(&world);
        if !scenario.is_empty() {
            world.scenario(scenario);
        }
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let options = LocalWorkflowEnvironmentOptions::builder()
            .server_executable(EphemeralExe::ExistingPath(cli))
            .port(port)
            .startup_timeout(Duration::from_secs(30))
            .build();
        let env = WorkflowEnvironment::start_local(options).await.unwrap();
        // The factory is told where its engine's server is.
        let factory = world.path("config/factory.toml");
        let text = std::fs::read_to_string(&factory).unwrap();
        std::fs::write(
            &factory,
            format!(
                "{text}\n[temporal]\naddress = \"http://127.0.0.1:{port}\"\nnamespace = \"default\"\ntask_queue = \"factory-{name}\"\nbeat_secs = 1\n{temporal}"
            ),
        )
        .unwrap();
        let worker = std::sync::Mutex::new(Shop::worker(&world));
        Some(Shop {
            world,
            env: Some(env),
            worker,
        })
    }

    /// Start a worker of the real engine command on the world's config.
    fn worker(world: &World) -> Child {
        Command::new(env!("CARGO_BIN_EXE_factory-engine"))
            .args(["worker", "--config"])
            .arg(world.path("config"))
            .stdout(Stdio::null())
            .stderr(if std::env::var_os("FACTORY_TEST_WORKER_LOG").is_some() {
                Stdio::inherit()
            } else {
                Stdio::null()
            })
            .spawn()
            .unwrap()
    }

    /// Kill the worker where it stands, as a crash would, and start another.
    fn replace_worker(&self) {
        let mut worker = self.worker.lock().unwrap();
        let _ = worker.kill();
        let _ = worker.wait();
        *worker = Shop::worker(&self.world);
    }

    fn client(&self) -> Client {
        self.env.as_ref().unwrap().client().clone()
    }

    fn start(&self, name: &str) -> (PipelineInput, temporalio_client::WorkflowStartOptions) {
        (
            PipelineInput {
                project: "toy".to_string(),
                bead: "toy-1".to_string(),
                beat_secs: 1,
                ..PipelineInput::default()
            },
            WorkflowStartOptions::new(format!("factory-{name}"), format!("pipeline-{name}"))
                .build(),
        )
    }

    async fn close(mut self) {
        {
            let mut worker = self.worker.lock().unwrap();
            let _ = worker.kill();
            let _ = worker.wait();
        }
        if let Some(env) = self.env.take() {
            env.shutdown().await.unwrap();
        }
    }
}

/// Fail unless everything a workflow started says what it is for: the
/// workflow itself, and every activity, timer, signal and child workflow.
/// Returns how many of those there were.
async fn assert_described(client: &Client, workflow: &str) -> usize {
    let json = client
        .get_workflow_handle::<temporalio_client::UntypedWorkflow>(workflow.to_string())
        .fetch_history(WorkflowFetchHistoryOptions::default())
        .to_json()
        .await
        .unwrap();
    let history: serde_json::Value = serde_json::from_slice(&json).unwrap();
    let mut seen = 0;
    for event in history["events"].as_array().unwrap() {
        let kind = event["attributes"]
            .as_object()
            .and_then(|attributes| attributes.keys().next().cloned())
            .unwrap_or_default();
        if ![
            "WorkflowExecutionStarted",
            "ActivityTaskScheduled",
            "TimerStarted",
            "SignalExternalWorkflowExecutionInitiated",
            "StartChildWorkflowExecutionInitiated",
        ]
        .iter()
        .any(|name| kind.starts_with(name))
        {
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
    seen
}

/// A goal's record, as it stands on disk.
fn goal_records(world: &World) -> Vec<factory_driver::goal::Goal> {
    std::fs::read_dir(world.path("state/goals"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| std::fs::read_to_string(entry.path().join("goal.json")).ok())
        .filter_map(|text| serde_json::from_str(&text).ok())
        .collect()
}

fn unit_record(world: &World, unit: &str) -> serde_json::Value {
    let text =
        std::fs::read_to_string(world.path(&format!("state/units/{unit}/unit.json"))).unwrap();
    serde_json::from_str(&text).unwrap()
}

/// What deploy asks of a release's engine, `factory-engine check`, is
/// asked over the stored goal definitions as well as the release's: one
/// stored for a pipeline the release no longer has makes it fail, and so
/// the release is refused. It needs no server.
#[test]
fn the_engines_check_fails_on_a_stored_definition_the_release_cannot_run() {
    let world = World::new("engine-check-stored", "true");
    let factory = world.path("config/factory.toml");
    let text = std::fs::read_to_string(&factory).unwrap();
    std::fs::write(
        &factory,
        format!(
            "{text}\n[temporal]\naddress = \"http://127.0.0.1:1\"\nnamespace = \"default\"\ntask_queue = \"factory-engine-check-stored\"\n"
        ),
    )
    .unwrap();
    std::fs::create_dir_all(world.path("config/goals/toy")).unwrap();
    std::fs::write(
        world.path("config/goals/toy/main.toml"),
        "id = \"main\"\n[director]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\n",
    )
    .unwrap();
    let flow = std::fs::read_to_string(world.path("config/flows/build-break-review.toml"))
        .unwrap()
        .replace("id = \"build-break-review\"", "id = \"careful\"");
    std::fs::write(world.path("config/flows/careful.toml"), flow).unwrap();
    factory_driver::goal::enable_definition(
        &world.config(),
        "toy",
        "id = \"main\"\nflow = \"careful\"\n[director]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\n",
    )
    .unwrap();
    let check = || {
        Command::new(env!("CARGO_BIN_EXE_factory-engine"))
            .args(["check", "--config"])
            .arg(world.path("config"))
            .output()
            .unwrap()
    };
    let passed = check();
    let said = String::from_utf8_lossy(&passed.stdout).into_owned();
    assert!(passed.status.success(), "{passed:?}");
    assert!(
        said.contains("toy/main: director") && said.contains("pipeline `careful`"),
        "{said}"
    );

    std::fs::remove_file(world.path("config/flows/careful.toml")).unwrap();
    let failed = check();
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&failed.stdout),
        String::from_utf8_lossy(&failed.stderr)
    );
    assert!(!failed.status.success(), "{said}");
    assert!(
        said.contains("goal definition `main` of `toy`: its pipeline cannot be used"),
        "{said}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bead_that_may_not_be_launched_ends_its_pipeline_at_once_and_says_why() {
    let Some(shop) = Shop::open("engine-refused", "true", "").await else {
        return;
    };
    // `toy-2` is someone else's: the driver will not start on it.
    let (mut input, options) = shop.start("engine-refused");
    input.bead = "toy-2".to_string();
    let client = shop.client();
    let handle = client
        .start_workflow(Pipeline::run, input, options)
        .await
        .unwrap();
    // It ends, where it once tried again for ever.
    let ended = tokio::time::timeout(
        Duration::from_secs(60),
        handle.get_result(WorkflowGetResultOptions::default()),
    )
    .await
    .expect("the pipeline went on trying to prepare a bead it had been refused");
    assert!(ended.is_err(), "{ended:?}");
    // Whoever asks is told why, and no unit was made.
    let standing = handle
        .query(Pipeline::standing, (), WorkflowQueryOptions::default())
        .await
        .unwrap();
    let reason = standing.refused.unwrap_or_default();
    assert!(reason.contains("may not be launched"), "{reason}");
    assert!(reason.contains("it is claimed by `someone`"), "{reason}");
    assert_eq!(standing.unit, "");
    // A turn that asks for a pipeline the server has, running or over,
    // is not made to ask for it again: only the other one is started.
    let asked = vec![
        serde_json::json!({"id": "pipeline-engine-refused", "bead": "toy-2"}),
        serde_json::json!({"id": "pipeline-engine-never", "bead": "toy-3"}),
    ];
    let wanted = factory_engine::steps::not_yet_there(&client, asked).await;
    assert_eq!(wanted.len(), 1, "{wanted:?}");
    assert_eq!(wanted[0]["bead"], "toy-3");
    // The driver was asked once.
    let json = handle
        .fetch_history(WorkflowFetchHistoryOptions::default())
        .to_json()
        .await
        .unwrap();
    let history = String::from_utf8_lossy(&json).into_owned();
    assert_eq!(
        history.matches("ActivityTaskScheduled").count(),
        1,
        "{history}"
    );
    assert!(
        !shop.world.path("state/units").exists() || {
            std::fs::read_dir(shop.world.path("state/units"))
                .unwrap()
                .count()
                == 0
        }
    );
    shop.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_real_bead_is_built_broken_fixed_reviewed_and_published_under_the_engine() {
    let Some(shop) = Shop::open("engine-full", "test -f feature.txt", "").await else {
        return;
    };
    let (input, options) = shop.start("engine-full");
    let handle = shop
        .client()
        .start_workflow(Pipeline::run, input, options)
        .await
        .unwrap();
    let end = handle
        .get_result(WorkflowGetResultOptions::default())
        .await
        .unwrap();
    assert!(end.ended);
    let steps: Vec<String> = end
        .done
        .iter()
        .map(|(step, outcome)| format!("{step}:{outcome}"))
        .collect();
    // The engine routed the flow the unit was pinned to, and the driver did
    // each step: the tester broke the change, the builder fixed it, and the
    // second time it held.
    assert_eq!(steps.first().map(String::as_str), Some("build:done"));
    assert_eq!(steps.last().map(String::as_str), Some("publish:done"));
    let breaks: Vec<&String> = steps
        .iter()
        .filter(|step| step.starts_with("break:"))
        .collect();
    assert_eq!(breaks, ["break:broken", "break:held"], "{steps:?}");
    assert!(
        steps.contains(&"fix:done".to_string()) && steps.contains(&"review:approve".to_string())
    );

    // The unit is the factory's own: its record says it ended, its branch
    // is on the remote with the work in it, and the bead is still claimed.
    let standing = handle
        .query(Pipeline::standing, (), WorkflowQueryOptions::default())
        .await
        .unwrap();
    assert!(standing.unit.starts_with("toy-1-"), "{standing:?}");
    let unit = unit_record(&shop.world, &standing.unit);
    assert_eq!(unit["status"], "ended");
    let branch = unit["branch"].as_str().unwrap().to_string();
    let remote = shop.world.path("remote.git");
    assert!(!run(&remote, "git", &["show", &format!("{branch}:feature.txt")]).is_empty());
    assert_eq!(
        run(
            &remote,
            "git",
            &["rev-list", "--count", &format!("main..{branch}")]
        ),
        "1",
        "one commit for the bead"
    );
    // Each agent ran as the driver runs it: the fix continued the
    // builder's session, and the tester and reviewer had fresh ones.
    let calls = shop.world.calls();
    let builder: Vec<&Vec<String>> = calls.iter().filter(|call| call[0] == "builder").collect();
    assert_eq!(builder.len(), 2, "{calls:?}");
    assert_eq!(
        builder[0][1], builder[1][1],
        "the fix is in the build's session"
    );
    assert_eq!(calls.iter().filter(|call| call[0] == "tester").count(), 2);
    // The unit's own log has every step, ended by the engine's routing.
    let log = std::fs::read_to_string(
        shop.world
            .path(&format!("state/units/{}/events.jsonl", standing.unit)),
    )
    .unwrap();
    assert_eq!(
        log.matches("\"kind\":\"step.ended\"").count(),
        steps.len(),
        "one record for each step the engine counted"
    );
    assert!(log.contains("\"next\":\"the engine decides\""));
    assert!(log.contains("\"kind\":\"unit.ended\""));
    assert!(
        !log.contains("\"kind\":\"unit.resumed\""),
        "being picked up for a step is not news"
    );
    shop.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_real_pipeline_that_needs_the_owner_parks_and_goes_on_with_the_answer() {
    // The builder asks a question the first time it runs.
    let Some(shop) = Shop::open("engine-ask", "true", "ask-once").await else {
        return;
    };
    let (input, options) = shop.start("engine-ask");
    let handle = shop
        .client()
        .start_workflow(Pipeline::run, input, options)
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(90);
    let standing = loop {
        let standing = handle
            .query(Pipeline::standing, (), WorkflowQueryOptions::default())
            .await
            .unwrap();
        if standing.parked.is_some() {
            break standing;
        }
        assert!(Instant::now() < deadline, "it never parked: {standing:?}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    // The pipeline waits, and the factory's record of the unit says so
    // too, with the question, where the rest of the factory will look.
    assert_eq!(standing.step, "build");
    assert_eq!(
        standing.parked.as_deref(),
        Some("step `build` needs the owner")
    );
    let unit = unit_record(&shop.world, &standing.unit);
    let parked = unit["status"]["parked"].as_str().unwrap();
    assert!(
        parked.starts_with("step `build` needs the owner"),
        "{parked}"
    );

    // The answer comes as a decision to go on, and reaches the builder.
    handle
        .signal(
            Pipeline::go_on,
            GoOn {
                from: "build".to_string(),
                note: "No shouting: use the short name.".to_string(),
            },
            WorkflowSignalOptions::default(),
        )
        .await
        .unwrap();
    let end = handle
        .get_result(WorkflowGetResultOptions::default())
        .await
        .unwrap();
    assert!(end.ended);
    assert_eq!(
        end.done.first().unwrap(),
        &("build".to_string(), "needs_owner".to_string())
    );
    assert_eq!(
        end.done.last().unwrap(),
        &("publish".to_string(), "done".to_string())
    );
    let calls = shop.world.calls();
    let second = (0..calls.len())
        .filter(|&i| calls[i][0] == "builder")
        .nth(1)
        .unwrap();
    let brief = std::fs::read_to_string(
        shop.world
            .path(&format!("bin/state/brief-{}.md", second + 1)),
    )
    .unwrap();
    assert!(
        brief.contains("No shouting: use the short name."),
        "{brief}"
    );
    assert_eq!(unit_record(&shop.world, &standing.unit)["status"], "ended");
    shop.close().await;
}

/// A pipeline that a goal runs does not wait when it parks: it ends, and
/// says why. Going on is a new pipeline that takes the unit up from the
/// factory's own record of it.
#[tokio::test(flavor = "multi_thread")]
async fn a_pipeline_run_for_a_goal_ends_where_it_parks_and_another_takes_the_unit_up() {
    let Some(shop) = Shop::open("engine-adopt", "true", "ask-once").await else {
        return;
    };
    let (mut input, options) = shop.start("engine-adopt");
    input.stop_at_park = true;
    let end = shop
        .client()
        .start_workflow(Pipeline::run, input, options)
        .await
        .unwrap()
        .get_result(WorkflowGetResultOptions::default())
        .await
        .unwrap();
    assert!(!end.ended);
    assert_eq!(end.parked.as_deref(), Some("step `build` needs the owner"));
    assert!(!end.unit.is_empty());
    let unit = unit_record(&shop.world, &end.unit);
    assert!(unit["status"]["parked"].is_string(), "{unit}");

    // Whoever runs the sub-goal continues it: from a step, with a note
    // for that step and a ruling for every step after it.
    let again = PipelineInput {
        unit: end.unit.clone(),
        beat_secs: 1,
        stop_at_park: true,
        adopt: Some(Adopt {
            from: Some("build".to_string()),
            note: Some("No shouting: use the short name.".to_string()),
            ruling: Some("Names are lower case in this project.".to_string()),
        }),
        ..PipelineInput::default()
    };
    let options = factory_engine::worker::pipeline_options(
        "factory-engine-adopt",
        "pipeline-engine-adopt-2",
        &again,
        "",
    );
    let handle = shop
        .client()
        .start_workflow(Pipeline::run, again, options)
        .await
        .unwrap();
    let second = handle
        .get_result(WorkflowGetResultOptions::default())
        .await
        .unwrap();
    assert!(second.ended, "{second:?}");
    assert_eq!(second.unit, end.unit);
    assert_eq!(
        second.done.first().unwrap(),
        &("build".to_string(), "done".to_string())
    );
    assert_eq!(
        second.done.last().unwrap(),
        &("publish".to_string(), "done".to_string())
    );
    // It went on as the unit's second round of the step, and the record
    // carries the engine's count of the routes it has taken.
    let unit = unit_record(&shop.world, &end.unit);
    assert_eq!(unit["status"], "ended");
    assert_eq!(unit["position"]["rounds"]["build"], 2, "{unit}");
    assert_eq!(unit["rulings"][0], "Names are lower case in this project.");
    // The note reached the builder, and the ruling a later step too.
    let calls = shop.world.calls();
    let brief_of = |role: &str, nth: usize| {
        let at = (0..calls.len())
            .filter(|&i| calls[i][0] == role)
            .nth(nth)
            .unwrap();
        std::fs::read_to_string(shop.world.path(&format!("bin/state/brief-{}.md", at + 1))).unwrap()
    };
    let builder = brief_of("builder", 1);
    assert!(
        builder.contains("No shouting: use the short name."),
        "{builder}"
    );
    let reviewer = brief_of("reviewer", 0);
    assert!(
        reviewer.contains("Names are lower case in this project."),
        "{reviewer}"
    );
    shop.close().await;
}

/// A unit something else was walking is taken up exactly where its
/// record says it stands, with no step named: what the service's own
/// pipelines need when they are handed to the engine as they are.
#[tokio::test(flavor = "multi_thread")]
async fn a_unit_is_taken_up_where_it_stands_when_no_step_is_named() {
    let Some(shop) = Shop::open("engine-stands", "true", "ask-once").await else {
        return;
    };
    let (mut input, options) = shop.start("engine-stands");
    input.stop_at_park = true;
    let end = shop
        .client()
        .start_workflow(Pipeline::run, input, options)
        .await
        .unwrap()
        .get_result(WorkflowGetResultOptions::default())
        .await
        .unwrap();
    assert_eq!(end.parked.as_deref(), Some("step `build` needs the owner"));
    let before = unit_record(&shop.world, &end.unit);
    let again = PipelineInput {
        unit: end.unit.clone(),
        beat_secs: 1,
        stop_at_park: true,
        adopt: Some(Adopt::default()),
        ..PipelineInput::default()
    };
    let options = factory_engine::worker::pipeline_options(
        "factory-engine-stands",
        "pipeline-engine-stands-2",
        &again,
        "",
    );
    let second = shop
        .client()
        .start_workflow(Pipeline::run, again, options)
        .await
        .unwrap()
        .get_result(WorkflowGetResultOptions::default())
        .await
        .unwrap();
    assert!(second.ended, "{second:?}");
    // The same step, in the same round: nothing was counted twice.
    assert_eq!(second.done.first().unwrap().0, "build");
    let after = unit_record(&shop.world, &end.unit);
    assert_eq!(after["status"], "ended");
    assert_eq!(
        after["position"]["rounds"]["build"],
        before["position"]["rounds"]["build"]
    );
    shop.close().await;
}

/// A whole goal with its pipelines on the engine: the service's own
/// supervisor runs the director, and every pipeline it starts, the bead
/// inside the sub-goal and the sub-goal being shipped, is a workflow.
#[tokio::test(flavor = "multi_thread")]
async fn a_goals_bead_and_its_sub_goals_shipping_run_as_workflows_on_the_engine() {
    use factory_driver::goal::{Phase, Supervisor};
    use factory_slack::socket::Incoming;

    let prepare = |world: &World| {
        world.with_github("green");
        let project = world.path("config/projects/toy.toml");
        let text = std::fs::read_to_string(&project)
            .unwrap()
            .replace(
                "merge_subject = \"Merge bead/{bead_short}: {title}\"",
                "merge_method = \"squash\"\nmerge_subject = \"{title} (#{number})\"",
            )
            .replace(
                "merge_body = \"{summary}\\n\\n{testing}\\n\\nBead: {bead}\"",
                "merge_body = \"{commit_body}\\n\\n{testing}\\n\\n{commit_trailers}\"",
            );
        assert!(text.contains("merge_method = \"squash\""));
        std::fs::write(&project, text).unwrap();
        std::fs::create_dir_all(world.path("config/goals/toy")).unwrap();
        std::fs::write(
            world.path("config/goals/toy/main.toml"),
            "id = \"main\"\nflow = \"zig-sdk-bead\"\nship_flow = \"zig-sdk-ship\"\n[director]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\n",
        )
        .unwrap();
    };
    let Some(shop) = Shop::open_after("engine-goal", "true", "", prepare).await else {
        return;
    };
    let world = &shop.world;
    // Goals run their pipelines on the engine, through its own command.
    let factory = world.path("config/factory.toml");
    let text = std::fs::read_to_string(&factory).unwrap();
    std::fs::write(
        &factory,
        format!(
            "{text}pipelines = true\nengine_bin = \"{}\"\n",
            env!("CARGO_BIN_EXE_factory-engine")
        ),
    )
    .unwrap();
    let say = |role: &str, n: u32, json: String| {
        std::fs::create_dir_all(world.path("bin/state")).unwrap();
        std::fs::write(world.path(&format!("bin/state/{role}-{n}.json")), json).unwrap();
    };
    let decide = |fields: &str| {
        let mut result = serde_json::json!({
            "summary": "Decided.", "start": [], "file": [], "edit": [], "dropped_findings": [],
            "open_subgoal": {"id": "", "title": "", "objective": "", "beads": []}, "ship": false,
            "ask_owner": "", "complete": false, "evidence": "", "journal_entry": "", "project_memory": ""
        });
        let extra: serde_json::Value = serde_json::from_str(fields).unwrap();
        for (key, value) in extra.as_object().unwrap() {
            result[key] = value.clone();
        }
        result.to_string()
    };
    say("intake", 1, r#"{"reply":"Here is the charter.","ready":true,"objective":"Ship the feature.","stop_when":"The bead has landed.","plan":"One sub-goal: toy-1.","edits":[]}"#.to_string());
    say(
        "director",
        1,
        decide(
            r#"{"open_subgoal":{"id":"feature","title":"The feature","objective":"It works.","beads":["toy-1"]},"start":[{"bead":"toy-1","reason":"first"}]}"#,
        ),
    );
    say("director", 2, decide(r#"{"ship":true}"#));
    say(
        "director",
        3,
        decide(r#"{"complete":true,"evidence":"The bead landed in one pull request."}"#),
    );

    // The supervisor is not async: it runs on a thread of its own, as it
    // does in the service.
    let config = world.config();
    let goal = tokio::task::spawn_blocking(move || {
        let mut sup = Supervisor::new(config, None, vec!["OWNER".to_string()], false).unwrap();
        let id = sup.open_directly("toy", "Ship the feature").unwrap();
        sup.incoming(Incoming::Action {
            channel: String::new(),
            user: "OWNER".to_string(),
            action_id: "goal_start".to_string(),
            value: id.clone(),
            message_ts: String::new(),
        })
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(240);
        while sup.goals().next().unwrap().wakes < 3 {
            assert!(
                Instant::now() < deadline,
                "the goal never reached its third wake: {:?}",
                sup.goals().next().unwrap().units
            );
            sup.pump(Duration::from_millis(200)).unwrap();
        }
        // Nothing ran on a thread of the service.
        assert_eq!(sup.live(), 0);
        sup.goals().next().unwrap().clone()
    })
    .await
    .unwrap();

    assert_eq!(goal.phase, Phase::ProposedComplete);
    assert_eq!(goal.subgoals[0].state, "landed");
    assert!(goal.subgoals[0].pull_request.is_some());
    let states: Vec<(&str, &str)> = goal
        .units
        .iter()
        .map(|unit| (unit.bead.as_str(), unit.state.as_str()))
        .collect();
    assert_eq!(states, [("toy-1", "absorbed"), ("feature", "landed")]);
    // The bead was absorbed into the sub-goal's branch and opened no pull
    // request of its own; the sub-goal was shipped as one, and merged.
    let gh = world.gh_calls();
    assert_eq!(
        gh.iter()
            .filter(|call| call.starts_with("pr create"))
            .count(),
        1
    );
    assert_eq!(
        gh.iter()
            .filter(|call| call.starts_with("pr merge"))
            .count(),
        1
    );
    // Each was a workflow, named from the goal's record of it, that ran
    // to its end and says what everything in it was for.
    for (name, unit) in [("toy-1", &goal.units[0]), ("feature", &goal.units[1])] {
        let workflow = format!("pipeline-{}-{name}-1-0", goal.id);
        let end = shop
            .client()
            .get_workflow_handle::<Pipeline>(workflow.clone())
            .get_result(WorkflowGetResultOptions::default())
            .await
            .unwrap();
        assert!(end.ended, "{workflow}: {end:?}");
        assert_eq!(Some(&end.unit), unit.unit.as_ref());
        let record = unit_record(world, &end.unit);
        assert_eq!(record["status"], "ended");
    }
    shop.close().await;
}

/// The tiers on the engine: no supervisor runs anywhere. The worker sees
/// that the project's workflow and the intake are running; the goal is
/// opened, started and taken to its end by signals to the project's
/// workflow, each dealt with as one turn; the pipelines a turn asks for
/// are child workflows; and the project hears of each from the watcher.
#[tokio::test(flavor = "multi_thread")]
async fn a_goal_runs_from_its_opening_to_its_end_with_its_tiers_on_the_engine() {
    use factory_driver::goal::{Event, Phase};
    use factory_engine::tiers::{INTAKE_ID, Project, Reach, project_id, tell};
    use factory_slack::socket::Incoming;

    let prepare = |world: &World| {
        world.with_github("green");
        let project = world.path("config/projects/toy.toml");
        let text = std::fs::read_to_string(&project)
            .unwrap()
            .replace(
                "merge_subject = \"Merge bead/{bead_short}: {title}\"",
                "merge_method = \"squash\"\nmerge_subject = \"{title} (#{number})\"",
            )
            .replace(
                "merge_body = \"{summary}\\n\\n{testing}\\n\\nBead: {bead}\"",
                "merge_body = \"{commit_body}\\n\\n{testing}\\n\\n{commit_trailers}\"",
            );
        assert!(text.contains("merge_method = \"squash\""));
        std::fs::write(&project, text).unwrap();
        std::fs::create_dir_all(world.path("config/goals/toy")).unwrap();
        std::fs::write(
            world.path("config/goals/toy/main.toml"),
            "id = \"main\"\nflow = \"zig-sdk-bead\"\nship_flow = \"zig-sdk-ship\"\n[director]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\n",
        )
        .unwrap();
        let say = |role: &str, n: u32, json: String| {
            std::fs::create_dir_all(world.path("bin/state")).unwrap();
            std::fs::write(world.path(&format!("bin/state/{role}-{n}.json")), json).unwrap();
        };
        let decide = |fields: &str| {
            let mut result = serde_json::json!({
                "summary": "Decided.", "start": [], "file": [], "edit": [], "dropped_findings": [],
                "open_subgoal": {"id": "", "title": "", "objective": "", "beads": []}, "ship": false,
                "ask_owner": "", "complete": false, "evidence": "", "journal_entry": "", "project_memory": ""
            });
            let extra: serde_json::Value = serde_json::from_str(fields).unwrap();
            for (key, value) in extra.as_object().unwrap() {
                result[key] = value.clone();
            }
            result.to_string()
        };
        say("intake", 1, r#"{"reply":"Here is the charter.","ready":true,"objective":"Ship the feature.","stop_when":"The bead has landed.","plan":"One sub-goal: toy-1.","edits":[]}"#.to_string());
        say(
            "director",
            1,
            decide(
                r#"{"open_subgoal":{"id":"feature","title":"The feature","objective":"It works.","beads":["toy-1"]},"start":[{"bead":"toy-1","reason":"first"}]}"#,
            ),
        );
        say("director", 2, decide(r#"{"ship":true}"#));
        say(
            "director",
            3,
            decide(r#"{"complete":true,"evidence":"The bead landed in one pull request."}"#),
        );
    };
    let Some(shop) = Shop::open_with("engine-tiers", "true", "", prepare, "tiers = true\n").await
    else {
        return;
    };
    let world = &shop.world;
    let client = shop.client();
    let reach = Reach {
        task_queue: "factory-engine-tiers".to_string(),
        beat_secs: 1,
        accounts: ACCOUNTS_ID.to_string(),
    };
    let wait = |what: &str, done: &dyn Fn() -> bool| {
        let deadline = Instant::now() + Duration::from_secs(240);
        while !done() {
            assert!(
                Instant::now() < deadline,
                "{what}: {:?}",
                goal_records(world)
                    .iter()
                    .map(|goal| (goal.phase.clone(), goal.units.clone()))
                    .collect::<Vec<_>>()
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    };

    // The owner opens a goal, and presses the button that starts it.
    let open = Event::Open {
        project: "toy".to_string(),
        text: "Ship the feature".to_string(),
    };
    tell(&client, &reach, "toy", open).await.unwrap();
    wait("the goal was never opened", &|| {
        !goal_records(world).is_empty()
    });
    let id = goal_records(world)[0].id.clone();
    let press = Event::Slack(Incoming::Action {
        channel: String::new(),
        user: "OWNER".to_string(),
        action_id: "goal_start".to_string(),
        value: id.clone(),
        message_ts: String::new(),
    });
    tell(&client, &reach, "toy", press).await.unwrap();
    wait("the goal never reached its end", &|| {
        goal_records(world)[0].phase == Phase::ProposedComplete
    });

    let goal = goal_records(world).remove(0);
    assert_eq!(goal.subgoals[0].state, "landed");
    assert!(goal.subgoals[0].pull_request.is_some());
    let states: Vec<(&str, &str)> = goal
        .units
        .iter()
        .map(|unit| (unit.bead.as_str(), unit.state.as_str()))
        .collect();
    assert_eq!(states, [("toy-1", "absorbed"), ("feature", "landed")]);
    assert_eq!(goal.wakes, 3);
    let gh = world.gh_calls();
    for call in ["pr create", "pr merge"] {
        assert_eq!(gh.iter().filter(|said| said.starts_with(call)).count(), 1);
    }
    // Each pipeline was a workflow the project's own started, named from
    // the goal's record of it, and ran to its end.
    for (name, unit) in [("toy-1", &goal.units[0]), ("feature", &goal.units[1])] {
        let workflow = format!("pipeline-{}-{name}-1-0", goal.id);
        let end = client
            .get_workflow_handle::<Pipeline>(workflow.clone())
            .get_result(WorkflowGetResultOptions::default())
            .await
            .unwrap();
        assert!(end.ended, "{workflow}: {end:?}");
        assert_eq!(Some(&end.unit), unit.unit.as_ref());
        assert_eq!(unit_record(world, &end.unit)["status"], "ended");
        assert!(assert_described(&client, &workflow).await > 3);
    }
    // The project's workflow took a turn for each thing that happened,
    // and says of every one what it was for. So does the intake.
    let standing = client
        .get_workflow_handle::<Project>(project_id("toy"))
        .query(Project::standing, (), WorkflowQueryOptions::default())
        .await
        .unwrap();
    assert_eq!(standing.trouble, None, "{standing:?}");
    assert_eq!(standing.waiting, 0, "{standing:?}");
    let lately = standing.lately.join("\n");
    for expected in [
        "pick up what was running before",
        "open a goal for the owner: “Ship the feature”",
        "the owner pressed `goal_start`",
        "the pipeline for `toy-1`",
        "→ started `toy-1`",
        "→ started `feature`",
        "the shipping of sub-goal `feature`",
    ] {
        assert!(
            lately.contains(expected),
            "missing {expected:?} in:\n{lately}"
        );
    }
    assert!(assert_described(&client, &project_id("toy")).await > 8);
    assert_eq!(assert_described(&client, INTAKE_ID).await, 3);
    // Nothing in what the project did stands as failed.
    let json = client
        .get_workflow_handle::<Project>(project_id("toy"))
        .fetch_history(WorkflowFetchHistoryOptions::default())
        .to_json()
        .await
        .unwrap();
    let history = String::from_utf8_lossy(&json).into_owned();
    for red in ["Failed", "TimedOut"] {
        assert!(!history.contains(red), "{red} in:\n{history}");
    }

    // Asked to stop at a safe point, the worker leaves by itself: the
    // listener and the watcher, which never end otherwise, do not hold it.
    std::fs::write(factory_driver::drain_flag(&shop.world.path("state")), "").unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    let left = loop {
        if let Some(status) = shop.worker.lock().unwrap().try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "the worker never left");
        std::thread::sleep(Duration::from_millis(200));
    };
    assert!(left.success(), "the worker left as {left}");
    shop.close().await;
}

/// The watcher says a pipeline never started once the server has not
/// known it for a while, and the project hears that only when its turn
/// comes. The turn before may be the one that started it: one turn that
/// runs agents can take far longer than the watcher waits. A report that
/// a pipeline never started, of one the server knows by the time the
/// turn is taken, leaves the goal as it is; of one the server still does
/// not know, it is handed to the goal as before.
#[tokio::test(flavor = "multi_thread")]
async fn a_goal_is_not_told_that_a_pipeline_the_server_knows_never_started() {
    use factory_driver::goal::Event;
    use factory_engine::steps::NEVER_STARTED;
    use factory_engine::tiers::{Project, Reach, project_id, tell};
    use factory_slack::socket::Incoming;

    let prepare = |world: &World| {
        world.with_github("green");
        std::fs::create_dir_all(world.path("config/goals/toy")).unwrap();
        std::fs::write(
            world.path("config/goals/toy/main.toml"),
            "id = \"main\"\nflow = \"zig-sdk-bead\"\nship_flow = \"zig-sdk-ship\"\n[director]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\n",
        )
        .unwrap();
        let decide = |fields: &str| {
            let mut result = serde_json::json!({
                "summary": "Decided.", "start": [], "file": [], "edit": [], "dropped_findings": [],
                "open_subgoal": {"id": "", "title": "", "objective": "", "beads": []}, "ship": false,
                "ask_owner": "", "complete": false, "evidence": "", "journal_entry": "", "project_memory": ""
            });
            let extra: serde_json::Value = serde_json::from_str(fields).unwrap();
            for (key, value) in extra.as_object().unwrap() {
                result[key] = value.clone();
            }
            result.to_string()
        };
        let say = |role: &str, n: u32, json: String| {
            std::fs::create_dir_all(world.path("bin/state")).unwrap();
            std::fs::write(world.path(&format!("bin/state/{role}-{n}.json")), json).unwrap();
        };
        say("intake", 1, r#"{"reply":"Here is the charter.","ready":true,"objective":"Ship the feature.","stop_when":"The bead has landed.","plan":"One sub-goal: toy-1.","edits":[]}"#.to_string());
        say(
            "director",
            1,
            decide(
                r#"{"open_subgoal":{"id":"feature","title":"The feature","objective":"It works.","beads":["toy-1"]},"start":[{"bead":"toy-1","reason":"first"}]}"#,
            ),
        );
        // What the director says if it is woken again.
        say("director", 2, decide("{}"));
    };
    // The builder holds the bead's pipeline running until it is let go.
    let Some(shop) = Shop::open_with(
        "engine-late",
        "true",
        "hold-builder",
        prepare,
        "tiers = true\n",
    )
    .await
    else {
        return;
    };
    let world = &shop.world;
    let client = shop.client();
    let reach = Reach {
        task_queue: "factory-engine-late".to_string(),
        beat_secs: 1,
        accounts: ACCOUNTS_ID.to_string(),
    };
    let wait = |what: &str, done: &dyn Fn() -> bool| {
        let deadline = Instant::now() + Duration::from_secs(120);
        while !done() {
            assert!(
                Instant::now() < deadline,
                "{what}: {:?}",
                goal_records(world)
                    .iter()
                    .map(|goal| (goal.phase.clone(), goal.units.clone()))
                    .collect::<Vec<_>>()
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    };
    let called = |role: &str| world.calls().iter().filter(|call| call[0] == role).count();
    let toy = || {
        goal_records(world)[0]
            .units
            .iter()
            .find(|unit| unit.bead == "toy-1")
            .cloned()
            .unwrap()
    };
    // What the goal's record says of its pipelines, and what its
    // director and foremen have still to hear.
    let told = || {
        let goal = serde_json::to_value(goal_records(world).remove(0)).unwrap();
        serde_json::json!({
            "phase": goal["phase"],
            "units": goal["units"],
            "subgoals": goal["subgoals"],
            "inbox": goal["inbox"],
            "failures": goal["failures"],
        })
    };
    // The turns that dealt with a report that a pipeline failed, once
    // each is over.
    let failed_turns = || async {
        let standing = client
            .get_workflow_handle::<Project>(project_id("toy"))
            .query(Project::standing, (), WorkflowQueryOptions::default())
            .await
            .unwrap();
        let turns = standing
            .lately
            .iter()
            .filter(|line| line.contains("the pipeline for `toy-1`") && line.contains("it failed"))
            .count();
        (turns, standing)
    };
    let turned = |count: usize| async move {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let (turns, standing) = failed_turns().await;
            if turns >= count {
                return standing;
            }
            assert!(
                Instant::now() < deadline,
                "the report was never dealt with: {standing:?}"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    };

    // The goal is opened and started; its director starts the bead, and
    // the bead's pipeline runs on the server.
    let open = Event::Open {
        project: "toy".to_string(),
        text: "Ship the feature".to_string(),
    };
    tell(&client, &reach, "toy", open).await.unwrap();
    wait("the goal was never opened", &|| {
        !goal_records(world).is_empty()
    });
    let id = goal_records(world)[0].id.clone();
    let press = Event::Slack(Incoming::Action {
        channel: String::new(),
        user: "OWNER".to_string(),
        action_id: "goal_start".to_string(),
        value: id.clone(),
        message_ts: String::new(),
    });
    tell(&client, &reach, "toy", press).await.unwrap();
    wait("the bead's pipeline never ran its builder", &|| {
        called("builder") > 0
    });
    // The watcher has told the goal the pipeline's unit, so nothing else
    // is on its way to the record.
    wait("the goal never heard the pipeline's unit", &|| {
        toy().unit.is_some()
    });
    let name = format!("pipeline-{id}-toy-1-1-0");
    client
        .get_workflow_handle::<Pipeline>(name.clone())
        .describe(temporalio_client::WorkflowDescribeOptions::default())
        .await
        .unwrap();
    assert_eq!(toy().state, "running");
    let before = told();

    // What the watcher says of a pipeline the server does not know.
    let never_started = |name: String| Event::Ended {
        goal: id.clone(),
        bead: "toy-1".to_string(),
        ship: false,
        unit: None,
        word: "failed".to_string(),
        rest: NEVER_STARTED.to_string(),
        name,
    };

    // Said of the pipeline that runs, as the watcher said it during the
    // turn that started it: the turn only looks at the records.
    tell(&client, &reach, "toy", never_started(name.clone()))
        .await
        .unwrap();
    let standing = turned(1).await;
    assert_eq!(standing.trouble, None, "{standing:?}");
    assert_eq!(told(), before, "the goal was told");
    assert_eq!(called("director"), 1, "the director was woken");

    // Said of a pipeline the server has never known, the goal is told.
    tell(
        &client,
        &reach,
        "toy",
        never_started(format!("pipeline-{id}-toy-1-9-9")),
    )
    .await
    .unwrap();
    let standing = turned(2).await;
    assert_eq!(standing.trouble, None, "{standing:?}");
    let unit = toy();
    assert_eq!(unit.state, "failed", "{unit:?}");
    assert_eq!(unit.detail, NEVER_STARTED);

    std::fs::write(world.path("bin/state/go-on"), "").unwrap();
    shop.close().await;
}

/// What the owner writes while every account is at its limit is not
/// dropped and need not be written again: the project's workflow keeps
/// it and hands it in again when an account is back.
#[tokio::test(flavor = "multi_thread")]
async fn what_the_owner_writes_while_no_account_is_free_is_answered_when_one_is() {
    use factory_driver::goal::Event;
    use factory_engine::tiers::{Project, Reach, project_id, tell};

    let until = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 12;
    let prepare = |world: &World| {
        world.with_github("green");
        let project = world.path("config/projects/toy.toml");
        let text = std::fs::read_to_string(&project).unwrap()
            + "\n[executive]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\nslots = 2\n";
        std::fs::write(&project, text).unwrap();
        std::fs::create_dir_all(world.path("config/goals/toy")).unwrap();
        std::fs::write(
            world.path("config/goals/toy/main.toml"),
            "id = \"main\"\nflow = \"zig-sdk-bead\"\nship_flow = \"zig-sdk-ship\"\n[director]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\n",
        )
        .unwrap();
        // One account, at its limit for a few seconds more.
        let factory = world.path("config/factory.toml");
        let text = std::fs::read_to_string(&factory).unwrap();
        std::fs::write(
            &factory,
            format!("limit_grace_secs = 0\n{text}\n[[account]]\nid = \"a\"\n"),
        )
        .unwrap();
        std::fs::create_dir_all(world.path("state")).unwrap();
        std::fs::write(
            world.path("state/accounts.json"),
            serde_json::json!({"a": {"blocked": {"*": until}, "updated_at": until - 12, "source": "test"}})
                .to_string(),
        )
        .unwrap();
        std::fs::create_dir_all(world.path("bin/state")).unwrap();
        std::fs::write(
            world.path("bin/state/executive-1.json"),
            r#"{"reply":"Nothing is running.","open_goal":{"definition":"","handoff":""}}"#,
        )
        .unwrap();
    };
    let Some(shop) = Shop::open_with("engine-waits", "true", "", prepare, "tiers = true\n").await
    else {
        return;
    };
    let world = &shop.world;
    let client = shop.client();
    let reach = Reach {
        task_queue: "factory-engine-waits".to_string(),
        beat_secs: 1,
        accounts: ACCOUNTS_ID.to_string(),
    };
    let asked = || {
        world
            .calls()
            .iter()
            .filter(|call| call[0] == "executive")
            .count()
    };
    let standing = || async {
        client
            .get_workflow_handle::<Project>(project_id("toy"))
            .query(Project::standing, (), WorkflowQueryOptions::default())
            .await
            .unwrap()
    };
    let said = || -> Vec<(String, String)> {
        std::fs::read_to_string(world.path("state/projects/toy/executive.json"))
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .and_then(|executive| {
                serde_json::from_value(executive["threads"]["7.7"]["said"].clone()).ok()
            })
            .unwrap_or_default()
    };

    let write = Event::Tell {
        project: "toy".to_string(),
        thread: "7.7".to_string(),
        text: "What is running?".to_string(),
    };
    tell(&client, &reach, "toy", write).await.unwrap();
    // The turn is over, nobody was asked, and the message is kept.
    let deadline = Instant::now() + Duration::from_secs(60);
    while !standing()
        .await
        .lately
        .iter()
        .any(|line| line.contains("put off"))
    {
        assert!(
            Instant::now() < deadline,
            "the message was never put off: {:?}",
            standing().await
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(asked(), 0);
    assert_eq!(
        said(),
        [("owner".to_string(), "What is running?".to_string())]
    );
    assert!(standing().await.trouble.is_none());

    // The worker dies while the message waits. Nothing of it was in the
    // worker: the one that starts in its place answers it.
    shop.replace_worker();

    // When the account is back it is answered, once, and the owner's
    // message is on record once.
    let deadline = Instant::now() + Duration::from_secs(90);
    while said().len() < 2 {
        assert!(
            Instant::now() < deadline,
            "the message was never answered: {:?}",
            standing().await
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(asked(), 1);
    assert_eq!(
        said(),
        [
            ("owner".to_string(), "What is running?".to_string()),
            ("executive".to_string(), "Nothing is running.".to_string()),
        ]
    );
    assert!(assert_described(&client, &project_id("toy")).await > 4);
    shop.close().await;
}

/// The accounts workflow in front of the real driver: the step is handed
/// an account by name, the driver reads that account's token itself, and a
/// run the account refuses is carried on under the next one.
#[tokio::test(flavor = "multi_thread")]
async fn a_real_run_refused_at_a_limit_is_handed_the_next_account_by_the_accounts_workflow() {
    let opened = Shop::open_after("engine-accounts", "true", "limit-default-once", |world| {
        let token = world.path("token-b");
        std::fs::write(&token, "tok-b\n").unwrap();
        let factory = world.path("config/factory.toml");
        let text = std::fs::read_to_string(&factory).unwrap();
        std::fs::write(
            &factory,
            format!(
                "limit_grace_secs = 0\n{text}\n[[account]]\nid = \"a\"\n\n[[account]]\nid = \"b\"\ntoken_file = \"{}\"\n",
                token.display()
            ),
        )
        .unwrap();
    })
    .await;
    let Some(shop) = opened else {
        return;
    };
    let (mut input, options) = shop.start("engine-accounts");
    input.accounts = ACCOUNTS_ID.to_string();
    let handle = shop
        .client()
        .start_workflow(Pipeline::run, input, options)
        .await
        .unwrap();
    let end = handle
        .get_result(WorkflowGetResultOptions::default())
        .await
        .unwrap();
    assert!(end.ended, "{end:?}");

    // The builder started under the first account, was refused, and
    // carried on in the same session under the second. Nothing after it
    // went back to the first.
    let calls = shop.world.calls();
    assert_eq!(
        (calls[0][0].as_str(), calls[0][3].as_str()),
        ("builder", "default")
    );
    assert_eq!(
        (calls[1][0].as_str(), calls[1][3].as_str()),
        ("builder", "tok-b")
    );
    assert_eq!(
        calls[0][1], calls[1][1],
        "the session continues across accounts"
    );
    assert!(
        calls[1..].iter().all(|call| call[3] == "tok-b"),
        "{calls:?}"
    );

    // The accounts workflow was told, and holds the first account back for
    // the model that was refused.
    let board = shop
        .client()
        .get_workflow_handle::<Accounts>(ACCOUNTS_ID.to_string())
        .query(Accounts::board, (), WorkflowQueryOptions::default())
        .await
        .unwrap();
    assert_eq!(board.picture.order, ["a", "b"]);
    assert!(
        !board.picture.states["a"].blocked.is_empty(),
        "the limit is not on the board: {board:?}"
    );
    assert!(board.waiting.is_empty());

    // The token was used where the run happened and is in no history.
    for workflow in [ACCOUNTS_ID, "pipeline-engine-accounts"] {
        let json = shop
            .client()
            .get_workflow_handle::<Pipeline>(workflow.to_string())
            .fetch_history(WorkflowFetchHistoryOptions::default())
            .to_json()
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&json).into_owned();
        // Payloads are base64 in a history's JSON: look for the token as
        // it would be written there, at each of the three alignments.
        for encoded in ["dG9rLW", "Rvay1i", "0b2stY"] {
            assert!(
                !text.contains(encoded),
                "a token is in the history of `{workflow}`"
            );
        }
        assert!(!text.contains("tok-b"));
    }
    shop.close().await;
}

/// A schedule a release brings is found when its worker starts. The
/// project's workflow was waiting with nothing due, and learns of a due
/// time only from a turn: the starting worker has it look, the run that is
/// overdue is made in that turn, and the workflow then waits for the next.
#[tokio::test(flavor = "multi_thread")]
async fn a_schedule_new_to_a_project_is_run_when_a_worker_starts_with_it_overdue() {
    use factory_engine::tiers::{Project, project_id};

    let prepare = |world: &World| {
        world.with_github("green");
        std::fs::create_dir_all(world.path("config/goals/toy")).unwrap();
        std::fs::write(
            world.path("config/goals/toy/main.toml"),
            "id = \"main\"\ndefault = true\n[director]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\n",
        )
        .unwrap();
    };
    let Some(shop) =
        Shop::open_with("engine-schedule", "true", "", prepare, "tiers = true\n").await
    else {
        return;
    };
    let world = &shop.world;
    let client = shop.client();
    let standing = || async {
        client
            .get_workflow_handle::<Project>(project_id("toy"))
            .query(Project::standing, (), WorkflowQueryOptions::default())
            .await
            .ok()
    };
    let now = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    };
    let maintainers = || {
        world
            .calls()
            .iter()
            .filter(|call| call[0] == "maintainer")
            .count()
    };

    // The project has no schedule yet: its workflow takes its first turn
    // and waits, with nothing due.
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let stands = standing().await;
        if stands
            .as_ref()
            .is_some_and(|stands| stands.turns >= 1 && stands.waiting == 0)
        {
            let stands = stands.unwrap();
            assert_eq!(
                (stands.due_at, stands.trouble.clone()),
                (None, None),
                "{stands:?}"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the project never took a turn: {stands:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(world.calls().is_empty(), "{:?}", world.calls());

    // A release brings a schedule, whose time passed while the old worker
    // ran: the worker of that release is started in its place.
    std::fs::write(
        world.path("config/goals/toy/upkeep.toml"),
        "id = \"upkeep\"\n[director]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\n[schedule]\nhour_utc = 4\nevery_days = 7\nmax_beads = 2\ncharter = \"Turn what keeps breaking into checks.\"\n",
    )
    .unwrap();
    let record = world.path("state/projects/toy/schedules/upkeep/scheduled.json");
    std::fs::create_dir_all(record.parent().unwrap()).unwrap();
    std::fs::write(
        &record,
        serde_json::json!({ "since": now() - 8 * 86_400 }).to_string(),
    )
    .unwrap();
    std::fs::create_dir_all(world.path("bin/state")).unwrap();
    std::fs::write(
        world.path("bin/state/maintainer-1.json"),
        serde_json::json!({
            "summary": "Cleanup on error paths keeps breaking.",
            "digest": "One bead: a check that injects allocation failure.",
            "journal": "Nothing else was worth a bead.",
            "file": [{"title": "Check every error path for leaks", "type": "task", "priority": 3,
                "description": "Error paths have leaked.", "acceptance_criteria": "A check fails when one leaks.",
                "parent": "", "discovered_from": "", "deferred": true}]
        })
        .to_string(),
    )
    .unwrap();
    shop.replace_worker();

    let ran = || -> u64 {
        std::fs::read_to_string(&record)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .and_then(|record| record["runs"].as_u64())
            .unwrap_or(0)
    };
    let deadline = Instant::now() + Duration::from_secs(120);
    while ran() == 0 {
        assert!(
            Instant::now() < deadline,
            "the schedule was never run: {:?}, {:?}, {}",
            world.calls(),
            standing().await,
            std::fs::read_to_string(record.with_file_name("events.jsonl")).unwrap_or_default()
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(maintainers(), 1, "{:?}", world.calls());
    let filed = std::fs::read_dir(world.path("beads"))
        .unwrap()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("filed-"))
        .count();
    assert_eq!(filed, 1);

    // The workflow now knows when the next run is, and waits for it: it
    // does not look again and again.
    let deadline = Instant::now() + Duration::from_secs(60);
    let stands = loop {
        let stands = standing().await;
        if let Some(stands) = stands.as_ref().filter(|stands| stands.due_at.is_some()) {
            break stands.clone();
        }
        assert!(Instant::now() < deadline, "no next time: {stands:?}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let due = stands.due_at.unwrap();
    assert!(
        due > now() + 3600 && due <= now() + 7 * 86_400,
        "{stands:?}"
    );
    assert_eq!(stands.trouble, None, "{stands:?}");
    let turns = stands.turns;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let later = standing().await.unwrap();
    assert_eq!((later.turns, maintainers()), (turns, 1), "{later:?}");
    shop.close().await;
}

/// A goal definition put in force while the worker runs is taken up by
/// that worker, as `factory goal enable` has it taken up: the engine's
/// `enabled` command. A project that had no definitions had no workflow,
/// and has one; a definition with a schedule has its project look at once,
/// so that a schedule already overdue is run then and not at the next
/// start. The worker is the same one throughout.
#[tokio::test(flavor = "multi_thread")]
async fn a_definition_put_in_force_is_taken_up_by_the_running_worker() {
    use factory_engine::tiers::{INTAKE_ID, Project, project_id};
    use temporalio_client::errors::WorkflowInteractionError;
    use temporalio_client::{UntypedWorkflow, WorkflowDescribeOptions};

    let Some(shop) = Shop::open_with(
        "engine-enable",
        "true",
        "",
        |world| world.with_github("green"),
        "tiers = true\n",
    )
    .await
    else {
        return;
    };
    let world = &shop.world;
    let client = shop.client();
    assert!(world.config().goal_definitions("toy").unwrap().is_empty());
    let worker_pid = shop.worker.lock().unwrap().id();
    let still_running = || {
        let mut worker = shop.worker.lock().unwrap();
        assert_eq!(worker.id(), worker_pid);
        assert!(worker.try_wait().unwrap().is_none(), "the worker ended");
    };
    let standing = || async {
        client
            .get_workflow_handle::<Project>(project_id("toy"))
            .query(Project::standing, (), WorkflowQueryOptions::default())
            .await
            .ok()
    };
    let now = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    };
    let enabled = |id: &str| {
        let said = Command::new(env!("CARGO_BIN_EXE_factory-engine"))
            .args(["enabled", "toy", id, "--config"])
            .arg(world.path("config"))
            .output()
            .unwrap();
        assert!(said.status.success(), "{said:?}");
        String::from_utf8_lossy(&said.stdout).into_owned()
    };

    // The worker starts the intake last: once it is there, the worker has
    // started every project's workflow it is going to, and toy, with no
    // definition, has none.
    let deadline = Instant::now() + Duration::from_secs(120);
    while client
        .get_workflow_handle::<UntypedWorkflow>(INTAKE_ID.to_string())
        .describe(WorkflowDescribeOptions::default())
        .await
        .is_err()
    {
        assert!(
            Instant::now() < deadline,
            "the worker never started the intake"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let none = client
        .get_workflow_handle::<UntypedWorkflow>(project_id("toy"))
        .describe(WorkflowDescribeOptions::default())
        .await;
    assert!(
        matches!(none, Err(WorkflowInteractionError::NotFound(_))),
        "{none:?}"
    );

    // The owner puts a first definition in force: the project's workflow
    // is started by the running worker, and takes its first turn.
    factory_driver::goal::enable_definition(
        &world.config(),
        "toy",
        "id = \"main\"\ndefault = true\n[director]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\n",
    )
    .unwrap();
    let said = enabled("main");
    assert!(!said.contains("to look now"), "{said}");
    let deadline = Instant::now() + Duration::from_secs(120);
    let first = loop {
        let stands = standing().await;
        if let Some(stands) = stands
            .as_ref()
            .filter(|stands| stands.turns >= 1 && stands.waiting == 0)
        {
            break stands.clone();
        }
        assert!(
            Instant::now() < deadline,
            "the project never took a turn: {stands:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert_eq!(
        (first.due_at, first.trouble.clone()),
        (None, None),
        "{first:?}"
    );
    // Nothing comes due by itself: the workflow waits.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(standing().await.unwrap().turns, first.turns);
    still_running();

    // Then one with a schedule whose time has passed. Nothing else would
    // wake the waiting workflow: the call has the project look now.
    let record = world.path("state/projects/toy/schedules/upkeep/scheduled.json");
    std::fs::create_dir_all(record.parent().unwrap()).unwrap();
    std::fs::write(
        &record,
        serde_json::json!({ "since": now() - 8 * 86_400 }).to_string(),
    )
    .unwrap();
    std::fs::create_dir_all(world.path("bin/state")).unwrap();
    std::fs::write(
        world.path("bin/state/maintainer-1.json"),
        serde_json::json!({
            "summary": "Cleanup on error paths keeps breaking.",
            "digest": "One bead: a check that injects allocation failure.",
            "journal": "Nothing else was worth a bead.",
            "file": [{"title": "Check every error path for leaks", "type": "task", "priority": 3,
                "description": "Error paths have leaked.", "acceptance_criteria": "A check fails when one leaks.",
                "parent": "", "discovered_from": "", "deferred": true}]
        })
        .to_string(),
    )
    .unwrap();
    factory_driver::goal::enable_definition(
        &world.config(),
        "toy",
        "id = \"upkeep\"\n[director]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\n[schedule]\nhour_utc = 4\nevery_days = 7\nmax_beads = 2\ncharter = \"Turn what keeps breaking into checks.\"\n",
    )
    .unwrap();
    let said = enabled("upkeep");
    assert!(said.contains("told project `toy` to look now"), "{said}");
    let ran = || -> u64 {
        std::fs::read_to_string(&record)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .and_then(|record| record["runs"].as_u64())
            .unwrap_or(0)
    };
    let deadline = Instant::now() + Duration::from_secs(120);
    while ran() == 0 {
        assert!(
            Instant::now() < deadline,
            "the schedule was never run: {:?}, {:?}",
            world.calls(),
            standing().await,
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let maintainers = world
        .calls()
        .iter()
        .filter(|call| call[0] == "maintainer")
        .count();
    assert_eq!(maintainers, 1, "{:?}", world.calls());
    let deadline = Instant::now() + Duration::from_secs(60);
    let stands = loop {
        let stands = standing().await;
        if let Some(stands) = stands.as_ref().filter(|stands| stands.due_at.is_some()) {
            break stands.clone();
        }
        assert!(Instant::now() < deadline, "no next time: {stands:?}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert_eq!(stands.trouble, None, "{stands:?}");
    let lately = stands.lately.join("\n");
    assert!(lately.contains("see to what has come due"), "{lately}");
    still_running();

    // A definition that is not in force is refused, and nothing is told.
    let refused = Command::new(env!("CARGO_BIN_EXE_factory-engine"))
        .args(["enabled", "toy", "nowhere", "--config"])
        .arg(world.path("config"))
        .output()
        .unwrap();
    assert!(!refused.status.success(), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .contains("project `toy` has no goal definition `nowhere` in force"),
        "{refused:?}"
    );
    shop.close().await;
}

/// The executive proposes a definition with a schedule and the owner
/// approves it, both as signals to the project's workflow on the running
/// worker. Approve puts it in force and has that worker take it up by the
/// engine's `enabled` command, as `factory goal enable` does: the project
/// is told to look now, and the schedule, already overdue on its record,
/// is run. No goal is opened, and the worker is the same one throughout.
#[tokio::test(flavor = "multi_thread")]
async fn a_scheduled_definition_the_owner_approves_is_run_by_the_running_worker() {
    use factory_driver::goal::Event;
    use factory_engine::tiers::{Project, Reach, project_id, tell};
    use factory_slack::socket::Incoming;

    const UPKEEP: &str = "id = \"upkeep\"\n[director]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\n[schedule]\nhour_utc = 4\nevery_days = 7\nmax_beads = 2\ncharter = \"Turn what keeps breaking into checks.\"\n";
    let now = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    };
    let prepare = |world: &World| {
        world.with_github("green");
        let project = world.path("config/projects/toy.toml");
        let text = std::fs::read_to_string(&project).unwrap()
            + "\n[executive]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\nslots = 2\n";
        std::fs::write(&project, text).unwrap();
        std::fs::create_dir_all(world.path("config/goals/toy")).unwrap();
        std::fs::write(
            world.path("config/goals/toy/main.toml"),
            "id = \"main\"\ndefault = true\n[director]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(world.path("bin/state")).unwrap();
        std::fs::write(
            world.path("bin/state/executive-1.json"),
            serde_json::json!({
                "reply": "A weekly upkeep run, below.",
                "close": [],
                "open_goal": {"definition": "", "handoff": ""},
                "propose_definition": {"toml": UPKEEP, "reason": "Nothing runs by the clock.", "handoff": ""},
            })
            .to_string(),
        )
        .unwrap();
    };
    // The engine's own command is how the worker is told.
    let temporal = format!(
        "tiers = true\nengine_bin = \"{}\"\n",
        env!("CARGO_BIN_EXE_factory-engine")
    );
    let Some(shop) =
        Shop::open_with("engine-approve-schedule", "true", "", prepare, &temporal).await
    else {
        return;
    };
    let world = &shop.world;
    let client = shop.client();
    let reach = Reach {
        task_queue: "factory-engine-approve-schedule".to_string(),
        beat_secs: 1,
        accounts: ACCOUNTS_ID.to_string(),
    };
    let worker_pid = shop.worker.lock().unwrap().id();
    let standing = || async {
        client
            .get_workflow_handle::<Project>(project_id("toy"))
            .query(Project::standing, (), WorkflowQueryOptions::default())
            .await
            .ok()
    };
    let proposal = || -> serde_json::Value {
        std::fs::read_to_string(world.path("state/projects/toy/executive.json"))
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .map(|executive| executive["threads"]["7.7"]["proposals"][0].clone())
            .unwrap_or_default()
    };

    // The owner asks; the executive proposes the scheduled definition.
    let ask = Event::Tell {
        project: "toy".to_string(),
        thread: "7.7".to_string(),
        text: "Look for what keeps breaking once a week.".to_string(),
    };
    tell(&client, &reach, "toy", ask).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    while proposal()["state"] != "waiting" {
        assert!(
            Instant::now() < deadline,
            "nothing was proposed: {:?}",
            standing().await
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(proposal()["schedule"], "every 7 day(s) at 04:00 UTC");
    assert!(
        !world
            .config()
            .goal_definitions("toy")
            .unwrap()
            .iter()
            .any(|definition| definition.id == "upkeep")
    );

    // Its record says a run is overdue, and the maintainer has its answer
    // ready. The owner approves.
    let record = world.path("state/projects/toy/schedules/upkeep/scheduled.json");
    std::fs::create_dir_all(record.parent().unwrap()).unwrap();
    std::fs::write(
        &record,
        serde_json::json!({ "since": now() - 8 * 86_400 }).to_string(),
    )
    .unwrap();
    std::fs::write(
        world.path("bin/state/maintainer-1.json"),
        serde_json::json!({
            "summary": "Cleanup on error paths keeps breaking.",
            "digest": "One bead: a check that injects allocation failure.",
            "journal": "Nothing else was worth a bead.",
            "file": [{"title": "Check every error path for leaks", "type": "task", "priority": 3,
                "description": "Error paths have leaked.", "acceptance_criteria": "A check fails when one leaks.",
                "parent": "", "discovered_from": "", "deferred": true}]
        })
        .to_string(),
    )
    .unwrap();
    let approve = Event::Slack(Incoming::Action {
        channel: String::new(),
        user: "OWNER".to_string(),
        action_id: "definition_approve".to_string(),
        value: "7.7/1".to_string(),
        message_ts: String::new(),
    });
    tell(&client, &reach, "toy", approve).await.unwrap();
    let ran = || -> u64 {
        std::fs::read_to_string(&record)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .and_then(|record| record["runs"].as_u64())
            .unwrap_or(0)
    };
    let deadline = Instant::now() + Duration::from_secs(120);
    while ran() == 0 {
        assert!(
            Instant::now() < deadline,
            "the schedule was never run: {:?}, {:?}, {}",
            world.calls(),
            standing().await,
            proposal()
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // It is stored and in force, and the worker was told by the engine's
    // `enabled` command, which had the project look now.
    let approved = proposal();
    assert_eq!(approved["state"], "approved", "{approved}");
    let hash = approved["hash"].as_str().unwrap();
    assert_eq!(
        std::fs::read_to_string(world.path(&format!("state/goals/toy/upkeep/{hash}.toml")))
            .unwrap(),
        UPKEEP
    );
    let log =
        std::fs::read_to_string(world.path("state/projects/toy/executive/events.jsonl")).unwrap();
    let logged: serde_json::Value = log
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .find(|event| event["kind"] == "definition.approved")
        .expect("definition.approved is logged");
    let said = logged["data"]["engine"]["said"].to_string();
    for expected in [
        "the tiers of project `toy` are running, under `upkeep` among the rest",
        "told project `toy` to look now: `upkeep` has a schedule",
    ] {
        assert!(said.contains(expected), "missing {expected:?} in {logged}");
    }
    assert_eq!(logged["data"]["schedule"], "every 7 day(s) at 04:00 UTC");
    let made = approved["made"].as_str().unwrap();
    assert!(
        made.contains(
            "it runs every 7 day(s) at 04:00 UTC, and is due now; no goal is opened under it"
        ),
        "{made}"
    );
    // The maintainer ran once, and no goal was opened by the approval.
    let maintainers = world
        .calls()
        .iter()
        .filter(|call| call[0] == "maintainer")
        .count();
    assert_eq!(maintainers, 1, "{:?}", world.calls());
    assert!(goal_records(world).is_empty());
    assert!(
        world.calls().iter().all(|call| call[0] != "intake"),
        "{:?}",
        world.calls()
    );
    let executive: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(world.path("state/projects/toy/executive.json")).unwrap(),
    )
    .unwrap();
    assert!(executive["threads"]["7.7"]["goal"].is_null());
    {
        let mut worker = shop.worker.lock().unwrap();
        assert_eq!(worker.id(), worker_pid);
        assert!(worker.try_wait().unwrap().is_none(), "the worker ended");
    }
    let stands = standing().await.unwrap();
    assert_eq!(stands.trouble, None, "{stands:?}");
    shop.close().await;
}
