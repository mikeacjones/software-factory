//! The factory's settings and the project registry, as typed TOML. Unknown
//! keys are errors, so a misspelt setting cannot be silently ignored.
//!
//! The config directory holds `factory.toml`, `projects/<id>.toml`,
//! `flows/<id>.toml` and `roles/<role>.md`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use factory_core::ProjectId;
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path}: {reason}")]
    Invalid { path: PathBuf, reason: String },
}

/// Factory-wide settings: `factory.toml`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Factory {
    /// Where units keep their state, logs, artifacts and worktrees.
    pub state_dir: PathBuf,
    #[serde(default = "default_claude")]
    pub claude_bin: String,
    #[serde(default = "default_gh")]
    pub gh_bin: String,
    /// The one Claude CLI version the factory runs. A different installed
    /// version stops a run before it starts.
    pub cli_version: String,
    pub allowed_models: Vec<String>,
    /// Replace a model wherever a flow names it. Tests map everything to Haiku.
    #[serde(default)]
    pub model_override: BTreeMap<String, String>,
    pub slack: Option<SlackSettings>,
    /// The Claude accounts workers may run under. With none listed there is
    /// one: the CLI's own login.
    #[serde(default, rename = "account")]
    pub accounts: Vec<Account>,
    /// How long after a usage window's reset time to wait before using the
    /// account again.
    #[serde(default = "default_limit_grace")]
    pub limit_grace_secs: u64,
}

fn default_limit_grace() -> u64 {
    60
}

/// A Claude subscription workers can run under.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Account {
    pub id: String,
    /// Where the account's token for running sessions is: a file only the
    /// owner can read, an environment variable, or a Keychain item. With
    /// none of them the account is the CLI's own login.
    pub token_file: Option<PathBuf>,
    pub token_env: Option<String>,
    pub token_keychain: Option<String>,
    /// The Keychain item holding a full login for this account, used only to
    /// read its usage windows without a model call.
    pub usage_keychain: Option<String>,
}

fn default_gh() -> String {
    "gh".to_string()
}

fn default_claude() -> String {
    "claude".to_string()
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlackSettings {
    /// A file only the owner can read, holding SLACK_BOT_TOKEN and
    /// SLACK_OWNER_USER_ID as KEY=value lines.
    pub env_file: PathBuf,
    /// How often the run's thread is read for owner replies.
    #[serde(default = "default_poll")]
    pub poll_secs: u64,
    /// How long a question waits for the owner before the unit is parked.
    #[serde(default = "default_answer_wait")]
    pub answer_wait_secs: u64,
}

fn default_poll() -> u64 {
    10
}

fn default_answer_wait() -> u64 {
    12 * 60 * 60
}

/// One project: `projects/<id>.toml`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    pub id: ProjectId,
    /// The project's main checkout. Worktrees are made from it, never in it.
    pub repo: PathBuf,
    #[serde(default = "default_remote")]
    pub remote: String,
    pub default_branch: String,
    pub beads: BeadSource,
    /// The name beads are claimed under.
    #[serde(default = "default_actor")]
    pub bead_actor: String,
    pub github: Option<GitHub>,
    /// The channel for this project, without `#`. The bot creates it if needed.
    pub slack_channel: Option<String>,
    /// The flow units of this project run.
    pub flow: String,
    /// What the publication step should know about this project's commit
    /// messages and pull requests, such as the trailers it expects.
    pub publication_note: Option<String>,
    /// The first part of a unit's branch name, by bead type. A type not
    /// listed here uses the built-in choice: see [`Project::branch_prefix`].
    #[serde(default)]
    pub branch_prefixes: BTreeMap<String, String>,
    /// Named sets of path globs that a flow step's `writes` refers to.
    #[serde(default)]
    pub scopes: BTreeMap<String, Vec<String>>,
    /// Paths no step may change, whatever its scopes say.
    #[serde(default)]
    pub forbidden_paths: Vec<String>,
    /// Named commands: checks and report generators.
    #[serde(default)]
    pub commands: BTreeMap<String, CommandSpec>,
    /// Commands run once in a new worktree before the first step.
    #[serde(default)]
    pub setup: Vec<String>,
    /// Commands run on the final commit, after the checkpoints are squashed
    /// and before the push. For checks that read the commit message, which
    /// does not exist until then.
    #[serde(default)]
    pub publish_checks: Vec<String>,
    #[serde(default)]
    pub limits: Limits,
}

