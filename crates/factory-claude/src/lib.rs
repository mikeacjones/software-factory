//! Everything that touches the `claude` binary: the exact command line of a
//! worker, a typed reading of its output stream, the one way each run ended,
//! and what it used.
//!
//! Workers are headless and in bypass mode. `is_error` on the result is the
//! authority on whether a run succeeded: the subtype has been seen to say
//! "success" on a run that failed.

pub mod accounts;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use factory_supervise::{Control, Spec, StreamExit};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use factory_supervise::Error as SuperviseError;

/// Start a new session, or carry on an earlier one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Session {
    Fresh,
    Resume(String),
}

/// One run of one agent step.
#[derive(Clone, Debug)]
pub struct RunSpec {
    pub claude_bin: String,
    pub model: String,
    pub effort: String,
    pub session: Session,
    /// The role's prompt, added to Claude Code's own system prompt.
    pub system_prompt: String,
    /// The JSON Schema the typed result must match.
    pub schema: Value,
    pub cwd: PathBuf,
    /// The briefing, fed on stdin.
    pub prompt: String,
    pub budget_usd: Option<f64>,
    pub timeout: Duration,
    /// The account's token, for an account that is not the CLI's own login.
    pub oauth_token: Option<String>,
    /// A file to write the run's session ID to as soon as the CLI reports
    /// it, which is at the start. If whatever is watching the run dies, the
    /// file says which session to resume instead of starting over. The
    /// caller removes it when the run has ended.
    pub session_file: Option<PathBuf>,
}

/// The exact command for a run. Always headless, always bypass mode, never
/// `--bare`, and a session is resumed by ID, never by "most recent".
pub fn command(spec: &RunSpec) -> Spec {
    let mut argv: Vec<String> = [
        spec.claude_bin.as_str(),
        "--print",
        "--output-format",
        "stream-json",
        "--verbose",
        "--permission-mode",
        "bypassPermissions",
        // The project's own settings are left out: its hooks were written for
        // people and other agents, and must not steer a worker.
        "--setting-sources",
        "user",
        "--model",
        &spec.model,
        "--effort",
        &spec.effort,
        "--append-system-prompt",
        &spec.system_prompt,
        "--json-schema",
        &spec.schema.to_string(),
    ]
    .iter()
    .map(|part| part.to_string())
    .collect();
    if let Session::Resume(id) = &spec.session {
        argv.extend(["--resume".to_string(), id.clone()]);
    }
    if let Some(budget) = spec.budget_usd {
        argv.extend(["--max-budget-usd".to_string(), format!("{budget:.2}")]);
    }

    let mut command = Spec::new(argv, &spec.cwd, spec.timeout);
    command.stdin = Some(spec.prompt.clone());
    // A worker must not inherit the identity of whatever session started the
    // factory, nor an effort or model override from its environment.
    command.env_remove = std::env::vars_os()
        .filter_map(|(key, _)| key.into_string().ok())
        .filter(|key| key.starts_with("CLAUDE") || key.starts_with("ANTHROPIC"))
        .collect();
    // Subagents run on the step's model, not on whatever the default is.
    command.env = vec![("CLAUDE_CODE_SUBAGENT_MODEL".to_string(), spec.model.clone())];
    // The account is chosen for each run, and reaches the CLI only here.
    if let Some(token) = &spec.oauth_token {
        command
            .env
            .push(("CLAUDE_CODE_OAUTH_TOKEN".to_string(), token.clone()));
    }
    command
}

/// One usage window of an account, as the rate-limit event reports it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Window {
    pub utilization: f64,
    pub resets_at: u64,
    pub status: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RateLimit {
    /// `allowed`, `allowed_warning` or `rejected`.
    pub status: String,
    pub resets_at: Option<u64>,
    pub limit_type: Option<String>,
    /// Keyed by window name, such as `five_hour` and `seven_day`.
    pub windows: BTreeMap<String, Window>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cost_usd: f64,
}

/// What one run used.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub cost_usd: f64,
    pub turns: u64,
    pub duration_ms: u64,
    pub models: BTreeMap<String, ModelUsage>,
    /// The account's windows after this run.
    pub rate_limit: Option<RateLimit>,
}

impl Usage {
    pub fn output_tokens(&self) -> u64 {
        self.models.values().map(|model| model.output_tokens).sum()
    }

