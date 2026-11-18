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
    pub allowed_models: Vec<String>,
    /// The agents a step may be run by besides Claude Code, by the name a
    /// step gives: `[agent.codex]`. A step that names none is run by
    /// `claude_bin`, on one of `allowed_models`.
    #[serde(default)]
    pub agent: BTreeMap<String, AgentSettings>,
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
    /// How the factory is kept running under launchd.
    #[serde(default)]
    pub service: ServiceSettings,
    /// The Temporal server the flow engine runs on, if any.
    pub temporal: Option<Temporal>,
    /// The classifier that is asked what kind of failure a failed check
    /// shows, if any. Without this table nothing is sent anywhere.
    pub judge: Option<JudgeSettings>,
}

/// The classifier a failed check's lines are sent to: a System One model
/// behind TypeSafe's API. What it says is recorded in the unit's log and
/// nothing is routed by it.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JudgeSettings {
    /// A file only the owner can read, holding the API key as a
    /// KEY=value line.
    pub env_file: PathBuf,
    /// The name the key has in that file.
    #[serde(default = "default_judge_key")]
    pub key_env: String,
    #[serde(default = "default_judge_url")]
    pub url: String,
    /// A fixed version, never an alias that moves: a new model is a new
    /// set of readings, to be compared before it is relied on.
    #[serde(default = "default_judge_model")]
    pub model: String,
    /// How long one reading may take before it is given up, in seconds.
    #[serde(default = "default_judge_timeout")]
    pub timeout_secs: u64,
}

fn default_judge_key() -> String {
    "TYPESAFE_API_KEY".to_string()
}

fn default_judge_url() -> String {
    "https://api.typesafe.ai/v1/systemone".to_string()
}

fn default_judge_model() -> String {
    "jev-1.13.0".to_string()
}

fn default_judge_timeout() -> u64 {
    5
}

fn default_limit_grace() -> u64 {
    60
}

/// The Temporal server the flow engine runs on.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Temporal {
    /// Where the server is, such as `http://192.168.101.240:7233`.
    pub address: String,
    pub namespace: String,
    /// The queue the factory's workers take pipelines and steps from.
    #[serde(default = "default_task_queue")]
    pub task_queue: String,
    /// The worker deployment the factory's workers are builds of.
    #[serde(default = "default_service_label")]
    pub deployment: String,
    /// How often a running step says it is alive, in seconds. A step whose
    /// worker died is given to another after three of these.
    #[serde(default = "default_beat")]
    pub beat_secs: u64,
    /// How often the accounts workflow asks again what each account has
    /// left, in seconds. Between those it acts on what runs report.
    #[serde(default = "default_accounts_check")]
    pub accounts_check_secs: u64,
    /// How often a Temporal Schedule sweeps the open beads of each project
    /// that keeps GitHub issues in step with them, in seconds: a bead with
    /// no issue gets one, and each issue's type and priority follow its
    /// bead's.
    #[serde(default = "default_issue_sync")]
    pub issue_sync_secs: u64,
    /// Goals run their pipelines on the engine: every bead, sub-goal bead
    /// and sub-goal being shipped is a workflow, started through the
    /// engine's own command. Off, the service runs them itself.
    #[serde(default)]
    pub pipelines: bool,
    /// The tiers are on the engine too: each project's executive,
    /// directors and foremen take their turns in a workflow, which is
    /// signalled what the owner does in Slack and how the pipelines are
    /// doing. The service then only starts the engine's worker.
    #[serde(default)]
    pub tiers: bool,
    /// The engine's command, when it is not `bin/factory-engine` under
    /// the state directory. Reaching the server from this machine is
    /// allowed to a program by its path, so it does not move.
    #[serde(default)]
    pub engine_bin: Option<PathBuf>,
}

fn default_issue_sync() -> u64 {
    3600
}

fn default_accounts_check() -> u64 {
    15 * 60
}

fn default_task_queue() -> String {
    "factory".to_string()
}

fn default_beat() -> u64 {
    20
}

/// How the factory is kept running: as a launchd agent of the desktop
/// session of the user it runs as.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceSettings {
    /// The launchd job's name.
    #[serde(default = "default_service_label")]
    pub label: String,
    /// Where the service and everything it starts find their tools, in
    /// order: launchd gives a job almost no `PATH` of its own.
    #[serde(default = "default_service_path")]
    pub path: Vec<String>,
    /// How many releases to keep.
    #[serde(default = "default_releases_kept")]
    pub releases_kept: usize,
    /// Start the service through the user's login shell, so that it and
    /// every agent and command it starts have the environment the user's
    /// own terminal has: the `PATH` and whatever else the shell's profile
    /// sets. Without it they have only `path`.
    #[serde(default = "default_login_shell")]
    pub login_shell: bool,
}