fn default_remote() -> String {
    "origin".to_string()
}

fn default_actor() -> String {
    "factory".to_string()
}

impl Project {
    /// The kind of change a bead of this type is, as the first part of its
    /// branch name: `feat`, `bug`, `chore` and so on. The branch says what
    /// the change is, not who made it.
    pub fn branch_prefix(&self, issue_type: &str) -> String {
        if let Some(prefix) = self.branch_prefixes.get(issue_type) {
            return prefix.clone();
        }
        match issue_type {
            "feature" | "task" | "" => "feat".to_string(),
            "bug" => "bug".to_string(),
            "chore" | "decision" => "chore".to_string(),
            // Any other type names itself: docs, spike, and whatever comes.
            other => other
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() {
                        c.to_ascii_lowercase()
                    } else {
                        '-'
                    }
                })
                .collect(),
        }
    }

    /// A bead's ID without the project's own prefix: `d51.16.62` for
    /// `temporal-sdk-zig-d51.16.62`.
    pub fn bead_short<'a>(&self, bead: &'a str) -> &'a str {
        bead.strip_prefix(&format!("{}-", self.id)).unwrap_or(bead)
    }
}

/// Where a project's beads live. Always stated, never taken from the working
/// directory or the environment.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum BeadSource {
    /// A bead database on a Dolt server.
    Dolt {
        host: String,
        port: u16,
        #[serde(default = "default_user")]
        user: String,
        database: String,
    },
    /// A directory of `<id>.json` files. For scratch projects and tests.
    Files { dir: PathBuf },
}