    /// Add another run's usage. The later run's rate-limit reading wins.
    pub fn add(&mut self, other: &Usage) {
        self.cost_usd += other.cost_usd;
        self.turns += other.turns;
        self.duration_ms += other.duration_ms;
        for (model, usage) in &other.models {
            let total = self.models.entry(model.clone()).or_default();
            total.input_tokens += usage.input_tokens;
            total.output_tokens += usage.output_tokens;
            total.cache_read_tokens += usage.cache_read_tokens;
            total.cache_write_tokens += usage.cache_write_tokens;
            total.cost_usd += usage.cost_usd;
        }
        if other.rate_limit.is_some() {
            self.rate_limit = other.rate_limit.clone();
        }
    }
}

/// The final event of a run.
#[derive(Clone, Debug, PartialEq)]
pub struct ResultEvent {
    pub is_error: bool,
    pub subtype: String,
    pub api_error_status: Option<u64>,
    pub text: String,
    pub structured_output: Option<Value>,
}

/// One line of the stream, as far as the factory reads it.
#[derive(Clone, Debug, PartialEq)]
pub enum StreamEvent {
    Init {
        session_id: String,
        model: String,
        version: String,
    },
    /// The model spoke or called tools.
    Assistant {
        text: Option<String>,
        tools: Vec<String>,
    },
    /// A tool finished.
    ToolResult,
    RateLimit(RateLimit),
    /// A result, what it used, and whether that is a part to add up (true)
    /// or a running total that replaces what came before (false).
    Result(ResultEvent, Usage, bool),
    /// A type this parser does not read. Tolerated and counted.
    Other(String),
}

/// Parse one line. `None` means the line is not a JSON object at all.
pub fn parse_line(line: &str) -> Option<StreamEvent> {
    let value: Value = serde_json::from_str(line).ok()?;
    let object = value.as_object()?;
    let kind = object.get("type").and_then(Value::as_str).unwrap_or("");
    let text_of = |key: &str| {
        object
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    Some(match kind {
        "system" if object.get("subtype").and_then(Value::as_str) == Some("init") => {
            StreamEvent::Init {
                session_id: text_of("session_id"),
                model: text_of("model"),
                version: text_of("claude_code_version"),
            }
        }
        "assistant" => {
            let content = value
                .pointer("/message/content")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let mut text = String::new();
            let mut tools = Vec::new();
            for block in &content {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        text.push_str(block.get("text").and_then(Value::as_str).unwrap_or(""))
                    }
                    Some("tool_use") => tools.push(
                        block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    ),
                    _ => {}
                }
            }
            StreamEvent::Assistant {
                text: (!text.trim().is_empty()).then_some(text),
                tools,
            }
        }
        "user" => StreamEvent::ToolResult,
        "rate_limit_event" => match object.get("rate_limit_info").map(rate_limit) {
            Some(info) => StreamEvent::RateLimit(info),
            None => StreamEvent::Other(kind.to_string()),
        },
        "result" => {
            let result = ResultEvent {
                is_error: object
                    .get("is_error")
                    .and_then(Value::as_bool)
                    // A result that does not say is not a success.
                    .unwrap_or(true),
                subtype: text_of("subtype"),
                api_error_status: object.get("api_error_status").and_then(Value::as_u64),
                text: text_of("result"),
                structured_output: object
                    .get("structured_output")
                    .filter(|output| !output.is_null())
                    .cloned(),
            };
            let (used, incremental) = usage(&value);
            StreamEvent::Result(result, used, incremental)
        }
        other => StreamEvent::Other(other.to_string()),
    })
}

fn rate_limit(info: &Value) -> RateLimit {
    let mut windows = BTreeMap::new();
    if let Some(map) = info.get("unifiedWindows").and_then(Value::as_object) {
        for (name, window) in map {
            windows.insert(
                name.clone(),
                Window {
                    utilization: window
                        .get("utilization")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0),
                    resets_at: window.get("resetsAt").and_then(Value::as_u64).unwrap_or(0),
                    status: window
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                },
            );
        }
    }
    RateLimit {
        status: info
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        resets_at: info.get("resetsAt").and_then(Value::as_u64),
        limit_type: info
            .get("rateLimitType")
            .and_then(Value::as_str)
            .map(str::to_string),
        windows,
    }
}

