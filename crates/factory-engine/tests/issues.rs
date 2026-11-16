//! The schedule that keeps a project's GitHub issues in step with its
//! beads, against a real Temporal server, with the scripted step standing
//! in for the sweep itself.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::{Duration, Instant};

use common::Rig;
use factory_engine::issues::{IssueSync, ensure_schedule, schedule_id};
use temporalio_client::WorkflowGetResultOptions;

/// How many sweeps the scripted step has been asked for.
fn sweeps(rig: &Rig) -> usize {
    std::fs::read_to_string(rig.dir.join("state/sweeps"))
        .unwrap_or_default()
        .lines()
        .count()
}

async fn wait_for(what: &str, secs: u64, mut done: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(secs);
    while !done() {
        assert!(Instant::now() < until, "waited {secs}s for {what}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// The schedule as the server has it.
async fn described(
    rig: &Rig,
) -> temporalio_common::protos::temporal::api::workflowservice::v1::DescribeScheduleResponse {
    rig.client()
        .get_schedule_handle(schedule_id("toy"))
        .describe(Default::default())
        .await
        .unwrap()
        .into_raw()
}

/// The workflows the schedule started, most recent last.
async fn started(rig: &Rig) -> Vec<String> {
    described(rig)
        .await
        .info
        .map(|info| info.recent_actions)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|action| action.start_workflow_result.map(|run| run.workflow_id))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_projects_issues_are_swept_on_a_schedule_the_worker_keeps() {
    let Some(mut rig) = Rig::start("issues").await else {
        return;
    };
    rig.worker("b1", "unversioned");
    let client = rig.client();

    // Made once, and run on its interval.
    assert!(
        ensure_schedule(&client, &rig.queue, "toy", 2, 1)
            .await
            .unwrap()
    );
    wait_for("two sweeps", 60, || sweeps(&rig) >= 2).await;
    let first = started(&rig).await;
    assert!(!first.is_empty());
    let run = client.get_workflow_handle::<IssueSync>(first[0].clone());
    let sweep = run
        .get_result(WorkflowGetResultOptions::default())
        .await
        .unwrap();
    assert_eq!(sweep.created, ["toy-1 https://github.com/me/toy/issues/1"]);
    // What it started, and the run itself, say what they are for.
    rig.assert_described(&first[0]).await;

    // A worker that starts again finds it as it should be and leaves it.
    assert!(
        !ensure_schedule(&client, &rig.queue, "toy", 2, 1)
            .await
            .unwrap()
    );
    let schedule = described(&rig).await.schedule.unwrap();
    assert_eq!(
        schedule.spec.as_ref().unwrap().interval[0]
            .interval
            .as_ref()
            .unwrap()
            .seconds,
        2
    );
    // One run at a time.
    assert_eq!(schedule.policies.as_ref().unwrap().overlap_policy, 1);

    // A sweep slower than the interval: the runs it would overlap are
    // skipped, not stacked up. A changed interval makes the schedule again.
    rig.scenario("slow-sweep");
    assert!(
        ensure_schedule(&client, &rig.queue, "toy", 1, 1)
            .await
            .unwrap()
    );
    let before = sweeps(&rig);
    tokio::time::sleep(Duration::from_secs(9)).await;
    let during = sweeps(&rig) - before;
    assert!(
        (1..=3).contains(&during),
        "{during} sweeps began in 9s of 5s sweeps"
    );
    let info = described(&rig).await.info.unwrap();
    assert!(info.overlap_skipped > 0, "{info:?}");

    // A sweep in which a bead could not be brought into step fails, and
    // says which.
    rig.scenario("sweep-fails");
    assert!(
        ensure_schedule(&client, &rig.queue, "toy", 2, 1)
            .await
            .unwrap()
    );
    let before = sweeps(&rig);
    wait_for("a failing sweep", 60, || sweeps(&rig) > before).await;
    let last = started(&rig).await.pop().unwrap();
    let failed = client
        .get_workflow_handle::<IssueSync>(last)
        .get_result(WorkflowGetResultOptions::default())
        .await
        .unwrap_err();
    assert!(
        format!("{failed:?}").contains("toy-9: gh said no"),
        "{failed:?}"
    );
}
