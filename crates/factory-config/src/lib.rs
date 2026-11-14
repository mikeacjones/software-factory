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
    /// The subject of the merge commit. `{bead}` is the bead ID, `{bead_short}`
    /// the ID without the project's prefix, `{title}` the pull request's title.
    pub merge_subject: Option<String>,
    /// Remove the unit's branch from the remote once it has merged.
    #[serde(default = "default_true")]
    pub delete_branch_after_merge: bool,
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
    pub argv: Vec<String>,
    pub timeout_secs: u64,
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
