//! One unit walked through the real flow and the real role prompts, with a
//! scripted stand-in for the `claude` CLI and a bare repository for a remote.
//! No model and no network.

// Test code: a failed setup step should stop the test where it happens.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::path::Path;
use std::process::Command;

use common::world::{World, repo_config, run};

use factory_driver::{Driver, DriverError, Options, Status};

const OPTIONS: Options = Options {
    slack: false,
    flow: None,
    within: None,
    continued: None,
    ruling: None,
    pausable: false,
    engine: false,
    account: None,
};

#[test]
fn a_unit_is_built_broken_fixed_reviewed_and_published() {
    let world = World::new("full", "test -f feature.txt");
    let mut driver = Driver::start(world.config(), "toy", "toy-1", &OPTIONS).unwrap();
    assert_eq!(driver.run().unwrap(), Status::Ended);

    // The steps, in the order the flow routed them.
    let kinds = world.event_kinds(&driver);
    let steps: Vec<&str> = kinds
        .iter()
        .filter(|kind| kind.contains(':'))
        .map(String::as_str)
        .collect();
    assert_eq!(
        steps,
        [
            "build:done",
            "fmt:pass",
            "readme:pass",
            "changelog:pass",
            "test:pass",
            "break:broken",
            "fix:done",
            "fmt:pass",
            "readme:pass",
            "changelog:pass",
            "test:pass",
            "break:held",
            "review:approve",
            "describe:ready",
            "publish:done",
        ]
    );
    for kind in [
        "bead.claimed",
        "worktree.created",
        "checkpoint",
        "scratch.discarded",
        "published.pushed",
        "unit.ended",
    ] {
        assert!(kinds.iter().any(|k| k == kind), "no {kind} event");
    }

    // Who ran, in which session and where.
    let calls = world.calls();
    let roles: Vec<&str> = calls.iter().map(|call| call[0].as_str()).collect();
    assert_eq!(
        roles,
        [
            "builder",
            "tester",
            "builder",
            "tester",
            "reviewer",
            "publisher"
        ]
    );
    let build_session = &calls[0][1];
    assert_eq!(
        &calls[2][1], build_session,
        "the fix continues the builder's session"
    );
    assert_eq!(
        &calls[5][1], build_session,
        "publication continues the builder's session"
    );
    assert_ne!(
        calls[1][1], calls[3][1],
        "each round of testing is a fresh session"
    );
    let unit = driver.unit().clone();
    let worktree = unit.worktree.to_str().unwrap();
    assert!(calls[0][2].ends_with(worktree.trim_start_matches("/private")));
    assert_ne!(
        calls[1][2], calls[0][2],
        "the tester works in a scratch worktree"
    );
    assert!(
        !Path::new(&calls[1][2]).exists(),
        "the scratch worktree is thrown away"
    );

    // The fix was told what the tester found, and where its probes are.
    let fix_brief = std::fs::read_to_string(world.path("bin/state/brief-3.md")).unwrap();
    assert!(fix_brief.contains("The adversarial tester broke the change"));
    assert!(fix_brief.contains("fails on empty input"));
    assert!(fix_brief.contains("Reproduce: sh regress.sh"));
    // The tester's regression test is in the builder's worktree, failing,
    // and the fix is told the command that must pass.
    assert!(
        fix_brief.contains("The tester left regression tests"),
        "{fix_brief}"
    );
    assert!(
        fix_brief.contains("- `sh regress.sh` (fails on empty input)"),
        "{fix_brief}"
    );
    let unit_now = driver.unit().clone();
    assert_eq!(unit_now.regressions.len(), 1);
    assert_eq!(unit_now.regressions[0].test_names, ["empty-input"]);
    assert!(kinds.iter().any(|kind| kind == "regressions.added"));
    // The second tester is a fresh session, told what the first one found.
    let second_test = std::fs::read_to_string(world.path("bin/state/brief-4.md")).unwrap();
    assert!(
        second_test.contains("## What has happened on this bead so far"),
        "{second_test}"
    );
    assert!(
        second_test.contains("- break (round 1): broken. Findings: [major] fails on empty input"),
        "{second_test}"
    );
    assert!(
        second_test.contains("- fix (round 1): done"),
        "{second_test}"
    );
    // The finding's class is on record, the next tester is given the key
    // to reuse, and nobody was told of classes while none was recorded.
    assert_eq!(unit_now.regressions[0].category, "empty-input");
    assert_eq!(
        factory_driver::finding_classes(&world.path("state"), "toy"),
        [factory_brief::ClassCount {
            class: "empty-input".to_string(),
            findings: 1,
            beads: 1
        }]
    );
    assert!(
        second_test.contains("## Classes of finding recorded on this project")
            && second_test.contains("- `empty-input`: 1 finding on 1 bead"),
        "{second_test}"
    );
    for first in ["brief-1.md", "brief-2.md"] {
        let brief = std::fs::read_to_string(world.path(&format!("bin/state/{first}"))).unwrap();
        assert!(!brief.contains("earlier beads of this project"), "{brief}");
        assert!(!brief.contains("Classes of finding"), "{brief}");
    }
    assert!(
        !fix_brief.contains("## Acceptance criteria"),
        "a continued session gets only what is new"
    );
    // The reviewer saw the checks and the tester's report, not the builder's words.
    let review_brief = std::fs::read_to_string(world.path("bin/state/brief-5.md")).unwrap();
    assert!(review_brief.contains("## Check `test`"));
    assert!(review_brief.contains("Adversarial testing"));
    assert!(!review_brief.contains("Added a line to feature.txt"));

    // The publisher continues the builder's session but is told what the
    // factory checked and what the tester and reviewer reported.
    let describe_brief = std::fs::read_to_string(world.path("bin/state/brief-6.md")).unwrap();
    assert!(describe_brief.contains("## Check `test`"));
    assert!(describe_brief.contains("Adversarial testing"));
    assert!(describe_brief.contains("## Review (step `review`, round 1)"));
    assert!(!describe_brief.contains("## Acceptance criteria"));

    // What was published: one commit on the unit's branch, without the probes.
    let remote = world.path("remote.git");
    let branch = format!("refs/heads/{}", unit.branch);
    assert_eq!(unit.branch, "feat/1-add-the-feature");
    assert_eq!(
        run(
            &remote,
            "git",
            &["rev-list", "--count", &format!("main..{branch}")]
        ),
        "1"
    );
    assert_eq!(
        run(&remote, "git", &["log", "-1", "--format=%s", &branch]),
        "Add the feature"
    );
    assert_eq!(
        run(&remote, "git", &["ls-tree", "-r", "--name-only", &branch]),
        "README.md\nfeature.txt\nfixed.txt\nregress.sh",
        "the regression test and the fix are published; the tester's other probes are not"
    );
    assert_eq!(
        run(&remote, "git", &["show", &format!("{branch}:feature.txt")]),
        "line 1\nline 3"
    );

    // Usage is tracked per step and adds up.
    assert_eq!(unit.usage["build"].runs, 1);
    assert_eq!(unit.usage["break"].runs, 2);
    assert_eq!(unit.usage["break"].usage.turns, 6);
    assert_eq!(unit.usage["test"].runs, 2);
    let report = factory_driver::usage_report(&unit, driver.flow());
    assert!(report.contains("total"), "{report}");
    assert!(
        report.contains("model claude-opus-5-5: est. $3.00"),
        "{report}"
    );
    assert!(report.contains("5h 20%"), "{report}");

    // The bead stays claimed until the pull request merges.
    let bead = std::fs::read_to_string(world.path("beads/toy-1.json")).unwrap();
    assert!(bead.contains("\"in_progress\"") && bead.contains("\"factory\""));
}