fn default_user() -> String {
    "root".to_string()
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitHub {
    /// `owner/name` of the repository pull requests are opened on.
    pub repo: String,
    /// Every repository the factory's code may write to for this project.
    pub write: Vec<String>,
    /// Checks that must be present and passed before a pull request counts as
    /// green. Every other check that exists must pass as well. Naming them
    /// means a workflow that has not started yet cannot be taken for a pass.
    #[serde(default)]
    pub required_checks: Vec<String>,
    #[serde(default = "default_ci_poll")]
    pub ci_poll_secs: u64,
    /// How long to wait for checks before parking the unit.
    #[serde(default = "default_ci_timeout")]
    pub ci_timeout_secs: u64,
    /// Green must still be green this long after it is first seen.
    #[serde(default = "default_ci_settle")]
    pub ci_settle_secs: u64,
    /// How a pull request reaches the default branch.
    #[serde(default)]
    pub merge_method: MergeMethod,
    /// The subject of the commit the landing makes (a merge commit, or the
    /// one commit of a squash). `{bead}` is the bead ID, `{bead_short}` the ID
    /// without the project's prefix, `{title}` the pull request's title and
    /// `{number}` its number.
    pub merge_subject: Option<String>,
    /// The body of that commit. `{summary}` is the paragraph the publication
    /// step wrote for a merge commit; `{commit_body}` and `{commit_trailers}`
    /// are the published commit's own body and its final block of trailers,
    /// for a squash; `{testing}` is the factory's own account of what passed;
    /// `{bead}` and `{bead_short}` the bead ID. Without it GitHub writes its
    /// default body.
    pub merge_body: Option<String>,
    /// Remove the unit's branch from the remote once it has merged.
    #[serde(default = "default_true")]
    pub delete_branch_after_merge: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MergeMethod {
    /// A merge commit on top of the branch's commits.
    #[default]
    Merge,
    /// One new commit holding the whole change.
    Squash,
    /// The branch's commits replayed onto the default branch.
    Rebase,
}

fn default_ci_poll() -> u64 {
    30
}

fn default_ci_timeout() -> u64 {
    3 * 60 * 60
}

fn default_ci_settle() -> u64 {
    60
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandSpec {
    /// `{base}` is replaced by the commit the unit's branch started from, and
    /// `{out}` by the directory the factory made for the artifact the step
    /// produces.
    pub argv: Vec<String>,
    pub timeout_secs: u64,
    /// For a command that produces an artifact: the file in it to show the
    /// steps that need it, such as an index.
    pub summary: Option<String>,
    /// For a command that produces an artifact: what a reader should know
    /// before using it.
    pub note: Option<String>,
}

impl CommandSpec {
    /// Whether the command writes an artifact the factory must make room for.
    pub fn wants_out(&self) -> bool {
        self.argv.iter().any(|arg| arg.contains("{out}"))
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// The wall-clock cap of an agent step, unless the flow step sets its own.
    #[serde(default = "default_step_timeout")]
    pub step_timeout_secs: u64,
    /// Passed to the CLI as its spending cap for one run of one step.
    pub step_budget_usd: Option<f64>,
}

fn default_step_timeout() -> u64 {
    90 * 60
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            step_timeout_secs: default_step_timeout(),
            step_budget_usd: None,
        }
    }
}

/// How goals of one kind run on a project: `goals/<project>/<id>.toml`.
///
/// A goal itself, the objective the owner wants reached, starts as a
/// conversation in Slack. This says who directs it and how.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalDefinition {
    pub id: String,
    /// The definition a project's goals use unless the owner names another.
    #[serde(default)]
    pub default: bool,
    /// The pipeline each bead of the goal goes through. Without it, the
    /// project's own flow.
    pub flow: Option<String>,
    /// The most pipelines the goal may have running at once.
    #[serde(default = "default_pipelines")]
    pub max_pipelines: u32,
    /// A standing objective and stop condition, for a goal the owner starts
    /// without saying more.
    pub objective: Option<String>,
    pub stop_when: Option<String>,
    pub director: Director,
    #[serde(default)]
    pub permissions: GoalPermissions,
}

fn default_pipelines() -> u32 {
    1
}

/// The agent that directs a goal.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Director {
    pub model: String,
    pub effort: String,
    /// The wall-clock cap of one wake.
    #[serde(default = "default_wake_timeout")]
    pub timeout_secs: u64,
}

fn default_wake_timeout() -> u64 {
    30 * 60
}

/// Whether the director's decision is carried out at once or put to the owner.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Permission {
    Allow,
    #[default]
    Ask,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalPermissions {
    /// Filing a new bead, with its type, priority and deferral.
    #[serde(default = "allow")]
    pub file_beads: Permission,
    /// Changing a bead that already exists: its acceptance criteria, its
    /// priority, or deferring it.
    #[serde(default)]
    pub edit_beads: Permission,
}

fn allow() -> Permission {
    Permission::Allow
}

impl Default for GoalPermissions {
    fn default() -> Self {
        Self {
            file_beads: Permission::Allow,
            edit_beads: Permission::Ask,
        }
    }
}

/// A config directory.
#[derive(Clone, Debug)]
pub struct ConfigDir(pub PathBuf);

impl ConfigDir {
    /// `$FACTORY_CONFIG_DIR`, or `~/.config/software-factory`.
    pub fn default_location() -> ConfigDir {
        match std::env::var_os("FACTORY_CONFIG_DIR") {
            Some(dir) => ConfigDir(PathBuf::from(dir)),
            None => ConfigDir(expand_home(Path::new("~/.config/software-factory"))),
        }
    }

    fn read(&self, relative: &str) -> Result<(PathBuf, String), ConfigError> {
        let path = self.0.join(relative);
        let text = std::fs::read_to_string(&path).map_err(|source| ConfigError::Io {
            path: path.clone(),
            source,
        })?;
        Ok((path, text))
    }