fn default_service_label() -> String {
    "software-factory".to_string()
}

fn default_service_path() -> Vec<String> {
    [
        "~/.local/bin",
        "/opt/homebrew/bin",
        "/opt/homebrew/sbin",
        "/usr/local/bin",
        "/usr/bin",
        "/bin",
        "/usr/sbin",
        "/sbin",
    ]
    .iter()
    .map(|dir| dir.to_string())
    .collect()
}

fn default_releases_kept() -> usize {
    5
}

fn default_login_shell() -> bool {
    true
}

impl Default for ServiceSettings {
    fn default() -> Self {
        ServiceSettings {
            label: default_service_label(),
            path: default_service_path(),
            releases_kept: default_releases_kept(),
            login_shell: default_login_shell(),
        }
    }
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
    /// A file of `KEY=value` lines, only the owner's to read, in which
    /// `token_env` names the token. It is then the account's one source:
    /// the factory's own environment, the Keychain and the CLI's login are
    /// not looked at, and a missing or empty value is an error, so that an
    /// account is the same subscription wherever the factory is started
    /// from and whoever is logged in to the CLI.
    pub token_env_file: Option<PathBuf>,
    /// The Keychain item holding a full login for this account, used only to
    /// read its usage windows without a model call.
    pub usage_keychain: Option<String>,
    /// Refresh that login when it is about to expire and write it back to
    /// the same Keychain item. A login Claude Code itself also uses needs
    /// `usage_lock_dir` as well.
    #[serde(default)]
    pub usage_refresh: bool,
    /// For a login Claude Code itself keeps fresh too: its configuration
    /// directory (`~/.claude` for the default login). The factory then
    /// refreshes as Claude Code does, under the lock Claude Code keeps
    /// there, and only a login that has not been refreshed meanwhile.
    pub usage_lock_dir: Option<PathBuf>,
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
    /// Post only what needs the owner, and name the owner in it: a
    /// question, something to approve, a goal that stopped by itself.
    /// What a pipeline, a director or a foreman would otherwise say as it
    /// goes is written to its log, where the dashboard reads it, and a
    /// pipeline opens a thread only to ask the owner something.
    #[serde(default)]
    pub quiet: bool,
}

fn default_poll() -> u64 {
    10
}

fn default_answer_wait() -> u64 {
    12 * 60 * 60
}

/// An agent a step may be run by, other than Claude Code.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSettings {
    /// The agent's binary.
    pub bin: String,
    /// The models a step run by this agent may name.
    pub allowed_models: Vec<String>,
    /// Tool servers a run of this agent is given, by name: the language
    /// servers, each behind a bridge that speaks the Model Context
    /// Protocol.
    #[serde(default)]
    pub mcp: BTreeMap<String, ToolServer>,
}

/// A tool server started for a run of an agent.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolServer {
    pub command: String,
    /// `{cwd}` is the directory the run works in.
    #[serde(default)]
    pub args: Vec<String>,
    /// Start the server only for a run whose directory holds this file:
    /// `build.zig` for the Zig language server, and so on.
    pub when_file: Option<String>,
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
    /// The most goals that may run on this project at once. A goal started
    /// beyond that waits in a queue and starts when a running one ends.
    #[serde(default = "default_goals")]
    pub max_goals: u32,
    /// The agent the owner talks to in the project's channel, above every
    /// goal's director. It opens goals, and decides which may run at the
    /// same time, up to its slots, which then stand in for `max_goals`.
    /// Without it a top-level message opens a goal with its director at
    /// once, and goals queue by `max_goals`.
    pub executive: Option<Tier>,
    /// Keep every branch the factory is working on pushed to the remote as
    /// it changes: a unit's after each step and whenever its agent commits
    /// during one, and a sub-goal's after each bead it absorbs. Work in
    /// hand then survives the loss of this machine. A bead's branch is
    /// removed from the remote once its work is in its sub-goal's branch
    /// and that branch has been pushed.
    #[serde(default)]
    pub sync_branches: bool,
    /// The owner has told every director of this project's goals to go with
    /// its own recommendations. A director that would ask the owner which
    /// way to go chooses itself and says what it chose in the goal's
    /// thread, and code refuses a wake that only asks and leaves nothing
    /// running. Its proposal that a goal is complete closes the goal. The
    /// owner still starts each goal from its charter. A goal definition's
    /// own `director_decides` overrides this.
    #[serde(default)]
    pub director_decides: bool,
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
    /// Regular expressions for lines that are tests or assertions in this
    /// project. A fix that removes a matching line is pointed out to the
    /// reviewer.
    #[serde(default)]
    pub test_patterns: Vec<String>,
    /// Paths no step may change, whatever its scopes say.
    #[serde(default)]
    pub forbidden_paths: Vec<String>,
    /// Files in the repository that say how its code is to be written. A
    /// builder is told to write to them, and a reviewer to judge idiom and
    /// quality against them and not against its own taste.
    #[serde(default)]
    pub standards: Vec<String>,
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