/// What one result event says was used since the result before it.
///
/// `modelUsage` and `total_cost_usd` are running totals for the whole
/// session, so on a resumed session they include every earlier run. The CLI
/// also reports the part since the last result, and that is what is counted.
/// The second value is false when only the running totals were present.
fn usage(result: &Value) -> (Usage, bool) {
    let (source, incremental) = match result
        .get("model_usage_since_last_result")
        .and_then(Value::as_object)
    {
        Some(map) => (Some(map), true),
        None => (result.get("modelUsage").and_then(Value::as_object), false),
    };
    let mut models = BTreeMap::new();
    for (model, entry) in source.into_iter().flatten() {
        let count = |key: &str| entry.get(key).and_then(Value::as_u64).unwrap_or(0);
        models.insert(
            model.clone(),
            ModelUsage {
                input_tokens: count("inputTokens"),
                output_tokens: count("outputTokens"),
                cache_read_tokens: count("cacheReadInputTokens"),
                cache_write_tokens: count("cacheCreationInputTokens"),
                cost_usd: entry.get("costUSD").and_then(Value::as_f64).unwrap_or(0.0),
            },
        );
    }
    let cost_usd = if incremental {
        models.values().map(|model| model.cost_usd).sum()
    } else {
        result
            .get("total_cost_usd")
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
    };
    let usage = Usage {
        cost_usd,
        turns: result.get("num_turns").and_then(Value::as_u64).unwrap_or(0),
        duration_ms: result
            .get("duration_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        models,
        rate_limit: None,
    };
    (usage, incremental)
}

/// What a run's stream said, gathered line by line.
#[derive(Clone, Debug, Default)]
pub struct Transcript {
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub version: Option<String>,
    pub rate_limit: Option<RateLimit>,
    /// The last result event. One run can emit several: work the model left
    /// in the background wakes it again, and each time it returns a result.
    pub result: Option<ResultEvent>,
    /// What every result of this run used, added up.
    pub used: Usage,
    pub results: u64,
    pub tool_calls: u64,
    /// The most recent thing the model said or did, for a status line.
    pub last_activity: Option<String>,
    /// Lines of a type this parser does not read, by type.
    pub unknown: BTreeMap<String, u64>,
    /// Lines that were not JSON objects.
    pub unparsed: u64,
}

impl Transcript {
    pub fn observe(&mut self, line: &str) {
        match parse_line(line) {
            None => self.unparsed += 1,
            Some(StreamEvent::Init {
                session_id,
                model,
                version,
            }) => {
                self.session_id = Some(session_id);
                self.model = Some(model);
                self.version = Some(version);
            }
            Some(StreamEvent::Assistant { text, tools }) => {
                self.tool_calls += tools.len() as u64;
                if let Some(tool) = tools.last() {
                    self.last_activity = Some(format!("using {tool}"));
                } else if let Some(text) = text {
                    self.last_activity = Some(first_line(&text, 140));
                }
            }
            Some(StreamEvent::ToolResult) => {}
            Some(StreamEvent::RateLimit(info)) => self.rate_limit = Some(info),
            Some(StreamEvent::Result(result, used, incremental)) => {
                if incremental {
                    self.used.add(&used);
                } else {
                    self.used = used;
                }
                self.results += 1;
                self.result = Some(result);
            }
            Some(StreamEvent::Other(kind)) => *self.unknown.entry(kind).or_insert(0) += 1,
        }
    }

    /// What the run used, with the account's windows as last reported.
    pub fn usage(&self) -> Usage {
        let mut usage = self.used.clone();
        usage.rate_limit = self.rate_limit.clone();
        usage
    }
}

fn first_line(text: &str, max: usize) -> String {
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    let mut out: String = line.trim().chars().take(max).collect();
    if line.trim().chars().count() > max {
        out.push('…');
    }
    out
}

/// The one way a run ended.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// The run finished and returned a typed result.
    Completed(Value),
    /// The account is at a usage limit. The session can be resumed later.
    Limited {
        window: Option<String>,
        resets_at: Option<u64>,
    },
    ApiError {
        status: Option<u64>,
        message: String,
    },
    AuthError(String),
    BudgetExceeded,
    /// The run finished without a usable typed result.
    InvalidResult(String),
    /// The process ended without a result event.
    Crashed {
        code: Option<i32>,
    },
    /// The run hit its wall-clock cap and was killed.
    TimedOut,
    /// The run was killed on request, for this reason.
    Killed(String),
}

