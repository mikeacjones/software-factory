//! A project in a box, for tests that run real pipelines: a repository with
//! one commit, a bare remote, beads in files, a config directory that uses
//! the repository's own flows and role prompts, and scripted stand-ins for
//! the `claude` and `gh` CLIs. Shared by the driver's tests and the engine's.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use factory_config::ConfigDir;
use factory_driver::Driver;

pub struct World {
    pub root: PathBuf,
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

pub fn run(dir: &Path, program: &str, args: &[&str]) -> String {
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

/// The repository's own config directory. The same from any crate of the
/// workspace: each is two levels below its root.
pub fn repo_config() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config")
}

/// Where the scripted stand-ins for `claude` and `gh` are kept.
fn driver_tests() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../factory-driver/tests")
}

impl World {
    /// A project with one commit, a bare remote, one bead and a config
    /// directory that uses the repository's own flow and role prompts.
    pub fn new(name: &str, test_command: &str) -> World {
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
        std::fs::copy(driver_tests().join("fake-claude.sh"), &fake).unwrap();
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
                "state_dir = \"{}\"\nclaude_bin = \"{}\"\nallowed_models = [\"claude-opus-5-5\"]\n",
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
runtime = ["runtime/**"]

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

    pub fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    /// Give the project a Slack channel, served by a local stand-in.
    pub fn with_slack(&self, slack_url: &str) {
        let env = self.path("slack.env");
        std::fs::write(
            &env,
            format!(
                "SLACK_BOT_TOKEN=xoxb-test\nSLACK_OWNER_USER_ID=OWNER\nSLACK_API_URL={}\n",
                slack_url
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
    pub fn with_github(&self, ci: &str) {
        use std::os::unix::fs::PermissionsExt;
        let gh = self.path("bin/gh");
        std::fs::copy(driver_tests().join("fake-gh.sh"), &gh).unwrap();
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

[commands.test-linux]
argv = [\"true\"]
timeout_secs = 30

[commands.reports]
argv = [\"sh\", \"-c\", \"echo scanned $1 > $0/INDEX.txt; echo No check failed. >> $0/INDEX.txt\", \"{{out}}\", \"{{base}}\"]
timeout_secs = 30
summary = \"INDEX.txt\"
note = \"A scan hit is a line to look at.\"

[commands.test-proof]
argv = [\"sh\", \"-c\", \"echo FAILS-AT-BASE feature.txt: the feature > $0/INDEX.txt\", \"{{out}}\"]
timeout_secs = 30
shown = \"each new test, run on the code as it was\"
summary = \"INDEX.txt\"
fail_when = ['^PASSES-AT-BASE ']
note = \"FAILS-AT-BASE is proof that a test tests the change.\"

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

    pub fn gh_calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.path("bin/state-gh/calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    pub fn scenario(&self, words: &str) {
        std::fs::create_dir_all(self.path("bin/state")).unwrap();
        std::fs::write(self.path("bin/state/scenario"), words).unwrap();
    }

    pub fn config(&self) -> ConfigDir {
        ConfigDir(self.path("config"))
    }

    pub fn calls(&self) -> Vec<Vec<String>> {
        std::fs::read_to_string(self.path("bin/state/calls"))
            .unwrap_or_default()
            .lines()
            .map(|line| line.split(' ').map(str::to_string).collect())
            .collect()
    }

    pub fn event_kinds(&self, driver: &Driver) -> Vec<String> {
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
