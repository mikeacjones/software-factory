//! The engine against a real Temporal server: a local one, started for
//! each test from the `temporal` CLI on this machine. A test that finds no
//! CLI says so and passes, so that the suite still runs where there is none.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::{Duration, Instant};

use common::Rig;
use factory_engine::engine::{GoOn, Pipeline, PipelineEnd};
use temporalio_client::{
    WorkflowGetResultOptions, WorkflowQueryOptions, WorkflowSignalOptions, WorkflowStartOptions,
};

/// The flow of the factory's own first pipeline, on made-up models.
const BUILD_BREAK_REVIEW: &str = r#"
id = "toy"
start = "build"

[[step]]
id = "build"
kind = "agent"
role = "builder"
model = "m"
effort = "medium"
writes = ["code"]
on.done = "test"

[[step]]
id = "test"
kind = "command"
command = "test"
on.pass = "break"
on.fail = { to = "fix", max = 2 }

[[step]]
id = "break"
kind = "agent"
role = "tester"
model = "m"
effort = "high"
workspace = "scratch"
on.held = "review"
on.broken = { to = "fix", max = 3 }

[[step]]
id = "review"
kind = "agent"
role = "reviewer"
model = "m"
effort = "medium"
workspace = "scratch"
on.approve = "describe"

[[step]]
id = "fix"
kind = "agent"
role = "builder"
model = "m"
effort = "medium"
session = "continue:build"
writes = ["code"]
on.done = { to = "test", max = 8 }

[[step]]
id = "describe"
kind = "agent"
role = "publisher"
model = "m"
effort = "low"
session = "continue:build"
on.ready = "publish"

[[step]]
id = "publish"
kind = "action"
action = "publish"
on.done = "end"
"#;

/// A pipeline nobody wrote engine code for: reproduce a bug, fix it, and
/// prove nothing else broke.
const BUG_FIX: &str = r#"
id = "bug-fix"
start = "reproduce"

[[step]]
id = "reproduce"
kind = "agent"
role = "tester"
model = "m"
effort = "high"
workspace = "scratch"
on.reproduced = "fix"
on.not_reproduced = "park"

[[step]]
id = "fix"
kind = "agent"
role = "builder"
model = "m"
effort = "medium"
writes = ["code"]
on.done = "regress"

[[step]]
id = "regress"
kind = "command"
command = "test"
on.pass = "end"
on.fail = { to = "fix", max = 2 }
"#;