impl Outcome {
    /// A short name for logs and messages.
    pub fn name(&self) -> &'static str {
        match self {
            Outcome::Completed(_) => "completed",
            Outcome::Limited { .. } => "limited",
            Outcome::ApiError { .. } => "api_error",
            Outcome::AuthError(_) => "auth_error",
            Outcome::BudgetExceeded => "budget_exceeded",
            Outcome::InvalidResult(_) => "invalid_result",
            Outcome::Crashed { .. } => "crashed",
            Outcome::TimedOut => "timed_out",
            Outcome::Killed(_) => "killed",
        }
    }
}

/// Decide how a run ended from how its process ended and what its stream said.
pub fn classify(transcript: &Transcript, exit: &StreamExit) -> Outcome {
    if let Some(reason) = &exit.killed {
        return Outcome::Killed(reason.clone());
    }
    if exit.timed_out {
        return Outcome::TimedOut;
    }
    let Some(result) = &transcript.result else {
        return Outcome::Crashed { code: exit.code };
    };
    let rejected = transcript
        .rate_limit
        .as_ref()
        .filter(|limit| limit.status == "rejected");
    if result.is_error {
        if let Some(limit) = rejected {
            return Outcome::Limited {
                window: limit.limit_type.clone(),
                resets_at: limit.resets_at,
            };
        }
        if result.subtype.contains("budget") {
            return Outcome::BudgetExceeded;
        }
        return match result.api_error_status {
            Some(401 | 403) => Outcome::AuthError(result.text.clone()),
            status => Outcome::ApiError {
                status,
                message: result.text.clone(),
            },
        };
    }
    match &result.structured_output {
        Some(output) if output.is_object() => Outcome::Completed(output.clone()),
        Some(_) => Outcome::InvalidResult("the typed result is not an object".to_string()),
        None => Outcome::InvalidResult("the run returned no typed result".to_string()),
    }
}

/// A finished run.
#[derive(Clone, Debug)]
pub struct Finished {
    pub outcome: Outcome,
    pub transcript: Transcript,
    pub exit: StreamExit,
}

/// Run a worker to the end. `tick` is called about once a second with what
/// the stream has said so far, and may ask for the run to be killed.
pub fn run(
    spec: &RunSpec,
    log: &Path,
    tick: &mut dyn FnMut(&Transcript) -> Control,
) -> Result<Finished, SuperviseError> {
    let transcript = std::cell::RefCell::new(Transcript::default());
    let exit = factory_supervise::stream(
        &command(spec),
        log,
        &mut |line| {
            let mut transcript = transcript.borrow_mut();
            let known = transcript.session_id.is_some();
            transcript.observe(line);
            if let (false, Some(id), Some(file)) =
                (known, &transcript.session_id, &spec.session_file)
            {
                let _ = std::fs::write(file, id);
            }
        },
        &mut || tick(&transcript.borrow()),
    )?;
    let transcript = transcript.into_inner();
    Ok(Finished {
        outcome: classify(&transcript, &exit),
        transcript,
        exit,
    })
}