fn default_goals() -> u32 {
    1
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
    /// What a builder handed a CI failure should know besides the failing
    /// logs: how a failure that does not happen on this machine can be
    /// reproduced on it. `{config}` is the factory's config directory.
    pub ci_failure_note: Option<String>,
    /// Once a check has failed, how long to go on waiting for the checks
    /// still running, so that every failure of the run is handed back
    /// together and fixed in one round. Zero hands the first one back at once.
    #[serde(default = "default_ci_red_wait")]
    pub ci_red_wait_secs: u64,
    /// How often the failed jobs of a pull request's CI are run again on
    /// the same commit before a builder is sent. Jobs that pass then failed
    /// for a reason that was not the change's: the step goes on as green,
    /// and the failure is recorded as a flake for whoever runs the goal to
    /// file. Jobs that fail every time go to the builder, told so. Zero
    /// runs nothing again.
    #[serde(default)]
    pub ci_reruns: u32,
    /// Regular expressions for the lines of a failed CI job's log that say
    /// what failed, as a command's `failure_lines` are for its output. They
    /// are what a flake is recorded with. Without them, the log's last lines.
    #[serde(default)]
    pub ci_failure_lines: Vec<String>,
    /// Keep GitHub issues in step with beads: a bead with no issue gets one
    /// when the factory files it or starts work on it, and a pull request
    /// must close the issue of every bead it lands.
    #[serde(default)]
    pub issues: bool,
    /// Labels for an issue the factory creates. `{type}` is the bead's type
    /// and `{priority}` its priority as a word (critical, high, medium, low,
    /// none). A label the repository does not have is left off.
    #[serde(default)]
    pub issue_labels: Vec<String>,
    /// The repository's own issue type for each bead type, by name, such as
    /// `bug = "Bug"`; `"*"` is for every type not listed. Set on an issue
    /// the factory creates, beside its labels. Empty sets none.
    #[serde(default)]
    pub issue_types: BTreeMap<String, String>,
    /// The repository's own issue field that holds the priority.
    pub issue_priority: Option<IssuePriority>,
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

/// A single-select issue field of the repository that holds an issue's
/// priority, such as GitHub's `Priority`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssuePriority {
    /// The field's name.
    pub field: String,
    /// The option for each bead priority, from 0 (most urgent) up.
    pub options: Vec<String>,
}

impl GitHub {
    /// The repository's issue type and priority option for a bead: the
    /// type's name, and the priority field with its option.
    pub fn issue_fields(
        &self,
        bead_type: &str,
        priority: i64,
    ) -> (Option<String>, Option<(String, String)>) {
        let issue_type = self
            .issue_types
            .get(bead_type)
            .or_else(|| self.issue_types.get("*"))
            .cloned();
        let option = self.issue_priority.as_ref().and_then(|setting| {
            let option = setting.options.get(usize::try_from(priority).ok()?)?;
            Some((setting.field.clone(), option.clone()))
        });
        (issue_type, option)
    }
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

fn default_ci_red_wait() -> u64 {
    20 * 60
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandSpec {
    /// `{base}` is replaced by the commit the unit's branch started from,
    /// `{out}` by the directory the factory made for the artifact the step
    /// produces, and `{config}` by the factory's config directory, for a
    /// tool the factory keeps for the project and the project does not have.
    pub argv: Vec<String>,
    pub timeout_secs: u64,
    /// What to call the command where people read it, such as a thread or a
    /// commit message, when its argv would mean nothing to them.
    pub shown: Option<String>,
    /// For a command that produces an artifact: the file in it to show the
    /// steps that need it, such as an index.
    pub summary: Option<String>,
    /// For a command that produces an artifact: what a reader should know
    /// before using it.
    pub note: Option<String>,
    /// For a command that produces an artifact with a summary: patterns
    /// (regular expressions, matched line by line) that make the step fail
    /// when the summary matches one, whatever the command's exit code. It is
    /// how a report that is otherwise advisory can still block on the few
    /// results that must never pass.
    #[serde(default)]
    pub fail_when: Vec<String>,
    /// How long the command has, after SIGTERM at its timeout, to stop what
    /// it started and print what it has before it is killed.
    #[serde(default = "default_stop_grace")]
    pub stop_grace_secs: u64,
    /// Patterns (regular expressions, matched line by line on the output of
    /// a run that failed or timed out) that mean the host was at fault, not
    /// the code: a download that came back broken, a daemon that is not
    /// running. Such a run counts as one that could not run, and parks the
    /// step: no fix round can help.
    #[serde(default)]
    pub could_not_run_when: Vec<String>,
    /// Exit codes that mean the same, for a tool that says so by its code.
    #[serde(default)]
    pub could_not_run_exit: Vec<i32>,
    /// Patterns that pick out the lines of a failed run's output that name
    /// what failed (a test, a compiler error), for telling one failure from
    /// another. Without them the last lines of the output stand for it.
    #[serde(default)]
    pub failure_lines: Vec<String>,
    /// What the factory finds out about a failed run before a builder is
    /// sent to fix it.
    #[serde(default)]
    pub triage: Triage,
    /// The command changes files in the worktree as it runs, such as one
    /// that regenerates a file and then compares it. In a group of commands
    /// it runs before the others start, and alone, so that none of them
    /// reads a file it is in the middle of writing.
    #[serde(default)]
    pub edits: bool,
}

/// Runs the factory makes of a command that failed, to tell a failure of the
/// change from one that is not: each is off unless the project turns it on,
/// because each costs another run of the command.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Triage {
    /// Run it once more on the change. A pass makes the failure a flake: the
    /// step passes, and the flake is recorded.
    #[serde(default)]
    pub rerun: bool,
    /// Run it on the code as it was at the unit's base: a copy of the
    /// worktree with every file the change touched put back. A failure there
    /// too is not the change's, and parks the step for whoever decides.
    #[serde(default)]
    pub base: bool,
}

fn default_stop_grace() -> u64 {
    30
}

impl CommandSpec {
    /// The command as people are shown it.
    pub fn display(&self) -> String {
        self.shown.clone().unwrap_or_else(|| self.argv.join(" "))
    }