    pub fn factory(&self) -> Result<Factory, ConfigError> {
        let (path, text) = self.read("factory.toml")?;
        let mut factory: Factory = parse(&path, &text)?;
        factory.state_dir = expand_home(&factory.state_dir);
        if let Some(slack) = &mut factory.slack {
            slack.env_file = expand_home(&slack.env_file);
        }
        if factory.accounts.is_empty() {
            factory.accounts.push(Account {
                id: "claude".to_string(),
                ..Account::default()
            });
        }
        let mut seen = std::collections::BTreeSet::new();
        for account in &mut factory.accounts {
            account.token_file = account.token_file.as_deref().map(expand_home);
            if !seen.insert(account.id.clone()) {
                return Err(ConfigError::Invalid {
                    path,
                    reason: format!("account `{}` is listed twice", account.id),
                });
            }
        }
        Ok(factory)
    }

    pub fn project(&self, id: &str) -> Result<Project, ConfigError> {
        let (path, text) = self.read(&format!("projects/{id}.toml"))?;
        let mut project: Project = parse(&path, &text)?;
        if project.id.as_str() != id {
            return Err(ConfigError::Invalid {
                path,
                reason: format!("id is `{}` but the file is named `{id}.toml`", project.id),
            });
        }
        project.repo = expand_home(&project.repo);
        if let BeadSource::Files { dir } = &mut project.beads {
            *dir = expand_home(dir);
        }
        for (list, names) in [
            ("setup", &project.setup),
            ("publish_checks", &project.publish_checks),
        ] {
            for name in names {
                if !project.commands.contains_key(name) {
                    return Err(ConfigError::Invalid {
                        path,
                        reason: format!("{list} names the command `{name}`, which is not defined"),
                    });
                }
            }
        }
        for (name, command) in &project.commands {
            if command.argv.is_empty() || command.timeout_secs == 0 {
                return Err(ConfigError::Invalid {
                    path,
                    reason: format!("command `{name}` needs an argv and a timeout"),
                });
            }
        }
        for (issue_type, prefix) in &project.branch_prefixes {
            let ok = !prefix.is_empty()
                && prefix
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
            if !ok {
                return Err(ConfigError::Invalid {
                    path,
                    reason: format!(
                        "branch prefix `{prefix}` for `{issue_type}` must be lowercase letters, digits and dashes"
                    ),
                });
            }
        }
        if let Some(github) = &project.github
            && !github.write.contains(&github.repo)
        {
            return Err(ConfigError::Invalid {
                path,
                reason: format!(
                    "github.repo `{}` is not in the write list, so no pull request could be opened",
                    github.repo
                ),
            });
        }
        Ok(project)
    }

    /// The IDs of every project in the registry.
    pub fn projects(&self) -> Result<Vec<String>, ConfigError> {
        let dir = self.0.join("projects");
        let mut ids = Vec::new();
        for entry in std::fs::read_dir(&dir).map_err(|source| ConfigError::Io {
            path: dir.clone(),
            source,
        })? {
            let name = entry
                .map_err(|source| ConfigError::Io {
                    path: dir.clone(),
                    source,
                })?
                .file_name()
                .to_string_lossy()
                .into_owned();
            if let Some(id) = name.strip_suffix(".toml") {
                ids.push(id.to_string());
            }
        }
        ids.sort();
        Ok(ids)
    }

    /// Every goal definition of a project. A project with none has no goals.
    pub fn goal_definitions(&self, project: &str) -> Result<Vec<GoalDefinition>, ConfigError> {
        let dir = self.0.join("goals").join(project);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Ok(Vec::new());
        };
        let mut definitions = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(id) = name.strip_suffix(".toml") else {
                continue;
            };
            let (path, text) = self.read(&format!("goals/{project}/{name}"))?;
            let definition: GoalDefinition = parse(&path, &text)?;
            let invalid = |reason: String| ConfigError::Invalid {
                path: path.clone(),
                reason,
            };
            if definition.id != id {
                return Err(invalid(format!(
                    "id is `{}` but the file is named `{name}`",
                    definition.id
                )));
            }
            if definition.max_pipelines == 0 {
                return Err(invalid("max_pipelines must be at least 1".to_string()));
            }
            definitions.push(definition);
        }
        definitions.sort_by(|a, b| a.id.cmp(&b.id));
        let defaults = definitions.iter().filter(|d| d.default).count();
        if defaults > 1 || (defaults == 0 && definitions.len() > 1) {
            return Err(ConfigError::Invalid {
                path: dir,
                reason: "exactly one goal definition of a project must be marked `default = true`"
                    .to_string(),
            });
        }
        Ok(definitions)
    }

    /// The text of a flow file.
    pub fn flow_text(&self, id: &str) -> Result<String, ConfigError> {
        Ok(self.read(&format!("flows/{id}.toml"))?.1)
    }

    /// The system prompt of a role.
    pub fn role_prompt(&self, role: &str) -> Result<String, ConfigError> {
        Ok(self.read(&format!("roles/{role}.md"))?.1)
    }
}

