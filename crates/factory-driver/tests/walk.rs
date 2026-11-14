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

    /// Give the project a GitHub repository, played by a scripted `gh` and
    /// the bare remote, and the Zig project's flow, which watches CI and lands.
    /// `ci` is what each look at the checks sees, one word per look.
    fn with_github(&self, ci: &str) {
        use std::os::unix::fs::PermissionsExt;
        let gh = self.path("bin/gh");
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake-gh.sh"),
            &gh,
        )
        .unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let state = self.path("bin/state-gh");
        std::fs::create_dir_all(&state).unwrap();
        let remote = self.path("remote.git");
        std::fs::write(state.join("remote"), remote.to_str().unwrap()).unwrap();
        std::fs::write(state.join("ci"), ci.replace(' ', "\n") + "\n").unwrap();

        // The project says it pushes to GitHub; git is told where that is.
        let toy = self.path("toy");
        run(
            &toy,
            "git",
            &["remote", "set-url", "origin", "git@github.com:me/toy.git"],
        );
        let key = format!("url.{}.insteadOf", remote.display());
        run(&toy, "git", &["config", &key, "git@github.com:me/toy.git"]);

        let factory = self.path("config/factory.toml");
        let text = std::fs::read_to_string(&factory).unwrap();
        std::fs::write(&factory, format!("gh_bin = \"{}\"\n{text}", gh.display())).unwrap();
        let project = self.path("config/projects/toy.toml");
        let text = std::fs::read_to_string(&project)
            .unwrap()
            .replace("flow = \"build-break-review\"", "flow = \"zig-sdk\"");
        std::fs::write(
            &project,
            format!(
                "{text}
[commands.samples-index]
argv = [\"true\"]
timeout_secs = 30

[commands.samples-test]
argv = [\"true\"]
timeout_secs = 30

[commands.test-releasesafe]
argv = [\"true\"]
timeout_secs = 30

[commands.reports]
argv = [\"sh\", \"-c\", \"echo scanned $1 > $0/INDEX.txt; echo No check failed. >> $0/INDEX.txt\", \"{{out}}\", \"{{base}}\"]
timeout_secs = 30
summary = \"INDEX.txt\"
note = \"A scan hit is a line to look at.\"

[github]
repo = \"me/toy\"
write = [\"me/toy\"]
required_checks = [\"fmt\", \"unit\"]
ci_poll_secs = 1
ci_settle_secs = 1
ci_timeout_secs = 30
merge_subject = \"Merge bead/{{bead_short}}: {{title}}\"
merge_body = \"{{summary}}\\n\\n{{testing}}\\n\\nBead: {{bead}}\"
"
            ),
        )
        .unwrap();
    }

    fn gh_calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.path("bin/state-gh/calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
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

const OPTIONS: Options = Options {
    slack: false,
    flow: None,
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
};

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
            "reports:pass",
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

    // The reports were written by a command, once for each version of the
    // code (the build, the fix after the tester, the fix after CI), into a
    // directory outside the worktree, and the steps that need them were
    // briefed with where they are and what their index says.
    assert_eq!(
        kinds
            .iter()
            .filter(|kind| *kind == "artifact.produced")
            .count(),
        3
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
    world.with_slack(&fake);
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