    /// The command as it can be typed: its argv, with the factory's config
    /// directory filled in. `{base}` and `{out}` stay as they are written.
    pub fn runnable(&self, config: &Path) -> String {
        self.argv
            .join(" ")
            .replace("{config}", &config.to_string_lossy())
    }

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
    /// How long one reproduction or regression command may run. One that is
    /// still running then counts as failing, which is how a hang is caught.
    #[serde(default = "default_reproduction_timeout")]
    pub reproduction_timeout_secs: u64,
}

fn default_reproduction_timeout() -> u64 {
    10 * 60
}

fn default_step_timeout() -> u64 {
    90 * 60
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            step_timeout_secs: default_step_timeout(),
            step_budget_usd: None,
            reproduction_timeout_secs: default_reproduction_timeout(),
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
    /// The pipeline that ships a sub-goal: run once on the sub-goal's branch,
    /// it opens the one pull request for all its beads and lands it. With it,
    /// `flow` is the light pipeline of one bead, which ends by absorbing into
    /// the sub-goal's branch. Without it every bead is its own pull request.
    pub ship_flow: Option<String>,
    /// The most pipelines the goal may have running at once.
    #[serde(default = "default_pipelines")]
    pub max_pipelines: u32,
    /// Files and directories that nearly every piece of work adds to: a
    /// README, a table of what is supported, changelog entries, a
    /// generated index. They never count when work is checked for
    /// overlap, or none of it could ever run side by side. Two pieces of
    /// work that change the same one are folded together when the second
    /// is absorbed or shipped, and a conflict there goes to its builder.
    #[serde(default)]
    pub shared_paths: Vec<String>,
    /// A standing objective and stop condition, for a goal the owner starts
    /// without saying more.
    pub objective: Option<String>,
    pub stop_when: Option<String>,
    /// A command of the project that decides the stop condition. The
    /// factory runs it on the default branch whenever that has moved, shows
    /// the director what it said, and refuses the director's proposal that
    /// the goal is complete while it fails. Without it the stop condition
    /// is the director's to judge and the owner's to accept.
    pub stop_check: Option<String>,
    pub director: Tier,
    /// The agent that runs each sub-goal: it starts the sub-goal's beads,
    /// rules on their findings and reports to the director. Without it the
    /// director does that itself.
    pub foreman: Option<Tier>,
    /// The agent that designs each sub-goal before any of its beads is
    /// built: the public interface it adds or changes, its error sets and
    /// invariants, where the code goes and the order of its beads. The
    /// director accepts the design or sends it back, and every agent that
    /// works on the sub-goal's beads is given it. Without it each builder
    /// settles those for its own bead.
    pub designer: Option<Tier>,
    /// When the factory runs this definition by itself, with no message
    /// from the owner. A definition with a schedule is not a goal the owner
    /// opens: its `director` tier is the agent of each run, which is given
    /// the charter and what the records show, and may only file beads.
    pub schedule: Option<Schedule>,
    #[serde(default)]
    pub permissions: GoalPermissions,
    /// Lighter models for director wakes that code can tell need less than
    /// the director's own. Without them every wake runs on `director`.
    #[serde(default)]
    pub routes: Routes,
    /// This definition's own setting of the project's `director_decides`.
    /// Left out, the project's applies: [`ConfigDir::goal_definitions`]
    /// fills it in, so a loaded definition always has it.
    pub director_decides: Option<bool>,
}

/// Which model a director wake runs on, by what woke it. Code decides
/// the route from the goal's record alone, with no model asked: a wake the
/// owner, a new goal, a landed sub-goal or a design waiting for a ruling
/// woke is the director's own; a foreman's report that a sub-goal is ready,
/// with every planned bead absorbed and nothing else in question, is a
/// `ship` wake; what is left over (an escalation, a pipeline or a ship
/// that failed, a note from the executive) is a `settle` wake. A lighter
/// model may make only the decisions its route allows, and hands anything
/// else up to the director's own model in the same wake.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Routes {
    /// May only ship the sub-goal, or hand it up.
    pub ship: Option<String>,
    /// May start, continue, ship, and talk to the foreman and the
    /// executive; never plan, file, change beads or end the goal.
    pub settle: Option<String>,
}