/// The version the installed CLI reports, such as `2.1.325`.
pub fn installed_version(claude_bin: &str) -> Result<String, SuperviseError> {
    let mut spec = Spec::new(
        [claude_bin, "--version"],
        std::env::temp_dir(),
        Duration::from_secs(30),
    );
    spec.env_remove = vec!["CLAUDECODE".to_string()];
    let out = factory_supervise::capture(&spec)?;
    Ok(out
        .stdout
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUCCESS: &str = include_str!("../fixtures/success-haiku-2.1.325.jsonl");
    const REJECTED: &str = include_str!("../fixtures/limit-rejected-reconstructed.jsonl");

    fn transcript(text: &str) -> Transcript {
        let mut transcript = Transcript::default();
        for line in text.lines() {
            transcript.observe(line);
        }
        transcript
    }

    fn exited(code: i32) -> StreamExit {
        StreamExit {
            code: Some(code),
            timed_out: false,
            killed: None,
            duration: Duration::from_secs(1),
            lines: 0,
        }
    }

    #[test]
    fn a_recorded_run_parses_into_session_result_and_usage() {
        let transcript = transcript(SUCCESS);
        assert_eq!(transcript.unparsed, 0);
        assert_eq!(
            transcript.session_id.as_deref(),
            Some("d2694d46-b201-4a35-a0fe-099b6b78576a")
        );
        assert_eq!(transcript.version.as_deref(), Some("2.1.325"));
        assert_eq!(transcript.tool_calls, 3);

        let Outcome::Completed(result) = classify(&transcript, &exited(0)) else {
            panic!("expected Completed");
        };
        assert_eq!(result["outcome"], "done");

        let usage = transcript.usage();
        assert_eq!(usage.turns, 4);
        assert!((usage.cost_usd - 0.00617234).abs() < 1e-9);
        let haiku = &usage.models["claude-haiku-5-5"];
        assert_eq!(haiku.output_tokens, 958);
        assert_eq!(haiku.cache_read_tokens, 50394);
        let limit = usage.rate_limit.unwrap();
        assert_eq!(limit.status, "allowed");
        assert_eq!(limit.windows["five_hour"].utilization, 0.15);
        assert_eq!(limit.windows["seven_day"].resets_at, 1794711600);
    }

    #[test]
    fn a_run_refused_at_the_limit_is_limited_whatever_its_subtype_says() {
        let transcript = transcript(REJECTED);
        // The result claims subtype "success"; is_error is the authority.
        assert_eq!(transcript.result.as_ref().unwrap().subtype, "success");
        assert_eq!(
            classify(&transcript, &exited(1)),
            Outcome::Limited {
                window: Some("five_hour".to_string()),
                resets_at: Some(1794680400)
            }
        );
    }

    #[test]
    fn an_error_result_is_never_completed() {
        let line = r#"{"type":"result","subtype":"success","is_error":true,"api_error_status":500,"result":"overloaded","structured_output":{"outcome":"done"}}"#;
        assert_eq!(
            classify(&transcript(line), &exited(0)),
            Outcome::ApiError {
                status: Some(500),
                message: "overloaded".to_string()
            }
        );
        let auth = line.replace("500", "401");
        assert!(matches!(
            classify(&transcript(&auth), &exited(1)),
            Outcome::AuthError(_)
        ));
        let budget = r#"{"type":"result","subtype":"error_max_budget_usd","is_error":true}"#;
        assert_eq!(
            classify(&transcript(budget), &exited(1)),
            Outcome::BudgetExceeded
        );
    }

    #[test]
    fn a_result_that_does_not_say_whether_it_failed_is_a_failure() {
        let line =
            r#"{"type":"result","subtype":"success","structured_output":{"outcome":"done"}}"#;
        assert!(matches!(
            classify(&transcript(line), &exited(0)),
            Outcome::ApiError { .. }
        ));
    }

    #[test]
    fn a_success_without_a_typed_result_is_invalid() {
        let line = r#"{"type":"result","subtype":"success","is_error":false,"result":"done!"}"#;
        assert!(matches!(
            classify(&transcript(line), &exited(0)),
            Outcome::InvalidResult(_)
        ));
    }

    #[test]
    fn no_result_event_is_a_crash_and_a_kill_is_a_kill() {
        let cut = SUCCESS.lines().take(5).collect::<Vec<_>>().join("\n");
        assert_eq!(
            classify(&transcript(&cut), &exited(137)),
            Outcome::Crashed { code: Some(137) }
        );
        let mut exit = exited(0);
        exit.code = None;
        exit.timed_out = true;
        assert_eq!(classify(&transcript(SUCCESS), &exit), Outcome::TimedOut);
        exit.killed = Some("owner said stop".to_string());
        assert_eq!(
            classify(&transcript(SUCCESS), &exit),
            Outcome::Killed("owner said stop".to_string())
        );
    }

    #[test]
    fn unknown_lines_are_counted_not_fatal() {
        let text = format!("not json\n{{\"type\":\"brand_new\"}}\n{SUCCESS}");
        let transcript = transcript(&text);
        assert_eq!(transcript.unparsed, 1);
        assert_eq!(transcript.unknown["brand_new"], 1);
        assert!(matches!(
            classify(&transcript, &exited(0)),
            Outcome::Completed(_)
        ));
    }

    fn spec(session: Session) -> RunSpec {
        RunSpec {
            claude_bin: "claude".to_string(),
            model: "claude-opus-5-5".to_string(),
            effort: "medium".to_string(),
            session,
            system_prompt: "You are the builder.".to_string(),
            schema: serde_json::json!({"type": "object"}),
            cwd: PathBuf::from("/work"),
            prompt: "the brief".to_string(),
            budget_usd: Some(25.0),
            timeout: Duration::from_secs(60),
            oauth_token: None,
            session_file: None,
        }
    }

    #[test]
    fn a_runs_session_is_written_down_as_soon_as_it_is_known() {
        let dir = std::env::temp_dir().join(format!("factory-session-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A stand-in for the CLI that says which session it is, and then is
        // still working when the watcher's patience runs out.
        let stand_in = dir.join("claude");
        std::fs::write(
            &stand_in,
            "#!/bin/sh\ncat > /dev/null\necho '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"sess-42\",\"model\":\"m\"}'\nsleep 30\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stand_in, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let file = dir.join("in-flight.session");
        let mut run = spec(Session::Fresh);
        run.claude_bin = stand_in.to_string_lossy().into_owned();
        run.cwd = dir.clone();
        run.timeout = Duration::from_millis(1500);
        run.session_file = Some(file.clone());
        let finished =
            super::run(&run, &dir.join("run.jsonl"), &mut |_| Control::Continue).unwrap();
        // The run never finished, but which session it was is on disk.
        assert!(finished.exit.timed_out);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "sess-42");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_command_line_is_headless_bypass_and_never_bare() {
        let command = command(&spec(Session::Fresh));
        let argv = command.argv.join(" ");
        assert!(argv.starts_with("claude --print --output-format stream-json --verbose"));
        assert!(argv.contains("--permission-mode bypassPermissions"));
        assert!(argv.contains("--model claude-opus-5-5 --effort medium"));
        assert!(argv.contains("--max-budget-usd 25.00"));
        assert!(!argv.contains("--bare"));
        assert!(!argv.contains("--continue"));
        assert!(!argv.contains("--resume"));
        assert_eq!(command.stdin.as_deref(), Some("the brief"));
        assert!(command.env.contains(&(
            "CLAUDE_CODE_SUBAGENT_MODEL".to_string(),
            "claude-opus-5-5".to_string()
        )));
    }

    #[test]
    fn a_session_is_resumed_by_id() {
        let command = command(&spec(Session::Resume("abc-123".to_string())));
        let argv = command.argv.join(" ");
        assert!(argv.contains("--resume abc-123"));
    }

    /// The last result of a real resumed Opus session: its running totals
    /// include the 17-minute build that came before this 12-second run.
    const RESUMED: &str = include_str!("../fixtures/resumed-result-opus-2.1.325.jsonl");

    #[test]
    fn a_resumed_session_is_charged_only_for_this_run() {
        let usage = transcript(RESUMED).usage();
        let opus = &usage.models["claude-opus-5-5"];
        assert_eq!(opus.output_tokens, 1528, "not the session's 24189");
        assert!(
            (usage.cost_usd - 0.099163).abs() < 1e-9,
            "not the session's 2.54"
        );
        assert_eq!(usage.turns, 3);
    }

    #[test]
    fn several_results_in_one_run_add_up_and_the_last_one_decides() {
        let part = |turns: u64, cost: f64, outcome: &str| {
            format!(
                r#"{{"type":"result","subtype":"success","is_error":false,"num_turns":{turns},"duration_ms":1000,"total_cost_usd":99.0,"modelUsage":{{"m":{{"outputTokens":9999,"costUSD":99.0}}}},"model_usage_since_last_result":{{"m":{{"outputTokens":10,"costUSD":{cost}}}}},"structured_output":{{"outcome":"{outcome}"}}}}"#
            )
        };
        let text = [
            part(32, 1.25, "early"),
            part(5, 0.5, "middle"),
            part(3, 0.25, "done"),
        ]
        .join("\n");
        let transcript = transcript(&text);
        assert_eq!(transcript.results, 3);
        let usage = transcript.usage();
        assert_eq!(usage.turns, 40);
        assert_eq!(usage.duration_ms, 3000);
        assert!((usage.cost_usd - 2.0).abs() < 1e-9);
        assert_eq!(usage.models["m"].output_tokens, 30);
        let Outcome::Completed(result) = classify(&transcript, &exited(0)) else {
            panic!("expected Completed");
        };
        assert_eq!(result["outcome"], "done");
    }

    #[test]
    fn usage_adds_up_and_keeps_the_latest_windows() {
        let first = transcript(SUCCESS).usage();
        let mut total = Usage::default();
        total.add(&first);
        total.add(&first);
        assert_eq!(total.turns, 8);
        assert_eq!(total.models["claude-haiku-5-5"].output_tokens, 1916);
        assert_eq!(total.output_tokens(), 1916);
        assert!(total.rate_limit.is_some());
    }
}