fn outcomes(end: &PipelineEnd) -> Vec<String> {
    end.done
        .iter()
        .map(|(step, outcome)| format!("{step}:{outcome}"))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_engine_runs_whatever_flow_it_is_given() {
    let Some(mut rig) = Rig::start("flows").await else {
        return;
    };
    rig.scenario("break-once");
    rig.worker("b1", "unversioned");
    let client = rig.client();
    let start = |unit: &str| WorkflowStartOptions::new(rig.queue.clone(), unit.to_string()).build();

    // The factory's own pipeline, with a tester that breaks the change once:
    // the routes of the definition decide what runs next.
    let first = client
        .start_workflow(
            Pipeline::run,
            rig.input("toy-1", BUILD_BREAK_REVIEW),
            start("toy-1"),
        )
        .await
        .unwrap();
    let end = first
        .get_result(WorkflowGetResultOptions::default())
        .await
        .unwrap();
    assert!(end.ended);
    assert_eq!(
        outcomes(&end),
        [
            "build:done",
            "test:pass",
            "break:broken",
            "fix:done",
            "test:pass",
            "break:held",
            "review:approve",
            "describe:ready",
            "publish:done"
        ]
    );

    // A different pipeline, on the same worker, with no code written for
    // it: its definition is all the engine needs.
    let second = client
        .start_workflow(Pipeline::run, rig.input("bug-7", BUG_FIX), start("bug-7"))
        .await
        .unwrap();
    let end = second
        .get_result(WorkflowGetResultOptions::default())
        .await
        .unwrap();
    assert_eq!(
        outcomes(&end),
        ["reproduce:reproduced", "fix:done", "regress:pass"]
    );
    // Each step was told what kind it is.
    let calls = rig.calls();
    assert!(
        calls.contains(&"toy-1 action publish round=1 session=- note=- build=b1".to_string()),
        "{calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|call| call.starts_with("bug-7 command regress "))
    );
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_parked_pipeline_waits_until_someone_continues_it() {
    let Some(mut rig) = Rig::start("parked").await else {
        return;
    };
    rig.scenario("test-fails");
    rig.worker("b1", "unversioned");
    let client = rig.client();
    let handle = client
        .start_workflow(
            Pipeline::run,
            rig.input("toy-1", BUILD_BREAK_REVIEW),
            WorkflowStartOptions::new(rig.queue.clone(), "toy-1".to_string()).build(),
        )
        .await
        .unwrap();

    // The test fails, is fixed twice to no avail, and fails again: the
    // flow allows no third fix, so the pipeline parks. It has not ended,
    // and anyone can ask where it stands.
    let deadline = Instant::now() + Duration::from_secs(60);
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
    assert_eq!(standing.step, "test");
    assert_eq!(
        standing.parked.as_deref(),
        Some("step `test` ended with `fail` more often than the flow allows")
    );
    assert_eq!(standing.done.len(), 6, "build, test, fix, test, fix, test");
    let calls_while_parked = rig.calls().len();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        rig.calls().len(),
        calls_while_parked,
        "nothing runs while it is parked"
    );

    // Whoever runs the sub-goal continues it, from a step and with a note.
    handle
        .signal(
            Pipeline::go_on,
            GoOn {
                from: "fix".to_string(),
                note: "The config path is wrong, not the parser.".to_string(),
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
        outcomes(&end)[6..],
        [
            "fix:done",
            "test:pass",
            "break:held",
            "review:approve",
            "describe:ready",
            "publish:done"
        ]
    );
    // The step it continued from was told why, and only that step.
    let calls = rig.calls();
    let noted: Vec<&String> = calls
        .iter()
        .filter(|call| !call.contains("note=-"))
        .collect();
    assert_eq!(noted.len(), 1, "{calls:?}");
    assert!(
        noted[0].starts_with("toy-1 agent fix round=3 ")
            && noted[0].contains("note=The config path is wrong, not the parser.")
    );
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_step_whose_worker_dies_resumes_its_session_under_the_next_worker() {
    let Some(mut rig) = Rig::start("crash").await else {
        return;
    };
    rig.scenario("hang-build");
    let first = rig.worker("b1", "unversioned");
    let client = rig.client();
    let handle = client
        .start_workflow(
            Pipeline::run,
            rig.input("toy-1", BUILD_BREAK_REVIEW),
            WorkflowStartOptions::new(rig.queue.clone(), "toy-1".to_string()).build(),
        )
        .await
        .unwrap();
    // The builder is at work, has said which session it is, and is nowhere
    // near done when its worker is killed.
    rig.wait_for("the build never started", || {
        rig.dir.join("state/hung.pid").exists()
    });
    tokio::time::sleep(Duration::from_millis(1500)).await;
    rig.kill(first);

    // Another worker comes up. Temporal notices the step has stopped
    // saying it is alive and gives it to this one, which resumes the
    // session the first run reported, and the pipeline runs to its end.
    rig.worker("b1", "unversioned");
    let end = handle
        .get_result(WorkflowGetResultOptions::default())
        .await
        .unwrap();
    assert!(end.ended);
    let calls = rig.calls();
    let builds: Vec<&String> = calls
        .iter()
        .filter(|call| call.contains(" build "))
        .collect();
    assert_eq!(
        builds,
        [
            "toy-1 agent build round=1 session=- note=- build=b1",
            "toy-1 agent build round=1 session=sess-toy-1-build-1 note=- build=b1"
        ]
    );
    // As far as the pipeline goes the build happened once.
    assert_eq!(
        outcomes(&end)
            .iter()
            .filter(|line| line.starts_with("build:"))
            .count(),
        1
    );
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_usage_limit_is_waited_out_on_a_timer_with_nothing_running() {
    let Some(mut rig) = Rig::start("limited").await else {
        return;
    };
    rig.scenario("limited");
    let first = rig.worker("b1", "unversioned");
    let client = rig.client();
    let handle = client
        .start_workflow(
            Pipeline::run,
            rig.input("toy-1", BUILD_BREAK_REVIEW),
            WorkflowStartOptions::new(rig.queue.clone(), "toy-1".to_string()).build(),
        )
        .await
        .unwrap();
    // The tester stops at a usage limit: the pipeline says it is waiting.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let standing = handle
            .query(Pipeline::standing, (), WorkflowQueryOptions::default())
            .await
            .unwrap();
        if standing.limited_secs == Some(3) {
            break;
        }
        assert!(Instant::now() < deadline, "it never waited: {standing:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // The wait is the server's. No worker is needed for it: this one goes
    // away, and one that comes up after the limit has lifted carries on.
    rig.kill(first);
    tokio::time::sleep(Duration::from_secs(4)).await;
    rig.worker("b1", "unversioned");
    let end = handle
        .get_result(WorkflowGetResultOptions::default())
        .await
        .unwrap();
    assert!(end.ended);
    assert_eq!(
        outcomes(&end)[2],
        "break:held",
        "the limit is not an outcome"
    );
    // The tester's second run resumed the session of the first.
    let calls = rig.calls();
    let breaks: Vec<&String> = calls
        .iter()
        .filter(|call| call.contains(" break "))
        .collect();
    assert_eq!(
        breaks,
        [
            "toy-1 agent break round=1 session=- note=- build=b1",
            "toy-1 agent break round=1 session=sess-toy-1-break-1 note=- build=b1"
        ]
    );
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pipeline_stays_on_the_build_of_the_engine_that_started_it() {
    let Some(mut rig) = Rig::start("versions").await else {
        return;
    };
    // The first pipeline's build waits until it is released.
    rig.scenario("hold-toy-1");
    let client = rig.client();
    let queue = rig.queue.clone();
    let start =
        move |unit: &str| WorkflowStartOptions::new(queue.clone(), unit.to_string()).build();

    // Build b1 of the engine is current, and a pipeline starts on it.
    rig.worker("b1", "versioned");
    rig.make_current("b1");
    let old = client
        .start_workflow(
            Pipeline::run,
            rig.input("toy-1", BUILD_BREAK_REVIEW),
            start("toy-1"),
        )
        .await
        .unwrap();
    rig.wait_for("the first pipeline never started", || {
        rig.dir.join("state/holding-toy-1").exists()
    });

    // A new build of the engine is deployed and made current while that
    // pipeline is in the middle of a step. A pipeline started now runs on
    // the new build.
    rig.worker("b2", "versioned");
    rig.make_current("b2");
    let new = client
        .start_workflow(
            Pipeline::run,
            rig.input("toy-2", BUILD_BREAK_REVIEW),
            start("toy-2"),
        )
        .await
        .unwrap();
    assert!(
        new.get_result(WorkflowGetResultOptions::default())
            .await
            .unwrap()
            .ended
    );

    // The first pipeline goes on, and finishes on the build it started on:
    // every one of its steps, also those begun after b2 became current.
    std::fs::write(rig.dir.join("state/release-toy-1"), "").unwrap();
    assert!(
        old.get_result(WorkflowGetResultOptions::default())
            .await
            .unwrap()
            .ended
    );
    let calls = rig.calls();
    let builds_of = |unit: &str| -> Vec<String> {
        let mut builds: Vec<String> = calls
            .iter()
            .filter(|call| call.starts_with(unit))
            .filter_map(|call| call.rsplit("build=").next().map(str::to_string))
            .collect();
        builds.dedup();
        builds
    };
    assert_eq!(builds_of("toy-1"), ["b1"], "{calls:?}");
    assert_eq!(builds_of("toy-2"), ["b2"], "{calls:?}");
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.starts_with("toy-1"))
            .count(),
        6,
        "all six steps of the first pipeline ran"
    );
    rig.stop().await;
}