impl GoalDefinition {
    /// Whether this goal's director goes with its own recommendations
    /// rather than waiting for the owner. See `Project::director_decides`.
    pub fn director_decides(&self) -> bool {
        self.director_decides == Some(true)
    }
}

/// When a definition runs by itself, and what each run is for.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Schedule {
    /// The hour of the day, in UTC, a run is due at.
    pub hour_utc: u32,
    /// A run is due every so many days.
    #[serde(default = "default_every_days")]
    pub every_days: u32,
    /// What the runs are for, given to the agent at every run.
    pub charter: String,
    /// The most beads one run may file.
    #[serde(default = "default_max_beads")]
    pub max_beads: u32,
    /// Commands of the project to run on the default branch before each
    /// run, whose output the agent is shown: a report code cannot compute
    /// from the factory's own records, such as one of duplicated code. A
    /// command that fails is shown too, and holds nothing up.
    #[serde(default)]
    pub reports: Vec<String>,
    /// The definition of the project that a run's beads are built under.
    /// With it a run may mark beads to build: the factory opens a goal
    /// under that definition for them, starts it with no word from the
    /// owner, and marks it complete when every one of them is closed.
    /// Without it a run only files, and what it files waits for a goal
    /// the owner opens.
    pub build_under: Option<String>,
    /// A run is also due, at once, when a class of defect has broken this
    /// many beads more than a run was last shown. Without it runs come by
    /// the clock only.
    pub when_class_breaks: Option<u32>,
    /// The most goals one run may propose to the owner. With it the
    /// definition is a review of the factory as a whole and not upkeep of
    /// its project: its agent is shown how the factory ran, from the
    /// records, and what became of what it proposed before, and proposes
    /// goals for any project. It files nothing, and nothing it proposes is
    /// opened without the owner's button.
    pub proposes: Option<u32>,
}

fn default_every_days() -> u32 {
    1
}

fn default_max_beads() -> u32 {
    3
}

fn default_pipelines() -> u32 {
    1
}

/// One level of the agents that run a goal: the director of the goal, or
/// the foreman of a sub-goal. Each is woken by events from the level below
/// it, decides, and has its decisions checked by code.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tier {
    pub model: String,
    pub effort: String,
    /// The wall-clock cap of one wake.
    #[serde(default = "default_wake_timeout")]
    pub timeout_secs: u64,
    /// The most pieces of work it may have running below it at once: bead
    /// pipelines for a foreman, sub-goals for a director. A maximum, never
    /// a target: the agent runs work together only when it is safe to.
    #[serde(default = "default_slots")]
    pub slots: u32,
    /// The model to run instead when every account is at its limit for
    /// `model`, so that the agent goes on rather than waits for a window
    /// to reopen. Without it the agent waits. A designer has none: what it
    /// designs binds every builder of its sub-goal.
    pub fallback_model: Option<String>,
}

