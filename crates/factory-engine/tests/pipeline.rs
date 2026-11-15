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
use factory_engine::engine::{GoOn, Pipeline, PipelineInput};
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
    worker: Child,
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
                "{text}\n[temporal]\naddress = \"http://127.0.0.1:{port}\"\nnamespace = \"default\"\ntask_queue = \"factory-{name}\"\nbeat_secs = 1\n"
            ),
        )
        .unwrap();
        let worker = Command::new(env!("CARGO_BIN_EXE_factory-engine"))
            .args(["worker", "--config"])
            .arg(world.path("config"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        Some(Shop {
            world,
            env: Some(env),
            worker,
        })
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
        let _ = self.worker.kill();
        let _ = self.worker.wait();
        if let Some(env) = self.env.take() {
            env.shutdown().await.unwrap();
        }
    }
}

fn unit_record(world: &World, unit: &str) -> serde_json::Value {
    let text =
        std::fs::read_to_string(world.path(&format!("state/units/{unit}/unit.json"))).unwrap();
    serde_json::from_str(&text).unwrap()
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