fn parse<T: serde::de::DeserializeOwned>(path: &Path, text: &str) -> Result<T, ConfigError> {
    toml::from_str(text).map_err(|err| ConfigError::Invalid {
        path: path.to_path_buf(),
        reason: err.to_string(),
    })
}

/// Replace a leading `~` with the home directory.
pub fn expand_home(path: &Path) -> PathBuf {
    match (path.strip_prefix("~"), std::env::var_os("HOME")) {
        (Ok(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => path.to_path_buf(),
    }
}

/// Read `KEY=value` lines. Blank lines and `#` comments are skipped, and one
/// pair of surrounding quotes is removed from a value.
pub fn read_env_file(path: &Path) -> Result<BTreeMap<String, String>, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut values = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|inner| inner.strip_suffix('"'))
                .or_else(|| {
                    value
                        .strip_prefix('\'')
                        .and_then(|inner| inner.strip_suffix('\''))
                })
                .unwrap_or(value);
            values.insert(key.trim().to_string(), value.to_string());
        }
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROJECT: &str = r#"
id = "toy"
repo = "~/toy"
default_branch = "main"
flow = "minimal"
forbidden_paths = [".github/**"]
setup = ["deps"]

[beads]
kind = "dolt"
host = "127.0.0.1"
port = 3306
database = "toy"

[github]
repo = "me/toy"
write = ["me/toy"]

[scopes]
code = ["src/**"]