fn default_slots() -> u32 {
    1
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
    /// For changes to beads only: carried out at once on a bead the goal's
    /// own agents filed, and put to the owner on any other. What the
    /// factory wrote down itself it may also put right itself.
    Own,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalPermissions {
    /// Filing a new bead, with its type, priority and deferral.
    #[serde(default = "allow")]
    pub file_beads: Permission,
    /// Changing a bead that already exists: its acceptance criteria, its
    /// priority, or deferring it. `own` lets the director change the beads
    /// its goal filed and asks the owner about every other.
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
        for dir in &mut factory.service.path {
            *dir = expand_home(Path::new(dir)).to_string_lossy().into_owned();
        }
        if let Some(slack) = &mut factory.slack {
            slack.env_file = expand_home(&slack.env_file);
        }
        for agent in factory.agent.values_mut() {
            for server in agent.mcp.values_mut() {
                server.command = expand_home(Path::new(&server.command))
                    .to_string_lossy()
                    .into_owned();
            }
        }
        if let Some(judge) = factory.judge.as_mut() {
            judge.env_file = expand_home(&judge.env_file);
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
            account.token_env_file = account.token_env_file.as_deref().map(expand_home);
            account.usage_lock_dir = account.usage_lock_dir.as_deref().map(expand_home);
            if account.token_env_file.is_some() {
                let wrong = if account.token_env.is_none() {
                    Some("but no `token_env` to name the token in it")
                } else if account.token_file.is_some() || account.token_keychain.is_some() {
                    Some("and another source for its token as well: it may have one")
                } else {
                    None
                };
                if let Some(wrong) = wrong {
                    return Err(ConfigError::Invalid {
                        path,
                        reason: format!("account `{}` has `token_env_file` {wrong}", account.id),
                    });
                }
            }
            if account.usage_refresh && account.usage_keychain.is_none() {
                return Err(ConfigError::Invalid {
                    path,
                    reason: format!(
                        "account `{}` has `usage_refresh` but no `usage_keychain` to refresh",
                        account.id
                    ),
                });
            }
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
        // A definition that does not say whether its director decides alone
        // takes the project's setting. A project that cannot be read here
        // fails wherever it is read next; until then its goals ask.
        let decides = self
            .project(project)
            .map(|project| project.director_decides)
            .unwrap_or(false);
        let mut definitions = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(id) = name.strip_suffix(".toml") else {
                continue;
            };
            let (path, text) = self.read(&format!("goals/{project}/{name}"))?;
            let mut definition: GoalDefinition = parse(&path, &text)?;
            definition.director_decides.get_or_insert(decides);
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
            // A design binds every builder of its sub-goal: it waits for
            // its own model rather than being written by another.
            if definition
                .designer
                .as_ref()
                .is_some_and(|designer| designer.fallback_model.is_some())
            {
                return Err(invalid(
                    "the designer cannot have a fallback_model: a design binds every builder of its sub-goal, so it waits for its own model"
                        .to_string(),
                ));
            }
            if definition.max_pipelines == 0 {
                return Err(invalid("max_pipelines must be at least 1".to_string()));
            }
            if definition.permissions.file_beads == Permission::Own {
                return Err(invalid(
                    "permissions.file_beads is `allow` or `ask`: `own` is for edit_beads, since a bead being filed is nobody's yet"
                        .to_string(),
                ));
            }
            for path in &definition.shared_paths {
                let plain = !path.is_empty()
                    && path != "."
                    && !path.starts_with('/')
                    && !path.split('/').any(|part| part == ".." || part.is_empty())
                    && !path.contains(['*', '?', '[', '\\']);
                if !plain {
                    return Err(invalid(format!(
                        "shared_paths: `{path}` is not a plain path inside the repository: name files and directories, with no patterns"
                    )));
                }
            }
            // Work that runs side by side has to be folded back together:
            // a sub-goal's beads by its foreman's pipelines, and sub-goals
            // by shipping one at a time.
            if definition.director.slots == 0 {
                return Err(invalid("director.slots must be at least 1".to_string()));
            }
            if definition.director.slots > 1 && definition.foreman.is_none() {
                return Err(invalid(
                    "director.slots above 1 needs a foreman: sub-goals worked on side by side are each run by one"
                        .to_string(),
                ));
            }
            if let Some(foreman) = &definition.foreman {
                if foreman.slots == 0 {
                    return Err(invalid("foreman.slots must be at least 1".to_string()));
                }
                if foreman.slots > definition.max_pipelines {
                    return Err(invalid(format!(
                        "foreman.slots is {} but max_pipelines, the limit for the whole goal, is {}",
                        foreman.slots, definition.max_pipelines
                    )));
                }
            }
            if definition.foreman.is_some() && definition.ship_flow.is_none() {
                return Err(invalid(
                    "a foreman runs a sub-goal, so the definition needs a ship_flow".to_string(),
                ));
            }
            if let Some(schedule) = &definition.schedule {
                if definition.default {
                    return Err(invalid(
                        "a definition with a schedule is run by the factory, so it cannot be the default for goals the owner opens".to_string(),
                    ));
                }
                if schedule.hour_utc > 23 || schedule.every_days == 0 {
                    return Err(invalid(
                        "schedule: hour_utc is 0 to 23 and every_days is at least 1".to_string(),
                    ));
                }
                if schedule.charter.trim().is_empty() || !(1..=10).contains(&schedule.max_beads) {
                    return Err(invalid(
                        "schedule: it needs a charter, and max_beads from 1 to 10".to_string(),
                    ));
                }
                if schedule.when_class_breaks == Some(0) {
                    return Err(invalid(
                        "schedule: when_class_breaks is at least 1".to_string(),
                    ));
                }
                if let Some(most) = schedule.proposes {
                    if !(1..=5).contains(&most) {
                        return Err(invalid("schedule: proposes is from 1 to 5".to_string()));
                    }
                    if schedule.build_under.is_some()
                        || schedule.when_class_breaks.is_some()
                        || !schedule.reports.is_empty()
                    {
                        return Err(invalid(
                            "schedule: a run that proposes goals files and builds nothing, so it cannot have build_under, reports or when_class_breaks".to_string(),
                        ));
                    }
                }
            }
            if definition.designer.is_some() && definition.ship_flow.is_none() {
                return Err(invalid(
                    "a designer designs a sub-goal, so the definition needs a ship_flow"
                        .to_string(),
                ));
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
    fn a_definition_takes_the_projects_director_decides_unless_it_says_otherwise() {
        let definition = |id: &str, extra: &str| {
            format!("id = \"{id}\"\n{extra}\n[director]\nmodel = \"m\"\neffort = \"high\"\n")
        };
        let write = |dir: &ConfigDir| {
            let goals = dir.0.join("goals/toy");
            std::fs::create_dir_all(&goals).unwrap();
            std::fs::write(
                goals.join("plain.toml"),
                definition("plain", "default = true"),
            )
            .unwrap();
            std::fs::write(
                goals.join("asks.toml"),
                definition("asks", "director_decides = false"),
            )
            .unwrap();
            std::fs::write(
                goals.join("decides.toml"),
                definition("decides", "director_decides = true"),
            )
            .unwrap();
        };
        let decides = |dir: &ConfigDir| {
            dir.goal_definitions("toy")
                .unwrap()
                .iter()
                .map(|d| (d.id.clone(), d.director_decides()))
                .collect::<Vec<_>>()
        };
        let owner = |plain: bool| {
            vec![
                ("asks".to_string(), false),
                ("decides".to_string(), true),
                ("plain".to_string(), plain),
            ]
        };

        // A designer may not fall back to another model.
        let dir = dir_with(PROJECT);
        let goals = dir.0.join("goals/toy");
        std::fs::create_dir_all(&goals).unwrap();
        std::fs::write(
            goals.join("main.toml"),
            format!(
                "{}[designer]\nmodel = \"m\"\neffort = \"high\"\nfallback_model = \"n\"\n",
                definition("main", "default = true")
            ),
        )
        .unwrap();
        let err = dir.goal_definitions("toy").unwrap_err().to_string();
        assert!(
            err.contains("the designer cannot have a fallback_model"),
            "{err}"
        );
        std::fs::remove_dir_all(&dir.0).unwrap();

        // By default every director asks.
        let dir = dir_with(PROJECT);
        write(&dir);
        assert!(!dir.project("toy").unwrap().director_decides);
        assert_eq!(decides(&dir), owner(false));
        std::fs::remove_dir_all(&dir.0).unwrap();

        // The project's setting reaches each definition that says nothing.
        let dir =
            dir_with(&PROJECT.replace("id = \"toy\"", "id = \"toy\"\ndirector_decides = true"));
        write(&dir);
        assert_eq!(decides(&dir), owner(true));
        std::fs::remove_dir_all(&dir.0).unwrap();
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
        std::fs::write(
            goals.join("upkeep.toml"),
            definition("upkeep", "[permissions]\nedit_beads = \"own\""),
        )
        .unwrap();
        let loaded = dir.goal_definitions("toy").unwrap();
        assert_eq!(
            loaded.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(),
            ["bugs", "parity", "upkeep"]
        );
        assert_eq!(loaded[1].max_pipelines, 2);
        assert_eq!(loaded[1].permissions.edit_beads, Permission::Ask);
        assert_eq!(loaded[1].permissions.file_beads, Permission::Allow);
        assert_eq!(loaded[0].permissions.edit_beads, Permission::Allow);
        // The director may change the beads its own goal filed, and no others.
        assert_eq!(loaded[2].permissions.edit_beads, Permission::Own);
        std::fs::remove_file(goals.join("upkeep.toml")).unwrap();
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
        // Shared paths are files and directories, named plainly.
        std::fs::write(
            goals.join("bugs.toml"),
            definition("bugs", "shared_paths = [\"README.md\", \"docs/*.md\"]"),
        )
        .unwrap();
        assert!(
            dir.goal_definitions("toy")
                .unwrap_err()
                .to_string()
                .contains("shared_paths: `docs/*.md` is not a plain path")
        );
        std::fs::write(goals.join("bugs.toml"), definition("bugs", "pipelines = 3")).unwrap();
        assert!(
            dir.goal_definitions("toy")
                .unwrap_err()
                .to_string()
                .contains("pipelines")
        );

        // A foreman is a second tier with the same shape as the director.
        // It needs sub-goals to run, and no tier runs work side by side yet.
        let foreman = "\n[foreman]\nmodel = \"f\"\neffort = \"medium\"\n";
        let with_foreman = |extra: &str, tail: &str| definition("bugs", extra) + foreman + tail;
        std::fs::write(
            goals.join("bugs.toml"),
            with_foreman("ship_flow = \"ship\"", ""),
        )
        .unwrap();
        let loaded = dir.goal_definitions("toy").unwrap();
        let tier = loaded[0].foreman.clone().unwrap();
        assert_eq!((tier.model.as_str(), tier.slots), ("f", 1));
        assert_eq!(tier.timeout_secs, loaded[0].director.timeout_secs);
        assert!(loaded[1].foreman.is_none());
        std::fs::write(goals.join("bugs.toml"), with_foreman("", "")).unwrap();
        assert!(
            dir.goal_definitions("toy")
                .unwrap_err()
                .to_string()
                .contains("needs a ship_flow")
        );
        // A foreman may run beads side by side, within the goal's own limit.
        std::fs::write(
            goals.join("bugs.toml"),
            with_foreman("ship_flow = \"ship\"", "slots = 2\n"),
        )
        .unwrap();
        assert!(
            dir.goal_definitions("toy")
                .unwrap_err()
                .to_string()
                .contains(
                    "foreman.slots is 2 but max_pipelines, the limit for the whole goal, is 1"
                )
        );
        std::fs::write(
            goals.join("bugs.toml"),
            with_foreman("ship_flow = \"ship\"\nmax_pipelines = 2", "slots = 2\n"),
        )
        .unwrap();
        assert_eq!(
            dir.goal_definitions("toy").unwrap()[0]
                .foreman
                .as_ref()
                .map(|tier| tier.slots),
            Some(2)
        );
        // A director may have sub-goals side by side only with foremen to
        // run them.
        std::fs::write(
            goals.join("bugs.toml"),
            definition("bugs", "") + "slots = 2\n",
        )
        .unwrap();
        assert!(
            dir.goal_definitions("toy")
                .unwrap_err()
                .to_string()
                .contains("director.slots above 1 needs a foreman")
        );
        std::fs::write(
            goals.join("bugs.toml"),
            definition("bugs", "ship_flow = \"ship\"") + "slots = 2\n" + foreman,
        )
        .unwrap();
        assert_eq!(dir.goal_definitions("toy").unwrap()[0].director.slots, 2);
        std::fs::remove_dir_all(&dir.0).unwrap();
    }

    #[test]
    fn an_account_that_refreshes_its_login_names_one_and_may_share_claude_codes_lock() {
        let dir = dir_with(PROJECT);
        let factory = |accounts: &str| {
            std::fs::write(
                dir.0.join("factory.toml"),
                format!("state_dir = \"/tmp/s\"\nallowed_models = [\"m\"]\n{accounts}"),
            )
            .unwrap();
            dir.factory()
        };
        let loaded = factory(
            "[[account]]\nid = \"claude\"\nusage_keychain = \"Claude Code-credentials\"\nusage_refresh = true\nusage_lock_dir = \"~/.claude\"\n",
        )
        .unwrap();
        let account = &loaded.accounts[0];
        assert!(account.usage_refresh);
        let lock_dir = account.usage_lock_dir.clone().unwrap();
        assert!(
            lock_dir.ends_with(".claude") && !lock_dir.starts_with("~"),
            "{lock_dir:?}"
        );
        // A token kept in a file of variables: the file and the variable
        // are both needed, and it is then the only source.
        let loaded = factory(
            "[[account]]\nid = \"a\"\ntoken_env_file = \"~/secrets.env\"\ntoken_env = \"A_TOKEN\"\n",
        )
        .unwrap();
        let file = loaded.accounts[0].token_env_file.clone().unwrap();
        assert!(
            file.ends_with("secrets.env") && !file.starts_with("~"),
            "{file:?}"
        );
        for (wrong, why) in [
            ("token_env_file = \"/x.env\"\n", "no `token_env`"),
            (
                "token_env_file = \"/x.env\"\ntoken_env = \"A\"\ntoken_keychain = \"k\"\n",
                "another source",
            ),
        ] {
            let refused = factory(&format!("[[account]]\nid = \"a\"\n{wrong}")).unwrap_err();
            assert!(refused.to_string().contains(why), "{refused}");
        }
        // Refreshing needs a login to refresh.
        let refused = factory("[[account]]\nid = \"claude\"\nusage_refresh = true\n").unwrap_err();
        assert!(
            refused
                .to_string()
                .contains("no `usage_keychain` to refresh"),
            "{refused}"
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
