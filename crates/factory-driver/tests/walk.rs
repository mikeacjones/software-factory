//! One unit walked through the real flow and the real role prompts, with a
//! scripted stand-in for the `claude` CLI and a bare repository for a remote.
//! No model and no network.

// Test code: a failed setup step should stop the test where it happens.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use factory_config::ConfigDir;
use factory_driver::{Driver, DriverError, Options, Status};

struct World {
    root: PathBuf,
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn run(dir: &Path, program: &str, args: &[&str]) -> String {
    let output = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{program} {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn repo_config() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config")
}

impl World {
    /// A project with one commit, a bare remote, one bead and a config
    /// directory that uses the repository's own flow and role prompts.
    fn new(name: &str, test_command: &str) -> World {
        let root = std::env::temp_dir().join(format!("factory-walk-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let world = World { root };
        let path = |relative: &str| world.root.join(relative);
        for dir in [
            "toy",
            "beads",
            "bin",
            "config/projects",
            "config/flows",
            "config/roles",
        ] {
            std::fs::create_dir_all(path(dir)).unwrap();
        }

        let toy = path("toy");
        run(&toy, "git", &["init", "--quiet", "-b", "main"]);
        run(&toy, "git", &["config", "user.name", "Test"]);
        run(&toy, "git", &["config", "user.email", "test@localhost"]);
        run(&toy, "git", &["config", "commit.gpgsign", "false"]);
        std::fs::write(toy.join("README.md"), "# toy\n").unwrap();
        run(&toy, "git", &["add", "--all"]);
        run(&toy, "git", &["commit", "--quiet", "-m", "first"]);
        run(
            &world.root,
            "git",
            &["init", "--quiet", "--bare", "remote.git"],
        );
        let remote = path("remote.git");
        run(
            &toy,
            "git",
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        run(&toy, "git", &["push", "--quiet", "origin", "main"]);

        std::fs::write(
            path("beads/toy-1.json"),
            r#"{"id":"toy-1","title":"Add the feature","description":"Add it.","acceptance_criteria":"- the feature exists","status":"open"}"#,
        )
        .unwrap();
        std::fs::write(
            path("beads/toy-2.json"),
            r#"{"id":"toy-2","title":"Taken","acceptance_criteria":"- x","status":"in_progress","assignee":"someone"}"#,
        )
        .unwrap();

        let fake = path("bin/claude");
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake-claude.sh"),
            &fake,
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

        for kind in ["flows", "roles"] {
            for entry in std::fs::read_dir(repo_config().join(kind)).unwrap() {
                let entry = entry.unwrap().path();
                std::fs::copy(
                    &entry,
                    path(&format!("config/{kind}")).join(entry.file_name().unwrap()),
                )
                .unwrap();
            }
        }
        std::fs::write(
            path("config/factory.toml"),
            format!(
                "state_dir = \"{}\"\nclaude_bin = \"{}\"\ncli_version = \"2.1.325\"\nallowed_models = [\"claude-opus-5-5\"]\n",
                path("state").display(),
                fake.display()
            ),
        )
        .unwrap();
        std::fs::write(
            path("config/projects/toy.toml"),
            format!(
                r#"id = "toy"
repo = "{}"
default_branch = "main"
flow = "build-break-review"
forbidden_paths = [".github/**"]

[beads]
kind = "files"
dir = "{}"

[scopes]
code = ["**"]

[commands.fmt]
argv = ["true"]
timeout_secs = 30

[commands.readme]
argv = ["true"]
timeout_secs = 30

[commands.changelog]
argv = ["true"]
timeout_secs = 30

[commands.test]
argv = ["sh", "-c", "{test_command}"]
timeout_secs = 30
"#,
                toy.display(),
                path("beads").display()
            ),
        )
        .unwrap();
        world
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    /// Give the project a Slack channel, served by a local stand-in.
    fn with_slack(&self, fake: &common::FakeSlack) {
        let env = self.path("slack.env");
        std::fs::write(
            &env,
            format!(
                "SLACK_BOT_TOKEN=xoxb-test\nSLACK_OWNER_USER_ID=OWNER\nSLACK_API_URL={}\n",
                fake.url
            ),
        )
        .unwrap();
        let factory = self.path("config/factory.toml");
        let text = std::fs::read_to_string(&factory).unwrap();
        std::fs::write(
            &factory,
            format!(
                "{text}\n[slack]\nenv_file = \"{}\"\npoll_secs = 1\nanswer_wait_secs = 20\n",
                env.display()
            ),
        )
        .unwrap();
        let project = self.path("config/projects/toy.toml");
        let text = std::fs::read_to_string(&project).unwrap();
        std::fs::write(
            &project,
            text.replace("flow = ", "slack_channel = \"toy\"\nflow = "),
        )
        .unwrap();
    }

    fn scenario(&self, words: &str) {
        std::fs::create_dir_all(self.path("bin/state")).unwrap();
        std::fs::write(self.path("bin/state/scenario"), words).unwrap();
    }

    fn config(&self) -> ConfigDir {
        ConfigDir(self.path("config"))
    }

    fn calls(&self) -> Vec<Vec<String>> {
        std::fs::read_to_string(self.path("bin/state/calls"))
            .unwrap_or_default()
            .lines()
            .map(|line| line.split(' ').map(str::to_string).collect())
            .collect()
    }

    fn event_kinds(&self, driver: &Driver) -> Vec<String> {
        std::fs::read_to_string(driver.event_log())
            .unwrap()
            .lines()
            .map(|line| {
                let event: serde_json::Value = serde_json::from_str(line).unwrap();
                match (event["kind"].as_str().unwrap(), event["step"].as_str()) {
                    ("step.ended", Some(step)) => {
                        format!("{step}:{}", event["data"]["outcome"].as_str().unwrap())
                    }
                    (kind, _) => kind.to_string(),
                }
            })
            .collect()
    }
}

const OPTIONS: Options = Options { slack: false };

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
    assert!(fix_brief.contains("Reproduce: sh probe.sh"));
    assert!(fix_brief.contains("probes.patch"));
    assert!(
        !fix_brief.contains("## Acceptance criteria"),
        "a continued session gets only what is new"
    );
    // The reviewer saw the checks and the tester's report, not the builder's words.
    let review_brief = std::fs::read_to_string(world.path("bin/state/brief-5.md")).unwrap();
    assert!(review_brief.contains("## Check `test`"));
    assert!(review_brief.contains("Adversarial testing"));
    assert!(!review_brief.contains("Added a line to feature.txt"));

    // What was published: one commit on the unit's branch, without the probes.
    let remote = world.path("remote.git");
    let branch = format!("refs/heads/{}", unit.branch);
    assert_eq!(unit.branch, "factory/toy-1-add-the-feature");
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
        "README.md\nfeature.txt"
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
    let mut driver = Driver::resume(world.config(), &unit_id, &OPTIONS).unwrap();
    assert_eq!(driver.run().unwrap(), Status::Ended);
    let roles: Vec<String> = world.calls().iter().map(|call| call[0].clone()).collect();
    assert_eq!(
        roles.iter().filter(|role| *role == "builder").count(),
        3,
        "build, its retry, one fix"
    );
    assert!(matches!(
        Driver::resume(world.config(), &unit_id, &OPTIONS),
        Err(DriverError::AlreadyEnded(_))
    ));
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

const SLACK: Options = Options { slack: true };

#[test]
fn the_owner_steers_and_answers_in_the_runs_thread() {
    let world = World::new("steer", "test -f feature.txt");
    world.scenario("ask-once");
    // The owner wrote two things before the first step, and a stranger one.
    let fake = common::FakeSlack::start_with(true, &common::REPLIES, Some("Yes, make it shout."));
    world.with_slack(&fake);

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
    world.with_slack(&fake);
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
    world.with_slack(&fake);
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