[commands.deps]
argv = ["true"]
timeout_secs = 60
"#;

    fn dir_with(project: &str) -> ConfigDir {
        let dir = std::env::temp_dir().join(format!(
            "factory-config-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(dir.join("projects")).unwrap();
        std::fs::write(dir.join("projects/toy.toml"), project).unwrap();
        ConfigDir(dir)
    }

    #[test]
    fn a_project_record_loads_with_defaults() {
        let dir = dir_with(PROJECT);
        let project = dir.project("toy").unwrap();
        std::fs::remove_dir_all(&dir.0).unwrap();
        assert_eq!(project.remote, "origin");
        assert_eq!(project.bead_actor, "factory");
        assert_eq!(project.limits.step_timeout_secs, 5400);
        assert!(!project.repo.starts_with("~"));
        assert!(matches!(project.beads, BeadSource::Dolt { port: 3306, .. }));
    }

    #[test]
    fn a_branch_says_what_kind_of_change_it_is() {
        let dir =
            dir_with(&PROJECT.replace("[beads]", "[branch_prefixes]\nbug = \"fix\"\n\n[beads]"));
        let project = dir.project("toy").unwrap();
        std::fs::remove_dir_all(&dir.0).unwrap();
        assert_eq!(project.branch_prefix("feature"), "feat");
        assert_eq!(project.branch_prefix("task"), "feat");
        assert_eq!(project.branch_prefix("chore"), "chore");
        assert_eq!(project.branch_prefix("docs"), "docs");
        assert_eq!(project.branch_prefix("Spike!"), "spike-");
        // The project's own choice wins.
        assert_eq!(project.branch_prefix("bug"), "fix");
        assert_eq!(project.bead_short("toy-d51.2"), "d51.2");
        assert_eq!(project.bead_short("other-7"), "other-7");
    }

    #[test]
    fn a_branch_prefix_that_git_would_dislike_is_refused() {
        let dir = dir_with(
            &PROJECT.replace("[beads]", "[branch_prefixes]\nbug = \"Bug Fix\"\n\n[beads]"),
        );
        let err = dir.project("toy").unwrap_err().to_string();
        std::fs::remove_dir_all(&dir.0).unwrap();
        assert!(err.contains("`Bug Fix`"), "{err}");
    }

    #[test]
    fn an_unknown_key_is_named() {
        let dir = dir_with(&PROJECT.replace("flow = \"minimal\"", "flow = \"minimal\"\nspeed = 3"));
        let err = dir.project("toy").unwrap_err().to_string();
        std::fs::remove_dir_all(&dir.0).unwrap();
        assert!(err.contains("speed"), "{err}");
    }

    #[test]
    fn a_pull_request_repo_outside_the_write_list_is_refused() {
        let dir = dir_with(&PROJECT.replace("write = [\"me/toy\"]", "write = [\"me/other\"]"));
        let err = dir.project("toy").unwrap_err().to_string();
        std::fs::remove_dir_all(&dir.0).unwrap();
        assert!(err.contains("not in the write list"), "{err}");
    }

    #[test]
    fn a_setup_command_must_be_defined() {
        let dir = dir_with(&PROJECT.replace("setup = [\"deps\"]", "setup = [\"nope\"]"));
        let err = dir.project("toy").unwrap_err().to_string();
        std::fs::remove_dir_all(&dir.0).unwrap();
        assert!(err.contains("`nope`"), "{err}");
    }

    #[test]
    fn goal_definitions_load_and_one_is_the_default() {
        let dir = dir_with(PROJECT);
        let goals = dir.0.join("goals/toy");
        std::fs::create_dir_all(&goals).unwrap();
        let definition = |id: &str, extra: &str| {
            format!("id = \"{id}\"\n{extra}\n[director]\nmodel = \"m\"\neffort = \"high\"\n")
        };
        std::fs::write(
            goals.join("parity.toml"),
            definition("parity", "default = true\nmax_pipelines = 2"),
        )
        .unwrap();
        std::fs::write(
            goals.join("bugs.toml"),
            definition("bugs", "[permissions]\nedit_beads = \"allow\""),
        )
        .unwrap();
        let loaded = dir.goal_definitions("toy").unwrap();
        assert_eq!(
            loaded.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(),
            ["bugs", "parity"]
        );
        assert_eq!(loaded[1].max_pipelines, 2);
        assert_eq!(loaded[1].permissions.edit_beads, Permission::Ask);
        assert_eq!(loaded[1].permissions.file_beads, Permission::Allow);
        assert_eq!(loaded[0].permissions.edit_beads, Permission::Allow);
        assert_eq!(dir.projects().unwrap(), ["toy"]);
        assert!(dir.goal_definitions("other").unwrap().is_empty());

        // Two defaults, a cap of zero and a misspelt key are each refused.
        std::fs::write(
            goals.join("bugs.toml"),
            definition("bugs", "default = true"),
        )
        .unwrap();
        assert!(
            dir.goal_definitions("toy")
                .unwrap_err()
                .to_string()
                .contains("exactly one")
        );
        std::fs::write(
            goals.join("bugs.toml"),
            definition("bugs", "max_pipelines = 0"),
        )
        .unwrap();
        assert!(
            dir.goal_definitions("toy")
                .unwrap_err()
                .to_string()
                .contains("at least 1")
        );
        std::fs::write(goals.join("bugs.toml"), definition("bugs", "pipelines = 3")).unwrap();
        assert!(
            dir.goal_definitions("toy")
                .unwrap_err()
                .to_string()
                .contains("pipelines")
        );
        std::fs::remove_dir_all(&dir.0).unwrap();
    }

    #[test]
    fn env_files_are_read_without_their_quotes() {
        let path = std::env::temp_dir().join(format!("factory-env-{}", std::process::id()));
        std::fs::write(&path, "# c\nA=1\nB=\"two\"\n\nC='three'\n").unwrap();
        let values = read_env_file(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(values["A"], "1");
        assert_eq!(values["B"], "two");
        assert_eq!(values["C"], "three");
    }
}