#[test]
fn a_builder_is_told_the_classes_of_defect_that_broke_earlier_beads_of_its_project() {
    let world = World::new("classes", "test -f feature.txt");
    // The tester holds, so this run adds nothing to the record.
    std::fs::create_dir_all(world.path("bin/state")).unwrap();
    std::fs::write(world.path("bin/state/broke"), "").unwrap();
    // What earlier units left: two beads of this project broken by one class,
    // one by another, a finding from before findings had a class, a bead of
    // another project, and a record that is not one.
    let record = |name: &str, project: &str, bead: &str, classes: &[&str]| {
        let dir = world.path(&format!("state/units/{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        let regressions: Vec<String> = classes
            .iter()
            .map(|class| format!(r#"{{"round":1,"finding":"f","command":"c","test_names":[],"category":"{class}"}}"#))
            .collect();
        std::fs::write(
            dir.join("unit.json"),
            format!(
                r#"{{"project":"{project}","bead":"{bead}","regressions":[{}]}}"#,
                regressions.join(",")
            ),
        )
        .unwrap();
    };
    record(
        "old-1",
        "toy",
        "toy-8",
        &["error-path-cleanup", "double-call"],
    );
    record(
        "old-2",
        "toy",
        "toy-9",
        &["error-path-cleanup", "error-path-cleanup", ""],
    );
    record("old-3", "other", "other-1", &["elsewhere"]);
    std::fs::create_dir_all(world.path("state/units/old-4")).unwrap();
    std::fs::write(world.path("state/units/old-4/unit.json"), "not json").unwrap();

    let mut driver = Driver::start(world.config(), "toy", "toy-1", &OPTIONS).unwrap();
    assert_eq!(driver.run().unwrap(), Status::Ended);

    let build = std::fs::read_to_string(world.path("bin/state/brief-1.md")).unwrap();
    assert!(
        build.contains(
            "## What has broken earlier beads of this project\n\nThe adversarial testers of this project's earlier beads proved these classes of defect, most frequent first."
        ),
        "{build}"
    );
    assert!(
        build.contains(
            "- `error-path-cleanup`: 3 findings on 2 beads\n- `double-call`: 1 finding on 1 bead\n"
        ),
        "{build}"
    );
    assert!(!build.contains("elsewhere"), "{build}");
    // The tester is given the same keys, to reuse. The reviewer is not.
    let test = std::fs::read_to_string(world.path("bin/state/brief-2.md")).unwrap();
    assert!(
        test.contains("## Classes of finding recorded on this project")
            && test.contains("- `error-path-cleanup`: 3 findings on 2 beads"),
        "{test}"
    );
    let review = std::fs::read_to_string(world.path("bin/state/brief-3.md")).unwrap();
    assert!(!review.contains("error-path-cleanup"), "{review}");
}

#[test]
fn a_run_cut_off_by_the_death_of_the_factory_resumes_its_session() {
    let world = World::new("cut-off", "true");
    // The tester holds, so the only builder run is the build itself.
    std::fs::create_dir_all(world.path("bin/state")).unwrap();
    std::fs::write(world.path("bin/state/broke"), "").unwrap();
    let driver = Driver::start(world.config(), "toy", "toy-1", &OPTIONS).unwrap();
    let unit = driver.unit().id.to_string();
    // The factory died while the builder was running: the unit is at its
    // first step, and the run's session was written down when it began.
    drop(driver);
    let dir = world.path(&format!("state/units/{unit}"));
    std::fs::write(dir.join("in-flight-build.session"), "sess-cut-off\n").unwrap();

    let mut driver = Driver::resume(world.config(), &unit, &OPTIONS, None).unwrap();
    assert_eq!(driver.run().unwrap(), Status::Ended);
    let calls = world.calls();
    let builds: Vec<&Vec<String>> = calls.iter().filter(|call| call[0] == "builder").collect();
    // The build was not started over: the cut-off session was resumed and
    // told to carry on, with no new briefing.
    assert_eq!(builds[0][1], "sess-cut-off");
    let told = std::fs::read_to_string(world.path("bin/state/brief-1.md")).unwrap();
    assert!(
        told.starts_with("You were interrupted before you finished this step."),
        "{told}"
    );
    assert!(
        !dir.join("in-flight-build.session").exists(),
        "nothing is in flight any more"
    );
    let kinds = world.event_kinds(&driver);
    assert!(kinds.contains(&"run.cut_off".to_string()));
    // Later steps of the same unit are briefed as usual.
    let tester = calls.iter().position(|call| call[0] == "tester").unwrap();
    let brief =
        std::fs::read_to_string(world.path(&format!("bin/state/brief-{}.md", tester + 1))).unwrap();
    assert!(!brief.starts_with("You were interrupted"), "{brief}");
}

/// Turn on keeping branches on the remote for the test project.
fn keep_branches_on_the_remote(world: &World) {
    let project = world.path("config/projects/toy.toml");
    let text = std::fs::read_to_string(&project).unwrap();
    assert!(text.contains("\nflow = "));
    std::fs::write(
        &project,
        text.replacen("\nflow = ", "\nsync_branches = true\nflow = ", 1),
    )
    .unwrap();
}

/// A unit's events, as (kind, step, data).
fn events_of(world: &World, unit: &str) -> Vec<(String, String, serde_json::Value)> {
    std::fs::read_to_string(world.path(&format!("state/units/{unit}/events.jsonl")))
        .unwrap()
        .lines()
        .map(|line| {
            let event: serde_json::Value = serde_json::from_str(line).unwrap();
            (
                event["kind"].as_str().unwrap_or("").to_string(),
                event["step"].as_str().unwrap_or("").to_string(),
                event["data"].clone(),
            )
        })
        .collect()
}

#[test]
fn a_units_branch_is_on_the_remote_as_it_changes_and_while_its_agent_works() {
    let world = World::new("sync", "test -f feature.txt");
    keep_branches_on_the_remote(&world);
    // The builder commits what it has and goes on working, as it is told to.
    world.scenario("commit-as-you-go");
    let mut driver = Driver::start(world.config(), "toy", "toy-1", &OPTIONS).unwrap();
    assert_eq!(driver.run().unwrap(), Status::Ended);
    let unit = driver.unit().id.to_string();
    let events = events_of(&world, &unit);
    let remote = world.path("remote.git");

    // The builder's commit reached the remote while the builder was still
    // at work: before its run had ended, and so before the factory had
    // looked at anything it did.
    let first_sync = events
        .iter()
        .position(|(kind, _, _)| kind == "branch.synced")
        .expect("the branch was never pushed");
    let first_run_end = events
        .iter()
        .position(|(kind, _, _)| kind == "run.ended")
        .unwrap();
    assert!(first_sync < first_run_end, "{events:?}");
    let (_, step, data) = &events[first_sync];
    assert_eq!(
        (step.as_str(), data["when"].as_str()),
        ("build", Some("during the run"))
    );
    let early = data["commit"].as_str().unwrap();
    assert_eq!(run(&remote, "git", &["cat-file", "-t", early]), "commit");
    assert_eq!(
        run(&remote, "git", &["log", "-1", "--format=%s", early]),
        "wip: a line"
    );

    // What the factory itself puts on the branch goes there too: the
    // tester's regression test, as soon as the step that found it ended.
    let after: Vec<&str> = events
        .iter()
        .filter(|(kind, _, data)| kind == "branch.synced" && data["when"] == "after the step")
        .map(|(_, step, _)| step.as_str())
        .collect();
    assert!(after.contains(&"break"), "{after:?}");
    for (_, _, data) in events.iter().filter(|(kind, _, _)| kind == "branch.synced") {
        let commit = data["commit"].as_str().unwrap();
        assert_eq!(run(&remote, "git", &["cat-file", "-t", commit]), "commit");
    }
    assert!(
        !events
            .iter()
            .any(|(kind, _, _)| kind == "branch.sync_failed")
    );

    // Publishing is unchanged: the branch ends as the one commit of the bead.
    let branch = driver.unit().branch.clone();
    assert_eq!(
        run(
            &remote,
            "git",
            &["rev-list", "--count", &format!("main..{branch}")]
        ),
        "1"
    );
    assert_eq!(
        run(&remote, "git", &["log", "-1", "--format=%s", &branch]),
        "Add the feature"
    );
}

/// Once a unit's branch has a pull request, only publishing moves it:
/// every push to it runs the project's whole CI. Work in hand is still kept
/// on the remote, on a branch of its own that is gone again once the work
/// is published.
#[test]
fn work_in_hand_on_a_branch_with_a_pull_request_is_kept_beside_it_not_pushed_to_it() {
    let world = World::new("sync-held", "true");
    // CI fails once: the builder is handed the failure while the pull
    // request is open, and commits as it goes.
    world.with_github("pending red early pending green");
    keep_branches_on_the_remote(&world);
    world.scenario("commit-as-you-go");
    let mut driver = Driver::start(world.config(), "toy", "toy-1", &OPTIONS).unwrap();
    assert_eq!(driver.run().unwrap(), Status::Ended);
    let unit = driver.unit().id.to_string();
    let branch = driver.unit().branch.clone();
    let holding = format!("wip/{branch}");
    let events = events_of(&world, &unit);
    let opened = events
        .iter()
        .position(|(kind, _, _)| kind == "published.pull_request")
        .expect("no pull request was opened");

    // Before the pull request the unit's own branch is what is kept.
    assert!(
        events[..opened]
            .iter()
            .filter(|(kind, _, _)| kind == "branch.synced")
            .all(|(_, _, data)| data["branch"] == branch.as_str())
    );
    // After it, nothing but publishing touches that branch. The builder's
    // commit in the middle of the fix went to the holding branch, while
    // the builder was still at work.
    let after: Vec<&(String, String, serde_json::Value)> = events[opened..]
        .iter()
        .filter(|(kind, _, _)| kind == "branch.synced")
        .collect();
    assert!(
        after
            .iter()
            .any(|(_, step, data)| step == "fix" && data["when"] == "during the run"),
        "{after:?}"
    );
    for (_, _, data) in &after {
        assert_eq!(data["branch"], holding.as_str(), "{after:?}");
    }
    // The fix was published by the publish step, and the holding branch
    // went with it: the remote is left with no branch of work in hand.
    let unheld = events
        .iter()
        .position(|(kind, _, _)| kind == "branch.unheld")
        .expect("the holding branch was never removed");
    assert_eq!(events[unheld].2["error"], serde_json::Value::Null);
    let republished = events[opened..]
        .iter()
        .position(|(kind, _, _)| kind == "published.pushed")
        .map(|at| at + opened)
        .expect("the fix was never published");
    assert_eq!(unheld, republished + 1, "{events:?}");
    assert_eq!(
        run(
            &world.path("remote.git"),
            "git",
            &["for-each-ref", "--format=%(refname)", "refs/heads/wip"]
        ),
        ""
    );
    assert!(
        !events
            .iter()
            .any(|(kind, _, _)| kind == "branch.sync_failed")
    );
}

#[test]
fn an_absorbed_beads_branch_leaves_the_remote_once_its_sub_goals_branch_is_there() {
    use factory_driver::goal::Supervisor;
    use factory_slack::socket::Incoming;
    use serde_json::json;
    use std::time::{Duration, Instant};

    let world = World::new("sync-subgoal", "true");
    world.with_github("green");
    keep_branches_on_the_remote(&world);
    std::fs::create_dir_all(world.path("bin/state")).unwrap();
    std::fs::write(world.path("bin/state/broke"), "").unwrap();
    std::fs::create_dir_all(world.path("config/goals/toy")).unwrap();
    std::fs::write(
        world.path("config/goals/toy/main.toml"),
        "id = \"main\"\nflow = \"zig-sdk-bead\"\nship_flow = \"zig-sdk-ship\"\n[director]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\n",
    )
    .unwrap();
    let say = |role: &str, n: u32, json: serde_json::Value| {
        std::fs::write(
            world.path(&format!("bin/state/{role}-{n}.json")),
            json.to_string(),
        )
        .unwrap();
    };
    let direct = |fields: serde_json::Value| {
        let mut result = json!({
            "summary": "Decided.", "start": [], "continue": [], "file": [], "edit": [],
            "dropped_findings": [], "open_subgoals": [], "ship": [], "tell_foreman": [],
            "ask_owner": "", "complete": false, "evidence": "", "journal_entry": "",
            "project_memory": ""
        });
        for (key, value) in fields.as_object().unwrap() {
            result[key] = value.clone();
        }
        result
    };
    say(
        "intake",
        1,
        json!({"reply": "Here is the charter.", "ready": true, "objective": "Ship the feature.", "stop_when": "toy-1 has landed.", "plan": "One sub-goal.", "paths": [], "edits": []}),
    );
    say(
        "director",
        1,
        direct(json!({
            "open_subgoals": [{"id": "feature", "title": "The feature", "objective": "It works.", "beads": ["toy-1"], "paths": []}],
            "start": [{"bead": "toy-1", "reason": "the only one"}],
        })),
    );
    say("director", 2, direct(json!({"ask_owner": "Ship it now?"})));

    let mut sup = Supervisor::new(world.config(), None, vec!["OWNER".to_string()], false).unwrap();
    let id = sup.open_directly("toy", "Ship the feature").unwrap();
    sup.incoming(Incoming::Action {
        channel: String::new(),
        user: "OWNER".to_string(),
        action_id: "goal_start".to_string(),
        value: id,
        message_ts: String::new(),
    })
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    while sup.goals().next().unwrap().wakes < 2 {
        assert!(Instant::now() < deadline, "the bead was never absorbed");
        sup.pump(Duration::from_millis(200)).unwrap();
    }
    let goal = sup.goals().next().unwrap().clone();
    let sub = &goal.subgoals[0];
    assert_eq!(sub.absorbed.len(), 1);
    let remote = world.path("remote.git");

    // The sub-goal's branch is on the remote with the bead's commit, and
    // nothing else of the factory's is: the bead's own branch was there
    // while it was being worked on, and is gone now that it is not needed.
    let branches = run(
        &remote,
        "git",
        &["branch", "--list", "--format=%(refname:short)"],
    );
    let mut branches: Vec<&str> = branches.lines().collect();
    branches.sort_unstable();
    assert_eq!(branches, ["feat/feature", "main"]);
    assert_eq!(
        run(&remote, "git", &["rev-parse", "feat/feature"]),
        sub.absorbed[0].1
    );
    // In that order: the bead's branch was pushed as it changed; when the
    // bead was absorbed the sub-goal's branch was pushed; only then was
    // the bead's branch taken off the remote.
    let unit = goal.units[0].unit.clone().unwrap();
    let events = events_of(&world, &unit);
    let at = |kind: &str| {
        events
            .iter()
            .position(|(found, _, _)| found == kind)
            .unwrap_or_else(|| panic!("no {kind} in the unit's log"))
    };
    assert!(at("branch.synced") < at("absorbed"));
    assert!(at("absorbed") < at("subgoal.synced"));
    assert!(at("subgoal.synced") < at("branch.unsynced"));
    let (_, _, gone) = &events[at("branch.unsynced")];
    assert!(gone["error"].is_null(), "{gone}");
    let (_, _, kept) = &events[at("subgoal.synced")];
    assert_eq!(kept["branch"], "feat/feature");
    // No pull request yet: that is still the sub-goal's shipping to do.
    assert!(!world.gh_calls().iter().any(|call| call.starts_with("pr ")));
}

#[test]
fn a_parked_unit_continues_from_the_step_it_stopped_at() {
    let world = World::new("resume", "test -f $ALLOW || exit 127");
    let allow = world.path("allow");
    let project = world.path("config/projects/toy.toml");
    let text = std::fs::read_to_string(&project).unwrap();
    std::fs::write(&project, text.replace("$ALLOW", allow.to_str().unwrap())).unwrap();
    world.scenario("reject-once");

    let mut driver = Driver::start(world.config(), "toy", "toy-1", &OPTIONS).unwrap();
    let Status::Parked(reason) = driver.run().unwrap() else {
        panic!("expected the unit to park");
    };
    assert!(
        reason.contains("the command `test` could not run"),
        "{reason}"
    );
    let kinds = world.event_kinds(&driver);
    // The builder's first result had no evidence: code sent it back.
    assert!(kinds.iter().any(|kind| kind == "result.rejected"));
    assert_eq!(world.calls().len(), 2);
    assert_eq!(
        world.calls()[0][1],
        world.calls()[1][1],
        "a rejected result continues the session"
    );
    let unit_id = driver.unit().id.to_string();
    assert_eq!(driver.unit().position.step, "test");
    drop(driver);

    // The owner fixes the host, and the unit carries on where it stopped.
    std::fs::write(&allow, "").unwrap();
    let mut driver = Driver::resume(world.config(), &unit_id, &OPTIONS, None).unwrap();
    assert_eq!(driver.run().unwrap(), Status::Ended);
    let roles: Vec<String> = world.calls().iter().map(|call| call[0].clone()).collect();
    assert_eq!(
        roles.iter().filter(|role| *role == "builder").count(),
        3,
        "build, its retry, one fix"
    );
    assert!(matches!(
        Driver::resume(world.config(), &unit_id, &OPTIONS, None),
        Err(DriverError::AlreadyEnded(_))
    ));
}

#[test]
fn a_forbidden_file_put_back_is_not_stopped_and_a_ruling_reaches_every_later_step() {
    let world = World::new("trespass", "test -f feature.txt");
    world.scenario("trespass");
    let mut driver = Driver::start(world.config(), "toy", "toy-1", &OPTIONS).unwrap();
    // The builder wrote under `.github`: the unit parks, as it should.
    let Status::Parked(reason) = driver.run().unwrap() else {
        panic!("expected the unit to park");
    };
    assert!(
        reason.contains(".github/ci.yml (no step may change this path)"),
        "{reason}"
    );
    assert!(driver.unit().trespassed.contains_key(".github/ci.yml"));
    let unit_id = driver.unit().id.to_string();
    drop(driver);

    // Continued without being told to put it back, the builder leaves the
    // file where it is. That it did not touch it this time excuses nothing.
    let leave = Options {
        continued: Some("Carry on.".to_string()),
        ..OPTIONS
    };
    let mut driver = Driver::resume(world.config(), &unit_id, &leave, Some("build")).unwrap();
    let Status::Parked(reason) = driver.run().unwrap() else {
        panic!("expected the unit to park again");
    };
    assert!(
        reason.contains("an earlier step's change to it is still there"),
        "{reason}"
    );
    drop(driver);

    // Told to put it back, it does. Taking the file away is a change to
    // that path as well, and once parked the unit for a second time.
    // A ruling comes with it, for every step from here on.
    let before = world.calls().len();
    let back = Options {
        continued: Some("You may not change `.github`: put it back.".to_string()),
        ruling: Some("Wiring the check into CI is the owner's to do, not this bead's.".to_string()),
        ..OPTIONS
    };
    let mut driver = Driver::resume(world.config(), &unit_id, &back, Some("build")).unwrap();
    assert_eq!(driver.run().unwrap(), Status::Ended);
    assert!(driver.unit().trespassed.is_empty());
    assert!(!driver.unit().worktree.join(".github").exists());

    // The builder was told what to do in the note. The tester and the
    // reviewer never see a note, and they were shown the ruling.
    let calls = world.calls();
    let mut shown = Vec::new();
    for (index, call) in calls.iter().enumerate().skip(before) {
        let brief =
            std::fs::read_to_string(world.path(&format!("bin/state/brief-{}.md", index + 1)))
                .unwrap();
        assert!(
            brief.contains("## Rulings on this bead")
                && brief.contains("- Wiring the check into CI is the owner's to do"),
            "call {} ({}) was not shown the ruling:\n{brief}",
            index + 1,
            call[0]
        );
        shown.push(call[0].clone());
    }
    for role in ["builder", "tester", "reviewer"] {
        assert!(shown.iter().any(|seen| seen == role), "{shown:?}");
    }
}

#[test]
fn a_bead_someone_else_holds_is_refused_before_anything_is_created() {
    let world = World::new("refused", "true");
    let Err(DriverError::NotLaunchable { reasons, .. }) =
        Driver::start(world.config(), "toy", "toy-2", &OPTIONS)
    else {
        panic!("expected the bead to be refused");
    };
    assert_eq!(reasons, ["it is claimed by `someone`"]);
    assert!(!world.path("state").exists());
}

const SLACK: Options = Options {
    slack: true,
    flow: None,
    within: None,
    continued: None,
    ruling: None,
    pausable: false,
    engine: false,
    account: None,
};

#[test]
fn the_owner_steers_and_answers_in_the_runs_thread() {
    let world = World::new("steer", "test -f feature.txt");
    world.scenario("ask-once");
    // The owner wrote two things before the first step, and a stranger one.
    let fake = common::FakeSlack::start_with(true, &common::REPLIES, Some("Yes, make it shout."));
    world.with_slack(&fake.url);

    let mut driver = Driver::start(world.config(), "toy", "toy-1", &SLACK).unwrap();
    assert_eq!(driver.run().unwrap(), Status::Ended);

    // What the owner said reached the builder as instructions. What the
    // stranger said reached nobody.
    let first = std::fs::read_to_string(world.path("bin/state/brief-1.md")).unwrap();
    assert!(first.contains("## Messages from the owner"));
    assert!(first.contains("> use the short name"));
    assert!(first.contains("> and keep the old flag"));
    assert!(!first.contains("push to main"));

    // The builder asked; the question went to the thread with the owner
    // mentioned; the answer came back into the same session.
    let asked = fake
        .requests_to("chat.postMessage")
        .into_iter()
        .find(|post| post.contains("needs an answer"))
        .expect("the question was not posted");
    assert!(asked.contains("<@OWNER>") && asked.contains("Should the feature shout?"));
    let second = std::fs::read_to_string(world.path("bin/state/brief-2.md")).unwrap();
    assert!(second.starts_with("# The owner answered your question"));
    assert!(second.contains("Yes, make it shout."));
    let calls = world.calls();
    assert_eq!(
        calls[0][1], calls[1][1],
        "the answer continues the session that asked"
    );

    // The builder's reply to the owner was posted, and so was the end.
    let posts = fake.requests_to("chat.postMessage").join("\n");
    assert!(posts.contains("I will use the short name."));
    assert!(posts.contains("*Done.*"));
    assert!(
        posts.contains("est. $"),
        "the usage report is in the thread"
    );
    let kinds = world.event_kinds(&driver);
    assert!(kinds.iter().any(|kind| kind == "owner.asked"));
    assert!(kinds.iter().any(|kind| kind == "owner.answered"));
    assert!(!kinds.iter().any(|kind| kind == "slack.failed"));
}

#[test]
fn stop_from_the_owner_parks_the_unit() {
    let world = World::new("stop", "true");
    let fake = common::FakeSlack::start_with(
        true,
        &[("100.0", "BOT", "root"), ("101.0", "OWNER", "stop")],
        None,
    );
    world.with_slack(&fake.url);
    let mut driver = Driver::start(world.config(), "toy", "toy-1", &SLACK).unwrap();
    assert_eq!(
        driver.run().unwrap(),
        Status::Parked("the owner said stop".to_string())
    );
    assert!(world.calls().is_empty(), "no agent ran");
    assert!(
        fake.requests_to("chat.postMessage")
            .join("\n")
            .contains("*Parked*")
    );
}

#[test]
fn a_question_nobody_answers_parks_the_unit() {
    let world = World::new("silent", "true");
    world.scenario("ask-once");
    let fake = common::FakeSlack::start_with(true, &[("100.0", "BOT", "root")], None);
    world.with_slack(&fake.url);
    let factory = world.path("config/factory.toml");
    let text = std::fs::read_to_string(&factory).unwrap();
    std::fs::write(
        &factory,
        text.replace("answer_wait_secs = 20", "answer_wait_secs = 2"),
    )
    .unwrap();
    let mut driver = Driver::start(world.config(), "toy", "toy-1", &SLACK).unwrap();
    let Status::Parked(reason) = driver.run().unwrap() else {
        panic!("expected the unit to park");
    };
    assert!(
        reason.contains("got no answer") && reason.contains("Should the feature shout?"),
        "{reason}"
    );
}

#[test]
fn a_check_on_the_final_commit_sends_the_text_back_before_anything_is_pushed() {
    let world = World::new("publish-check", "true");
    let project = world.path("config/projects/toy.toml");
    let text = std::fs::read_to_string(&project).unwrap();
    std::fs::write(
        &project,
        format!(
            "{}\n[commands.trailer]\nargv = [\"sh\", \"-c\", \"git log -1 --format=%B | grep -q '^Changelog:' || {{ echo 'no Changelog trailer'; exit 1; }}\"]\ntimeout_secs = 30\n",
            text.replace("flow = ", "publish_checks = [\"trailer\"]\nflow = ")
        ),
    )
    .unwrap();

    let mut driver = Driver::start(world.config(), "toy", "toy-1", &OPTIONS).unwrap();
    assert_eq!(driver.run().unwrap(), Status::Ended);
    let kinds = world.event_kinds(&driver);
    let tail: Vec<&str> = kinds
        .iter()
        .filter(|kind| {
            kind.starts_with("describe:")
                || kind.starts_with("publish:")
                || *kind == "published.pushed"
        })
        .map(String::as_str)
        .collect();
    // The first message had no trailer: nothing was pushed until it did.
    assert_eq!(
        tail,
        [
            "describe:ready",
            "publish:failed",
            "describe:ready",
            "published.pushed",
            "publish:done"
        ]
    );
    let calls = world.calls();
    let second =
        std::fs::read_to_string(world.path(&format!("bin/state/brief-{}.md", calls.len())))
            .unwrap();
    assert!(second.contains("publish check `trailer`") && second.contains("no Changelog trailer"));
    assert!(second.contains("## Checks on the final commit"));
    let branch = format!("refs/heads/{}", driver.unit().branch);
    let remote = world.path("remote.git");
    assert_eq!(
        run(
            &remote,
            "git",
            &["rev-list", "--count", &format!("main..{branch}")]
        ),
        "1"
    );
    assert!(
        run(&remote, "git", &["log", "-1", "--format=%B", &branch])
            .contains("Changelog: none (test)")
    );
}

/// The steps from the first publication on.
fn after_publish(kinds: &[String]) -> Vec<&str> {
    let first = kinds
        .iter()
        .position(|kind| kind == "publish:done")
        .unwrap();
    kinds[first..]
        .iter()
        .filter(|kind| kind.contains(':') || *kind == "landed" || *kind == "bead.closed")
        .map(String::as_str)
        .collect()
}

/// A check fails while others are still running. The builder is not
/// handed the first failure alone: the factory waits for the run to finish
/// and hands back everything that failed, to be fixed in one round.
#[test]
fn a_failing_ci_run_is_waited_out_and_every_failure_goes_back_together() {
    let world = World::new("ci-all", "true");
    // One check fails with another still running, twice over; then that
    // one fails too. After the fix everything passes.
    world.with_github("pending failing failing red-both green");
    // The project says how a failure that happens only in CI can be
    // reproduced here, with a tool the factory keeps in its config.
    let project = world.path("config/projects/toy.toml");
    let text = std::fs::read_to_string(&project).unwrap()
        + "ci_failure_note = \"To see it fail here, run `sh {config}/tools/linux.sh`.\"\n";
    std::fs::write(&project, text).unwrap();
    let mut driver = Driver::start(world.config(), "toy", "toy-1", &OPTIONS).unwrap();
    assert_eq!(driver.run().unwrap(), Status::Ended);
    let unit = driver.unit().id.to_string();
    let events = events_of(&world, &unit);

    // The first failure was seen and said, and nothing was handed back yet.
    let failing: Vec<&(String, String, serde_json::Value)> = events
        .iter()
        .filter(|(kind, _, _)| kind == "ci.failing")
        .collect();
    assert_eq!(failing.len(), 1, "it is said once, not at every look");
    assert_eq!(failing[0].2["failed"], serde_json::json!(["fmt"]));
    assert_eq!(failing[0].2["running"], serde_json::json!(["unit"]));
    // One red, once nothing was running, with both failures in it.
    let red: Vec<&(String, String, serde_json::Value)> = events
        .iter()
        .filter(|(kind, _, _)| kind == "ci.red")
        .collect();
    assert_eq!(red.len(), 1, "{red:?}");
    assert_eq!(red[0].2["failed"], serde_json::json!(["fmt", "unit"]));
    let at = |kind: &str| events.iter().position(|(k, _, _)| k == kind).unwrap();
    assert!(at("ci.failing") < at("ci.red"));
    assert_eq!(
        events[at("ci.red")..]
            .iter()
            .filter(|(kind, step, _)| kind == "step.started" && step == "fix")
            .count(),
        1,
        "one round of fixing for both failures"
    );

    // The builder was told about both, each with its log.
    let calls = world.calls();
    let fix = (0..calls.len())
        .rfind(|&i| calls[i][0] == "builder")
        .expect("the builder was not run for the fix");
    let brief =
        std::fs::read_to_string(world.path(&format!("bin/state/brief-{}.md", fix + 1))).unwrap();
    for expected in [
        "- `fmt`",
        "- `unit`",
        "The end of the failing log of `fmt`",
        "The end of the failing log of `unit`",
        "More than one check failed.",
    ] {
        assert!(
            brief.contains(expected),
            "missing {expected:?} in:\n{brief}"
        );
    }
    let note = format!(
        "To see it fail here, run `sh {}/tools/linux.sh`.",
        world.path("config").display()
    );
    assert!(brief.contains(&note), "missing {note:?} in:\n{brief}");
}

/// A project command may be a tool the factory keeps for the project in its
/// own config directory. It is run from there, people are shown the name
/// the project gives it, and a builder it fails for is given the command as
/// it can be typed.
#[test]
fn a_command_can_be_a_tool_kept_in_the_factorys_config() {
    let world = World::new("config-tool", "true");
    std::fs::create_dir_all(world.path("config/tools")).unwrap();
    // It fails the first time it is run and passes after that.
    std::fs::write(
        world.path("config/tools/check.sh"),
        "#!/bin/sh\nhere=$(dirname \"$0\")\n[ -f \"$here/ran\" ] && exit 0\ntouch \"$here/ran\"\necho 'not on Linux'\nexit 1\n",
    )
    .unwrap();
    let project = world.path("config/projects/toy.toml");
    let text = std::fs::read_to_string(&project).unwrap();
    let old = "[commands.test]\nargv = [\"sh\", \"-c\", \"true\"]\n";
    assert!(text.contains(old), "{text}");
    std::fs::write(
        &project,
        text.replace(
            old,
            "[commands.test]\nargv = [\"sh\", \"{config}/tools/check.sh\"]\nshown = \"the check, on Linux\"\n",
        ),
    )
    .unwrap();
    let mut driver = Driver::start(world.config(), "toy", "toy-1", &OPTIONS).unwrap();
    assert_eq!(driver.run().unwrap(), Status::Ended);
    let unit = driver.unit().id.to_string();
    let events = events_of(&world, &unit);
    let tool = format!("{}/tools/check.sh", world.path("config").display());

    // It ran from the config directory, failed once, and passed after that.
    let ran: Vec<&(String, String, serde_json::Value)> = events
        .iter()
        .filter(|(kind, step, _)| kind == "command.ran" && step == "test")
        .collect();
    assert!(ran.len() >= 2, "{ran:?}");
    assert_eq!(ran[0].2["argv"], serde_json::json!(["sh", tool]));
    assert_eq!(ran[0].2["verdict"], "Failed { code: Some(1) }");
    assert_eq!(ran.last().unwrap().2["verdict"], "Passed");
    // The thread has the project's name for it, the builder the command.
    let said: Vec<String> = events
        .iter()
        .filter(|(kind, _, _)| kind == "slack.say")
        .map(|(_, _, data)| data["text"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(
        said.iter()
            .any(|text| text.contains("`the check, on Linux` failed")),
        "{said:?}"
    );
    assert!(
        !said.iter().any(|text| text.contains("check.sh")),
        "{said:?}"
    );
    let calls = world.calls();
    let told = format!("The command `test` (`sh {tool}`) failed");
    let briefed = (0..calls.len())
        .filter(|&i| calls[i][0] == "builder")
        .map(|i| {
            std::fs::read_to_string(world.path(&format!("bin/state/brief-{}.md", i + 1))).unwrap()
        })
        .any(|brief| brief.contains(&told));
    assert!(briefed, "no builder was told: {told}");
}

#[test]
fn a_red_check_goes_back_to_the_builder_and_green_is_merged_without_asking_anyone() {
    let world = World::new("land", "true");
    // First pull request: CI is still running, then one check fails. After
    // the fix: a look before every workflow has started, then running, then green.
    world.with_github("pending red early pending green");

    let mut driver = Driver::start(world.config(), "toy", "toy-1", &OPTIONS).unwrap();
    assert_eq!(driver.run().unwrap(), Status::Ended);
    let kinds = world.event_kinds(&driver);
    assert_eq!(
        after_publish(&kinds),
        [
            "publish:done",
            "ci:red",
            "fix:done",
            "fmt:pass",
            "index:pass",
            "readme:pass",
            "changelog:pass",
            "test:pass",
            "test-safe:pass",
            "samples:pass",
            "test-linux:pass",
            "reports:pass",
            "proof:pass",
            "break:held",
            "review:approve",
            "describe:ready",
            "publish:done",
            "ci:green",
            "landed",
            "bead.closed",
            "land:done",
        ]
    );

    // The reports and the test proof were each written by a command, once
    // for each version of the code (the build, the fix after the tester, the
    // fix after CI), into a directory outside the worktree, and the steps
    // that need them were briefed with where they are and what their index
    // says.
    assert_eq!(
        kinds
            .iter()
            .filter(|kind| *kind == "artifact.produced")
            .count(),
        6
    );
    let all_calls = world.calls();
    let review = all_calls
        .iter()
        .rposition(|call| call[0] == "reviewer")
        .unwrap();
    let review_brief =
        std::fs::read_to_string(world.path(&format!("bin/state/brief-{}.md", review + 1))).unwrap();
    assert!(
        review_brief.contains("## Reports: `reports`"),
        "{review_brief}"
    );
    assert!(review_brief.contains("No check failed."));
    assert!(review_brief.contains("A scan hit is a line to look at."));
    let unit_now = driver.unit().clone();
    assert!(
        review_brief.contains(&format!("scanned {}", unit_now.base)),
        "{{base}} is filled in"
    );
    let reports = &unit_now.artifacts["reports"];
    assert!(reports.path.join("INDEX.txt").exists());
    assert!(!reports.path.starts_with(&unit_now.worktree));
    // They describe the last checkpoint, which holds exactly what was published.
    let toy = world.path("toy");
    let tree = |commit: &str| run(&toy, "git", &["rev-parse", &format!("{commit}^{{tree}}")]);
    assert_eq!(tree(&reports.commit), tree(&unit_now.checkpoint));
    let tester = all_calls
        .iter()
        .rposition(|call| call[0] == "tester")
        .unwrap();
    let tester_brief =
        std::fs::read_to_string(world.path(&format!("bin/state/brief-{}.md", tester + 1))).unwrap();
    assert!(tester_brief.contains("## Reports: `reports`"));
    // The builder is not given them: its fix step does not say it needs them.
    let build_brief = std::fs::read_to_string(world.path("bin/state/brief-1.md")).unwrap();
    assert!(!build_brief.contains("## Reports"));

    // The failing log reached the builder's own session, with nothing else re-sent.
    let calls = world.calls();
    let fix = calls.iter().rposition(|call| call[0] == "builder").unwrap();
    assert_eq!(
        calls[fix][1], calls[0][1],
        "the CI failure goes to the session that built it"
    );
    let brief =
        std::fs::read_to_string(world.path(&format!("bin/state/brief-{}.md", fix + 1))).unwrap();
    assert!(brief.contains("GitHub CI failed on the pull request"));
    assert!(brief.contains("- `unit`: https://github.com/me/toy/actions/runs/55/job/66"));
    assert!(brief.contains("error: expected 4, found 5"));
    // No agent ran after the last publication: green needs no opinion.
    let agents_after_green = kinds
        .iter()
        .skip(
            kinds
                .iter()
                .rposition(|kind| kind == "publish:done")
                .unwrap(),
        )
        .filter(|kind| *kind == "run.started")
        .count();
    assert_eq!(agents_after_green, 0);

    // "early" showed one passed check and nothing else. With the required
    // list that is not green, so the merge waited for the real thing.
    let gh = world.gh_calls();
    let merges: Vec<&String> = gh
        .iter()
        .filter(|call| call.starts_with("pr merge"))
        .collect();
    assert_eq!(merges.len(), 1);
    assert!(merges[0].contains("--match-head-commit"));
    assert!(
        merges[0].contains("--subject Merge bead/1: Add the feature"),
        "{}",
        merges[0]
    );

    // What landed: the published commit, under a merge commit on main.
    let unit = driver.unit().clone();
    let remote = world.path("remote.git");
    let merged = unit.merged.clone().unwrap();
    assert_eq!(run(&remote, "git", &["rev-parse", "main"]), merged);
    assert_eq!(
        run(&remote, "git", &["log", "-1", "--format=%s", "main"]),
        "Merge bead/1: Add the feature"
    );
    assert_eq!(
        run(&remote, "git", &["show", "main:feature.txt"])
            .lines()
            .count(),
        3
    );
    // The merge commit says what landed, what passed, and which bead it was.
    let body = run(&remote, "git", &["log", "-1", "--format=%b", "main"]);
    assert!(
        body.starts_with("The branch adds feature.txt, which the bead asked for."),
        "{body}"
    );
    assert!(body.contains("\n\nTesting: on "), "{body}");
    assert!(
        body.replace('\n', " ")
            .contains("all 2 CI checks passed on pull request 7's head."),
        "{body}"
    );
    assert!(body.trim_end().ends_with("Bead: toy-1"), "{body}");
    assert!(
        body.lines().all(|line| line.chars().count() <= 72),
        "{body}"
    );

    // The bead is closed, the worktree and both copies of the branch are gone.
    let bead = std::fs::read_to_string(world.path("beads/toy-1.json")).unwrap();
    assert!(bead.contains("\"closed\"") && bead.contains(&merged));
    assert!(!unit.worktree.exists());
    let branches = run(&remote, "git", &["branch", "--list", "feat/*"]);
    assert_eq!(branches, "", "the remote branch should be deleted");
    assert_eq!(
        run(&world.path("toy"), "git", &["branch", "--list", "feat/*"]),
        ""
    );
}

#[test]
fn a_pull_request_that_conflicts_is_not_merged() {
    let world = World::new("conflict", "true");
    world.with_github("green");
    std::fs::write(world.path("bin/state-gh/mergeable"), "CONFLICTING").unwrap();
    let mut driver = Driver::start(world.config(), "toy", "toy-1", &OPTIONS).unwrap();
    let Status::Parked(reason) = driver.run().unwrap() else {
        panic!("expected the unit to park");
    };
    assert!(reason.contains("cannot be merged as it stands"), "{reason}");
    assert!(reason.contains("CONFLICTING"), "{reason}");
    assert!(
        !world
            .gh_calls()
            .iter()
            .any(|call| call.starts_with("pr merge"))
    );
    let bead = std::fs::read_to_string(world.path("beads/toy-1.json")).unwrap();
    assert!(bead.contains("\"in_progress\""), "the bead stays open");
}

#[test]
fn a_push_by_someone_else_stops_the_landing() {
    let world = World::new("foreign-push", "true");
    world.with_github("pending pending pending pending pending pending pending pending green");
    let mut driver = Driver::start(world.config(), "toy", "toy-1", &OPTIONS).unwrap();
    // Someone pushes to the unit's branch while CI is running.
    let remote = world.path("remote.git");
    let toy = world.path("toy");
    let pusher = std::thread::spawn(move || {
        for _ in 0..200 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let listed = Command::new("git")
                .args(["branch", "--list", "feat/*"])
                .current_dir(&remote)
                .output()
                .unwrap();
            let branch = String::from_utf8_lossy(&listed.stdout).trim().to_string();
            if !branch.is_empty() {
                run(
                    &toy,
                    "git",
                    &[
                        "commit",
                        "--quiet",
                        "--allow-empty",
                        "-m",
                        "not the factory's",
                    ],
                );
                let refspec = format!("+HEAD:refs/heads/{branch}");
                run(
                    &toy,
                    "git",
                    &["push", "--quiet", remote.to_str().unwrap(), &refspec],
                );
                return;
            }
        }
    });
    let status = driver.run().unwrap();
    pusher.join().unwrap();
    let Status::Parked(reason) = status else {
        panic!("expected the unit to park");
    };
    assert!(
        reason.contains("not the commit the factory published"),
        "{reason}"
    );
    assert!(
        !world
            .gh_calls()
            .iter()
            .any(|call| call.starts_with("pr merge"))
    );
}

#[test]
fn the_owner_can_put_an_ended_unit_onto_the_projects_current_flow() {
    // A unit that ended under a flow with no CI watching...
    let world = World::new("redirect", "true");
    world.with_github("green");
    let project = world.path("config/projects/toy.toml");
    let zig = std::fs::read_to_string(&project).unwrap();
    std::fs::copy(
        repo_config().join("flows/zig-sdk.toml"),
        world.path("config/flows/old.toml"),
    )
    .unwrap();
    let old = std::fs::read_to_string(world.path("config/flows/old.toml"))
        .unwrap()
        .replace("id = \"zig-sdk\"", "id = \"old\"")
        .replace("on.done = \"ci\"", "on.done = \"end\"");
    let cut = old.find("# No model from here on").unwrap();
    std::fs::write(world.path("config/flows/old.toml"), &old[..cut]).unwrap();
    std::fs::write(
        &project,
        zig.replace("flow = \"zig-sdk\"", "flow = \"old\""),
    )
    .unwrap();

    let mut driver = Driver::start(world.config(), "toy", "toy-1", &OPTIONS).unwrap();
    assert_eq!(driver.run().unwrap(), Status::Ended);
    assert!(driver.unit().merged.is_none());
    let unit_id = driver.unit().id.to_string();
    let agents = world.calls().len();
    drop(driver);

    // ...is moved to the `ci` step of the flow the project names now.
    std::fs::write(&project, zig).unwrap();
    assert!(matches!(
        Driver::resume(world.config(), &unit_id, &OPTIONS, None),
        Err(DriverError::AlreadyEnded(_))
    ));
    let mut driver = Driver::resume(world.config(), &unit_id, &OPTIONS, Some("ci")).unwrap();
    assert_eq!(driver.run().unwrap(), Status::Ended);
    assert!(driver.unit().merged.is_some());
    assert_eq!(world.calls().len(), agents, "no agent ran to land it");
    let bead = std::fs::read_to_string(world.path("beads/toy-1.json")).unwrap();
    assert!(bead.contains("\"closed\""));
}

/// A unit continued at a step that lacks what it needs parks, and says which
/// step makes that: here a unit that began before its flow proved tests by
/// command, taken up at the tester on the flow as it is now. Continued from
/// the step it is told, it makes what the flow has gained on its way.
#[test]
fn a_unit_continued_at_a_step_that_lacks_what_it_needs_is_told_where_to_go_on() {
    let world = World::new("gained", "true");
    world.with_github("green");
    let project = world.path("config/projects/toy.toml");
    let zig = std::fs::read_to_string(&project).unwrap();
    // The flow as it was before it proved tests by command, and without
    // its landing.
    let old = std::fs::read_to_string(repo_config().join("flows/zig-sdk.toml"))
        .unwrap()
        .replace("id = \"zig-sdk\"", "id = \"old\"")
        .replace("on.pass = \"proof\"", "on.pass = \"break\"")
        .replace(
            "needs = [\"reports\", \"test-proof\"]",
            "needs = [\"reports\"]",
        )
        .replace("on.done = \"ci\"", "on.done = \"end\"");
    assert!(
        old.contains("on.pass = \"break\"")
            && !old.contains("needs = [\"reports\", \"test-proof\"]")
    );
    let cut = old.find("# No model from here on").unwrap();
    let old = &old[..cut];
    // The step itself is taken out: from its comment to the step after it.
    let from = old
        .find("# Each test the change adds or alters is run")
        .unwrap();
    let to = from + old[from..].find("id = \"proof\"").unwrap();
    let to = to + old[to..].find("\n[[step]]").unwrap();
    let old = format!("{}{}", &old[..from], &old[to + 1..]);
    assert!(!old.contains("test-proof"), "{old}");
    std::fs::write(world.path("config/flows/old.toml"), &old).unwrap();
    std::fs::write(
        &project,
        zig.replace("flow = \"zig-sdk\"", "flow = \"old\""),
    )
    .unwrap();

    let mut driver = Driver::start(world.config(), "toy", "toy-1", &OPTIONS).unwrap();
    assert_eq!(driver.run().unwrap(), Status::Ended);
    assert!(!driver.unit().artifacts.contains_key("test-proof"));
    let unit_id = driver.unit().id.to_string();
    drop(driver);

    // Continued at the tester on the flow as it is now, its reports are of
    // an earlier commit and it has no proof at all. The first is said.
    std::fs::write(&project, zig).unwrap();
    let mut driver = Driver::resume(world.config(), &unit_id, &OPTIONS, Some("break")).unwrap();
    let Status::Parked(reason) = driver.run().unwrap() else {
        panic!("expected the unit to park");
    };
    assert!(
        reason.starts_with("step `break` needs `reports`: it describes commit `")
            && reason.ends_with(
                ". Step `reports` makes it: continue the unit from there, or from a step before it"
            ),
        "{reason}"
    );
    drop(driver);

    // Continued where it was told to, it passes the step the flow gained
    // and goes on to its landing.
    let mut driver = Driver::resume(world.config(), &unit_id, &OPTIONS, Some("reports")).unwrap();
    assert_eq!(driver.run().unwrap(), Status::Ended);
    assert!(driver.unit().artifacts.contains_key("test-proof"));
    assert!(driver.unit().merged.is_some());
}

#[test]
fn a_project_can_land_by_squashing() {
    let world = World::new("squash", "true");
    world.with_github("green");
    let project = world.path("config/projects/toy.toml");
    let text = std::fs::read_to_string(&project).unwrap();
    let text = text
        .replace(
            "merge_subject = \"Merge bead/{bead_short}: {title}\"",
            "merge_method = \"squash\"\nmerge_subject = \"{title} (#{number})\"",
        )
        .replace(
            "merge_body = \"{summary}\\n\\n{testing}\\n\\nBead: {bead}\"",
            "merge_body = \"{commit_body}\\n\\n{testing}\\n\\n{commit_trailers}\"",
        )
        .replace(
            "flow = \"zig-sdk\"",
            "flow = \"zig-sdk\"\npublication_note = \"End with the trailer Bead: <id>.\"",
        );
    assert!(
        text.contains("merge_method = \"squash\""),
        "the project record was not rewritten"
    );
    std::fs::write(&project, text).unwrap();

    let mut driver = Driver::start(world.config(), "toy", "toy-1", &OPTIONS).unwrap();
    assert_eq!(driver.run().unwrap(), Status::Ended);

    let merges: Vec<String> = world
        .gh_calls()
        .into_iter()
        .filter(|call| call.starts_with("pr merge"))
        .collect();
    assert_eq!(merges.len(), 1);
    assert!(
        merges[0].contains("--squash") && !merges[0].contains("--merge "),
        "{}",
        merges[0]
    );

    // One commit on main, with one parent: no merge commit.
    let remote = world.path("remote.git");
    let merged = driver.unit().merged.clone().unwrap();
    assert_eq!(run(&remote, "git", &["rev-parse", "main"]), merged);
    assert_eq!(run(&remote, "git", &["rev-list", "--count", "main"]), "2");
    assert_eq!(
        run(&remote, "git", &["rev-list", "--parents", "-1", "main"])
            .split(' ')
            .count(),
        2
    );
    assert_eq!(
        run(&remote, "git", &["log", "-1", "--format=%s", "main"]),
        "Add the feature (#7)"
    );
    // Its body is the change's own body, then what was tested.
    let body = run(&remote, "git", &["log", "-1", "--format=%b", "main"]);
    assert!(
        body.starts_with("It adds lines to feature.txt.\n\nTesting: on "),
        "{body}"
    );
    assert!(!body.contains("{"), "an unfilled placeholder: {body}");
    // The publisher was told the project's rule for trailers.
    let calls = world.calls();
    let brief = std::fs::read_to_string(world.path(&format!("bin/state/brief-{}.md", calls.len())))
        .unwrap();
    assert!(
        brief.contains("End with the trailer Bead: <id>."),
        "{brief}"
    );
    let bead = std::fs::read_to_string(world.path("beads/toy-1.json")).unwrap();
    assert!(bead.contains("\"closed\""));
}

/// A whole goal with a scripted director: opened by a message, agreed in
/// conversation, started by a button, moved on by a pipeline ending, and
/// ended by the owner's Accept with no agent involved.
#[test]
fn a_goal_is_agreed_run_on_events_and_ended_by_the_owner() {
    use factory_driver::goal::{Phase, Supervisor};
    use factory_slack::socket::Incoming;
    use std::time::{Duration, Instant};

    let world = World::new("goal", "true");
    world.with_github("green");
    let fake = common::FakeSlack::start_with(true, &[("100.0", "BOT", "root")], None);
    world.with_slack(&fake.url);
    world.scenario("minor-finding");
    std::fs::write(
        world.path("beads/toy-3.json"),
        r#"{"id":"toy-3","title":"The follow-up","description":"Later.","status":"open","priority":3}"#,
    )
    .unwrap();
    std::fs::create_dir_all(world.path("config/goals/toy")).unwrap();
    std::fs::write(
        world.path("config/goals/toy/main.toml"),
        "id = \"main\"\nmax_pipelines = 1\n[director]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\n",
    )
    .unwrap();
    let say = |role: &str, n: u32, json: &str| {
        std::fs::create_dir_all(world.path("bin/state")).unwrap();
        std::fs::write(world.path(&format!("bin/state/{role}-{n}.json")), json).unwrap();
    };
    let decide = |extra: &str| {
        format!(
            r#"{{"summary":"Decided.","start":[],"file":[],"edit":[],"dropped_findings":[],"ask_owner":"","complete":false,"evidence":"","journal_entry":"","project_memory":""{extra}}}"#
        )
        .replace("\"start\":[],\"file\":[],\"edit\":[],", &if extra.contains("\"start\"") || extra.contains("\"file\"") {
            String::new()
        } else {
            "\"start\":[],\"file\":[],\"edit\":[],".to_string()
        })
    };
    say(
        "intake",
        1,
        r#"{"reply":"What should toy-3 accept?","ready":false,"objective":"","stop_when":"","plan":"","edits":[]}"#,
    );
    say(
        "intake",
        2,
        r#"{"reply":"Here is the charter.","ready":true,"objective":"Ship the feature and its follow-up.","stop_when":"toy-1 has landed.","plan":"toy-1 first, then toy-3.","edits":[{"bead":"toy-3","acceptance_criteria":"- it exists","priority":-1,"defer":"unchanged","reason":"the owner confirmed"}]}"#,
    );
    // The first decision starts a bead someone else holds: code refuses it.
    say(
        "director",
        1,
        &decide(r#","start":[{"bead":"toy-2","reason":"urgent"}],"file":[],"edit":[]"#),
    );
    say(
        "director",
        2,
        &decide(r#","start":[{"bead":"toy-1","reason":"first"}],"file":[],"edit":[]"#).replace(
            r#""journal_entry":"""#,
            r#""journal_entry":"Started with toy-1.""#,
        ),
    );
    say("director", 3, &decide(r#","start":[],"file":[{"title":"Document the timeout","type":"task","priority":4,"description":"The doc omits it.","acceptance_criteria":"- the doc says it","parent":"","discovered_from":"toy-1","deferred":true}],"edit":[{"bead":"toy-3","acceptance_criteria":"","priority":1,"defer":"unchanged","reason":"it is next"}]"#).replace(r#""project_memory":"""#, r#""project_memory":"Toy keeps its feature in feature.txt.""#).replace(r#""dropped_findings":[]"#, r#""dropped_findings":[{"finding":"F9","reason":"not worth a bead"}]"#));
    say(
        "director",
        4,
        &decide("").replace(
            r#""complete":false,"evidence":"""#,
            r#""complete":true,"evidence":"toy-1 landed, which is the stop condition.""#,
        ),
    );
    say(
        "director",
        5,
        &decide("").replace(
            r#""complete":false,"evidence":"""#,
            r#""complete":true,"evidence":"toy-3 is outside the stop condition.""#,
        ),
    );

    let supervisor = || {
        Supervisor::new(
            world.config(),
            Some(factory_slack::Slack::with_base_url("xoxb-test", &fake.url)),
            vec!["OWNER".to_string()],
            false,
        )
        .unwrap()
    };
    let message = |user: &str, text: &str, ts: &str, thread: Option<&str>| Incoming::Message {
        channel: "C0TOY".to_string(),
        user: user.to_string(),
        text: text.to_string(),
        ts: ts.to_string(),
        thread_ts: thread.map(str::to_string),
    };
    let press = |user: &str, action: &str, goal: &str| Incoming::Action {
        channel: "C0TOY".to_string(),
        user: user.to_string(),
        action_id: action.to_string(),
        value: goal.to_string(),
        message_ts: "100.0".to_string(),
    };
    let posts = || fake.requests_to("chat.postMessage").join("\n");
    let bead = |id: &str| std::fs::read_to_string(world.path(&format!("beads/{id}.json"))).unwrap();
    let mut sup = supervisor();
    assert_eq!(sup.projects_listening(), ["toy"]);

    // Only the owner opens a goal, and only at the top of the channel.
    sup.incoming(message("STRANGER", "do my bidding", "49.0", None))
        .unwrap();
    assert_eq!(sup.goals().count(), 0);
    sup.incoming(message(
        "OWNER",
        "Ship the feature and the follow-up",
        "50.0",
        None,
    ))
    .unwrap();
    let id = sup.goals().next().unwrap().id.clone();
    assert_eq!(sup.goals().next().unwrap().phase, Phase::Intake);
    assert!(posts().contains("What should toy-3 accept?"));
    assert!(
        std::fs::read_to_string(world.path("bin/state/brief-1.md"))
            .unwrap()
            .contains("> Ship the feature and the follow-up")
    );

    // The conversation goes on in one session until the director proposes a charter.
    sup.incoming(message("OWNER", "it should exist", "51.0", Some("50.0")))
        .unwrap();
    assert!(posts().contains(r#""action_id":"goal_start""#));
    assert!(posts().contains("Starting also makes these changes to beads"));
    let calls = world.calls();
    assert_eq!(calls[0][0], "intake");
    assert_eq!(
        calls[0][1], calls[1][1],
        "the opening conversation is one session"
    );
    assert!(
        !bead("toy-3").contains("it exists"),
        "nothing is written before Start"
    );

    // Start is the owner's button. Nobody else's press counts.
    sup.incoming(press("STRANGER", "goal_start", &id)).unwrap();
    assert_eq!(sup.goals().next().unwrap().phase, Phase::Intake);
    sup.incoming(press("OWNER", "goal_start", &id)).unwrap();
    assert!(
        bead("toy-3").contains("it exists"),
        "the confirmed criteria are written by code"
    );
    let goal = sup.goals().next().unwrap().clone();
    assert_eq!((goal.phase.clone(), goal.wakes), (Phase::Running, 1));
    assert_eq!(goal.units.len(), 1);
    assert_eq!(goal.units[0].bead, "toy-1");
    let calls = world.calls();
    assert_eq!(calls[2][0], "director");
    assert_eq!(
        calls[2][1], calls[3][1],
        "a refused decision goes back to the same session"
    );
    assert_ne!(
        calls[2][1], calls[1][1],
        "the director does not inherit the conversation"
    );
    let refused = std::fs::read_to_string(world.path("bin/state/brief-4.md")).unwrap();
    assert!(
        refused.contains("`toy-2` may not be launched: it is claimed by `someone`"),
        "{refused}"
    );

    // The pipeline ending is the event that wakes the director again.
    let deadline = Instant::now() + Duration::from_secs(90);
    while sup.goals().next().unwrap().wakes < 2 {
        assert!(
            Instant::now() < deadline,
            "the pipeline never reported back"
        );
        sup.pump(Duration::from_millis(200)).unwrap();
    }
    assert!(
        bead("toy-1").contains("\"closed\""),
        "the pipeline landed its bead"
    );
    let calls = world.calls();
    let directors: Vec<usize> = (0..calls.len())
        .filter(|&i| calls[i][0] == "director")
        .collect();
    assert_eq!(directors.len(), 3);
    assert_ne!(
        calls[directors[2]][1], calls[directors[1]][1],
        "each wake is a fresh session"
    );
    let woke =
        std::fs::read_to_string(world.path(&format!("bin/state/brief-{}.md", directors[2] + 1)))
            .unwrap();
    assert!(
        woke.contains("The pipeline for `toy-1` ended: **landed**"),
        "{woke}"
    );
    assert!(
        woke.contains("F1 (review, minor): The doc omits the timeout"),
        "{woke}"
    );
    assert!(
        woke.contains("Started with toy-1."),
        "the journal carries over"
    );
    // It filed the finding as a deferred bead at once, and asked before
    // changing a bead that already existed.
    let filed = std::fs::read_dir(world.path("beads"))
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .find(|name| name.starts_with("filed-"))
        .expect("no bead was filed");
    let filed = std::fs::read_to_string(world.path("beads").join(filed)).unwrap();
    assert!(
        filed.contains("Document the timeout")
            && filed.contains("\"deferred\"")
            && filed.contains("\"priority\": 4"),
        "{filed}"
    );
    let goal_log =
        std::fs::read_to_string(world.path(&format!("state/goals/{id}/events.jsonl"))).unwrap();
    assert!(
        goal_log.contains(r#""kind":"finding.dropped""#) && goal_log.contains("not worth a bead")
    );
    assert!(goal_log.contains(r#""kind":"bead.filed""#));
    assert!(posts().contains(r#""action_id":"request_approve""#));
    assert!(
        bead("toy-3").contains("\"priority\": 3"),
        "not changed until approved"
    );
    assert!(sup.goals().next().unwrap().request.is_some());

    sup.incoming(press("OWNER", "request_approve", &id))
        .unwrap();
    assert!(bead("toy-3").contains("\"priority\": 1"));
    assert_eq!(sup.goals().next().unwrap().phase, Phase::ProposedComplete);
    assert!(posts().contains(r#""action_id":"goal_accept""#));
    let calls = world.calls();
    let approved =
        std::fs::read_to_string(world.path(&format!("bin/state/brief-{}.md", calls.len())))
            .unwrap();
    assert!(
        approved.contains("The owner approved your request"),
        "{approved}"
    );
    assert!(
        approved.contains("Toy keeps its feature in feature.txt."),
        "the project memory is read back"
    );

    // A reply instead of Accept is guidance: the goal goes on.
    sup.incoming(message(
        "OWNER",
        "not yet: what about toy-3?",
        "60.0",
        Some("50.0"),
    ))
    .unwrap();
    let calls = world.calls();
    let guided =
        std::fs::read_to_string(world.path(&format!("bin/state/brief-{}.md", calls.len())))
            .unwrap();
    assert!(guided.contains("> not yet: what about toy-3?"), "{guided}");
    assert_eq!(sup.goals().next().unwrap().phase, Phase::ProposedComplete);
    drop(sup);

    // Accept ends the goal with no agent, even after a restart.
    let before = world.calls().len();
    let mut sup = supervisor();
    assert_eq!(sup.goals().next().unwrap().phase, Phase::ProposedComplete);
    sup.incoming(press("OWNER", "goal_accept", &id)).unwrap();
    assert_eq!(sup.goals().next().unwrap().phase, Phase::Complete);
    assert_eq!(world.calls().len(), before, "accepting runs no agent");
    assert!(posts().contains("Goal complete"));
    // Everything the goal said went to its own thread.
    let off_thread = fake
        .requests_to("chat.postMessage")
        .into_iter()
        .filter(|post| !post.contains(r#""thread_ts":"50.0""#))
        .count();
    assert_eq!(off_thread, 0);
}

#[test]
fn a_goal_definition_that_cannot_run_is_refused() {
    use factory_driver::goal::check_definitions;
    let world = World::new("goal-defs", "true");
    std::fs::create_dir_all(world.path("config/goals/toy")).unwrap();
    let write = |flow: &str, model: &str| {
        std::fs::write(
            world.path("config/goals/toy/main.toml"),
            format!("id = \"main\"\n{flow}\n[director]\nmodel = \"{model}\"\neffort = \"high\"\n"),
        )
        .unwrap();
    };
    write("", "claude-opus-5-5");
    assert_eq!(
        check_definitions(&world.config()).unwrap(),
        ["toy/main: director claude-opus-5-5 (high), pipeline `build-break-review`, 1 at a time"]
    );
    write("flow = \"no-such-flow\"", "claude-opus-5-5");
    let err = check_definitions(&world.config()).unwrap_err().to_string();
    assert!(
        err.contains("goal definition `main` of `toy`: its pipeline cannot be used"),
        "{err}"
    );
    write("", "some-other-model");
    let err = check_definitions(&world.config()).unwrap_err().to_string();
    assert!(
        err.contains("the director's model `some-other-model` is not allowed"),
        "{err}"
    );
}

#[test]
fn a_run_refused_at_a_limit_continues_its_session_under_another_account() {
    let world = World::new("accounts", "true");
    world.scenario("limit-default-once");
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

    let mut driver = Driver::start(world.config(), "toy", "toy-1", &OPTIONS).unwrap();
    assert_eq!(driver.run().unwrap(), Status::Ended);

    // The builder started under the first account, was refused, and carried
    // on in the same session under the second. Nothing went back to the
    // first: it is out until its window resets.
    let calls = world.calls();
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
    assert!(calls[1..].iter().all(|call| call[3] == "tok-b"));

    let log = std::fs::read_to_string(driver.event_log()).unwrap();
    assert!(log.contains(r#""kind":"account.limited""#) && log.contains(r#""account":"a""#));
    assert!(log.contains(r#""account":"b""#));
    assert!(
        !log.contains("tok-b"),
        "a token must never reach the event log"
    );
    assert!(
        !std::fs::read_to_string(world.path("state/accounts.json"))
            .unwrap()
            .contains("tok-b")
    );

    // What the factory now knows about each account.
    let states = factory_claude::accounts::Tracker::new(&world.path("state")).load();
    let now = factory_core::now_unix();
    let back = states["a"].available_at("claude-opus-5-5", now);
    assert!(
        back > now + 3000 && back <= now + 3600,
        "a is out until its reset"
    );
    assert_eq!(states["b"].available_at("claude-opus-5-5", now), 0);
    assert_eq!(states["b"].source, "run");
}

/// A goal worked in a sub-goal: two beads, the second blocked by the first,
/// each absorbed into the sub-goal's branch by the light pipeline; then the
/// sub-goal shipped once as one pull request and squashed onto main.
#[test]
fn a_sub_goal_absorbs_its_beads_and_ships_as_one_pull_request() {
    use factory_driver::goal::{Phase, Supervisor};
    use factory_slack::socket::Incoming;
    use std::time::{Duration, Instant};

    let world = World::new("subgoal", "true");
    world.with_github("green");
    world.scenario("forget-closes");
    // The second bead is blocked by the first, which stays open until the
    // sub-goal lands.
    std::fs::write(
        world.path("beads/toy-3.json"),
        r#"{"id":"toy-3","title":"The follow-up","description":"Later.","acceptance_criteria":"- it follows","status":"open","open_blockers":["toy-1"]}"#,
    )
    .unwrap();
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
    // Every bead gets a GitHub issue, and the pull request must close them.
    let text = text.replace(
        "write = [\"me/toy\"]",
        "write = [\"me/toy\"]\nissues = true\nissue_types = { bug = \"Bug\", \"*\" = \"Task\" }\nissue_priority = { field = \"Priority\", options = [\"Critical\", \"High\", \"Medium\"] }",
    );
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
    say("intake", 1, r#"{"reply":"Here is the charter.","ready":true,"objective":"Ship the feature.","stop_when":"Both beads have landed.","plan":"One sub-goal: toy-1, then toy-3.","edits":[]}"#.to_string());
    say(
        "director",
        1,
        decide(
            r#"{"open_subgoal":{"id":"feature","title":"The feature and its follow-up","objective":"Both parts work.","beads":["toy-1","toy-3"]},"start":[{"bead":"toy-1","reason":"first"}]}"#,
        ),
    );
    say(
        "director",
        2,
        decide(r#"{"start":[{"bead":"toy-3","reason":"its blocker is absorbed"}]}"#),
    );
    say("director", 3, decide(r#"{"ship":true}"#));
    say(
        "director",
        4,
        decide(r#"{"complete":true,"evidence":"Both beads landed in one pull request."}"#),
    );

    let mut sup = Supervisor::new(world.config(), None, vec!["OWNER".to_string()], false).unwrap();
    // No Slack in this test: the goal is opened directly, as the owner's
    // message would open it.
    assert!(
        sup.projects_listening().is_empty(),
        "no Slack, so no channel is known"
    );
    let id = sup.open_directly("toy", "Ship the feature").unwrap();
    sup.incoming(Incoming::Action {
        channel: String::new(),
        user: "OWNER".to_string(),
        action_id: "goal_start".to_string(),
        value: id.clone(),
        message_ts: String::new(),
    })
    .unwrap();
    let remote = world.path("remote.git");
    let bead = |id: &str| std::fs::read_to_string(world.path(&format!("beads/{id}.json"))).unwrap();
    let goal = |sup: &Supervisor| sup.goals().next().unwrap().clone();
    let wait_for = |sup: &mut Supervisor, wakes: u32| {
        let deadline = Instant::now() + Duration::from_secs(120);
        while sup.goals().next().unwrap().wakes < wakes {
            assert!(
                Instant::now() < deadline,
                "the goal never reached wake {wakes}"
            );
            sup.pump(Duration::from_millis(200)).unwrap();
        }
    };

    // The sub-goal has a branch of its own, and the first bead is running in it.
    let opened = goal(&sup);
    assert_eq!(opened.subgoals.len(), 1);
    assert_eq!(opened.subgoals[0].branch, "feat/feature");
    assert!(opened.subgoals[0].worktree.join("README.md").exists());

    // The first bead is absorbed: one commit on the sub-goal's branch, no
    // pull request, nothing pushed, and its bead still open.
    wait_for(&mut sup, 2);
    let after_one = goal(&sup);
    let sub = &after_one.subgoals[0];
    assert_eq!(sub.absorbed.len(), 1);
    assert_eq!(sub.absorbed[0].0, "toy-1");
    assert_eq!(
        run(
            &sub.worktree,
            "git",
            &["rev-list", "--count", &format!("{}..HEAD", sub.base)]
        ),
        "1"
    );
    assert_eq!(
        run(&sub.worktree, "git", &["rev-parse", "HEAD"]),
        sub.absorbed[0].1
    );
    assert!(
        bead("toy-1").contains("\"in_progress\""),
        "an absorbed bead stays open"
    );
    assert!(
        !world.gh_calls().iter().any(|call| call.starts_with("pr ")),
        "a bead opens no pull request"
    );
    assert_eq!(
        run(&remote, "git", &["branch", "--list", "feat/*"]),
        "",
        "nothing is pushed yet"
    );
    // The second bead was started although its blocker is still open,
    // because the sub-goal has absorbed that blocker.
    assert_eq!(after_one.units[1].bead, "toy-3");

    // It builds on the first: its commit sits on top of it.
    wait_for(&mut sup, 3);
    let after_two = goal(&sup);
    let sub = &after_two.subgoals[0];
    assert_eq!(
        sub.absorbed
            .iter()
            .map(|(bead, _)| bead.as_str())
            .collect::<Vec<_>>(),
        ["toy-1", "toy-3"]
    );
    assert_eq!(
        run(
            &sub.worktree,
            "git",
            &["rev-list", "--count", &format!("{}..HEAD", sub.base)]
        ),
        "2"
    );
    assert_eq!(
        sub.state, "shipping",
        "the director shipped it at the third wake"
    );
    // Each bead ran the light pipeline: no samples, no pull request, no CI.
    let first_unit = after_two.units[0].unit.clone().unwrap();
    let first_log =
        std::fs::read_to_string(world.path(&format!("state/units/{first_unit}/events.jsonl")))
            .unwrap();
    for absent in [
        "\"step\":\"samples\"",
        "\"step\":\"ci\"",
        "published.pushed",
    ] {
        assert!(
            !first_log.contains(absent),
            "the bead pipeline ran {absent}"
        );
    }
    assert!(first_log.contains("\"kind\":\"absorbed\""));
    assert!(
        first_log.contains("\"kind\":\"command.skipped\"")
            && first_log.contains("\"when_changed\":[\"runtime\"]"),
        "the optimized tests are skipped when the bead does not touch the runtime"
    );

    // Shipping: the heavy pipeline once, one pull request with both commits,
    // squashed onto main, and both beads closed.
    wait_for(&mut sup, 4);
    let shipped = goal(&sup);
    assert_eq!(shipped.phase, Phase::ProposedComplete);
    assert_eq!(shipped.subgoals[0].state, "landed");
    assert!(shipped.subgoals[0].pull_request.is_some());
    let gh = world.gh_calls();
    assert_eq!(
        gh.iter()
            .filter(|call| call.starts_with("pr create"))
            .count(),
        1
    );
    let merges: Vec<&String> = gh
        .iter()
        .filter(|call| call.starts_with("pr merge"))
        .collect();
    assert_eq!(merges.len(), 1);
    assert!(merges[0].contains("--squash"));
    let ship_unit = shipped
        .units
        .iter()
        .find(|unit| unit.ship)
        .unwrap()
        .unit
        .clone()
        .unwrap();
    let ship_log =
        std::fs::read_to_string(world.path(&format!("state/units/{ship_unit}/events.jsonl")))
            .unwrap();
    for present in [
        "\"step\":\"test-safe\"",
        "\"step\":\"samples\"",
        "\"kind\":\"ci.green\"",
        "\"kind\":\"landed\"",
    ] {
        assert!(ship_log.contains(present), "shipping did not run {present}");
    }
    // The publisher of the whole was told which commits and beads it covers.
    // (Its last call is the retry after its first text was refused.)
    let calls = world.calls();
    let publishers: Vec<usize> = (0..calls.len())
        .filter(|&i| calls[i][0] == "publisher")
        .collect();
    let publisher = publishers[publishers.len() - 2];
    let ship_brief =
        std::fs::read_to_string(world.path(&format!("bin/state/brief-{}.md", publisher + 1)))
            .unwrap();
    assert!(
        ship_brief.contains("## The commits on this branch, one for each bead"),
        "{ship_brief}"
    );
    assert!(
        ship_brief.contains("The beads: toy-1, toy-3."),
        "{ship_brief}"
    );
    assert!(
        ship_brief.contains("### toy-3: The follow-up"),
        "the beads' criteria are in the brief"
    );

    // Neither bead had a GitHub issue: each got one when its pipeline
    // started, and the one pull request closes both.
    assert_eq!(
        gh.iter()
            .filter(|call| call.starts_with("issue create"))
            .count(),
        2
    );
    assert!(bead("toy-1").contains("https://github.com/me/toy/issues/1"));
    assert!(bead("toy-3").contains("https://github.com/me/toy/issues/2"));
    // Each issue also got the repository's own type and priority, by ID:
    // a task of the most urgent priority, which these beads are.
    let mutations: Vec<&String> = gh
        .iter()
        .filter(|call| call.starts_with("api graphql") && call.contains("mutation"))
        .collect();
    assert_eq!(
        mutations
            .iter()
            .filter(|call| call.contains("updateIssueIssueType") && call.contains("type=IT_task"))
            .count(),
        2,
        "{mutations:?}"
    );
    assert_eq!(
        mutations
            .iter()
            .filter(|call| call.contains("setIssueFieldValue")
                && call.contains("field=IFSS_priority")
                && call.contains("option=O_critical"))
            .count(),
        2,
        "{mutations:?}"
    );
    assert!(
        first_log.contains("\"kind\":\"issue.fields\""),
        "{first_log}"
    );
    assert!(
        ship_brief.contains("## GitHub issues this pull request closes"),
        "{ship_brief}"
    );
    // The publisher first left them out: code refused the text.
    assert!(
        ship_log.contains("`pr_body` does not close issue #1")
            && ship_log.contains("does not close issue #2"),
        "a pull request that closes no issue must be refused"
    );
    let pr_body = std::fs::read_to_string(world.path("bin/state-gh/pr-body")).unwrap();
    assert!(
        pr_body.contains("Closes #1") && pr_body.contains("Closes #2"),
        "{pr_body}"
    );
    assert_eq!(
        run(&remote, "git", &["rev-list", "--count", "main"]),
        "2",
        "one squashed commit on main"
    );
    assert_eq!(
        run(&remote, "git", &["log", "-1", "--format=%s", "main"]),
        "Add the feature (#7)"
    );
    assert!(run(&remote, "git", &["log", "-1", "--format=%b", "main"]).contains("Testing: on "));
    // Both beads' work is in it: the first bead's build and fix, the second's build.
    assert_eq!(
        run(&remote, "git", &["show", "main:feature.txt"])
            .lines()
            .count(),
        3
    );
    for id in ["toy-1", "toy-3"] {
        assert!(
            bead(id).contains("\"closed\""),
            "{id} is closed when the sub-goal lands"
        );
    }
    assert!(!shipped.subgoals[0].worktree.exists());
    assert_eq!(
        run(&world.path("toy"), "git", &["branch", "--list", "feat/*"]),
        ""
    );
    assert_eq!(run(&remote, "git", &["branch", "--list", "feat/*"]), "");

    sup.incoming(Incoming::Action {
        channel: String::new(),
        user: "OWNER".to_string(),
        action_id: "goal_accept".to_string(),
        value: id,
        message_ts: String::new(),
    })
    .unwrap();
    assert_eq!(sup.goals().next().unwrap().phase, Phase::Complete);
}

#[test]
fn a_foreman_runs_a_sub_goals_beads_and_the_director_only_plans_and_ships() {
    use factory_driver::goal::{Phase, Supervisor};
    use factory_slack::socket::Incoming;
    use std::time::{Duration, Instant};

    let world = World::new("foreman", "true");
    world.with_github("green");
    // Every reviewer leaves one minor finding for someone to rule on.
    world.scenario("minor-finding");
    std::fs::write(
        world.path("beads/toy-3.json"),
        r#"{"id":"toy-3","title":"The follow-up","description":"Later.","acceptance_criteria":"- it follows","status":"open","open_blockers":["toy-1"]}"#,
    )
    .unwrap();
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
    std::fs::write(&project, text).unwrap();
    std::fs::create_dir_all(world.path("config/goals/toy")).unwrap();
    std::fs::write(
        world.path("config/goals/toy/main.toml"),
        "id = \"main\"\nflow = \"zig-sdk-bead\"\nship_flow = \"zig-sdk-ship\"\n[director]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\n[foreman]\nmodel = \"claude-opus-5-5\"\neffort = \"medium\"\n",
    )
    .unwrap();
    let say = |role: &str, n: u32, json: String| {
        std::fs::create_dir_all(world.path("bin/state")).unwrap();
        std::fs::write(world.path(&format!("bin/state/{role}-{n}.json")), json).unwrap();
    };
    let with = |mut result: serde_json::Value, fields: &str| {
        let extra: serde_json::Value = serde_json::from_str(fields).unwrap();
        for (key, value) in extra.as_object().unwrap() {
            result[key] = value.clone();
        }
        result.to_string()
    };
    let direct = |fields: &str| {
        with(
            serde_json::json!({
                "summary": "Decided.", "start": [], "file": [], "edit": [], "dropped_findings": [],
                "open_subgoal": {"id": "", "title": "", "objective": "", "beads": []}, "ship": false,
                "tell_foreman": "", "ask_owner": "", "complete": false, "evidence": "",
                "journal_entry": "", "project_memory": ""
            }),
            fields,
        )
    };
    let foreman = |fields: &str| {
        with(
            serde_json::json!({
                "summary": "Decided.", "start": [], "file": [], "dropped_findings": [],
                "report": "working", "message": "", "journal_entry": ""
            }),
            fields,
        )
    };
    say("intake", 1, r#"{"reply":"Here is the charter.","ready":true,"objective":"Ship the feature.","stop_when":"Both beads have landed.","plan":"One sub-goal: toy-1, then toy-3.","edits":[]}"#.to_string());
    // The director plans and opens; it starts nothing.
    say(
        "director",
        1,
        direct(
            r#"{"open_subgoal":{"id":"feature","title":"The feature and its follow-up","objective":"Both parts work.","beads":["toy-1","toy-3"]}}"#,
        ),
    );
    // The foreman asks before it starts, and the director answers.
    say(
        "foreman",
        1,
        foreman(r#"{"report":"escalate","message":"Is toy-3 meant to build on toy-1?"}"#),
    );
    say(
        "director",
        2,
        direct(r#"{"tell_foreman":"Yes: toy-1, then toy-3."}"#),
    );
    say(
        "foreman",
        2,
        foreman(
            r#"{"start":[{"bead":"toy-1","reason":"first"}],"journal_entry":"The director confirmed the order."}"#,
        ),
    );
    // toy-1 is absorbed with a finding: the foreman files it to run here,
    // and is woken again at once to go on.
    say(
        "foreman",
        3,
        foreman(
            r#"{"file":[{"title":"Document the timeout","type":"task","priority":2,"description":"The reviewer found the doc silent on it.","acceptance_criteria":"- the doc says it","parent":"","discovered_from":"toy-1","deferred":false,"run_here":true}]}"#,
        ),
    );
    say(
        "foreman",
        4,
        foreman(r#"{"start":[{"bead":"toy-3","reason":"its blocker is absorbed"}]}"#),
    );
    say(
        "foreman",
        5,
        foreman(
            r#"{"start":[{"bead":"filed-4","reason":"the fix this sub-goal should not ship without"}],"dropped_findings":[{"finding":"F1","reason":"the same finding, already filed"}]}"#,
        ),
    );
    say(
        "foreman",
        6,
        foreman(
            r#"{"report":"ready","message":"toy-1, toy-3 and the doc fix are absorbed.","dropped_findings":[{"finding":"F1","reason":"fixed by the bead that just landed"}]}"#,
        ),
    );
    // The director hears from the sub-goal again only now.
    say("director", 3, direct(r#"{"ship":true}"#));
    say(
        "director",
        4,
        direct(r#"{"complete":true,"evidence":"The sub-goal landed as one pull request."}"#),
    );

    let mut sup = Supervisor::new(world.config(), None, vec!["OWNER".to_string()], false).unwrap();
    let id = sup.open_directly("toy", "Ship the feature").unwrap();
    sup.incoming(Incoming::Action {
        channel: String::new(),
        user: "OWNER".to_string(),
        action_id: "goal_start".to_string(),
        value: id.clone(),
        message_ts: String::new(),
    })
    .unwrap();
    let goal = |sup: &Supervisor| sup.goals().next().unwrap().clone();
    let roles =
        |world: &World, role: &str| world.calls().iter().filter(|call| call[0] == role).count();
    // The brief of the nth call of a role.
    let brief = |world: &World, role: &str, n: usize| {
        let calls = world.calls();
        let index = (0..calls.len())
            .filter(|&i| calls[i][0] == role)
            .nth(n - 1)
            .unwrap();
        std::fs::read_to_string(world.path(&format!("bin/state/brief-{}.md", index + 1))).unwrap()
    };

    // Start ran the whole exchange: the director opened the sub-goal, its
    // foreman escalated, the director answered, and the foreman started the
    // first bead. Nothing waited for a pipeline.
    let started = goal(&sup);
    assert_eq!(started.wakes, 2);
    let sub = &started.subgoals[0];
    let record = sub.foreman.clone().unwrap();
    assert_eq!(
        (record.wakes, record.waiting, record.exchanges),
        (2, None, 0)
    );
    assert_eq!(started.units.len(), 1);
    assert_eq!(started.units[0].bead, "toy-1");
    assert_eq!(started.units[0].subgoal.as_deref(), Some("feature"));
    let asked = brief(&world, "director", 2);
    assert!(
        asked.contains("- The foreman of sub-goal `feature` has **escalated** and waits for you:\n\n> Is toy-3 meant to build on toy-1?"),
        "{asked}"
    );
    let answered = brief(&world, "foreman", 2);
    assert!(
        answered.contains("- The director wrote to you:\n\n> Yes: toy-1, then toy-3."),
        "{answered}"
    );
    // The foreman reads the sub-goal's branch, not the default branch's
    // checkout the director uses, and not the branch's own worktree.
    let calls = world.calls();
    let place = |role: &str| {
        calls
            .iter()
            .find(|call| call[0] == role)
            .map(|call| call[2].clone())
            .unwrap()
    };
    assert!(
        place("foreman").ends_with("/foreman-feature"),
        "{}",
        place("foreman")
    );
    assert!(place("director").ends_with("/checkout"));

    // The beads run one after another, and no bead's end wakes the director.
    let deadline = Instant::now() + Duration::from_secs(180);
    while goal(&sup).wakes < 3 {
        assert!(
            Instant::now() < deadline,
            "the sub-goal was never reported ready"
        );
        sup.pump(Duration::from_millis(200)).unwrap();
        if goal(&sup).wakes < 3 {
            assert_eq!(
                roles(&world, "director"),
                2,
                "the director ran while its foreman was working"
            );
        }
    }
    let ready = goal(&sup);
    let sub = &ready.subgoals[0];
    assert_eq!(
        sub.absorbed
            .iter()
            .map(|(bead, _)| bead.as_str())
            .collect::<Vec<_>>(),
        ["toy-1", "toy-3", "filed-4"]
    );
    assert_eq!(sub.added, ["filed-4"]);
    assert_eq!(sub.foreman.clone().unwrap().wakes, 6);
    assert_eq!(
        sub.state, "shipping",
        "the director shipped it at its third wake"
    );
    assert_eq!(roles(&world, "foreman"), 6);

    // What the foreman was told at each step.
    let third = brief(&world, "foreman", 3);
    for expected in [
        "- The pipeline for `toy-1` ended: **absorbed**.",
        "  - F1 (review, minor): The doc omits the timeout at `feature.txt:1`",
        "## Wake 2\n\nThe director confirmed the order.",
        "- Not yet absorbed: toy-3.",
    ] {
        assert!(
            third.contains(expected),
            "missing {expected:?} in:\n{third}"
        );
    }
    let fourth = brief(&world, "foreman", 4);
    assert!(
        fourth.contains(
            "- The beads you filed to run here now exist and are part of this sub-goal: filed-4."
        ),
        "{fourth}"
    );
    assert!(
        fourth.contains("- Filed by you to run here: filed-4."),
        "{fourth}"
    );
    // The bead it filed is linked to the one it came from.
    let filed = std::fs::read_to_string(world.path("beads/filed-4.json")).unwrap();
    assert!(filed.contains("Discovered from toy-1"), "{filed}");
    // What the director was told when the sub-goal came back to it: the
    // report, and none of the beads' own events.
    let handed = brief(&world, "director", 3);
    assert!(
        handed.contains("- The foreman of sub-goal `feature` reports it **ready** to ship and waits for you:\n\n> toy-1, toy-3 and the doc fix are absorbed."),
        "{handed}"
    );
    assert!(!handed.contains("- The pipeline for"), "{handed}");

    // Shipping and ending are the director's and the owner's, as before.
    let deadline = Instant::now() + Duration::from_secs(180);
    while goal(&sup).wakes < 4 {
        assert!(Instant::now() < deadline, "the sub-goal never landed");
        sup.pump(Duration::from_millis(200)).unwrap();
    }
    let shipped = goal(&sup);
    assert_eq!(shipped.phase, Phase::ProposedComplete);
    assert_eq!(shipped.subgoals[0].state, "landed");
    assert_eq!(roles(&world, "director"), 4);
    assert_eq!(
        roles(&world, "foreman"),
        6,
        "shipping does not wake the foreman"
    );
    assert!(
        !world
            .path(&format!("state/goals/{id}/foreman-feature"))
            .exists(),
        "a landed sub-goal's foreman keeps no checkout"
    );
    let gh = world.gh_calls();
    assert_eq!(
        gh.iter()
            .filter(|call| call.starts_with("pr create"))
            .count(),
        1
    );
    let remote = world.path("remote.git");
    assert_eq!(
        run(&remote, "git", &["rev-list", "--count", "main"]),
        "2",
        "one squashed commit on main"
    );
    for bead in ["toy-1", "toy-3", "filed-4"] {
        let text = std::fs::read_to_string(world.path(&format!("beads/{bead}.json"))).unwrap();
        assert!(
            text.contains("\"closed\""),
            "{bead} is closed when the sub-goal lands"
        );
    }
    let log =
        std::fs::read_to_string(world.path(&format!("state/goals/{id}/events.jsonl"))).unwrap();
    for kind in [
        "foreman.ran",
        "foreman.decided",
        "foreman.told",
        "director.ran",
    ] {
        assert!(
            log.contains(&format!("\"kind\":\"{kind}\"")),
            "no {kind} in the goal's log"
        );
    }

    sup.incoming(Incoming::Action {
        channel: String::new(),
        user: "OWNER".to_string(),
        action_id: "goal_accept".to_string(),
        value: id,
        message_ts: String::new(),
    })
    .unwrap();
    assert_eq!(sup.goals().next().unwrap().phase, Phase::Complete);
}

#[test]
fn a_foreman_that_cannot_run_is_retried_and_then_handed_to_the_director() {
    use factory_driver::goal::Supervisor;
    use factory_slack::socket::Incoming;
    use std::time::{Duration, Instant};

    let world = World::new("foreman-fails", "true");
    // The project's commands and its scripted GitHub; nothing here ships.
    world.with_github("green");
    std::fs::create_dir_all(world.path("config/goals/toy")).unwrap();
    std::fs::write(
        world.path("config/goals/toy/main.toml"),
        "id = \"main\"\nflow = \"zig-sdk-bead\"\nship_flow = \"zig-sdk-ship\"\n[director]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\n[foreman]\nmodel = \"claude-opus-5-5\"\neffort = \"medium\"\n",
    )
    .unwrap();
    let say = |role: &str, n: u32, json: serde_json::Value| {
        std::fs::create_dir_all(world.path("bin/state")).unwrap();
        std::fs::write(
            world.path(&format!("bin/state/{role}-{n}.json")),
            json.to_string(),
        )
        .unwrap();
    };
    let direct = |fields: serde_json::Value| {
        let mut result = serde_json::json!({
            "summary": "Decided.", "start": [], "file": [], "edit": [], "dropped_findings": [],
            "open_subgoal": {"id": "", "title": "", "objective": "", "beads": []}, "ship": false,
            "tell_foreman": "", "ask_owner": "", "complete": false, "evidence": "",
            "journal_entry": "", "project_memory": ""
        });
        for (key, value) in fields.as_object().unwrap() {
            result[key] = value.clone();
        }
        result
    };
    say(
        "intake",
        1,
        serde_json::json!({"reply": "Here is the charter.", "ready": true, "objective": "Ship the feature.", "stop_when": "toy-1 has landed.", "plan": "One sub-goal.", "edits": []}),
    );
    say(
        "director",
        1,
        direct(
            serde_json::json!({"open_subgoal": {"id": "feature", "title": "The feature", "objective": "It works.", "beads": ["toy-1"]}}),
        ),
    );
    // The foreman has no script yet: every run returns nothing code accepts.

    let supervisor =
        || Supervisor::new(world.config(), None, vec!["OWNER".to_string()], false).unwrap();
    let mut sup = supervisor();
    let id = sup.open_directly("toy", "Ship the feature").unwrap();
    sup.incoming(Incoming::Action {
        channel: String::new(),
        user: "OWNER".to_string(),
        action_id: "goal_start".to_string(),
        value: id.clone(),
        message_ts: String::new(),
    })
    .unwrap();
    let foreman = |sup: &Supervisor| {
        sup.goals().next().unwrap().subgoals[0]
            .foreman
            .clone()
            .unwrap()
    };
    let calls = |role: &str| world.calls().iter().filter(|call| call[0] == role).count();

    // One failed wake: the refused result went back to the same session
    // twice, the event it was owed is kept, and a retry is set.
    let first = foreman(&sup);
    assert_eq!((first.failures, first.wakes, first.inbox.len()), (1, 0, 1));
    assert!(first.retry_at.is_some() && first.waiting.is_none());
    assert_eq!(calls("foreman"), 3);
    let sessions: Vec<String> = world
        .calls()
        .iter()
        .filter(|call| call[0] == "foreman")
        .map(|call| call[1].clone())
        .collect();
    assert!(sessions.iter().all(|session| session == &sessions[0]));
    assert_eq!(
        calls("director"),
        1,
        "one failure is not the director's business"
    );

    // A restart finds the foreman owed its event and wakes it again. After
    // five failures in a row the sub-goal goes to the director instead.
    say(
        "director",
        2,
        direct(serde_json::json!({"ask_owner": "The foreman cannot run. Try again?"})),
    );
    for failures in 2..=4 {
        drop(sup);
        sup = supervisor();
        sup.resume().unwrap();
        assert_eq!(foreman(&sup).failures, failures);
        assert_eq!(calls("director"), 1);
    }
    drop(sup);
    sup = supervisor();
    sup.resume().unwrap();
    let gave_up = foreman(&sup);
    assert_eq!(gave_up.waiting.as_deref(), Some("escalated"));
    assert_eq!((gave_up.failures, gave_up.retry_at), (0, None));
    assert_eq!(calls("foreman"), 15);
    assert_eq!(calls("director"), 2);
    let calls_now = world.calls();
    let index = calls_now
        .iter()
        .rposition(|call| call[0] == "director")
        .unwrap();
    let told =
        std::fs::read_to_string(world.path(&format!("bin/state/brief-{}.md", index + 1))).unwrap();
    assert!(
        told.contains("has **escalated** and waits for you:\n\n> The foreman could not be run five times in a row"),
        "{told}"
    );
    // A foreman that waits is left alone, restart or not.
    drop(sup);
    sup = supervisor();
    sup.resume().unwrap();
    assert_eq!(calls("foreman"), 15);

    // The owner says to try again, the director passes it on, and this time
    // the foreman runs: it still has the event it was first owed.
    say(
        "director",
        3,
        direct(serde_json::json!({"tell_foreman": "Try once more."})),
    );
    say(
        "foreman",
        16,
        serde_json::json!({"summary": "Starting.", "start": [{"bead": "toy-1", "reason": "the only one"}], "file": [], "dropped_findings": [], "report": "working", "message": "", "journal_entry": ""}),
    );
    say(
        "foreman",
        17,
        serde_json::json!({"summary": "Done.", "start": [], "file": [], "dropped_findings": [], "report": "ready", "message": "toy-1 is absorbed.", "journal_entry": ""}),
    );
    say(
        "director",
        4,
        direct(serde_json::json!({"ask_owner": "Ship it now?"})),
    );
    let thread = sup.goals().next().unwrap().thread_ts.clone();
    sup.incoming(Incoming::Message {
        channel: String::new(),
        user: "OWNER".to_string(),
        text: "Yes, try again.".to_string(),
        ts: "2.0".to_string(),
        thread_ts: Some(thread),
    })
    .unwrap();
    let running = foreman(&sup);
    assert_eq!(
        (running.wakes, running.waiting, running.exchanges),
        (1, None, 0)
    );
    let calls_now = world.calls();
    let index = calls_now
        .iter()
        .rposition(|call| call[0] == "foreman")
        .unwrap();
    let woken =
        std::fs::read_to_string(world.path(&format!("bin/state/brief-{}.md", index + 1))).unwrap();
    for expected in [
        "- The director opened this sub-goal and handed it to you.",
        "- The director wrote to you:\n\n> Try once more.",
    ] {
        assert!(
            woken.contains(expected),
            "missing {expected:?} in:\n{woken}"
        );
    }

    // The bead runs, and the foreman hands the sub-goal back.
    let deadline = Instant::now() + Duration::from_secs(120);
    while sup.goals().next().unwrap().wakes < 4 {
        assert!(
            Instant::now() < deadline,
            "the sub-goal was never reported ready"
        );
        sup.pump(Duration::from_millis(200)).unwrap();
    }
    let done = sup.goals().next().unwrap().clone();
    assert_eq!(done.subgoals[0].absorbed.len(), 1);
    assert_eq!(foreman(&sup).waiting.as_deref(), Some("ready"));
}

/// A goal whose foreman may run two beads at once, started, with both of
/// its beads running side by side on the paths given. Returns the supervisor.
fn two_beads_side_by_side(
    world: &World,
    scenario: &str,
    paths: [&str; 2],
) -> factory_driver::goal::Supervisor {
    use factory_driver::goal::Supervisor;
    use factory_slack::socket::Incoming;

    world.with_github("green");
    world.scenario(scenario);
    // No tester breaks anything here: the beads themselves are the subject.
    std::fs::create_dir_all(world.path("bin/state")).unwrap();
    std::fs::write(world.path("bin/state/broke"), "").unwrap();
    std::fs::write(
        world.path("beads/toy-3.json"),
        r#"{"id":"toy-3","title":"The other part","description":"Beside it.","acceptance_criteria":"- it is there","status":"open"}"#,
    )
    .unwrap();
    std::fs::create_dir_all(world.path("config/goals/toy")).unwrap();
    std::fs::write(
        world.path("config/goals/toy/main.toml"),
        "id = \"main\"\nflow = \"zig-sdk-bead\"\nship_flow = \"zig-sdk-ship\"\nmax_pipelines = 2\n[director]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\n[foreman]\nmodel = \"claude-opus-5-5\"\neffort = \"medium\"\nslots = 2\n",
    )
    .unwrap();
    let say = |role: &str, n: u32, json: serde_json::Value| {
        std::fs::write(
            world.path(&format!("bin/state/{role}-{n}.json")),
            json.to_string(),
        )
        .unwrap();
    };
    let direct = |fields: serde_json::Value| {
        let mut result = serde_json::json!({
            "summary": "Decided.", "start": [], "file": [], "edit": [], "dropped_findings": [],
            "open_subgoal": {"id": "", "title": "", "objective": "", "beads": []}, "ship": false,
            "tell_foreman": "", "ask_owner": "", "complete": false, "evidence": "",
            "journal_entry": "", "project_memory": ""
        });
        for (key, value) in fields.as_object().unwrap() {
            result[key] = value.clone();
        }
        result
    };
    let foreman = |fields: serde_json::Value| {
        let mut result = serde_json::json!({
            "summary": "Decided.", "start": [], "file": [], "dropped_findings": [],
            "report": "working", "message": "", "journal_entry": ""
        });
        for (key, value) in fields.as_object().unwrap() {
            result[key] = value.clone();
        }
        result
    };
    say(
        "intake",
        1,
        serde_json::json!({"reply": "Here is the charter.", "ready": true, "objective": "Ship both parts.", "stop_when": "Both beads have landed.", "plan": "One sub-goal, its two beads side by side.", "edits": []}),
    );
    say(
        "director",
        1,
        direct(
            serde_json::json!({"open_subgoal": {"id": "feature", "title": "Both parts", "objective": "Both parts work.", "beads": ["toy-1", "toy-3"]}}),
        ),
    );
    say(
        "foreman",
        1,
        foreman(serde_json::json!({"start": [
            {"bead": "toy-1", "reason": "independent of the other", "paths": [paths[0]]},
            {"bead": "toy-3", "reason": "independent of the other", "paths": [paths[1]]},
        ]})),
    );
    // The first to end leaves the other running; the second makes it whole.
    say("foreman", 2, foreman(serde_json::json!({})));
    say(
        "foreman",
        3,
        foreman(serde_json::json!({"report": "ready", "message": "Both are absorbed."})),
    );
    say(
        "director",
        2,
        direct(serde_json::json!({"ask_owner": "Ship it now?"})),
    );

    let mut sup = Supervisor::new(world.config(), None, vec!["OWNER".to_string()], false).unwrap();
    let id = sup.open_directly("toy", "Ship both parts").unwrap();
    sup.incoming(Incoming::Action {
        channel: String::new(),
        user: "OWNER".to_string(),
        action_id: "goal_start".to_string(),
        value: id,
        message_ts: String::new(),
    })
    .unwrap();
    // Both were started by the one decision, each on its own paths.
    let goal = sup.goals().next().unwrap().clone();
    assert_eq!(goal.units.len(), 2);
    for (unit, path) in goal.units.iter().zip(paths) {
        assert_eq!(unit.state, "running");
        assert_eq!(unit.paths, [path]);
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
    while sup.goals().next().unwrap().wakes < 2 {
        assert!(
            std::time::Instant::now() < deadline,
            "the two beads were never both absorbed"
        );
        sup.pump(std::time::Duration::from_millis(200)).unwrap();
    }
    sup
}

/// The event logs of a goal's two units: the one absorbed first, then the other.
fn in_order_absorbed(world: &World, goal: &factory_driver::goal::Goal) -> [String; 2] {
    let log = |bead: &str| {
        let unit = goal
            .units
            .iter()
            .find(|unit| unit.bead == bead)
            .and_then(|unit| unit.unit.clone())
            .unwrap();
        std::fs::read_to_string(world.path(&format!("state/units/{unit}/events.jsonl"))).unwrap()
    };
    let absorbed = &goal.subgoals[0].absorbed;
    assert_eq!(absorbed.len(), 2, "{absorbed:?}");
    [log(&absorbed[0].0), log(&absorbed[1].0)]
}

#[test]
fn two_beads_built_side_by_side_are_absorbed_one_onto_the_other() {
    let world = World::new("side-by-side", "true");
    // The second declaration is wrong on purpose: toy-3 writes under `toy-3`.
    let sup = two_beads_side_by_side(&world, "own-file", ["toy-1", "elsewhere"]);
    let goal = sup.goals().next().unwrap().clone();
    let sub = &goal.subgoals[0];
    assert_eq!(
        sub.foreman.clone().unwrap().waiting.as_deref(),
        Some("ready")
    );

    // Each ran in a worktree of its own from the same tip, at the same time:
    // both builders were called before either bead was absorbed.
    let calls = world.calls();
    let builders: Vec<&Vec<String>> = calls.iter().filter(|call| call[0] == "builder").collect();
    assert_eq!(builders.len(), 2);
    assert_ne!(builders[0][2], builders[1][2], "one worktree each");
    let [first, second] = in_order_absorbed(&world, &goal);

    // The first was absorbed as it stood. The second found the branch moved:
    // the first was merged into it, the checks ran again with no agent, and
    // only then was it absorbed.
    assert!(!first.contains("\"kind\":\"tip.merged\""));
    assert!(second.contains("\"kind\":\"tip.merged\""), "{second}");
    assert!(second.contains("\"conflicts\":[]"));
    let after: Vec<&str> = second
        .lines()
        .skip_while(|line| !line.contains("\"kind\":\"tip.merged\""))
        .filter(|line| line.contains("\"kind\":\"step.ended\""))
        .filter_map(|line| line.split("\"step\":\"").nth(1)?.split('"').next())
        .collect();
    assert_eq!(
        after,
        [
            "absorb",
            "recheck-fmt",
            "recheck-index",
            "recheck-readme",
            "recheck-changelog",
            "recheck-test",
            "recheck-test-safe",
            "absorb"
        ]
    );
    // The sub-goal's branch holds one commit for each, in a line, with both
    // beads' files.
    assert_eq!(
        run(
            &sub.worktree,
            "git",
            &["rev-list", "--count", &format!("{}..HEAD", sub.base)]
        ),
        "2"
    );
    assert_eq!(
        run(
            &sub.worktree,
            "git",
            &[
                "rev-list",
                "--count",
                "--merges",
                &format!("{}..HEAD", sub.base)
            ]
        ),
        "0"
    );
    assert_eq!(
        run(&sub.worktree, "git", &["rev-parse", "HEAD"]),
        sub.absorbed[1].1
    );
    assert_eq!(
        run(&sub.worktree, "git", &["rev-parse", "HEAD~1"]),
        sub.absorbed[0].1
    );
    for bead in ["toy-1", "toy-3"] {
        assert!(sub.worktree.join(bead).join("feature.txt").exists());
    }

    // Each builder was told which paths were its own, and the foreman was
    // told that toy-3 left the ones declared for it.
    let brief = |index: usize| {
        std::fs::read_to_string(world.path(&format!("bin/state/brief-{}.md", index + 1))).unwrap()
    };
    let told: Vec<String> = (0..calls.len())
        .filter(|&i| calls[i][0] == "builder")
        .map(brief)
        .collect();
    assert!(
        told.iter()
            .any(|text| text.contains("The paths set aside for this one: toy-1.")),
        "{}",
        told[0]
    );
    let reports: String = (0..calls.len())
        .filter(|&i| calls[i][0] == "foreman")
        .map(brief)
        .collect();
    assert!(
        reports
            .contains("  It changed files outside the paths declared for it: toy-3/feature.txt."),
        "{reports}"
    );
    assert!(!reports.contains("declared for it: toy-1/feature.txt"));
}

#[test]
fn two_beads_that_collide_are_merged_by_the_builder_of_the_second() {
    let world = World::new("collide", "true");
    // Both builders write the same file, whatever the foreman declared, and
    // the builder's first try at the conflict leaves the markers in.
    let sup = two_beads_side_by_side(&world, "lazy-merge", ["a", "b"]);
    let goal = sup.goals().next().unwrap().clone();
    let sub = &goal.subgoals[0];
    let [first, second] = in_order_absorbed(&world, &goal);
    assert!(!first.contains("\"kind\":\"tip.merged\""));
    assert!(
        second.contains("\"conflicts\":[\"feature.txt\"]"),
        "{second}"
    );

    // The conflict went to the second bead's builder, in the session that
    // built it. Its first answer still had markers, which code found before
    // anything was absorbed, and sent back.
    let conflicts = second.matches("\"outcome\":\"conflict\"").count();
    assert_eq!(conflicts, 2, "{second}");
    let calls = world.calls();
    let fixes: Vec<usize> = (0..calls.len())
        .filter(|&i| calls[i][0] == "builder")
        .skip(2)
        .collect();
    assert_eq!(fixes.len(), 2, "two fixes, by the builder");
    let brief = |index: usize| {
        std::fs::read_to_string(world.path(&format!("bin/state/brief-{}.md", index + 1))).unwrap()
    };
    assert!(
        brief(fixes[0]).contains("these files conflicted: feature.txt."),
        "{}",
        brief(fixes[0])
    );
    assert!(
        brief(fixes[1]).contains("These files still hold conflict markers"),
        "{}",
        brief(fixes[1])
    );
    let builders: Vec<&Vec<String>> = calls.iter().filter(|call| call[0] == "builder").collect();
    assert!(
        builders[..2].iter().any(|built| built[1] == builders[2][1]),
        "the fix continues a builder's session"
    );
    // After the builder's change the whole pipeline ran again on it.
    let after: Vec<&str> = second
        .lines()
        .rev()
        .take_while(|line| !line.contains("\"outcome\":\"conflict\""))
        .filter(|line| line.contains("\"kind\":\"step.ended\""))
        .filter_map(|line| line.split("\"step\":\"").nth(1)?.split('"').next())
        .collect();
    for step in ["fix", "test", "break", "review", "describe", "absorb"] {
        assert!(after.contains(&step), "{step} did not run again: {after:?}");
    }

    // What was absorbed has both beads' lines and no marker, as two commits
    // in a line.
    let merged = std::fs::read_to_string(sub.worktree.join("feature.txt")).unwrap();
    assert!(
        !merged.contains("<<<<<<<") && !merged.contains(">>>>>>>"),
        "{merged}"
    );
    assert!(merged.lines().count() >= 2, "{merged}");
    assert_eq!(
        run(
            &sub.worktree,
            "git",
            &["rev-list", "--count", &format!("{}..HEAD", sub.base)]
        ),
        "2"
    );
    assert_eq!(
        run(
            &sub.worktree,
            "git",
            &[
                "rev-list",
                "--count",
                "--merges",
                &format!("{}..HEAD", sub.base)
            ]
        ),
        "0"
    );
}

/// A goal whose director may have two sub-goals at once, run until both
/// have shipped: `left` holds toy-1 and `right` holds toy-3, on the paths
/// given. Returns the goal as it ended and the two ship pipelines' logs.
fn two_sub_goals_side_by_side(
    world: &World,
    scenario: &str,
    paths: [&str; 2],
) -> (factory_driver::goal::Goal, [String; 2]) {
    use factory_driver::goal::{Phase, Supervisor};
    use factory_slack::socket::Incoming;

    world.with_github("green");
    world.scenario(scenario);
    std::fs::create_dir_all(world.path("bin/state")).unwrap();
    std::fs::write(world.path("bin/state/broke"), "").unwrap();
    std::fs::write(
        world.path("beads/toy-3.json"),
        r#"{"id":"toy-3","title":"The other part","description":"Beside it.","acceptance_criteria":"- it is there","status":"open"}"#,
    )
    .unwrap();
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
    std::fs::write(&project, text).unwrap();
    std::fs::create_dir_all(world.path("config/goals/toy")).unwrap();
    std::fs::write(
        world.path("config/goals/toy/main.toml"),
        "id = \"main\"\nflow = \"zig-sdk-bead\"\nship_flow = \"zig-sdk-ship\"\nmax_pipelines = 2\n[director]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\nslots = 2\n[foreman]\nmodel = \"claude-opus-5-5\"\neffort = \"medium\"\n",
    )
    .unwrap();
    let say = |role: &str, n: u32, json: serde_json::Value| {
        std::fs::write(
            world.path(&format!("bin/state/{role}-{n}.json")),
            json.to_string(),
        )
        .unwrap();
    };
    let direct = |fields: serde_json::Value| {
        let mut result = serde_json::json!({
            "summary": "Decided.", "start": [], "file": [], "edit": [], "dropped_findings": [],
            "open_subgoals": [], "ship": [], "tell_foreman": [], "ask_owner": "",
            "complete": false, "evidence": "", "journal_entry": "", "project_memory": ""
        });
        for (key, value) in fields.as_object().unwrap() {
            result[key] = value.clone();
        }
        result
    };
    let foreman = |fields: serde_json::Value| {
        let mut result = serde_json::json!({
            "summary": "Decided.", "start": [], "file": [], "dropped_findings": [],
            "report": "working", "message": "", "journal_entry": ""
        });
        for (key, value) in fields.as_object().unwrap() {
            result[key] = value.clone();
        }
        result
    };
    say(
        "intake",
        1,
        serde_json::json!({"reply": "Here is the charter.", "ready": true, "objective": "Ship both parts.", "stop_when": "Both beads have landed.", "plan": "Two sub-goals, side by side.", "edits": []}),
    );
    say(
        "director",
        1,
        direct(serde_json::json!({"open_subgoals": [
            {"id": "left", "title": "The left part", "objective": "It works.", "beads": ["toy-1"], "paths": [paths[0]]},
            {"id": "right", "title": "The right part", "objective": "It works.", "beads": ["toy-3"], "paths": [paths[1]]},
        ]})),
    );
    // Each sub-goal's foreman starts its one bead, then reports it ready.
    say(
        "foreman",
        1,
        foreman(
            serde_json::json!({"start": [{"bead": "toy-1", "reason": "the only one", "paths": []}]}),
        ),
    );
    say(
        "foreman",
        2,
        foreman(
            serde_json::json!({"start": [{"bead": "toy-3", "reason": "the only one", "paths": []}]}),
        ),
    );
    for n in [3, 4] {
        say(
            "foreman",
            n,
            foreman(serde_json::json!({"report": "ready", "message": "Its bead is absorbed."})),
        );
    }
    // The director waits for both, then ships them in order.
    say("director", 2, direct(serde_json::json!({})));
    say(
        "director",
        3,
        direct(serde_json::json!({"ship": ["left", "right"]})),
    );
    say("director", 4, direct(serde_json::json!({})));
    say(
        "director",
        5,
        direct(serde_json::json!({"complete": true, "evidence": "Both sub-goals landed."})),
    );

    let mut sup = Supervisor::new(world.config(), None, vec!["OWNER".to_string()], false).unwrap();
    let id = sup.open_directly("toy", "Ship both parts").unwrap();
    sup.incoming(Incoming::Action {
        channel: String::new(),
        user: "OWNER".to_string(),
        action_id: "goal_start".to_string(),
        value: id,
        message_ts: String::new(),
    })
    .unwrap();
    // Both sub-goals are open, each with a foreman and its bead running on
    // the sub-goal's own paths.
    let started = sup.goals().next().unwrap().clone();
    assert_eq!(
        started
            .subgoals
            .iter()
            .map(|sub| (sub.id.as_str(), sub.state.as_str(), sub.paths.clone()))
            .collect::<Vec<_>>(),
        [
            ("left", "open", vec![paths[0].to_string()]),
            ("right", "open", vec![paths[1].to_string()])
        ]
    );
    assert_eq!(started.units.len(), 2);
    for (unit, (sub, path)) in started
        .units
        .iter()
        .zip([("left", paths[0]), ("right", paths[1])])
    {
        assert_eq!(unit.state, "running");
        assert_eq!(unit.subgoal.as_deref(), Some(sub));
        assert_eq!(unit.paths, [path], "a bead keeps to its sub-goal's paths");
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
    while sup.goals().next().unwrap().phase != Phase::ProposedComplete {
        assert!(
            std::time::Instant::now() < deadline,
            "the two sub-goals never both landed: {:?}",
            sup.goals()
                .next()
                .unwrap()
                .subgoals
                .iter()
                .map(|sub| (&sub.id, &sub.state))
                .collect::<Vec<_>>()
        );
        sup.pump(std::time::Duration::from_millis(200)).unwrap();
    }
    let goal = sup.goals().next().unwrap().clone();
    let ship_log = |sub: &str| {
        let unit = goal
            .units
            .iter()
            .find(|unit| unit.ship && unit.bead == sub)
            .and_then(|unit| unit.unit.clone())
            .unwrap();
        std::fs::read_to_string(world.path(&format!("state/units/{unit}/events.jsonl"))).unwrap()
    };
    let logs = [ship_log("left"), ship_log("right")];
    (goal, logs)
}

#[test]
fn two_sub_goals_worked_on_side_by_side_land_one_after_the_other() {
    let world = World::new("sub-goals-side-by-side", "true");
    let (goal, [left, right]) = two_sub_goals_side_by_side(&world, "own-file", ["toy-1", "toy-3"]);
    assert_eq!(goal.wakes, 5);
    for sub in &goal.subgoals {
        assert_eq!(sub.state, "landed");
        assert!(sub.foreman.clone().unwrap().wakes >= 2);
    }
    // Both were being built at once: the second foreman started its bead
    // before the first bead ended.
    let log = std::fs::read_to_string(world.path(&format!("state/goals/{}/events.jsonl", goal.id)))
        .unwrap();
    let position = |needle: &str| log.find(needle).unwrap_or_else(|| panic!("no {needle}"));
    assert!(
        position("\"bead\":\"toy-3\",\"paths\"") < position("\"kind\":\"subgoal.shipping\""),
        "{log}"
    );

    // Shipping was one at a time, in the order the director gave: `right`
    // waited in line while `left` shipped, and started when it had landed.
    assert!(log.contains("\"kind\":\"subgoal.queued\""), "{log}");
    assert!(log.contains("\"behind\":\"left\""), "{log}");
    let shipping: Vec<usize> = log
        .match_indices("\"kind\":\"subgoal.shipping\"")
        .map(|(at, _)| at)
        .collect();
    assert_eq!(shipping.len(), 2);
    assert!(left.contains("\"kind\":\"landed\""));

    // `left` was cut from the default branch as it still was. `right` was
    // not, by the time its turn came: the default branch, with `left` on it,
    // was merged in before anything was verified, so what CI saw and what
    // landed was the two together.
    assert!(left.contains("\"outcome\":\"current\""), "{left}");
    assert!(!left.contains("\"kind\":\"tip.merged\""));
    assert!(right.contains("\"kind\":\"tip.merged\""), "{right}");
    assert!(right.contains("\"conflicts\":[]") && right.contains("\"sibling\":false"));
    let steps: Vec<&str> = right
        .lines()
        .filter(|line| line.contains("\"kind\":\"step.ended\""))
        .filter_map(|line| line.split("\"step\":\"").nth(1)?.split('"').next())
        .collect();
    assert_eq!(&steps[..3], ["sync", "fmt", "index"], "{steps:?}");
    for step in ["test", "test-safe", "samples", "ci", "land"] {
        assert!(steps.contains(&step), "{step} did not run after the sync");
    }

    // Two pull requests, two squashed commits on main in order, both
    // beads' work there, and both beads closed.
    let gh = world.gh_calls();
    for call in ["pr create", "pr merge"] {
        assert_eq!(gh.iter().filter(|line| line.starts_with(call)).count(), 2);
    }
    let remote = world.path("remote.git");
    assert_eq!(run(&remote, "git", &["rev-list", "--count", "main"]), "3");
    assert_eq!(
        run(&remote, "git", &["rev-list", "--count", "--merges", "main"]),
        "0"
    );
    for bead in ["toy-1", "toy-3"] {
        assert!(
            !run(
                &remote,
                "git",
                &["show", &format!("main:{bead}/feature.txt")]
            )
            .is_empty()
        );
        let text = std::fs::read_to_string(world.path(&format!("beads/{bead}.json"))).unwrap();
        assert!(text.contains("\"closed\""), "{bead} is closed");
    }
    assert!(
        run(&remote, "git", &["show", "--stat", "--format=", "main~1"])
            .contains("toy-1/feature.txt"),
        "`left` landed first"
    );
}

#[test]
fn a_sub_goal_that_collides_with_one_that_landed_is_merged_before_it_ships() {
    let world = World::new("sub-goals-collide", "true");
    // Both sub-goals' builders write the same file, whatever was declared,
    // and the first try at the conflict leaves the markers in.
    let (goal, [left, right]) = two_sub_goals_side_by_side(&world, "lazy-merge", ["a", "b"]);
    for sub in &goal.subgoals {
        assert_eq!(sub.state, "landed");
    }
    assert!(!left.contains("\"kind\":\"tip.merged\""));
    assert!(right.contains("\"conflicts\":[\"feature.txt\"]"), "{right}");
    // The conflict went to a builder on the sub-goal's branch. Its first
    // answer left markers in, and nothing was pushed until they were gone.
    assert_eq!(
        right.matches("\"outcome\":\"conflict\"").count(),
        2,
        "{right}"
    );
    let before_push = &right[..right.find("\"kind\":\"published.pushed\"").unwrap()];
    assert_eq!(before_push.matches("\"outcome\":\"conflict\"").count(), 2);
    assert_eq!(
        right.matches("\"kind\":\"published.pushed\"").count(),
        1,
        "pushed once, after the conflict was resolved"
    );
    let calls = world.calls();
    let told: Vec<String> = (0..calls.len())
        .filter(|&i| calls[i][0] == "builder")
        .map(|i| {
            std::fs::read_to_string(world.path(&format!("bin/state/brief-{}.md", i + 1))).unwrap()
        })
        .collect();
    assert!(
        told.iter().any(|brief| brief.contains(
            "other work landed on the default branch first. The default branch has been merged into yours, and these files conflicted: feature.txt."
        )),
        "{}",
        told.last().unwrap()
    );
    assert!(
        told.iter()
            .any(|brief| brief.contains("These files still hold conflict markers"))
    );

    // What landed holds both sub-goals' lines and no marker.
    let remote = world.path("remote.git");
    let merged = run(&remote, "git", &["show", "main:feature.txt"]);
    assert!(
        !merged.contains("<<<<<<<") && !merged.contains(">>>>>>>"),
        "{merged}"
    );
    assert!(merged.lines().count() >= 2, "{merged}");
    assert_eq!(run(&remote, "git", &["rev-list", "--count", "main"]), "3");
}

#[test]
fn the_owner_talks_to_the_executive_which_opens_goals_and_says_which_run_together() {
    use factory_driver::goal::{Phase, Supervisor};
    use factory_slack::socket::Incoming;
    use serde_json::json;

    let world = World::new("executive", "true");
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
    std::fs::create_dir_all(world.path("bin/state")).unwrap();
    let say = |role: &str, n: u32, json: serde_json::Value| {
        std::fs::write(
            world.path(&format!("bin/state/{role}-{n}.json")),
            json.to_string(),
        )
        .unwrap();
    };
    let front = |reply: &str, definition: &str, handoff: &str| json!({"reply": reply, "open_goal": {"definition": definition, "handoff": handoff}});
    let charter = |paths: serde_json::Value| json!({"reply": "Here is the charter.", "ready": true, "objective": "Do it.", "stop_when": "It is done.", "plan": "One sub-goal.", "paths": paths, "edits": []});
    let asks = json!({
        "summary": "Decided.", "start": [], "file": [], "edit": [], "dropped_findings": [],
        "open_subgoals": [], "ship": [], "tell_foreman": [], "ask_owner": "Which bead first?",
        "complete": false, "evidence": "", "journal_entry": "", "project_memory": ""
    });
    let calls = |role: &str| world.calls().iter().filter(|call| call[0] == role).count();
    // The brief of the nth call of a role.
    let brief = |role: &str, n: usize| {
        let all = world.calls();
        let index = (0..all.len())
            .filter(|&i| all[i][0] == role)
            .nth(n - 1)
            .unwrap();
        std::fs::read_to_string(world.path(&format!("bin/state/brief-{}.md", index + 1))).unwrap()
    };
    let mut sup = Supervisor::new(world.config(), None, vec!["OWNER".to_string()], false).unwrap();
    let phase = |sup: &Supervisor, id: &str| {
        sup.goals()
            .find(|goal| goal.id == id)
            .map(|goal| goal.phase.clone())
            .unwrap()
    };
    let press = |sup: &mut Supervisor, action: &str, goal: &str| {
        sup.incoming(Incoming::Action {
            channel: String::new(),
            user: "OWNER".to_string(),
            action_id: action.to_string(),
            value: goal.to_string(),
            message_ts: String::new(),
        })
        .unwrap();
    };
    let write = |sup: &mut Supervisor, thread: &str, text: &str| {
        sup.incoming(Incoming::Message {
            channel: String::new(),
            user: "OWNER".to_string(),
            text: text.to_string(),
            ts: "9.9".to_string(),
            thread_ts: Some(thread.to_string()),
        })
        .unwrap();
    };

    // A question is answered, and opens nothing.
    say("executive", 1, front("Nothing is running.", "", ""));
    let (thread, opened) = sup.tell_executive("toy", None, "What is running?").unwrap();
    assert_eq!(opened, None);
    assert_eq!(sup.goals().count(), 0);
    let first = brief("executive", 1);
    for expected in [
        "# The owner wrote in the channel of project `toy`\n\n> What is running?",
        "- `main`: no standing objective",
        "## Goals of this project that have not ended\n\nNone.",
    ] {
        assert!(
            first.contains(expected),
            "missing {expected:?} in:\n{first}"
        );
    }

    // The same conversation goes on in the same session. A goal of a kind
    // the project does not have is refused and goes back to that session.
    say("executive", 2, front("Opening one.", "nope", "x"));
    say(
        "executive",
        3,
        front(
            "I have opened a goal. Its director takes this thread from here.",
            "main",
            "Nothing else is running on the project.",
        ),
    );
    say(
        "intake",
        1,
        json!({"reply": "Only under src/a?", "ready": false, "objective": "", "stop_when": "", "plan": "", "paths": [], "edits": []}),
    );
    let (_, opened) = sup
        .tell_executive("toy", Some(&thread), "Then ship the feature.")
        .unwrap();
    let a = opened.expect("the executive opened a goal");
    let sessions: Vec<String> = world
        .calls()
        .iter()
        .filter(|call| call[0] == "executive")
        .map(|call| call[1].clone())
        .collect();
    assert_eq!(sessions.len(), 3);
    assert!(sessions.iter().all(|session| session == &sessions[0]));
    assert_eq!(phase(&sup, &a), Phase::Intake);
    // The director was told what the executive learned and what was said.
    let handed = brief("intake", 1);
    for expected in [
        "The owner opened it with this message:\n\n> What is running?\n> \n> Then ship the feature.",
        "## From the project's executive, who took the owner's message and opened this goal\n\nNothing else is running on the project.",
        "executive:\n> Nothing is running.",
    ] {
        assert!(
            handed.contains(expected),
            "missing {expected:?} in:\n{handed}"
        );
    }
    // From here the thread is the goal's: the owner's reply reaches the
    // director, not the executive.
    say("intake", 2, charter(json!(["src/a"])));
    write(&mut sup, &thread, "Yes, only src/a.");
    assert_eq!(calls("executive"), 3);
    assert_eq!(calls("intake"), 2);
    let goal_a = sup.goals().find(|goal| goal.id == a).unwrap().clone();
    assert_eq!(goal_a.charter.unwrap().paths, ["src/a"]);

    // With nothing else running, Start starts it. No one is asked.
    say("director", 1, asks.clone());
    press(&mut sup, "goal_start", &a);
    assert_eq!(phase(&sup, &a), Phase::Running);
    assert_eq!(calls("executive"), 3);

    // A second goal, on other paths. Started while the first runs, it
    // waits for the executive, which lets it run beside the first.
    say(
        "executive",
        4,
        front(
            "Opened. It can keep to the docs.",
            "main",
            "A goal is running on src/a. This one could keep to docs.",
        ),
    );
    say("intake", 3, charter(json!(["docs"])));
    let (_, opened) = sup
        .tell_executive("toy", None, "Also fix the docs.")
        .unwrap();
    let b = opened.unwrap();
    assert!(brief("executive", 4).contains(&format!("- `{a}` (running): Do it.\n  Paths: src/a.")));
    say(
        "executive",
        5,
        json!({"summary": "The two touch different trees.", "start": [{"goal": b, "reason": "docs only"}]}),
    );
    say("director", 2, asks.clone());
    press(&mut sup, "goal_start", &b);
    assert_eq!(phase(&sup, &a), Phase::Running);
    assert_eq!(phase(&sup, &b), Phase::Running);
    let judged = brief("executive", 5);
    for expected in [
        "# Goals are waiting to run on project `toy`".to_string(),
        "This project may have 2 goals running at once.".to_string(),
        format!("## Running\n\n- `{a}` (running): Do it.\n  Paths: src/a."),
        format!("## Waiting, longest first\n\n- `{b}` (waiting to run): Do it.\n  Paths: docs."),
    ] {
        assert!(
            judged.contains(&expected),
            "missing {expected:?} in:\n{judged}"
        );
    }
    assert_ne!(
        world
            .calls()
            .iter()
            .rfind(|call| call[0] == "executive")
            .unwrap()[1],
        sessions[0],
        "deciding what runs is a fresh session, not the conversation"
    );

    // A third, which could touch anything. The project is at its limit, so
    // it queues with no one asked.
    say(
        "executive",
        6,
        front("Opened.", "main", "Two goals are running."),
    );
    say("intake", 4, charter(json!([])));
    let (_, opened) = sup
        .tell_executive("toy", None, "And tidy everything.")
        .unwrap();
    let c = opened.unwrap();
    press(&mut sup, "goal_start", &c);
    assert_eq!(phase(&sup, &c), Phase::Queued);
    assert_eq!(calls("executive"), 6);

    // One of the two ends. There is room, but the third has no paths: the
    // executive's wish to start it is refused by code, and it stays in line.
    say(
        "executive",
        7,
        json!({"summary": "Start it.", "start": [{"goal": c, "reason": "there is room"}]}),
    );
    say(
        "executive",
        8,
        json!({"summary": "It could touch anything, so it waits.", "start": []}),
    );
    let b_thread = sup
        .goals()
        .find(|goal| goal.id == b)
        .unwrap()
        .thread_ts
        .clone();
    write(&mut sup, &b_thread, "stop");
    assert_eq!(phase(&sup, &b), Phase::Stopped);
    assert_eq!(phase(&sup, &c), Phase::Queued);
    assert_eq!(calls("executive"), 8);
    let log =
        std::fs::read_to_string(world.path("state/projects/toy/executive/events.jsonl")).unwrap();
    assert!(
        log.contains("would run beside another goal, and its charter names no paths"),
        "{log}"
    );

    // When the last running goal ends, the one in line starts by itself.
    say("director", 3, asks);
    write(&mut sup, &thread, "stop");
    assert_eq!(phase(&sup, &a), Phase::Stopped);
    assert_eq!(phase(&sup, &c), Phase::Running);
    assert_eq!(
        calls("executive"),
        8,
        "nothing to judge when nothing else runs"
    );

    // The executive's record survives a restart.
    drop(sup);
    let sup = Supervisor::new(world.config(), None, vec!["OWNER".to_string()], false).unwrap();
    let executive = sup.executives().next().unwrap();
    assert_eq!(executive.threads.len(), 3);
    assert_eq!(executive.threads[&thread].goal.as_deref(), Some(a.as_str()));
    assert_eq!(executive.wakes, 6);
}

#[test]
fn a_parked_pipeline_is_continued_by_a_foreman_that_takes_over_the_sub_goal() {
    use factory_driver::goal::Supervisor;
    use factory_slack::socket::Incoming;
    use serde_json::json;
    use std::time::{Duration, Instant};

    let world = World::new("continue", "true");
    world.with_github("green");
    // The builder asks the owner a question nobody is there to answer, so
    // the bead's pipeline parks with its work kept.
    world.scenario("ask-once");
    std::fs::create_dir_all(world.path("bin/state")).unwrap();
    std::fs::write(world.path("bin/state/broke"), "").unwrap();
    std::fs::create_dir_all(world.path("config/goals/toy")).unwrap();
    // The goal starts under a definition with no foreman: its director
    // starts the beads itself.
    let definition = "id = \"main\"\nflow = \"zig-sdk-bead\"\nship_flow = \"zig-sdk-ship\"\n[director]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\n";
    std::fs::write(world.path("config/goals/toy/main.toml"), definition).unwrap();
    let say = |role: &str, n: u32, json: serde_json::Value| {
        std::fs::write(
            world.path(&format!("bin/state/{role}-{n}.json")),
            json.to_string(),
        )
        .unwrap();
    };
    let direct = |fields: serde_json::Value| {
        let mut result = json!({
            "summary": "Decided.", "start": [], "continue": [], "file": [], "edit": [],
            "dropped_findings": [], "open_subgoals": [], "ship": [], "tell_foreman": [],
            "ask_owner": "", "complete": false, "evidence": "", "journal_entry": "",
            "project_memory": ""
        });
        for (key, value) in fields.as_object().unwrap() {
            result[key] = value.clone();
        }
        result
    };
    let foreman = |fields: serde_json::Value| {
        let mut result = json!({
            "summary": "Decided.", "start": [], "continue": [], "file": [],
            "dropped_findings": [], "report": "working", "message": "", "journal_entry": ""
        });
        for (key, value) in fields.as_object().unwrap() {
            result[key] = value.clone();
        }
        result
    };
    say(
        "intake",
        1,
        json!({"reply": "Here is the charter.", "ready": true, "objective": "Ship the feature.", "stop_when": "toy-1 has landed.", "plan": "One sub-goal.", "paths": [], "edits": []}),
    );
    say(
        "director",
        1,
        direct(json!({
            "open_subgoals": [{"id": "feature", "title": "The feature", "objective": "It works.", "beads": ["toy-1"], "paths": []}],
            "start": [{"bead": "toy-1", "reason": "the only one"}],
        })),
    );
    // The director cannot move a parked pipeline by starting it again, so
    // it asks the owner.
    say(
        "director",
        2,
        direct(json!({"ask_owner": "The pipeline parked. Will you continue it?"})),
    );

    let supervisor =
        || Supervisor::new(world.config(), None, vec!["OWNER".to_string()], false).unwrap();
    let mut sup = supervisor();
    let id = sup.open_directly("toy", "Ship the feature").unwrap();
    sup.incoming(Incoming::Action {
        channel: String::new(),
        user: "OWNER".to_string(),
        action_id: "goal_start".to_string(),
        value: id.clone(),
        message_ts: String::new(),
    })
    .unwrap();
    let pump_until = |sup: &mut Supervisor, what: &str, done: &dyn Fn(&Supervisor) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(120);
        while !done(sup) {
            assert!(Instant::now() < deadline, "{what}");
            sup.pump(Duration::from_millis(200)).unwrap();
        }
    };
    pump_until(&mut sup, "the pipeline never parked", &|sup| {
        sup.goals().next().unwrap().wakes >= 2
    });
    let stuck = sup.goals().next().unwrap().clone();
    assert_eq!(stuck.units.len(), 1);
    assert_eq!(stuck.units[0].state, "parked");
    assert!(stuck.units[0].detail.contains("Should the feature shout?"));
    assert!(stuck.subgoals[0].foreman.is_none());
    let unit = stuck.units[0].unit.clone().unwrap();
    // Starting it again is not a way out: code refuses it before anything runs.
    let refused = factory_driver::goal::decision_problems(
        &world.config().goal_definitions("toy").unwrap()[0],
        &stuck,
        &[],
        &|_| Vec::new(),
        &[],
        &direct(json!({"start": [{"bead": "toy-1", "reason": "again"}]})),
    );
    assert!(!refused.is_empty());

    // The definition gains a foreman, and the factory is restarted. The open
    // sub-goal gets one, which is told it takes over part-way and continues
    // the parked pipeline from the step it names.
    std::fs::write(
        world.path("config/goals/toy/main.toml"),
        format!("{definition}[foreman]\nmodel = \"claude-opus-5-5\"\neffort = \"medium\"\n"),
    )
    .unwrap();
    say(
        "foreman",
        1,
        foreman(
            json!({"continue": [{"bead": "toy-1", "from": "build", "note": "Use the short name, and do not ask again."}]}),
        ),
    );
    say(
        "foreman",
        2,
        foreman(json!({"report": "ready", "message": "toy-1 is absorbed."})),
    );
    say("director", 3, direct(json!({"ask_owner": "Ship it now?"})));
    drop(sup);
    let mut sup = supervisor();
    sup.resume().unwrap();
    let adopted = sup.goals().next().unwrap().clone();
    let record = adopted.subgoals[0].foreman.clone().unwrap();
    assert_eq!((record.wakes, record.waiting.clone()), (1, None));
    assert_eq!(adopted.units.len(), 1, "the same pipeline goes on");
    assert_eq!(adopted.units[0].unit.as_deref(), Some(unit.as_str()));
    assert_eq!(adopted.units[0].state, "running");
    assert_eq!(adopted.units[0].continued, 1);
    assert_eq!(adopted.units[0].subgoal.as_deref(), Some("feature"));
    assert_eq!(adopted.wakes, 2, "the director was not woken for any of it");

    pump_until(
        &mut sup,
        "the continued pipeline was never absorbed",
        &|sup| sup.goals().next().unwrap().wakes >= 3,
    );
    let done = sup.goals().next().unwrap().clone();
    assert_eq!(done.units[0].state, "absorbed");
    assert_eq!(done.subgoals[0].absorbed.len(), 1);
    assert_eq!(done.subgoals[0].absorbed[0].0, "toy-1");
    assert_eq!(
        done.subgoals[0].foreman.clone().unwrap().waiting.as_deref(),
        Some("ready")
    );

    // What each was told.
    let calls = world.calls();
    let brief = |role: &str, n: usize| {
        let index = (0..calls.len())
            .filter(|&i| calls[i][0] == role)
            .nth(n - 1)
            .unwrap();
        std::fs::read_to_string(world.path(&format!("bin/state/brief-{}.md", index + 1))).unwrap()
    };
    let taken_over = brief("foreman", 1);
    for expected in [
        "- You are taking over this sub-goal part-way.".to_string(),
        "- `toy-1`: parked Parked:".to_string(),
        format!("units/{unit}/events.jsonl"),
        "The bead pipeline's steps, in order: build, fmt, index,".to_string(),
    ] {
        assert!(
            taken_over.contains(&expected),
            "missing {expected:?} in:\n{taken_over}"
        );
    }
    // The step it continued from was told why, in the foreman's words.
    let builders = calls.iter().filter(|call| call[0] == "builder").count();
    let continued = brief("builder", 2);
    for expected in [
        "This pipeline stopped and has been continued by the agent that runs its sub-goal.",
        "It had parked:",
        "Use the short name, and do not ask again.",
    ] {
        assert!(
            continued.contains(expected),
            "missing {expected:?} in:\n{continued}"
        );
    }
    assert!(builders >= 2);
    // The unit's own record shows one pipeline, redirected, then absorbed.
    let log =
        std::fs::read_to_string(world.path(&format!("state/units/{unit}/events.jsonl"))).unwrap();
    assert!(log.contains("\"kind\":\"unit.redirected\"") && log.contains("\"kind\":\"absorbed\""));
    let goal_log =
        std::fs::read_to_string(world.path(&format!("state/goals/{id}/events.jsonl"))).unwrap();
    for kind in ["subgoal.adopted", "pipeline.continued"] {
        assert!(
            goal_log.contains(&format!("\"kind\":\"{kind}\"")),
            "no {kind}"
        );
    }
}

#[test]
fn the_service_stops_at_a_safe_point_and_the_next_one_carries_on() {
    use factory_driver::goal::Supervisor;
    use factory_slack::socket::Incoming;
    use serde_json::json;
    use std::time::{Duration, Instant};

    let world = World::new("drain", "true");
    world.with_github("green");
    std::fs::create_dir_all(world.path("bin/state")).unwrap();
    std::fs::write(world.path("bin/state/broke"), "").unwrap();
    std::fs::create_dir_all(world.path("config/goals/toy")).unwrap();
    std::fs::write(
        world.path("config/goals/toy/main.toml"),
        "id = \"main\"\nflow = \"zig-sdk-bead\"\nship_flow = \"zig-sdk-ship\"\n[director]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\n[foreman]\nmodel = \"claude-opus-5-5\"\neffort = \"medium\"\n",
    )
    .unwrap();
    let say = |role: &str, n: u32, json: serde_json::Value| {
        std::fs::write(
            world.path(&format!("bin/state/{role}-{n}.json")),
            json.to_string(),
        )
        .unwrap();
    };
    let direct = |fields: serde_json::Value| {
        let mut result = json!({
            "summary": "Decided.", "start": [], "continue": [], "file": [], "edit": [],
            "dropped_findings": [], "open_subgoals": [], "ship": [], "tell_foreman": [],
            "ask_owner": "", "complete": false, "evidence": "", "journal_entry": "",
            "project_memory": ""
        });
        for (key, value) in fields.as_object().unwrap() {
            result[key] = value.clone();
        }
        result
    };
    let foreman = |fields: serde_json::Value| {
        let mut result = json!({
            "summary": "Decided.", "start": [], "continue": [], "file": [],
            "dropped_findings": [], "report": "working", "message": "", "journal_entry": ""
        });
        for (key, value) in fields.as_object().unwrap() {
            result[key] = value.clone();
        }
        result
    };
    say(
        "intake",
        1,
        json!({"reply": "Here is the charter.", "ready": true, "objective": "Ship the feature.", "stop_when": "toy-1 has landed.", "plan": "One sub-goal.", "paths": [], "edits": []}),
    );
    say(
        "director",
        1,
        direct(
            json!({"open_subgoals": [{"id": "feature", "title": "The feature", "objective": "It works.", "beads": ["toy-1"], "paths": []}]}),
        ),
    );
    say(
        "foreman",
        1,
        foreman(json!({"start": [{"bead": "toy-1", "reason": "the only one", "paths": []}]})),
    );
    say(
        "director",
        2,
        direct(json!({"ask_owner": "Noted. Anything else?"})),
    );
    say(
        "foreman",
        2,
        foreman(json!({"report": "ready", "message": "toy-1 is absorbed."})),
    );
    say("director", 3, direct(json!({"ask_owner": "Ship it now?"})));

    let flag = factory_driver::drain_flag(&world.path("state"));
    let supervisor =
        || Supervisor::new(world.config(), None, vec!["OWNER".to_string()], false).unwrap();
    let calls = |role: &str| world.calls().iter().filter(|call| call[0] == role).count();
    // Pump until the service has reached its safe point.
    let drain = |sup: &mut Supervisor| {
        let deadline = Instant::now() + Duration::from_secs(120);
        while !sup.drained() {
            assert!(Instant::now() < deadline, "the service never came to rest");
            sup.pump(Duration::from_millis(100)).unwrap();
        }
        sup.pump(Duration::ZERO).unwrap();
    };

    // A restart is asked for just as a goal starts its first pipeline. The
    // agents that were deciding finish; the pipeline stops before it has
    // run anything, and is still the goal's running pipeline.
    std::fs::create_dir_all(world.path("state")).unwrap();
    std::fs::write(&flag, "").unwrap();
    let mut sup = supervisor();
    let id = sup.open_directly("toy", "Ship the feature").unwrap();
    sup.incoming(Incoming::Action {
        channel: String::new(),
        user: "OWNER".to_string(),
        action_id: "goal_start".to_string(),
        value: id.clone(),
        message_ts: String::new(),
    })
    .unwrap();
    drain(&mut sup);
    assert_eq!(sup.live(), 0);
    let paused = sup.goals().next().unwrap().clone();
    assert_eq!(paused.units.len(), 1);
    assert_eq!(paused.units[0].state, "running", "nothing has ended");
    let unit = paused.units[0].unit.clone().unwrap();
    let log_path = world.path(&format!("state/units/{unit}/events.jsonl"));
    let log = std::fs::read_to_string(&log_path).unwrap();
    assert!(log.contains("\"kind\":\"unit.paused\""), "{log}");
    assert!(
        !log.contains("\"kind\":\"step.started\""),
        "no step was started"
    );
    assert_eq!(calls("builder"), 0);
    // What the owner writes while it is stopping is kept, not handled:
    // handling it would wake an agent.
    let thread = paused.thread_ts.clone();
    sup.park_incoming(
        vec![Incoming::Message {
            channel: String::new(),
            user: "OWNER".to_string(),
            text: "One more thing: keep it short.".to_string(),
            ts: "5.5".to_string(),
            thread_ts: Some(thread),
        }]
        .into_iter(),
    );
    assert_eq!(calls("director"), 1);
    drop(sup);

    // The next service clears the request, continues the pipeline where it
    // stopped, and handles what was kept.
    std::fs::remove_file(&flag).unwrap();
    let mut sup = supervisor();
    sup.resume().unwrap();
    assert_eq!(sup.live(), 1, "the pipeline is running again");
    sup.take_parked_incoming();
    assert_eq!(
        calls("director"),
        2,
        "the owner's message reached the director"
    );
    assert!(!world.path("state/incoming.jsonl").exists());
    let calls_now = world.calls();
    let index = calls_now
        .iter()
        .rposition(|call| call[0] == "director")
        .unwrap();
    let heard =
        std::fs::read_to_string(world.path(&format!("bin/state/brief-{}.md", index + 1))).unwrap();
    assert!(
        heard.contains("> One more thing: keep it short."),
        "{heard}"
    );

    // A restart is asked for again, now with the pipeline somewhere in the
    // middle. It finishes the step it is in. If that was its last, the
    // foreman is owed the news but is not woken by a service that is leaving.
    std::fs::write(&flag, "").unwrap();
    let foremen = calls("foreman");
    drain(&mut sup);
    assert_eq!(sup.live(), 0);
    assert_eq!(
        calls("foreman"),
        foremen,
        "no agent was woken while stopping"
    );
    let resting = sup.goals().next().unwrap().clone();
    let owed = resting.subgoals[0].foreman.clone().unwrap().inbox.len();
    match resting.units[0].state.as_str() {
        "running" => assert_eq!(owed, 0),
        "absorbed" => assert_eq!(owed, 1, "its end is kept for the foreman"),
        other => panic!("the pipeline is `{other}`"),
    }
    drop(sup);

    // And the service after that finishes the job.
    std::fs::remove_file(&flag).unwrap();
    let mut sup = supervisor();
    sup.resume().unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    while sup.goals().next().unwrap().wakes < 3 {
        assert!(
            Instant::now() < deadline,
            "the sub-goal was never reported ready"
        );
        sup.pump(Duration::from_millis(200)).unwrap();
    }
    let done = sup.goals().next().unwrap().clone();
    assert_eq!(done.units.len(), 1, "one pipeline all along");
    assert_eq!(done.units[0].state, "absorbed");
    assert_eq!(done.subgoals[0].absorbed.len(), 1);
    assert_eq!(calls("foreman"), 2);
    // Every step ran once: nothing was repeated and nothing skipped.
    assert_eq!(calls("builder"), 1);
    assert_eq!(calls("tester"), 1);
    assert_eq!(calls("reviewer"), 1);
    let log = std::fs::read_to_string(&log_path).unwrap();
    assert!(log.matches("\"kind\":\"unit.resumed\"").count() >= 1);
    let ran: Vec<&str> = log
        .lines()
        .filter(|line| line.contains("\"kind\":\"step.ended\""))
        .filter_map(|line| line.split("\"step\":\"").nth(1)?.split('"').next())
        .collect();
    let mut once = ran.clone();
    once.dedup();
    assert_eq!(ran, once);
    assert_eq!(ran.first(), Some(&"build"));
    assert_eq!(ran.last(), Some(&"absorb"));
}

#[test]
fn a_finding_is_proved_by_a_failing_test_and_a_fix_by_that_test_passing() {
    let world = World::new("proof", "true");
    // The tester's first reproduction passes when the factory runs it, and
    // the builder's first fix leaves the regression test failing.
    world.scenario("bogus-finding lazy-fix");
    let mut driver = Driver::start(world.config(), "toy", "toy-1", &OPTIONS).unwrap();
    assert_eq!(driver.run().unwrap(), Status::Ended);

    let calls = world.calls();
    let roles: Vec<&str> = calls.iter().map(|call| call[0].as_str()).collect();
    assert_eq!(
        roles,
        [
            "builder",
            "tester",
            "tester",
            "builder",
            "builder",
            "tester",
            "reviewer",
            "publisher"
        ]
    );
    assert_eq!(
        calls[1][1], calls[2][1],
        "the refused finding goes back to the same tester"
    );
    assert_eq!(
        calls[3][1], calls[4][1],
        "the refused fix goes back to the same builder"
    );
    let refused_finding = std::fs::read_to_string(world.path("bin/state/brief-3.md")).unwrap();
    assert!(
        refused_finding.contains("the factory ran the reproduction of `fails on empty input` (`true`) in your worktree and it passed"),
        "{refused_finding}"
    );
    let refused_fix = std::fs::read_to_string(world.path("bin/state/brief-5.md")).unwrap();
    assert!(
        refused_fix.contains("the regression test for `fails on empty input` still fails"),
        "{refused_fix}"
    );
    assert!(refused_fix.contains("sh regress.sh"), "{refused_fix}");
    // Only the finding that was proved joined the branch, once.
    let kinds = world.event_kinds(&driver);
    assert_eq!(
        kinds
            .iter()
            .filter(|kind| *kind == "regressions.added")
            .count(),
        1
    );
    assert_eq!(
        kinds
            .iter()
            .filter(|kind| *kind == "result.rejected")
            .count(),
        2
    );
    assert_eq!(driver.unit().regressions.len(), 1);
    assert_eq!(driver.unit().regressions[0].command, "sh regress.sh");
}

#[test]
fn a_report_line_that_must_not_pass_goes_back_to_the_builder() {
    let world = World::new("gate", "true");
    world.with_github("green");
    // The tester holds at once, so that only the report sends work back.
    std::fs::create_dir_all(world.path("bin/state")).unwrap();
    std::fs::write(world.path("bin/state/broke"), "").unwrap();
    let project = world.path("config/projects/toy.toml");
    let text = std::fs::read_to_string(&project).unwrap();
    let gated = text.replace(
        "argv = [\"sh\", \"-c\", \"echo scanned $1 > $0/INDEX.txt; echo No check failed. >> $0/INDEX.txt\", \"{out}\", \"{base}\"]",
        "argv = [\"sh\", \"-c\", \"if [ -f fixed.txt ]; then echo No check failed. > $0/INDEX.txt; else echo FAILED: changelog-check gitleaks > $0/INDEX.txt; fi\", \"{out}\"]\nfail_when = ['^FAILED:.*gitleaks']",
    );
    assert_ne!(gated, text, "the reports command was not rewritten");
    std::fs::write(&project, gated).unwrap();

    let mut driver = Driver::start(world.config(), "toy", "toy-1", &OPTIONS).unwrap();
    assert_eq!(driver.run().unwrap(), Status::Ended);
    let kinds = world.event_kinds(&driver);
    let reports: Vec<&str> = kinds
        .iter()
        .filter(|kind| {
            kind.starts_with("reports:") || kind.starts_with("fix:") || kind.starts_with("break:")
        })
        .map(String::as_str)
        .collect();
    assert_eq!(
        reports,
        ["reports:fail", "fix:done", "reports:pass", "break:held"]
    );
    assert_eq!(
        kinds
            .iter()
            .filter(|kind| *kind == "report.blocked")
            .count(),
        1
    );
    let fix_brief = std::fs::read_to_string(world.path("bin/state/brief-2.md")).unwrap();
    assert!(
        fix_brief.contains("show a result that must not pass"),
        "{fix_brief}"
    );
    assert!(
        fix_brief.contains("FAILED: changelog-check gitleaks"),
        "{fix_brief}"
    );
}

#[test]
fn a_new_test_that_passes_without_the_change_goes_back_to_the_builder_before_the_tester() {
    let world = World::new("test-proof", "true");
    world.with_github("green");
    // The tester holds at once, so that only the proof sends work back.
    std::fs::create_dir_all(world.path("bin/state")).unwrap();
    std::fs::write(world.path("bin/state/broke"), "").unwrap();
    // The project's tool, in small: the new test passes on the code as it
    // was until the builder has been told, and one more needs the new code.
    let project = world.path("config/projects/toy.toml");
    let text = std::fs::read_to_string(&project).unwrap();
    let proving = text.replace(
        "argv = [\"sh\", \"-c\", \"echo FAILS-AT-BASE feature.txt: the feature > $0/INDEX.txt\", \"{out}\"]",
        "argv = [\"sh\", \"-c\", \"echo NO-BUILD-AT-BASE feature.txt: needs the new code > $0/INDEX.txt; if [ -f fixed.txt ]; then echo FAILS-AT-BASE feature.txt: the feature >> $0/INDEX.txt; else echo PASSES-AT-BASE feature.txt: the feature >> $0/INDEX.txt; fi\", \"{out}\"]",
    );
    assert_ne!(proving, text, "the proof command was not rewritten");
    std::fs::write(&project, proving).unwrap();

    let mut driver = Driver::start(world.config(), "toy", "toy-1", &OPTIONS).unwrap();
    assert_eq!(driver.run().unwrap(), Status::Ended);
    let kinds = world.event_kinds(&driver);
    let steps: Vec<&str> = kinds
        .iter()
        .filter(|kind| {
            ["reports:", "proof:", "fix:", "break:", "review:"]
                .iter()
                .any(|step| kind.starts_with(step))
        })
        .map(String::as_str)
        .collect();
    assert_eq!(
        steps,
        [
            "reports:pass",
            "proof:fail",
            "fix:done",
            "reports:pass",
            "proof:pass",
            "break:held",
            "review:approve"
        ]
    );
    // The builder is given the test by name, and no tester ran before it.
    let calls = world.calls();
    assert_eq!(calls[1][0], "builder");
    let fix_brief = std::fs::read_to_string(world.path("bin/state/brief-2.md")).unwrap();
    assert!(
        fix_brief.contains("    PASSES-AT-BASE feature.txt: the feature"),
        "{fix_brief}"
    );
    // The proof that went on is on record as it stood, with the test that
    // could not be built without the change, and the tester and the
    // reviewer are both given it.
    let unit = driver.unit().clone();
    let proof =
        std::fs::read_to_string(unit.artifacts["test-proof"].path.join("INDEX.txt")).unwrap();
    assert_eq!(
        proof,
        "NO-BUILD-AT-BASE feature.txt: needs the new code\nFAILS-AT-BASE feature.txt: the feature\n"
    );
    for (brief, role) in [(3, "tester"), (4, "reviewer")] {
        assert_eq!(calls[brief - 1][0], role);
        let text =
            std::fs::read_to_string(world.path(&format!("bin/state/brief-{brief}.md"))).unwrap();
        assert!(
            text.contains("NO-BUILD-AT-BASE feature.txt: needs the new code")
                && text.contains("FAILS-AT-BASE is proof that a test tests the change."),
            "{text}"
        );
    }
}

/// A goal with a designer: a sub-goal is designed, and its design accepted by
/// the director, before anything is built in it; everyone who then works on
/// it is given the design.
#[test]
fn a_sub_goal_is_designed_and_the_design_accepted_before_any_bead_of_it_is_built() {
    use factory_driver::goal::{Supervisor, check_definitions};
    use factory_slack::socket::Incoming;
    use std::time::{Duration, Instant};

    let world = World::new("designer", "true");
    world.with_github("green");
    std::fs::create_dir_all(world.path("config/goals/toy")).unwrap();
    let definition = world.path("config/goals/toy/main.toml");
    let tiers = "[director]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\n[foreman]\nmodel = \"claude-opus-5-5\"\neffort = \"medium\"\n[designer]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\n";
    // A designer designs sub-goals, so a goal without them cannot have one.
    std::fs::write(
        &definition,
        "id = \"main\"\n[director]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\n[designer]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\n",
    )
    .unwrap();
    let refused = check_definitions(&world.config()).unwrap_err().to_string();
    assert!(
        refused.contains("a designer designs a sub-goal, so the definition needs a ship_flow"),
        "{refused}"
    );
    std::fs::write(
        &definition,
        format!("id = \"main\"\nflow = \"zig-sdk-bead\"\nship_flow = \"zig-sdk-ship\"\n{tiers}"),
    )
    .unwrap();
    let lines = check_definitions(&world.config()).unwrap().join("\n");
    assert!(lines.contains("designer claude-opus-5-5 (high)"), "{lines}");

    let say = |role: &str, n: u32, json: serde_json::Value| {
        std::fs::create_dir_all(world.path("bin/state")).unwrap();
        std::fs::write(
            world.path(&format!("bin/state/{role}-{n}.json")),
            json.to_string(),
        )
        .unwrap();
    };
    let with = |mut result: serde_json::Value, fields: serde_json::Value| {
        for (key, value) in fields.as_object().unwrap() {
            result[key] = value.clone();
        }
        result
    };
    let direct = |fields: serde_json::Value| {
        with(
            serde_json::json!({
                "summary": "Decided.", "start": [], "file": [], "edit": [], "dropped_findings": [],
                "open_subgoals": [], "ship": [], "tell_foreman": [], "design": [],
                "ask_owner": "", "complete": false, "evidence": "", "journal_entry": "", "project_memory": ""
            }),
            fields,
        )
    };
    let foreman = |fields: serde_json::Value| {
        with(
            serde_json::json!({
                "summary": "Decided.", "start": [], "file": [], "dropped_findings": [],
                "report": "working", "message": "", "journal_entry": ""
            }),
            fields,
        )
    };
    say(
        "intake",
        1,
        serde_json::json!({"reply": "Here is the charter.", "ready": true, "objective": "Ship the feature.", "stop_when": "toy-1 has landed.", "plan": "One sub-goal.", "edits": []}),
    );
    say(
        "director",
        1,
        direct(
            serde_json::json!({"open_subgoals": [{"id": "feature", "title": "The feature", "objective": "It works.", "beads": ["toy-1"], "paths": []}]}),
        ),
    );
    // The designer first says the split is wrong, and the director holds to it.
    say(
        "designer",
        1,
        serde_json::json!({"summary": "It cannot be designed as split.", "verdict": "split_wrong", "interface": "", "errors_and_invariants": "", "layout": "", "order": [], "open_questions": "", "split": "toy-1 needs a type that no bead adds."}),
    );
    say(
        "director",
        2,
        direct(
            serde_json::json!({"design": [{"subgoal": "feature", "accept": false, "note": "The split stands: toy-1 adds the type itself."}]}),
        ),
    );
    say(
        "designer",
        2,
        serde_json::json!({"summary": "One options type, added by toy-1.", "verdict": "designed", "interface": "`pub const FeatureOptions = struct { loud: bool = false };`", "errors_and_invariants": "`error.FeatureOff` when it is disabled. The caller owns the line it passes.", "layout": "`feature.txt` holds it.", "order": [{"bead": "toy-1", "reason": "it is the only one"}], "open_questions": "", "split": ""}),
    );
    say(
        "director",
        3,
        direct(serde_json::json!({"design": [{"subgoal": "feature", "accept": true, "note": ""}]})),
    );
    say(
        "foreman",
        1,
        foreman(
            serde_json::json!({"start": [{"bead": "toy-1", "reason": "the design puts it first"}]}),
        ),
    );
    say(
        "foreman",
        2,
        foreman(serde_json::json!({"report": "ready", "message": "toy-1 is absorbed."})),
    );
    say(
        "director",
        4,
        direct(serde_json::json!({"ask_owner": "Shall I ship it?"})),
    );

    let mut sup = Supervisor::new(world.config(), None, vec!["OWNER".to_string()], false).unwrap();
    let id = sup.open_directly("toy", "Ship the feature").unwrap();
    sup.incoming(Incoming::Action {
        channel: String::new(),
        user: "OWNER".to_string(),
        action_id: "goal_start".to_string(),
        value: id.clone(),
        message_ts: String::new(),
    })
    .unwrap();
    let goal = |sup: &Supervisor| sup.goals().next().unwrap().clone();
    let brief = |role: &str, n: usize| {
        let calls = world.calls();
        let index = (0..calls.len())
            .filter(|&i| calls[i][0] == role)
            .nth(n - 1)
            .unwrap_or_else(|| panic!("no call {n} of the {role}"));
        std::fs::read_to_string(world.path(&format!("bin/state/brief-{}.md", index + 1))).unwrap()
    };

    // Starting the goal ran the whole exchange: the director opened the
    // sub-goal, its designer objected, the director sent that back, the
    // designer designed, the director accepted, and only then was the
    // foreman woken, which started the bead.
    let tiers: Vec<String> = world
        .calls()
        .iter()
        .map(|call| call[0].clone())
        .filter(|role| ["director", "designer", "foreman"].contains(&role.as_str()))
        .collect();
    assert_eq!(
        tiers,
        [
            "director", "designer", "director", "designer", "director", "foreman"
        ]
    );
    let started = goal(&sup);
    let design = started.subgoals[0].design.clone().unwrap();
    assert_eq!((design.state.as_str(), design.rounds), ("accepted", 2));
    assert!(
        design
            .text
            .starts_with("### The public interface\n\n`pub const FeatureOptions"),
        "{}",
        design.text
    );
    assert_eq!(
        std::fs::read_to_string(world.path(&format!("state/goals/{id}/design-feature.md")))
            .unwrap(),
        design.text
    );

    let first = brief("designer", 1);
    assert!(
        first.starts_with("# The design of sub-goal `feature` of goal `"),
        "{first}"
    );
    assert!(
        first.contains(
            "### toy-1: Add the feature\n\nAdd it.\n\nAcceptance criteria:\n\n- the feature exists"
        ),
        "{first}"
    );
    assert!(!first.contains("sent your last result back"), "{first}");
    let objection = brief("director", 2);
    assert!(
        objection.contains("Its designer says it is split wrongly, and has designed nothing.")
            && objection.contains("> toy-1 needs a type that no bead adds."),
        "{objection}"
    );
    let again = brief("designer", 2);
    assert!(
        again.contains("## The director sent your last result back\n\n> The split stands: toy-1 adds the type itself.")
            && again.contains("### What you said of the split\n\n> toy-1 needs a type that no bead adds."),
        "{again}"
    );
    let proposed = brief("director", 3);
    assert!(
        proposed.contains("## The design of sub-goal `feature`, for you to accept or send back")
            && proposed.contains("`pub const FeatureOptions = struct { loud: bool = false };`")
            && proposed.contains("1. `toy-1`: it is the only one"),
        "{proposed}"
    );
    let handed = brief("foreman", 1);
    assert!(
        handed.contains("## The design of this sub-goal")
            && handed.contains("`error.FeatureOff` when it is disabled."),
        "{handed}"
    );

    // The bead is built, broken, fixed and reviewed, and every agent of its
    // pipeline was given the design in its first brief.
    let deadline = Instant::now() + Duration::from_secs(120);
    while goal(&sup).units.iter().any(|unit| unit.state == "running") {
        assert!(Instant::now() < deadline, "the pipeline did not end");
        sup.pump(Duration::from_millis(200)).unwrap();
    }
    let ended = goal(&sup);
    assert_eq!(ended.units[0].state, "absorbed", "{:?}", ended.units[0]);
    for role in ["builder", "tester", "reviewer"] {
        let text = brief(role, 1);
        assert!(
            text.contains("## The design of this sub-goal\n\nThe sub-goal this bead belongs to was designed before any of its beads was built")
                && text.contains("`pub const FeatureOptions = struct { loud: bool = false };`"),
            "{role}: {text}"
        );
    }
    let events =
        std::fs::read_to_string(world.path(&format!("state/goals/{id}/events.jsonl"))).unwrap();
    let at = |kind: &str| {
        events
            .find(&format!("\"{kind}\""))
            .unwrap_or_else(|| panic!("no {kind} in the goal's log"))
    };
    assert!(at("design.disputed") < at("design.sent_back"));
    assert!(at("design.sent_back") < at("design.proposed"));
    assert!(at("design.proposed") < at("design.accepted"));
}

/// A schedule that names a definition to build under closes the loop with
/// nobody in it: a class of defect that keeps breaking beads calls a run,
/// the run marks beads to build, and the factory opens a goal for them,
/// starts it and ends it when they are closed.
#[test]
fn a_maintenance_run_builds_what_it_marks_in_a_goal_the_factory_opens_and_ends() {
    use factory_driver::goal::{Phase, Scheduled, Supervisor, check_definitions};
    use std::time::{Duration, Instant};

    let world = World::new("maintbuild", "true");
    world.with_github("green");
    std::fs::create_dir_all(world.path("config/goals/toy")).unwrap();
    let tier = "[director]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\n";
    std::fs::write(
        world.path("config/goals/toy/main.toml"),
        format!("id = \"main\"\ndefault = true\n{tier}"),
    )
    .unwrap();
    let upkeep = world.path("config/goals/toy/upkeep.toml");
    let timed = |under: &str| {
        format!(
            "id = \"upkeep\"\n{tier}[schedule]\nhour_utc = 4\nevery_days = 7\nmax_beads = 2\ncharter = \"Turn what keeps breaking into checks.\"\nbuild_under = \"{under}\"\nwhen_class_breaks = 3\n"
        )
    };
    // What a run marks is built under a definition that can run a goal.
    for (under, why) in [
        (
            "upkeep",
            "its schedule builds under `upkeep`, which has a schedule itself and so runs no goal",
        ),
        (
            "nightly",
            "its schedule builds under `nightly`, which is not a goal definition of the project",
        ),
    ] {
        std::fs::write(&upkeep, timed(under)).unwrap();
        let refused = check_definitions(&world.config()).unwrap_err().to_string();
        assert!(refused.contains(why), "{refused}");
    }
    std::fs::write(&upkeep, timed("main")).unwrap();
    let lines = check_definitions(&world.config()).unwrap().join("\n");
    assert!(
        lines.contains("at most 2 bead(s) a run, and builds what it marks under `main`, called at once when a class of defect breaks 3 more bead(s)"),
        "{lines}"
    );

    let say = |role: &str, n: u32, json: serde_json::Value| {
        std::fs::create_dir_all(world.path("bin/state")).unwrap();
        std::fs::write(
            world.path(&format!("bin/state/{role}-{n}.json")),
            json.to_string(),
        )
        .unwrap();
    };
    let check = |build: bool| {
        serde_json::json!({"title": "A lint rule for cleanup on error paths", "type": "task", "priority": 2,
            "description": "Three beads leaked on an error path: toy-7, toy-8, toy-9.",
            "acceptance_criteria": "- the rule fails on each of the three cases",
            "parent": "", "discovered_from": "", "deferred": false, "build": build,
            "closes_class": "error-path-cleanup"})
    };
    let mut sup = Supervisor::new(world.config(), None, vec!["OWNER".to_string()], false).unwrap();
    let record_path = world.path("state/projects/toy/schedules/upkeep/scheduled.json");
    let record = || -> Scheduled {
        serde_json::from_str(&std::fs::read_to_string(&record_path).unwrap()).unwrap()
    };
    let now = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    };
    let count = |role: &str| world.calls().iter().filter(|call| call[0] == role).count();
    // The brief of a role's Nth call.
    let brief = |role: &str, nth: usize| {
        let line = world
            .calls()
            .iter()
            .enumerate()
            .filter(|(_, call)| call[0] == role)
            .nth(nth - 1)
            .map(|(index, _)| index + 1)
            .unwrap();
        std::fs::read_to_string(world.path(&format!("bin/state/brief-{line}.md"))).unwrap()
    };
    let events = || {
        std::fs::read_to_string(world.path("state/projects/toy/schedules/upkeep/events.jsonl"))
            .unwrap()
    };

    // Nothing is due by the clock, and nothing has broken: no run.
    assert!(!sup.pump(Duration::ZERO).unwrap());
    assert!(world.calls().is_empty());
    assert!(sup.next_due().unwrap() > now());

    // A class of defect breaks a third bead: a run is due at once.
    for bead in ["toy-7", "toy-8", "toy-9"] {
        let dir = world.path(&format!("state/units/{bead}-1"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("unit.json"),
            serde_json::json!({"project": "toy", "bead": bead, "regressions": [{"category": "error-path-cleanup"}]}).to_string(),
        )
        .unwrap();
    }
    assert!(sup.next_due().unwrap() <= now());
    // This run files the check and leaves it.
    say(
        "maintainer",
        1,
        serde_json::json!({"summary": "Error paths keep leaking.", "digest": "One bead: a lint rule.",
            "journal": "Filed the rule; not built yet.", "file": [check(false)], "build": [], "build_plan": "", "paths": []}),
    );
    assert!(sup.pump(Duration::ZERO).unwrap());
    assert_eq!(count("maintainer"), 1);
    let shown = brief("maintainer", 1);
    assert!(
        shown.contains("## Why this run is now\n\nIt was not due by the clock. The factory called it because a class of defect has broken more beads since a run last looked: `error-path-cleanup`.")
            && shown.contains("You may also have beads built.")
            && shown.contains("under the definition `main`"),
        "{shown}"
    );
    let first = record();
    assert_eq!(first.backlog.len(), 1, "{first:?}");
    assert_eq!(first.seen.get("error-path-cleanup"), Some(&3));
    assert!(first.goal.is_none());
    assert_eq!(sup.goals().count(), 0, "filing alone opens no goal");
    let filed = first.backlog[0].clone();
    // The bead says which class of defect its check closes, and the
    // factory keeps that: the class has a check being built.
    let checks = || {
        std::fs::read_to_string(world.path("state/projects/toy/checks.json")).unwrap_or_default()
    };
    let kept: serde_json::Value = serde_json::from_str(&checks()).unwrap();
    assert_eq!(
        kept["error-path-cleanup"],
        serde_json::json!({"bead": filed, "landed_at": null}),
        "{kept}"
    );
    assert!(events().contains("\"check.filed\""), "{}", events());
    // The class has broken nothing more: it calls no second run.
    assert!(!sup.pump(Duration::ZERO).unwrap());
    assert!(sup.next_due().unwrap() > now());
    assert_eq!(count("maintainer"), 1);

    // The next run, at its time, is shown the bead that waits and has it
    // built. The goal's director starts it; nobody pressed anything.
    let overdue = |record: Scheduled| {
        let back = Scheduled {
            since: now() - 8 * 86_400,
            ..record
        };
        std::fs::write(&record_path, serde_json::to_string(&back).unwrap()).unwrap();
    };
    say(
        "maintainer",
        2,
        serde_json::json!({"summary": "The rule is still the thing to build.", "digest": "Building the lint rule.",
            "journal": "Had the rule built.", "file": [], "build": [filed],
            "build_plan": "One bead. The rule fails on each of the three cases.", "paths": ["tools"]}),
    );
    say(
        "director",
        1,
        serde_json::json!({"summary": "Starting the rule.", "start": [{"bead": filed, "reason": "the only bead"}],
            "file": [], "edit": [], "dropped_findings": [], "ask_owner": "", "complete": false,
            "evidence": "", "journal_entry": "", "project_memory": ""}),
    );
    overdue(first);
    assert!(sup.pump(Duration::ZERO).unwrap());
    assert_eq!(count("maintainer"), 2);
    let shown = brief("maintainer", 2);
    assert!(
        shown.contains(&format!(
            "## Beads earlier runs filed that nothing has built\n\n- `{filed}`: A lint rule for cleanup on error paths\n"
        )) && !shown.contains("## Why this run is now")
            && shown.contains(&format!(
                "- `error-path-cleanup`: 3 findings on 3 beads; check being built by `{filed}`\n"
            )),
        "{shown}"
    );
    assert_eq!(sup.goals().count(), 1);
    let goal = sup.goals().next().unwrap().clone();
    let built = goal.maintenance.clone().unwrap();
    assert_eq!(
        (built.schedule.as_str(), &built.beads),
        ("upkeep", &vec![filed.clone()])
    );
    assert_eq!(goal.definition, "main");
    let charter = goal.charter.clone().unwrap();
    assert!(
        charter.objective.contains(&format!(
            "- `{filed}`: A lint rule for cleanup on error paths"
        )) && charter
            .stop_when
            .contains(&format!("Every one of these beads is closed: `{filed}`."))
            && charter.plan == "One bead. The rule fails on each of the three cases."
            && charter.paths == ["tools"],
        "{charter:?}"
    );
    assert_eq!(count("director"), 1, "its director was woken at once");
    assert_eq!(count("intake"), 0, "there is no opening conversation");
    let told = brief("director", 1);
    assert!(
        told.contains(
            "The factory started this goal for maintenance run 2 of `upkeep`. Nobody pressed Start."
        ) && told.contains("The factory opened this goal by itself")
            && told.contains("> The rule is still the thing to build."),
        "{told}"
    );
    let second = record();
    assert_eq!(second.goal.as_deref(), Some(goal.id.as_str()));
    assert!(second.backlog.is_empty(), "{second:?}");
    assert!(events().contains("\"schedule.building\""), "{}", events());

    // While that goal builds, a run may file and may not have more built.
    say(
        "maintainer",
        3,
        serde_json::json!({"summary": "More.", "digest": "Another.", "journal": "",
            "file": [check(true)], "build": [], "build_plan": "Build it.", "paths": []}),
    );
    say(
        "maintainer",
        4,
        serde_json::json!({"summary": "Nothing more.", "digest": "Nothing was worth a bead.", "journal": "",
            "file": [], "build": [], "build_plan": "", "paths": []}),
    );
    overdue(second);
    assert!(sup.pump(Duration::ZERO).unwrap());
    assert_eq!(count("maintainer"), 4);
    assert!(
        brief("maintainer", 3).contains(&format!(
            "The goal `{}`, which the factory opened for an earlier run, is still building",
            goal.id
        )) && brief("maintainer", 4).contains(&format!(
            "build: the goal `{}`, opened for an earlier run, is still building",
            goal.id
        )),
        "the run was told, and its build was refused"
    );
    assert_eq!(sup.goals().count(), 1, "one such goal at a time");

    // The bead lands and is closed: the goal is complete, and no agent and
    // no owner said so.
    let deadline = Instant::now() + Duration::from_secs(120);
    while sup.goals().next().unwrap().phase != Phase::Complete {
        assert!(Instant::now() < deadline, "the goal never completed");
        sup.pump(Duration::from_millis(200)).unwrap();
    }
    let text = std::fs::read_to_string(world.path(&format!("beads/{filed}.json"))).unwrap();
    assert!(text.contains("\"closed\""), "{text}");
    assert_eq!(
        count("director"),
        1,
        "the pipeline's end woke no director: code ended the goal"
    );
    let log = std::fs::read_to_string(world.path(&format!("state/goals/{}/events.jsonl", goal.id)))
        .unwrap();
    assert!(
        log.contains("\"goal.complete\"")
            && log.contains("\"by\":\"code\"")
            && !log.contains("goal_accept"),
        "{log}"
    );
    // The check has landed: the class is checked from here on, at the
    // count of beads it had broken, and the run after is told so.
    let kept: serde_json::Value = serde_json::from_str(&checks()).unwrap();
    assert_eq!(
        kept["error-path-cleanup"],
        serde_json::json!({"bead": filed, "landed_at": 3}),
        "{kept}"
    );
    // With that goal over, a later run may build again.
    say(
        "maintainer",
        5,
        serde_json::json!({"summary": "Quiet.", "digest": "Nothing was worth a bead.", "journal": "",
            "file": [], "build": [], "build_plan": "", "paths": []}),
    );
    overdue(record());
    assert!(sup.pump(Duration::ZERO).unwrap());
    let last = brief("maintainer", 5);
    assert!(
        last.contains("You may also have beads built.")
            && last.contains(&format!(
                "- `error-path-cleanup`: 3 findings on 3 beads; checked since `{filed}` landed\n"
            )),
        "{last}"
    );
}

/// A definition with a schedule is run by the factory at its times: one
/// agent, shown what the records say, which files beads and starts nothing.
#[test]
fn a_scheduled_definition_is_run_when_due_and_its_agent_only_files_beads() {
    use factory_driver::goal::{Scheduled, Supervisor, check_definitions};
    use std::time::Duration;

    let world = World::new("maintainer", "true");
    world.with_github("green");
    std::fs::create_dir_all(world.path("config/goals/toy")).unwrap();
    let tier = "[director]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\n";
    let timed = "[schedule]\nhour_utc = 4\nevery_days = 7\nmax_beads = 2\ncharter = \"Turn what keeps breaking into checks.\"\n";
    std::fs::write(
        world.path("config/goals/toy/main.toml"),
        format!("id = \"main\"\ndefault = true\n{tier}"),
    )
    .unwrap();
    let upkeep = world.path("config/goals/toy/upkeep.toml");
    // The factory runs it, so the owner cannot open it, and it has nothing
    // of a goal that builds.
    for (extra, why) in [
        (
            "default = true\n",
            "it cannot be the default for goals the owner opens",
        ),
        (
            "stop_check = \"test-proof\"\n",
            "it cannot have a foreman, a designer or a stop_check",
        ),
    ] {
        std::fs::write(&upkeep, format!("id = \"upkeep\"\n{extra}{tier}{timed}")).unwrap();
        let refused = match check_definitions(&world.config()) {
            Err(err) => err.to_string(),
            Ok(lines) => panic!("accepted: {lines:?}"),
        };
        assert!(refused.contains(why), "{refused}");
    }
    std::fs::write(
        &upkeep,
        format!("id = \"upkeep\"\n{tier}{timed}reports = [\"clones\"]\n"),
    )
    .unwrap();
    let refused = check_definitions(&world.config()).unwrap_err().to_string();
    assert!(
        refused.contains("its schedule's report `clones` is not a command of the project"),
        "{refused}"
    );
    // A report is a command of the project, run on the default branch
    // before each run. This one finds something, and says so by failing.
    let toy = world.path("config/projects/toy.toml");
    let text = std::fs::read_to_string(&toy).unwrap();
    std::fs::write(
        &toy,
        format!("{text}\n[commands.clones]\nargv = [\"sh\", \"-c\", \"echo feature.txt:1-9 = other.txt:1-9 at $1; exit 3\", \"clones\", \"{{base}}\"]\ntimeout_secs = 30\nshown = \"duplicated code\"\n"),
    )
    .unwrap();
    let lines = check_definitions(&world.config()).unwrap().join("\n");
    assert!(
        lines.contains("toy/upkeep: scheduled, maintainer claude-opus-5-5 (high), every 7 day(s) at 04:00 UTC, at most 2 bead(s) a run"),
        "{lines}"
    );

    // Three beads of the project were broken by the same class of defect.
    for bead in ["toy-7", "toy-8", "toy-9"] {
        let dir = world.path(&format!("state/units/{bead}-1"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("unit.json"),
            serde_json::json!({"project": "toy", "bead": bead, "regressions": [{"category": "error-path-cleanup"}]}).to_string(),
        )
        .unwrap();
    }
    let say = |n: u32, json: serde_json::Value| {
        std::fs::create_dir_all(world.path("bin/state")).unwrap();
        std::fs::write(
            world.path(&format!("bin/state/maintainer-{n}.json")),
            json.to_string(),
        )
        .unwrap();
    };
    let bead = |title: &str| {
        serde_json::json!({"title": title, "type": "task", "priority": 3,
            "description": "Three beads leaked on an error path: toy-7, toy-8, toy-9.",
            "acceptance_criteria": "A check fails when an error path leaks.",
            "parent": "", "discovered_from": "", "deferred": true})
    };
    // It first asks for more than it may: three beads, and a pipeline.
    say(
        1,
        serde_json::json!({"summary": "Looked.", "digest": "Three things.", "journal": "",
            "file": [bead("One"), bead("Two"), bead("Three")],
            "start": [{"bead": "toy-1", "reason": "it is ready"}]}),
    );
    say(
        2,
        serde_json::json!({"summary": "Cleanup on error paths keeps breaking.",
            "digest": "Error paths have leaked in three beads. One bead: a check that injects allocation failure.",
            "journal": "I did not file the README drift: it is one line.",
            "file": [bead("Check every error path for leaks")]}),
    );
    say(
        3,
        serde_json::json!({"summary": "Nothing new.", "digest": "Nothing was worth a bead; I read the unit records.", "journal": "Quiet week.", "file": []}),
    );

    let mut sup = Supervisor::new(world.config(), None, vec!["OWNER".to_string()], false).unwrap();
    let record_path = world.path("state/projects/toy/schedules/upkeep/scheduled.json");
    let record = || -> Scheduled {
        serde_json::from_str(&std::fs::read_to_string(&record_path).unwrap()).unwrap()
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
            .into_iter()
            .filter(|call| call[0] == "maintainer")
            .collect::<Vec<_>>()
    };
    let brief =
        |n: usize| std::fs::read_to_string(world.path(&format!("bin/state/brief-{n}.md"))).unwrap();
    let overdue = |record: Scheduled| {
        let back = Scheduled {
            since: now() - 8 * 86_400,
            ..record
        };
        std::fs::write(&record_path, serde_json::to_string(&back).unwrap()).unwrap();
    };

    // Seen for the first time, it counts from now: nothing runs, and it is
    // next due within the week.
    assert!(!sup.pump(Duration::ZERO).unwrap());
    assert!(world.calls().is_empty());
    let first = record();
    assert_eq!((first.runs, first.commit.as_str()), (0, ""));
    let due = sup.next_due().unwrap();
    assert!(due > now() && due <= now() + 7 * 86_400, "{due}");

    // A worker that was away over its time runs it once when it is back.
    overdue(first);
    assert!(sup.pump(Duration::ZERO).unwrap());
    let calls = maintainers();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(world.calls().len(), 2, "nobody else was run");
    assert_eq!(
        calls[0][1], calls[1][1],
        "the refusal went back to the same session"
    );
    assert!(
        calls[0][2].ends_with("state/projects/toy/schedules/upkeep/checkout"),
        "{calls:?}"
    );
    let shown = brief(1);
    assert!(
        shown.starts_with("# Maintenance of project `toy`, run 1\n\n## What these runs are for\n\nTurn what keeps breaking into checks.")
            && shown.contains("File beads: at most 2 in this run")
            && shown.contains("- `error-path-cleanup`: 3 findings on 3 beads")
            && shown.contains("no earlier run to compare with")
            && shown.contains("toy-1 | P"),
        "{shown}"
    );
    // The default branch's own CI, and the report, as they are now.
    let tip = run(
        &world.path("remote.git"),
        "git",
        &["rev-parse", "refs/heads/main"],
    )
    .trim()
    .to_string();
    assert!(
        shown.contains(&format!(
            "### The default branch's own CI\n\nThe last CI run on `main` passed, on `{}`: 1 workflow(s).\n\n### Report: duplicated code\n\nRun on the default branch just now.\n\n```\n(it exited with code 3)\nfeature.txt:1-9 = other.txt:1-9 at {tip}\n```\n\n## The tracker",
            &tip[..10]
        )),
        "{shown}"
    );
    let sent_back = brief(2);
    assert!(
        sent_back.contains(
            "`start`: a maintenance run files beads and marks which to build; it does nothing else"
        ) && sent_back.contains("file: 3 beads, and one run may file 2."),
        "{sent_back}"
    );
    // One bead is in the tracker, deferred, and nothing was started.
    let filed: Vec<String> = std::fs::read_dir(world.path("beads"))
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("filed-"))
        .collect();
    assert_eq!(filed.len(), 1, "{filed:?}");
    let text = std::fs::read_to_string(world.path("beads").join(&filed[0])).unwrap();
    assert!(
        text.contains("Check every error path for leaks") && text.contains("\"deferred\""),
        "{text}"
    );
    assert!(
        sup.goals().next().is_none(),
        "a scheduled run is not a goal"
    );
    assert!(!world.path("state/units/toy-1").exists());
    let ran = record();
    assert_eq!(ran.runs, 1);
    assert_eq!(
        ran.journal,
        "I did not file the README drift: it is one line."
    );
    assert_eq!(ran.commit.len(), 40, "{}", ran.commit);
    assert!(ran.since + 60 > now());
    let events =
        std::fs::read_to_string(world.path("state/projects/toy/schedules/upkeep/events.jsonl"))
            .unwrap();
    for part in [
        "\"schedule.ran\"",
        "\"bead.filed\"",
        ":broom: *upkeep* · run 1 of `toy`",
        "Error paths have leaked in three beads.",
        "• filed `filed-",
    ] {
        assert!(events.contains(part), "no {part} in {events}");
    }
    // It is not due again until its next time.
    assert!(!sup.pump(Duration::ZERO).unwrap());
    assert!(sup.next_due().unwrap() > now());
    assert_eq!(maintainers().len(), 2);

    // The next run is given what this one left, and may file nothing. The
    // default branch's CI has gone red meanwhile.
    std::fs::write(world.path("bin/state-gh/branch-ci"), "failure\n").unwrap();
    overdue(ran);
    assert!(sup.pump(Duration::ZERO).unwrap());
    assert_eq!(maintainers().len(), 3);
    let next = brief(3);
    assert!(
        next.starts_with("# Maintenance of project `toy`, run 2\n")
            && next.contains("## What the last run left for you\n\nI did not file the README drift: it is one line.")
            && next.contains("nothing has landed since the last run")
            && next.contains(&format!(
                "The last CI run on `main` failed, on `{}`. Every sub-goal that ships takes that in.\n\n- `ci`: https://github.com/me/toy/actions/runs/55\n",
                &tip[..10]
            ))
            && next.contains("Check every error path for leaks"),
        "{next}"
    );
    let events =
        std::fs::read_to_string(world.path("state/projects/toy/schedules/upkeep/events.jsonl"))
            .unwrap();
    assert!(events.contains("It filed nothing."), "{events}");
    assert_eq!(record().runs, 2);

    // A record that cannot be read does not leave a schedule that never
    // runs: it is written again, counting from now, and the log says so. A
    // record that only lacks what a later release added is read.
    std::fs::write(&record_path, "{\"since\": \"long ago\"}").unwrap();
    assert!(!sup.pump(Duration::ZERO).unwrap());
    let again = record();
    assert!(again.runs == 0 && again.since + 60 > now(), "{again:?}");
    let due = sup.next_due().unwrap();
    assert!(due > now() && due <= now() + 7 * 86_400, "{due}");
    let events =
        std::fs::read_to_string(world.path("state/projects/toy/schedules/upkeep/events.jsonl"))
            .unwrap();
    assert!(events.contains("\"schedule.unreadable\""), "{events}");
    assert_eq!(maintainers().len(), 3);
    let partial: Scheduled = serde_json::from_str("{\"since\": 5, \"runs\": 2}").unwrap();
    assert_eq!(
        (partial.since, partial.runs, partial.retry_at),
        (5, 2, None)
    );
}

/// While the default branch's own CI is red, nothing ships on the project:
/// the director is told, the owner is told once, a ship is refused, and a
/// sub-goal the director ships as the fix goes ahead.
#[test]
fn nothing_ships_while_the_default_branch_is_red_unless_it_is_the_fix() {
    use factory_driver::goal::Supervisor;
    use factory_slack::socket::Incoming;
    use std::time::{Duration, Instant};

    let world = World::new("default-red", "true");
    world.with_github("green");
    std::fs::create_dir_all(world.path("config/goals/toy")).unwrap();
    std::fs::write(
        world.path("config/goals/toy/main.toml"),
        "id = \"main\"\nflow = \"zig-sdk-bead\"\nship_flow = \"zig-sdk-ship\"\n[director]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\n[foreman]\nmodel = \"claude-opus-5-5\"\neffort = \"medium\"\n",
    )
    .unwrap();
    let say = |role: &str, n: u32, json: serde_json::Value| {
        std::fs::create_dir_all(world.path("bin/state")).unwrap();
        std::fs::write(
            world.path(&format!("bin/state/{role}-{n}.json")),
            json.to_string(),
        )
        .unwrap();
    };
    let with = |mut result: serde_json::Value, fields: serde_json::Value| {
        for (key, value) in fields.as_object().unwrap() {
            result[key] = value.clone();
        }
        result
    };
    let direct = |fields: serde_json::Value| {
        with(
            serde_json::json!({
                "summary": "Decided.", "start": [], "file": [], "edit": [], "dropped_findings": [],
                "open_subgoals": [], "ship": [], "ship_on_red": "", "tell_foreman": [], "design": [],
                "ask_owner": "", "complete": false, "evidence": "", "journal_entry": "", "project_memory": ""
            }),
            fields,
        )
    };
    let foreman = |fields: serde_json::Value| {
        with(
            serde_json::json!({
                "summary": "Decided.", "start": [], "file": [], "dropped_findings": [],
                "report": "working", "message": "", "journal_entry": ""
            }),
            fields,
        )
    };
    say(
        "intake",
        1,
        serde_json::json!({"reply": "Here is the charter.", "ready": true, "objective": "Ship the feature.", "stop_when": "toy-1 has landed.", "plan": "One sub-goal.", "edits": []}),
    );
    say(
        "director",
        1,
        direct(
            serde_json::json!({"open_subgoals": [{"id": "feature", "title": "The feature", "objective": "It works.", "beads": ["toy-1"], "paths": []}]}),
        ),
    );
    say(
        "foreman",
        1,
        foreman(serde_json::json!({"start": [{"bead": "toy-1", "reason": "the only one"}]})),
    );
    say(
        "foreman",
        2,
        foreman(serde_json::json!({"report": "ready", "message": "toy-1 is absorbed."})),
    );
    // The director ships as if nothing were wrong, is refused, and then
    // ships the sub-goal as the fix.
    say(
        "director",
        2,
        direct(serde_json::json!({"ship": ["feature"]})),
    );
    say(
        "director",
        3,
        direct(
            serde_json::json!({"ship": ["feature"], "ship_on_red": "The last landing dropped a line of feature.txt, and toy-1 puts it back."}),
        ),
    );
    say(
        "director",
        4,
        direct(serde_json::json!({"ask_owner": "Is that all?"})),
    );

    // The default branch's own CI has failed on its tip, and passes again
    // on whatever lands next.
    let tip = run(
        &world.path("remote.git"),
        "git",
        &["rev-parse", "refs/heads/main"],
    )
    .trim()
    .to_string();
    std::fs::write(
        world.path("bin/state-gh/branch-ci"),
        format!("failure {tip}\n"),
    )
    .unwrap();

    let mut sup = Supervisor::new(world.config(), None, vec!["OWNER".to_string()], false).unwrap();
    let id = sup.open_directly("toy", "Ship the feature").unwrap();
    sup.incoming(Incoming::Action {
        channel: String::new(),
        user: "OWNER".to_string(),
        action_id: "goal_start".to_string(),
        value: id.clone(),
        message_ts: String::new(),
    })
    .unwrap();
    let goal = |sup: &Supervisor| sup.goals().next().unwrap().clone();
    let brief = |role: &str, n: usize| {
        let calls = world.calls();
        let index = (0..calls.len())
            .filter(|&i| calls[i][0] == role)
            .nth(n - 1)
            .unwrap_or_else(|| panic!("no call {n} of the {role}"));
        std::fs::read_to_string(world.path(&format!("bin/state/brief-{}.md", index + 1))).unwrap()
    };
    let deadline = Instant::now() + Duration::from_secs(180);
    while goal(&sup).units.iter().any(|unit| unit.state == "running")
        || goal(&sup).subgoals.iter().any(|sub| sub.state != "landed")
    {
        assert!(
            Instant::now() < deadline,
            "the sub-goal did not land: {:?}",
            goal(&sup).subgoals
        );
        sup.pump(Duration::from_millis(200)).unwrap();
    }

    // The director was told at its first wake, when it could still open
    // sub-goals and have beads built.
    let first = brief("director", 1);
    assert!(
        first.contains(&format!(
            "## The default branch's own CI is red\n\nThe last CI run on `main` failed, on `{}`. Every sub-goal that ships takes that in.\n\n- `ci`: https://github.com/me/toy/actions/runs/55\n",
            &tip[..10]
        )) && first.contains("the factory refuses `ship`"),
        "{first}"
    );
    // Its ship was refused, and the refusal went back to the same session.
    let refused = brief("director", 3);
    assert!(
        refused.contains(&format!(
            "ship: the default branch's own CI is red on `{}`, and nothing ships on this project until it passes",
            &tip[..10]
        )),
        "{refused}"
    );
    let directors: Vec<Vec<String>> = world
        .calls()
        .into_iter()
        .filter(|call| call[0] == "director")
        .collect();
    assert_eq!(directors[1][1], directors[2][1], "{directors:?}");
    // The owner was told once, in the project's channel, however often a
    // director was woken while the same commit was red.
    let channel =
        std::fs::read_to_string(world.path("state/projects/toy/executive/events.jsonl")).unwrap();
    assert_eq!(
        channel
            .matches(":red_circle: *The default branch of `toy` is red.*")
            .count(),
        1,
        "{channel}"
    );
    assert!(
        channel.contains("https://github.com/me/toy/actions/runs/55"),
        "{channel}"
    );
    assert!(directors.len() >= 3, "{directors:?}");
    // Shipped as the fix, it went ahead, and the goal's thread says why.
    let events =
        std::fs::read_to_string(world.path(&format!("state/goals/{id}/events.jsonl"))).unwrap();
    assert!(
        events.contains("\"ship.on_red\"")
            && events.contains(
                "Shipping `feature` although the default branch's CI is red, as the fix for it"
            )
            && events.contains("toy-1 puts it back."),
        "{events}"
    );
    let ended = goal(&sup);
    assert_eq!(ended.subgoals[0].state, "landed", "{:?}", ended.subgoals);
    assert!(
        ended
            .units
            .iter()
            .any(|unit| unit.ship && unit.state == "landed"),
        "{:?}",
        ended.units
    );
    // With the fix landed the default branch passes, and the director woken
    // by the landing is told nothing of it.
    assert_eq!(ended.default_red, None);
    assert!(!brief("director", directors.len()).contains("CI is red"));
}

/// A goal whose definition names a stop check is complete when the command
/// says so, not when its director does.
#[test]
fn a_goal_is_not_proposed_as_complete_while_its_stop_check_fails() {
    use factory_driver::goal::{Phase, Supervisor};
    use factory_slack::socket::Incoming;

    let world = World::new("stop-check", "true");
    // The project's conformance command passes once the default branch has
    // the file that says so.
    let project = world.path("config/projects/toy.toml");
    let text = std::fs::read_to_string(&project).unwrap();
    std::fs::write(
        &project,
        format!(
            "{text}\n[commands.conformance]\nargv = [\"sh\", \"-c\", \"if [ -f conformance.ok ]; then echo passing 3, failing 0, not written 0; else echo passing 2, failing 1, not written 0; exit 1; fi\"]\ntimeout_secs = 30\nshown = \"the conformance suite\"\n"
        ),
    )
    .unwrap();
    std::fs::create_dir_all(world.path("config/goals/toy")).unwrap();
    let definition = world.path("config/goals/toy/main.toml");
    let body = "[director]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\n";
    // A stop check that is not a command of the project is refused.
    std::fs::write(
        &definition,
        format!("id = \"main\"\nstop_check = \"no-such\"\n{body}"),
    )
    .unwrap();
    let refused = factory_driver::goal::check_definitions(&world.config()).unwrap_err();
    assert!(
        refused
            .to_string()
            .contains("its stop_check `no-such` is not a command of the project"),
        "{refused}"
    );
    std::fs::write(
        &definition,
        format!("id = \"main\"\nstop_check = \"conformance\"\n{body}"),
    )
    .unwrap();
    factory_driver::goal::check_definitions(&world.config()).unwrap();

    let say = |role: &str, n: u32, json: serde_json::Value| {
        std::fs::create_dir_all(world.path("bin/state")).unwrap();
        std::fs::write(
            world.path(&format!("bin/state/{role}-{n}.json")),
            json.to_string(),
        )
        .unwrap();
    };
    let direct = |fields: serde_json::Value| {
        let mut result = serde_json::json!({
            "summary": "Decided.", "start": [], "file": [], "edit": [], "dropped_findings": [],
            "ask_owner": "", "complete": false, "evidence": "", "journal_entry": "", "project_memory": ""
        });
        for (key, value) in fields.as_object().unwrap() {
            result[key] = value.clone();
        }
        result
    };
    say(
        "intake",
        1,
        serde_json::json!({"reply": "Here is the charter.", "ready": true, "objective": "Match the suite.", "stop_when": "Every feature of the suite passes.", "plan": "As the suite shows.", "edits": []}),
    );
    // The director says it is done; told that the command fails, it asks.
    say(
        "director",
        1,
        direct(serde_json::json!({"complete": true, "evidence": "It all looks done."})),
    );
    say(
        "director",
        2,
        direct(serde_json::json!({"ask_owner": "One feature still fails. Shall I plan it?"})),
    );
    // Once the default branch passes, the same proposal stands.
    say(
        "director",
        3,
        direct(serde_json::json!({"complete": true, "evidence": "The suite passes."})),
    );

    let mut sup = Supervisor::new(world.config(), None, vec!["OWNER".to_string()], false).unwrap();
    let id = sup.open_directly("toy", "Match the suite").unwrap();
    sup.incoming(Incoming::Action {
        channel: String::new(),
        user: "OWNER".to_string(),
        action_id: "goal_start".to_string(),
        value: id.clone(),
        message_ts: String::new(),
    })
    .unwrap();
    let brief =
        |n: usize| std::fs::read_to_string(world.path(&format!("bin/state/brief-{n}.md"))).unwrap();
    let directors = |world: &World| -> Vec<usize> {
        world
            .calls()
            .iter()
            .enumerate()
            .filter(|(_, call)| call[0] == "director")
            .map(|(at, _)| at + 1)
            .collect()
    };
    let goal = sup.goals().next().unwrap().clone();
    assert_eq!(goal.phase, Phase::Running, "the proposal was refused");
    let check = goal.stop_check.clone().unwrap();
    assert!(!check.passed);
    assert_eq!(check.how, "failed (exit code 1)");
    assert_eq!(check.text.trim(), "passing 2, failing 1, not written 0");
    let turns = directors(&world);
    assert_eq!(
        turns.len(),
        2,
        "the refused proposal went back to the same session"
    );
    let first = brief(turns[0]);
    assert!(
        first.contains("## The stop check\n\nA command decides this goal's stop condition, and the factory runs it: `the conformance suite`. It failed (exit code 1) on commit `"),
        "{first}"
    );
    assert!(
        first.contains("passing 2, failing 1, not written 0"),
        "{first}"
    );
    let second = brief(turns[1]);
    assert!(
        second.contains(
            "(`the conformance suite`) failed (exit code 1), so the goal is not complete"
        ),
        "{second}"
    );

    // The default branch comes to pass. The owner answers, the director is
    // woken, the command runs again on the new commit, and the proposal is
    // put to the owner with what the command said.
    let toy = world.path("toy");
    std::fs::write(toy.join("conformance.ok"), "").unwrap();
    run(&toy, "git", &["add", "--all"]);
    run(&toy, "git", &["commit", "--quiet", "-m", "pass the suite"]);
    run(&toy, "git", &["push", "--quiet", "origin", "main"]);
    sup.incoming(Incoming::Message {
        channel: goal.channel.clone(),
        user: "OWNER".to_string(),
        text: "It landed. Look again.".to_string(),
        ts: "99.0".to_string(),
        thread_ts: Some(goal.thread_ts.clone()),
    })
    .unwrap();
    let goal = sup.goals().next().unwrap().clone();
    let check = goal.stop_check.clone().unwrap();
    assert!(check.passed, "{check:?}");
    assert_ne!(check.commit, "");
    assert_eq!(goal.phase, Phase::ProposedComplete);
    let turns = directors(&world);
    assert_eq!(turns.len(), 3);
    let third = brief(turns[2]);
    assert!(third.contains("It passed on commit `"), "{third}");
    assert!(
        third.contains("passing 3, failing 0, not written 0"),
        "{third}"
    );
    let events =
        std::fs::read_to_string(world.path(&format!("state/goals/{id}/events.jsonl"))).unwrap();
    assert_eq!(events.matches("\"stop_check.ran\"").count(), 2, "{events}");
}

/// A second goal on a project waits for the first, and starts by itself.
#[test]
fn a_goal_started_while_another_runs_is_queued_and_starts_when_that_ends() {
    use factory_driver::goal::{Phase, Supervisor};
    use factory_slack::socket::Incoming;

    let world = World::new("queue", "true");
    std::fs::create_dir_all(world.path("config/goals/toy")).unwrap();
    std::fs::write(
        world.path("config/goals/toy/main.toml"),
        "id = \"main\"\n[director]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(world.path("bin/state")).unwrap();
    let say = |role: &str, n: u32, json: &str| {
        std::fs::write(world.path(&format!("bin/state/{role}-{n}.json")), json).unwrap();
    };
    let charter = r#"{"reply":"Here is the charter.","ready":true,"objective":"Do it.","stop_when":"It is done.","plan":"toy-1.","edits":[]}"#;
    // Each director, when it wakes, asks the owner something: a goal that is
    // running without a pipeline.
    let ask = r#"{"summary":"Thinking.","start":[],"file":[],"edit":[],"dropped_findings":[],"open_subgoal":{"id":"","title":"","objective":"","beads":[]},"ship":false,"ask_owner":"Which first?","complete":false,"evidence":"","journal_entry":"","project_memory":""}"#;
    say("intake", 1, charter);
    say("intake", 2, charter);
    say("director", 1, ask);
    say("director", 2, ask);

    let mut sup = Supervisor::new(world.config(), None, vec!["OWNER".to_string()], false).unwrap();
    let act = |action: &str, goal: &str| Incoming::Action {
        channel: String::new(),
        user: "OWNER".to_string(),
        action_id: action.to_string(),
        value: goal.to_string(),
        message_ts: String::new(),
    };
    let phase = |sup: &Supervisor, id: &str| {
        sup.goals()
            .find(|goal| goal.id == id)
            .unwrap()
            .phase
            .clone()
    };
    let directors = || {
        world
            .calls()
            .iter()
            .filter(|call| call[0] == "director")
            .count()
    };

    // Two goals opened in the same second are still two goals.
    let first = sup.open_directly("toy", "The first thing").unwrap();
    let second = sup.open_directly("toy", "The second thing").unwrap();
    assert_ne!(first, second);
    assert_eq!(sup.goals().count(), 2);

    sup.incoming(act("goal_start", &first)).unwrap();
    assert_eq!(phase(&sup, &first), Phase::Running);
    assert_eq!(directors(), 1);

    // The project runs one goal at a time: the second waits, and no director
    // runs for it.
    sup.incoming(act("goal_start", &second)).unwrap();
    assert_eq!(phase(&sup, &second), Phase::Queued);
    assert_eq!(directors(), 1, "a queued goal runs nothing");
    // A restart does not let it jump the queue.
    drop(sup);
    let mut sup = Supervisor::new(world.config(), None, vec!["OWNER".to_string()], false).unwrap();
    sup.resume().unwrap();
    assert_eq!(phase(&sup, &second), Phase::Queued);
    assert_eq!(directors(), 1);

    // When the first ends, the second starts by itself.
    sup.incoming(act("goal_stop", &first)).unwrap();
    assert_eq!(phase(&sup, &first), Phase::Stopped);
    assert_eq!(phase(&sup, &second), Phase::Running);
    assert_eq!(
        directors(),
        2,
        "the queued goal's director woke when its turn came"
    );
    let log =
        std::fs::read_to_string(world.path(&format!("state/goals/{second}/events.jsonl"))).unwrap();
    assert!(log.contains(r#""kind":"goal.queued""#) && log.contains(r#""kind":"goal.unqueued""#));

    // With room for two, a third goal would not wait.
    let project = world.path("config/projects/toy.toml");
    let text = std::fs::read_to_string(&project).unwrap();
    std::fs::write(&project, text.replace("flow = ", "max_goals = 2\nflow = ")).unwrap();
    say("intake", 3, charter);
    say("director", 3, ask);
    let third = sup.open_directly("toy", "The third thing").unwrap();
    sup.incoming(act("goal_start", &third)).unwrap();
    assert_eq!(phase(&sup, &third), Phase::Running);
}

/// Tracker work that is no goal's: a bead that should be closed without
/// work. The owner can ask the executive for it, and a director passes it
/// up to the executive, not to the owner. A closure that holds only once
/// another bead has landed is carried out by code when it has.
#[test]
fn the_executive_closes_a_bead_for_the_owner_or_a_director_now_or_once_another_lands() {
    use factory_driver::goal::{Phase, Supervisor};
    use factory_slack::socket::Incoming;
    use serde_json::json;
    use std::time::Duration;

    let world = World::new("exec-close", "true");
    world.with_github("green");
    let project = world.path("config/projects/toy.toml");
    let text = std::fs::read_to_string(&project).unwrap()
        + "issues = true\n\n[executive]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\nslots = 2\n";
    std::fs::write(&project, text).unwrap();
    std::fs::create_dir_all(world.path("config/goals/toy")).unwrap();
    std::fs::write(
        world.path("config/goals/toy/main.toml"),
        "id = \"main\"\nflow = \"zig-sdk-bead\"\nship_flow = \"zig-sdk-ship\"\n[director]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\n",
    )
    .unwrap();
    std::fs::write(
        world.path("beads/toy-3.json"),
        r#"{"id":"toy-3","title":"The feature, asked for again","acceptance_criteria":"- the feature exists","status":"open","external_ref":"https://github.com/me/toy/issues/5"}"#,
    )
    .unwrap();
    std::fs::write(
        world.path("beads/toy-4.json"),
        r#"{"id":"toy-4","title":"The feature, a third time","acceptance_criteria":"- the feature exists","status":"open"}"#,
    )
    .unwrap();
    std::fs::create_dir_all(world.path("bin/state")).unwrap();
    let say = |role: &str, n: u32, json: serde_json::Value| {
        std::fs::write(
            world.path(&format!("bin/state/{role}-{n}.json")),
            json.to_string(),
        )
        .unwrap();
    };
    let status = |bead: &str| {
        let text = std::fs::read_to_string(world.path(&format!("beads/{bead}.json"))).unwrap();
        serde_json::from_str::<serde_json::Value>(&text).unwrap()["status"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let brief = |role: &str, n: usize| {
        let all = world.calls();
        let index = (0..all.len())
            .filter(|&i| all[i][0] == role)
            .nth(n - 1)
            .unwrap();
        std::fs::read_to_string(world.path(&format!("bin/state/brief-{}.md", index + 1))).unwrap()
    };
    let closed_issues =
        || std::fs::read_to_string(world.path("bin/state-gh/issues-closed")).unwrap_or_default();
    let pending = |sup: &Supervisor| {
        sup.executives()
            .next()
            .map(|executive| executive.pending.clone())
            .unwrap_or_default()
    };
    let mut sup = Supervisor::new(world.config(), None, vec!["OWNER".to_string()], false).unwrap();

    // The owner asks. A bead someone is working on is refused, and the
    // refusal goes back to the same conversation. The second answer waits
    // for toy-1 to land: nothing is closed yet.
    let close = |bead: &str, kind: &str, once: &str| json!({"bead": bead, "reason": "toy-1 delivers the same feature.", "as": kind, "once_closed": once});
    say(
        "executive",
        1,
        json!({"reply": "Closing it.", "open_goal": {"definition": "", "handoff": ""}, "close": [close("toy-2", "duplicate", "")]}),
    );
    say(
        "executive",
        2,
        json!({"reply": "I will close toy-3 when toy-1 has landed.", "open_goal": {"definition": "", "handoff": ""}, "close": [close("toy-3", "delivered", "toy-1")]}),
    );
    let (_, opened) = sup
        .tell_executive(
            "toy",
            None,
            "toy-3 is the same work as toy-1. Close it once toy-1 lands.",
        )
        .unwrap();
    assert_eq!(opened, None);
    assert!(
        brief("executive", 2).contains("close: `toy-2` is being worked on"),
        "{}",
        brief("executive", 2)
    );
    assert_eq!(status("toy-2"), "in_progress");
    assert_eq!(status("toy-3"), "open");
    assert_eq!(pending(&sup).len(), 1);
    assert_eq!(pending(&sup)[0].once_closed, "toy-1");
    assert_eq!(closed_issues(), "");

    // toy-1 lands. Code closes toy-3, and its issue with it, with no
    // agent run for it.
    let calls_before = world.calls().len();
    factory_driver::bead_source(&world.config().project("toy").unwrap())
        .close("toy-1", "Merged.", "factory")
        .unwrap();
    sup.pump(Duration::ZERO).unwrap();
    assert_eq!(status("toy-3"), "closed");
    assert!(pending(&sup).is_empty());
    assert_eq!(world.calls().len(), calls_before);
    let issues = closed_issues();
    assert!(
        issues.contains("5 --repo me/toy --reason completed --comment Closed as already delivered, by the project's executive: toy-1 delivers the same feature."),
        "{issues}"
    );

    // A director finds the same about another bead. It cannot close a
    // bead and does not ask the owner: it passes it up, and the executive
    // is run for it, with what the director said.
    say(
        "executive",
        3,
        json!({"reply": "Opened.", "open_goal": {"definition": "main", "handoff": "Nothing else is running."}, "close": []}),
    );
    say(
        "intake",
        1,
        json!({"reply": "Here is the charter.", "ready": true, "objective": "Do it.", "stop_when": "It is done.", "plan": "One sub-goal.", "paths": [], "edits": []}),
    );
    let (_, opened) = sup
        .tell_executive("toy", None, "Ship the feature.")
        .unwrap();
    let goal = opened.unwrap();
    say(
        "director",
        1,
        json!({
            "summary": "Waiting on the owner.", "start": [], "file": [], "edit": [], "dropped_findings": [],
            "open_subgoals": [], "ship": [], "tell_foreman": [],
            "tell_executive": "toy-4 asks for what toy-1 delivered, which has landed. It should be closed.",
            "ask_owner": "Which bead first?",
            "complete": false, "evidence": "", "journal_entry": "", "project_memory": ""
        }),
    );
    say(
        "executive",
        4,
        json!({"summary": "toy-4 repeats toy-1, which is closed.", "close": [close("toy-4", "duplicate", "")], "replies": []}),
    );
    sup.incoming(Incoming::Action {
        channel: String::new(),
        user: "OWNER".to_string(),
        action_id: "goal_start".to_string(),
        value: goal.clone(),
        message_ts: String::new(),
    })
    .unwrap();
    assert_eq!(
        sup.goals().find(|other| other.id == goal).unwrap().phase,
        Phase::Running
    );
    assert_eq!(status("toy-4"), "open");
    sup.pump(Duration::ZERO).unwrap();
    assert_eq!(status("toy-4"), "closed");
    let desk = brief("executive", 4);
    for expected in [
        "# Directors have passed something up to you, on project `toy`".to_string(),
        format!(
            "## From the director of goal `{goal}`\n\n> toy-4 asks for what toy-1 delivered, which has landed. It should be closed."
        ),
    ] {
        assert!(desk.contains(&expected), "missing {expected:?} in:\n{desk}");
    }
    let executive = sup.executives().next().unwrap();
    assert!(executive.memos.is_empty());
    // toy-4 had no issue: nothing more was closed on GitHub.
    assert_eq!(closed_issues().lines().count(), 1);
    // It is run once for what was passed up, not again on every turn.
    let calls_before = world.calls().len();
    sup.pump(Duration::ZERO).unwrap();
    assert_eq!(world.calls().len(), calls_before);
}

#[test]
fn the_issue_sweep_gives_each_open_bead_one_issue_and_leaves_those_being_given_one() {
    let world = World::new("sweep", "true");
    world.with_github("green");
    let project = world.path("config/projects/toy.toml");
    let text = std::fs::read_to_string(&project).unwrap().replace(
        "write = [\"me/toy\"]",
        "write = [\"me/toy\"]\nissues = true\nissue_labels = [\"type::{type}\"]\nissue_types = { bug = \"Bug\", \"*\" = \"Task\" }\nissue_priority = { field = \"Priority\", options = [\"Critical\", \"High\", \"Medium\"] }",
    );
    std::fs::write(&project, text).unwrap();
    // One the factory's pipeline has claimed, and one with its issue.
    std::fs::write(
        world.path("beads/toy-4.json"),
        r#"{"id":"toy-4","title":"Being built","acceptance_criteria":"- x","status":"in_progress","assignee":"factory"}"#,
    )
    .unwrap();
    std::fs::write(
        world.path("beads/toy-5.json"),
        r#"{"id":"toy-5","title":"Tracked","acceptance_criteria":"- x","status":"open","issue_type":"bug","external_ref":"https://github.com/me/toy/issues/40"}"#,
    )
    .unwrap();
    let config = world.config();
    let factory = config.factory().unwrap();
    let project = config.project("toy").unwrap();
    let issues_made = || {
        std::fs::read_to_string(world.path("bin/state-gh/issues"))
            .map(|n| n.trim().parse::<u32>().unwrap())
            .unwrap_or(0)
    };

    // Every bead was filed a moment ago: whoever filed it is making its issue.
    let sweep = factory_driver::sweep_issues(&factory, &project, 3600).unwrap();
    assert!(sweep.kept);
    assert_eq!(sweep.open, 4, "{sweep:?}");
    assert!(sweep.created.is_empty(), "{sweep:?}");
    assert_eq!(issues_made(), 0);
    assert!(
        sweep
            .left
            .iter()
            .any(|line| line.starts_with("toy-1: filed")),
        "{sweep:?}"
    );
    assert!(
        sweep
            .left
            .contains(&"toy-4: its pipeline gives it its issue".to_string()),
        "{sweep:?}"
    );
    // The bead with an issue has its type and priority set all the same.
    assert_eq!(sweep.changed.len(), 1, "{sweep:?}");
    assert!(sweep.changed[0].starts_with("toy-5: "), "{sweep:?}");

    // Old enough: each bead without an issue gets one, someone else's
    // claimed bead too, and the factory's own is still left to its pipeline.
    let sweep = factory_driver::sweep_issues(&factory, &project, 0).unwrap();
    let created: Vec<&str> = sweep
        .created
        .iter()
        .map(|line| line.split(' ').next().unwrap())
        .collect();
    assert_eq!(created, ["toy-1", "toy-2"], "{sweep:?}");
    assert!(sweep.failed.is_empty(), "{sweep:?}");
    assert_eq!(issues_made(), 2);
    let source = factory_driver::bead_source(&project);
    for (id, url) in [
        ("toy-1", "https://github.com/me/toy/issues/1"),
        ("toy-2", "https://github.com/me/toy/issues/2"),
    ] {
        assert_eq!(source.read(id).unwrap().external_ref, url);
    }
    assert!(source.read("toy-4").unwrap().external_ref.is_empty());
    // Made as the factory makes an issue: labelled, and naming its bead.
    let created_calls: Vec<String> = world
        .gh_calls()
        .into_iter()
        .filter(|call| call.starts_with("issue create"))
        .collect();
    assert_eq!(created_calls.len(), 2);
    assert!(created_calls[0].contains("type::task"), "{created_calls:?}");

    // A second sweep makes no second issue.
    let sweep = factory_driver::sweep_issues(&factory, &project, 0).unwrap();
    assert!(sweep.created.is_empty(), "{sweep:?}");
    assert_eq!(issues_made(), 2);

    // A project that does not keep issues is not swept.
    let mut quiet = project.clone();
    quiet.github.as_mut().unwrap().issues = false;
    let sweep = factory_driver::sweep_issues(&factory, &quiet, 0).unwrap();
    assert_eq!(sweep, factory_driver::IssueSweep::default());
}

/// A world whose `test` command is `script`, with `extra` lines added to the
/// command's table (the last table of the project file). `{root}` in the
/// script is the world's own directory, outside the worktree.
fn triage_world(name: &str, script: &str, extra: &str) -> World {
    let world = World::new(name, "true");
    let script = script.replace("{root}", &world.path("").display().to_string());
    let project = world.path("config/projects/toy.toml");
    let text = std::fs::read_to_string(&project).unwrap();
    let old = "[commands.test]\nargv = [\"sh\", \"-c\", \"true\"]\ntimeout_secs = 30\n";
    assert!(text.ends_with(old), "{text}");
    let script = script.replace('"', "\\\"");
    std::fs::write(
        &project,
        text.replace(
            old,
            &format!(
                "[commands.test]\nargv = [\"sh\", \"-c\", \"{script}\"]\ntimeout_secs = 30\n{extra}"
            ),
        ),
    )
    .unwrap();
    world
}

/// How many times a builder was run: the build and each fix.
fn builder_runs(world: &World) -> usize {
    world
        .calls()
        .iter()
        .filter(|call| call[0] == "builder")
        .count()
}

fn events_named(world: &World, driver: &Driver, kind: &str) -> Vec<serde_json::Value> {
    events_of(world, &driver.unit().id.to_string())
        .into_iter()
        .filter(|(k, _, _)| k == kind)
        .map(|(_, _, data)| data)
        .collect()
}

/// A check that fails on the base too, with every file the change touched
/// put back, is not the change's: the unit parks with both results and no
/// builder is sent to fix it.
#[test]
fn a_check_that_fails_on_the_base_too_parks_without_a_fix_round() {
    let world = triage_world(
        "triage-base-fails",
        "echo 'error: the server never answers'; exit 1",
        "triage = { base = true }\n",
    );
    let mut driver = Driver::start(world.config(), "toy", "toy-1", &OPTIONS).unwrap();
    let Status::Parked(reason) = driver.run().unwrap() else {
        panic!("the unit should park");
    };
    assert!(reason.contains("on the base"), "{reason}");
    assert!(reason.contains("not the change's"), "{reason}");
    assert!(reason.contains("the server never answers"), "{reason}");
    let base = events_named(&world, &driver, "command.base");
    assert_eq!(base.len(), 1, "{base:?}");
    assert_eq!(base[0]["verdict"], "Failed { code: Some(1) }");
    assert_eq!(
        builder_runs(&world),
        1,
        "only the build ran: {:?}",
        world.calls()
    );
    // The copy at the base is gone, and the worktree still has the change.
    let unit_dir = world.path(&format!("state/units/{}", driver.unit().id));
    assert!(
        !std::fs::read_dir(&unit_dir).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("at-base-")),
        "the copy at the base is removed"
    );
    assert!(driver.unit().worktree.join("feature.txt").exists());
}

/// A check that passes on the base fails because of the change: the builder
/// is sent, and told that the base passed.
#[test]
fn a_check_that_passes_on_the_base_goes_to_the_builder_with_that_said() {
    let world = triage_world(
        "triage-base-passes",
        "if [ -f feature.txt ] && [ ! -f {root}/failed-once ]; then touch {root}/failed-once; echo 'error: feature broke it'; exit 1; fi",
        "triage = { base = true }\n",
    );
    let mut driver = Driver::start(world.config(), "toy", "toy-1", &OPTIONS).unwrap();
    assert_eq!(driver.run().unwrap(), Status::Ended);
    let base = events_named(&world, &driver, "command.base");
    assert_eq!(base.len(), 1, "{base:?}");
    assert_eq!(base[0]["verdict"], "Passed");
    let ended: Vec<String> = world
        .event_kinds(&driver)
        .into_iter()
        .filter(|k| k.contains(':'))
        .collect();
    let at = ended
        .iter()
        .position(|k| k == "test:fail")
        .expect("test failed once");
    assert_eq!(ended[at + 1], "fix:done", "{ended:?}");
    assert_eq!(
        ended.iter().filter(|k| *k == "test:fail").count(),
        1,
        "{ended:?}"
    );
    // The builder run sent for the failure is the first after the build.
    let calls = world.calls();
    let fix = (0..calls.len())
        .filter(|&i| calls[i][0] == "builder")
        .nth(1)
        .unwrap();
    let brief =
        std::fs::read_to_string(world.path(&format!("bin/state/brief-{}.md", fix + 1))).unwrap();
    assert!(
        brief.contains("it passed: the failure comes with the change"),
        "{brief}"
    );
}

/// A check that passes when run again was a flake: the step passes, the flake
/// is recorded, and no builder is sent.
#[test]
fn a_check_that_passes_when_run_again_is_a_flake_and_no_builder_is_sent() {
    let world = triage_world(
        "triage-flaky",
        "if [ ! -f {root}/failed-once ]; then touch {root}/failed-once; echo 'connection reset'; exit 1; fi",
        "triage = { rerun = true }\n",
    );
    let mut driver = Driver::start(world.config(), "toy", "toy-1", &OPTIONS).unwrap();
    assert_eq!(driver.run().unwrap(), Status::Ended);
    let flaky = events_named(&world, &driver, "command.flaky");
    assert_eq!(flaky.len(), 1, "{flaky:?}");
    // The tester's scripted break sends one fix round of its own; the flake
    // sends none.
    let kinds = world.event_kinds(&driver);
    assert!(kinds.contains(&"test:pass".to_string()), "{kinds:?}");
    assert!(!kinds.contains(&"test:fail".to_string()), "{kinds:?}");
    let said: Vec<String> = events_named(&world, &driver, "slack.say")
        .iter()
        .map(|data| data["text"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(said.iter().any(|t| t.contains("a flake")), "{said:?}");
}

/// Output the project names as the host's fault parks the step: no fix
/// round can help.
#[test]
fn a_failure_the_project_names_as_the_hosts_parks_without_a_fix_round() {
    let world = triage_world(
        "triage-host",
        "echo 'starting the dev server failed: invalid ephemeral server download'; exit 1",
        "could_not_run_when = [\"invalid ephemeral server download\"]\n",
    );
    let mut driver = Driver::start(world.config(), "toy", "toy-1", &OPTIONS).unwrap();
    let Status::Parked(reason) = driver.run().unwrap() else {
        panic!("the unit should park");
    };
    assert!(reason.contains("could not run properly"), "{reason}");
    assert!(
        reason.contains("invalid ephemeral server download"),
        "{reason}"
    );
    assert_eq!(events_named(&world, &driver, "command.host_fault").len(), 1);
    assert_eq!(
        builder_runs(&world),
        1,
        "only the build ran: {:?}",
        world.calls()
    );
}

/// A check that fails the same way after a fix round parks: another round
/// would likely go as the last one did.
#[test]
fn a_check_that_fails_the_same_way_after_a_fix_parks_instead_of_another_round() {
    let world = triage_world(
        "triage-repeat",
        "if [ -f feature.txt ]; then echo \"error: 'feature test' failed\"; exit 1; fi",
        "failure_lines = [\"^error: \"]\n",
    );
    let mut driver = Driver::start(world.config(), "toy", "toy-1", &OPTIONS).unwrap();
    let Status::Parked(reason) = driver.run().unwrap() else {
        panic!("the unit should park");
    };
    assert!(reason.contains("the same way"), "{reason}");
    assert!(reason.contains("feature test"), "{reason}");
    assert_eq!(events_named(&world, &driver, "command.repeated").len(), 1);
    assert_eq!(
        builder_runs(&world),
        2,
        "the build and one fix: {:?}",
        world.calls()
    );
}

/// A builder sent to fix a failure that is not its branch's may say so: the
/// fix ends `already_done`, nothing is committed, and the checks run again
/// without another builder run or a rejected result.
#[test]
fn a_fix_that_finds_nothing_to_change_goes_on_through_the_checks() {
    let world = triage_world(
        "fix-nothing",
        "if [ ! -f {root}/failed-once ]; then touch {root}/failed-once; echo 'error: the server never answers'; exit 1; fi",
        "",
    );
    world.scenario("fix-nothing");
    let mut driver = Driver::start(world.config(), "toy", "toy-1", &OPTIONS).unwrap();
    assert_eq!(driver.run().unwrap(), Status::Ended);
    let ended: Vec<String> = world
        .event_kinds(&driver)
        .into_iter()
        .filter(|k| k.contains(':'))
        .collect();
    let at = ended
        .iter()
        .position(|k| k == "fix:already_done")
        .unwrap_or_else(|| panic!("no fix ended already_done: {ended:?}"));
    assert_eq!(ended[at - 1], "test:fail", "{ended:?}");
    assert_eq!(ended[at + 1], "fmt:pass", "{ended:?}");
    assert!(
        events_named(&world, &driver, "result.rejected").is_empty(),
        "an already_done fix is accepted as it is"
    );
}

/// A ship that parked before its pull request, shipped again, goes on from
/// the start of its flow: the default branch is taken in again and every
/// check runs, and no builder is sent to the step it parked in.
#[test]
fn a_parked_ship_shipped_again_starts_from_its_sync() {
    use factory_driver::goal::{Phase, Supervisor};
    use factory_slack::socket::Incoming;
    use std::time::{Duration, Instant};

    // The ship's first test run fails for the host (a broken download), in
    // the sub-goal's worktree only; every later run passes.
    let world = triage_world(
        "ship-again",
        "case $PWD in *feature*) if [ ! -f {root}/failed-once ]; then touch {root}/failed-once; echo 'invalid ephemeral server download'; exit 1; fi ;; esac",
        "could_not_run_when = [\"invalid ephemeral server download\"]\n",
    );
    world.with_github("green");
    let project = world.path("config/projects/toy.toml");
    let text = std::fs::read_to_string(&project).unwrap().replace(
        "merge_subject = \"Merge bead/{bead_short}: {title}\"",
        "merge_method = \"squash\"\nmerge_subject = \"{title} (#{number})\"",
    );
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
    say("intake", 1, r#"{"reply":"Here is the charter.","ready":true,"objective":"Ship the feature.","stop_when":"It has landed.","plan":"One sub-goal: toy-1.","edits":[]}"#.to_string());
    say(
        "director",
        1,
        decide(
            r#"{"open_subgoal":{"id":"feature","title":"The feature","objective":"It works.","beads":["toy-1"]},"start":[{"bead":"toy-1","reason":"first"}]}"#,
        ),
    );
    say("director", 2, decide(r#"{"ship":true}"#));
    say("director", 3, decide(r#"{"ship":true}"#));
    say(
        "director",
        4,
        decide(r#"{"complete":true,"evidence":"It landed."}"#),
    );

    let mut sup = Supervisor::new(world.config(), None, vec!["OWNER".to_string()], false).unwrap();
    let id = sup.open_directly("toy", "Ship the feature").unwrap();
    sup.incoming(Incoming::Action {
        channel: String::new(),
        user: "OWNER".to_string(),
        action_id: "goal_start".to_string(),
        value: id.clone(),
        message_ts: String::new(),
    })
    .unwrap();
    let goal = |sup: &Supervisor| sup.goals().next().unwrap().clone();
    let wait_for = |sup: &mut Supervisor, wakes: u32| {
        let deadline = Instant::now() + Duration::from_secs(120);
        while sup.goals().next().unwrap().wakes < wakes {
            assert!(
                Instant::now() < deadline,
                "the goal never reached wake {wakes}"
            );
            sup.pump(Duration::from_millis(200)).unwrap();
        }
    };

    // The ship parks at its first test run, for the host.
    wait_for(&mut sup, 3);
    let ship_unit = goal(&sup)
        .units
        .iter()
        .find(|unit| unit.ship)
        .and_then(|unit| unit.unit.clone())
        .expect("the ship has a unit");
    let ship_log = || {
        std::fs::read_to_string(world.path(&format!("state/units/{ship_unit}/events.jsonl")))
            .unwrap()
    };
    assert!(
        ship_log().contains("\"kind\":\"command.host_fault\""),
        "{}",
        ship_log()
    );

    // Shipped again, it goes on from its sync and lands.
    wait_for(&mut sup, 4);
    let done = goal(&sup);
    assert_eq!(done.phase, Phase::ProposedComplete);
    assert_eq!(done.subgoals[0].state, "landed");
    let log = ship_log();
    let resumed: Vec<&str> = log
        .lines()
        .filter(|line| line.contains("\"kind\":\"unit.redirected\""))
        .collect();
    let steps: Vec<String> = log
        .lines()
        .filter(|line| line.contains("\"kind\":\"step.") || line.contains("\"kind\":\"unit."))
        .map(|line| {
            let event: serde_json::Value = serde_json::from_str(line).unwrap();
            format!("{} {} {}", event["kind"], event["step"], event["data"])
        })
        .collect();
    assert_eq!(resumed.len(), 1, "{steps:#?}");
    assert!(resumed[0].contains("\"step\":\"sync\""), "{resumed:?}");
    let syncs = log
        .lines()
        .filter(|line| {
            line.contains("\"kind\":\"step.started\"") && line.contains("\"step\":\"sync\"")
        })
        .count();
    assert_eq!(syncs, 2, "the default branch is taken in again");
    assert!(
        !log.contains("\"step\":\"fix\""),
        "no builder was sent to the ship"
    );
}
