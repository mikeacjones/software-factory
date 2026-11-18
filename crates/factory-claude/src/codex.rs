//! The `codex` binary as an agent: `codex exec`, headless, with approvals
//! and its own sandbox off, as a Claude worker runs in bypass mode.
//!
//! What differs from Claude Code, and what is done about it:
//!
//! - There is no flag for a role prompt. The role is put at the top of the
//!   briefing of a fresh session; a resumed session already has it.
//! - The schema of the typed result is given as a file, written beside the
//!   run's log. The result is the last thing the agent says, as JSON.
//! - Usage is tokens only, with no price, and is a running total for the
//!   session. What a resumed run used is the total less what the session
//!   had used before, which is kept in a file beside the log.
//! - There is no budget cap, and runs are not spread over accounts: the
//!   tool uses its own login.

use std::path::{Path, PathBuf};

use factory_supervise::{Spec, StreamExit};
use serde_json::{Value, json};

use crate::harness::Harness;
use crate::{ModelUsage, Outcome, ResultEvent, RunSpec, Session, Transcript, Usage, first_line};

pub struct Codex;

/// The effort as codex names it. It has nothing above `xhigh`.
fn effort(effort: &str) -> &str {
    match effort {
        "max" => "xhigh",
        other => other,
    }
}

/// The briefing as the run is given it.
fn briefing(spec: &RunSpec) -> String {
    // The roles call the language server the `LSP` tool, as Claude Code
    // has it. Here it is a tool server's tools.
    let tools = if spec.tools.is_empty() {
        String::new()
    } else {
        format!(
            "\n\nWhere your role speaks of the `LSP` tool: you have it as the tools of the tool server{} {} (definition, references, hover, diagnostics).",
            if spec.tools.len() == 1 { "" } else { "s" },
            spec.tools
                .iter()
                .map(|tool| format!("`{}`", tool.name))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    match &spec.session {
        Session::Fresh if !spec.system_prompt.trim().is_empty() => format!(
            "# Your role\n\n{}{tools}\n\n# Your briefing\n\n{}\n\nEnd with the structured result and nothing else.",
            spec.system_prompt.trim(),
            spec.prompt
        ),
        _ => spec.prompt.clone(),
    }
}

/// Where a session's usage so far is kept, beside the run's log.
fn usage_file(log: &Path, session: &str) -> PathBuf {
    let name: String = session
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    log.with_file_name(format!("codex-usage-{name}.json"))
}

fn tokens(usage: &ModelUsage) -> Value {
    json!({ "input": usage.input_tokens, "cached": usage.cache_read_tokens,
            "output": usage.output_tokens })
}

impl Harness for Codex {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn command(&self, spec: &RunSpec, log: &Path) -> std::io::Result<Spec> {
        let schema = log.with_extension("schema.json");
        std::fs::write(&schema, spec.schema.to_string())?;
        let mut argv = vec![spec.bin.clone(), "exec".to_string()];
        if let Session::Resume(id) = &spec.session {
            argv.extend(["resume".to_string(), id.clone()]);
        }
        argv.extend(
            [
                "--json",
                "--dangerously-bypass-approvals-and-sandbox",
                "--skip-git-repo-check",
                // The user's own settings and rules were written for a
                // person at a terminal, and must not steer a worker.
                "--ignore-user-config",
                "--ignore-rules",
                "--model",
                &spec.model,
                "--config",
                &format!("model_reasoning_effort=\"{}\"", effort(&spec.effort)),
                "--output-schema",
                &schema.to_string_lossy(),
            ]
            .iter()
            .map(|part| part.to_string()),
        );
        // The user's settings are left out, and with them any tool server
        // named there: the run's own are given here, each time.
        for tool in &spec.tools {
            let key = format!("mcp_servers.{}", tool.name);
            // A JSON string, and a JSON list of strings, are TOML too.
            argv.extend([
                "--config".to_string(),
                format!("{key}.command={}", json!(tool.command)),
                "--config".to_string(),
                format!("{key}.args={}", json!(tool.args)),
            ]);
        }
        // The briefing is read from stdin.
        argv.push("-".to_string());
        let mut command = Spec::new(argv, &spec.cwd, spec.timeout);
        command.stdin = Some(briefing(spec));
        // A worker must not inherit the identity of whatever session
        // started the factory, of either tool.
        command.env_remove = std::env::vars_os()
            .filter_map(|(key, _)| key.into_string().ok())
            .filter(|key| {
                key.starts_with("CLAUDE")
                    || key.starts_with("ANTHROPIC")
                    || (key.starts_with("CODEX_") && key != "CODEX_HOME")
            })
            .collect();
        Ok(command)
    }

    fn begin(&self, spec: &RunSpec, transcript: &mut Transcript) {
        transcript.model = Some(spec.model.clone());
    }

    fn observe(&self, transcript: &mut Transcript, line: &str) {
        let Some(value) = serde_json::from_str::<Value>(line)
            .ok()
            .filter(Value::is_object)
        else {
            transcript.unparsed += 1;
            return;
        };
        let text = |pointer: &str| {
            value
                .pointer(pointer)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        let kind = text("/type");
        match kind.as_str() {
            "thread.started" => transcript.session_id = Some(text("/thread_id")),
            "item.started" | "item.completed" => {
                let started = kind == "item.started";
                match (text("/item/type").as_str(), started) {
                    ("agent_message", false) => {
                        let said = text("/item/text");
                        if !said.trim().is_empty() {
                            transcript.last_activity = Some(first_line(&said, 140));
                            transcript.last_message = Some(said);
                        }
                    }
                    ("command_execution", true) => {
                        transcript.tool_calls += 1;
                        transcript.last_activity = Some(format!(
                            "running {}",
                            first_line(&text("/item/command"), 120)
                        ));
                    }
                    // A tool call that is not a command is counted when it
                    // ends: not every kind reports its start.
                    ("command_execution" | "agent_message" | "reasoning" | "error", _) => {}
                    (_, false) => transcript.tool_calls += 1,
                    _ => {}
                }
            }
            "turn.completed" => {
                let count = |key: &str| {
                    value
                        .pointer(&format!("/usage/{key}"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0)
                };
                let cached = count("cached_input_tokens");
                let model = transcript.model.clone().unwrap_or_default();
                // The session's running total: `settle` takes off what
                // earlier runs of the session used.
                let mut used = Usage {
                    turns: 1,
                    ..Usage::default()
                };
                used.models.insert(
                    model,
                    ModelUsage {
                        input_tokens: count("input_tokens").saturating_sub(cached),
                        output_tokens: count("output_tokens"),
                        cache_read_tokens: cached,
                        cache_write_tokens: count("cache_write_input_tokens"),
                        cost_usd: 0.0,
                    },
                );
                transcript.used = used;
                transcript.results += 1;
                let said = transcript.last_message.clone().unwrap_or_default();
                transcript.result = Some(ResultEvent {
                    is_error: false,
                    subtype: kind,
                    api_error_status: None,
                    structured_output: typed(&said),
                    text: said,
                });
            }
            "turn.failed" | "error" => {
                let message = match text("/error/message") {
                    message if message.is_empty() => text("/message"),
                    message => message,
                };
                transcript.results += 1;
                transcript.result = Some(ResultEvent {
                    is_error: true,
                    subtype: kind,
                    api_error_status: None,
                    text: message,
                    structured_output: None,
                });
            }
            "turn.started" => {}
            _ => *transcript.unknown.entry(kind).or_insert(0) += 1,
        }
    }

    fn settle(&self, _spec: &RunSpec, log: &Path, transcript: &mut Transcript) {
        let (Some(session), Some(model)) =
            (transcript.session_id.clone(), transcript.model.clone())
        else {
            return;
        };
        let Some(total) = transcript.used.models.get(&model).cloned() else {
            return;
        };
        let file = usage_file(log, &session);
        let before: Value = std::fs::read_to_string(&file)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        let had = |key: &str| before.get(key).and_then(Value::as_u64).unwrap_or(0);
        let _ = std::fs::write(&file, tokens(&total).to_string());
        if let Some(used) = transcript.used.models.get_mut(&model) {
            used.input_tokens = total.input_tokens.saturating_sub(had("input"));
            used.cache_read_tokens = total.cache_read_tokens.saturating_sub(had("cached"));
            used.output_tokens = total.output_tokens.saturating_sub(had("output"));
        }
    }

    fn classify(&self, transcript: &Transcript, exit: &StreamExit) -> Outcome {
        if let Some(reason) = &exit.killed {
            return Outcome::Killed(reason.clone());
        }
        if exit.timed_out {
            return Outcome::TimedOut;
        }
        let Some(result) = &transcript.result else {
            return Outcome::Crashed { code: exit.code };
        };
        if result.is_error {
            let said = result.text.to_lowercase();
            if ["401", "unauthorized", "not logged in", "login"]
                .iter()
                .any(|sign| said.contains(sign))
            {
                return Outcome::AuthError(result.text.clone());
            }
            // A usage limit is reported as an error like any other: when
            // it ends is not in the stream, so there is nothing to wait
            // for, and the step is tried again and then parked.
            let limited = said.contains("usage limit") || said.contains("rate limit");
            return Outcome::ApiError {
                status: limited.then_some(429),
                message: result.text.clone(),
            };
        }
        match &result.structured_output {
            Some(output) if output.is_object() => Outcome::Completed(output.clone()),
            Some(_) => Outcome::InvalidResult("the typed result is not an object".to_string()),
            None => Outcome::InvalidResult("the run returned no typed result".to_string()),
        }
    }
}

/// The typed result in what the agent last said: JSON, with or without a
/// code fence around it.
fn typed(said: &str) -> Option<Value> {
    let text = said.trim();
    let text = text
        .strip_prefix("```json")
        .or_else(|| text.strip_prefix("```"))
        .and_then(|rest| rest.strip_suffix("```"))
        .unwrap_or(text);
    serde_json::from_str(text.trim()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const SUCCESS: &str = include_str!("../fixtures/codex-success-0.169.1.jsonl");

    fn spec(session: Session, dir: &Path) -> RunSpec {
        RunSpec {
            agent: "codex".to_string(),
            bin: "codex".to_string(),
            model: "gpt-6.2-sol".to_string(),
            effort: "max".to_string(),
            session,
            system_prompt: "You are the builder.".to_string(),
            schema: json!({ "type": "object" }),
            cwd: dir.to_path_buf(),
            prompt: "Build the bead.".to_string(),
            budget_usd: Some(25.0),
            tools: vec![crate::ToolServer {
                name: "zls".to_string(),
                command: "/opt/bridge".to_string(),
                args: vec!["--workspace".to_string(), "/w t".to_string()],
            }],
            timeout: Duration::from_secs(60),
            oauth_token: None,
            session_file: None,
        }
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

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("factory-codex-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn read(spec: &RunSpec, text: &str) -> Transcript {
        let mut transcript = Transcript::default();
        Codex.begin(spec, &mut transcript);
        for line in text.lines() {
            Codex.observe(&mut transcript, line);
        }
        transcript
    }

    #[test]
    fn a_fresh_run_is_headless_unsandboxed_and_given_its_role_with_the_briefing() {
        let dir = scratch("fresh");
        let log = dir.join("001-build.jsonl");
        let command = Codex.command(&spec(Session::Fresh, &dir), &log).unwrap();
        let argv = command.argv.join(" ");
        assert!(argv.starts_with("codex exec --json --dangerously-bypass-approvals-and-sandbox"));
        assert!(argv.contains("--model gpt-6.2-sol"));
        assert!(argv.contains("model_reasoning_effort=\"xhigh\""), "{argv}");
        assert!(argv.contains("--ignore-user-config") && argv.ends_with(" -"));
        assert!(!argv.contains("resume") && !argv.contains("budget"));
        let schema = dir.join("001-build.schema.json");
        assert!(argv.contains(&format!("--output-schema {}", schema.display())));
        assert_eq!(
            std::fs::read_to_string(&schema).unwrap(),
            r#"{"type":"object"}"#
        );
        // Its tool servers are given on the command line, since the
        // user's settings are left out.
        assert!(
            argv.contains("--config mcp_servers.zls.command=\"/opt/bridge\"")
                && argv.contains("--config mcp_servers.zls.args=[\"--workspace\",\"/w t\"]"),
            "{argv}"
        );
        let stdin = command.stdin.unwrap();
        assert!(stdin.contains("You are the builder.") && stdin.contains("Build the bead."));
        assert!(
            stdin.contains("the tools of the tool server `zls`"),
            "{stdin}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_resumed_run_names_its_session_and_is_not_told_its_role_again() {
        let dir = scratch("resume");
        let log = dir.join("002-fix.jsonl");
        let command = Codex
            .command(&spec(Session::Resume("abc-123".to_string()), &dir), &log)
            .unwrap();
        assert!(
            command
                .argv
                .join(" ")
                .starts_with("codex exec resume abc-123 --json")
        );
        assert_eq!(command.stdin.as_deref(), Some("Build the bead."));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_recorded_run_parses_into_session_result_and_usage() {
        let dir = scratch("parse");
        let spec = spec(Session::Fresh, &dir);
        let transcript = read(&spec, SUCCESS);
        assert_eq!(transcript.unparsed, 0);
        assert_eq!(
            transcript.session_id.as_deref(),
            Some("01a1ebcf-1d62-71b0-a5b2-74185654debf")
        );
        // One command was run, and the startup warning is not a failure.
        assert_eq!(transcript.tool_calls, 1);
        let Outcome::Completed(result) = Codex.classify(&transcript, &exited(0)) else {
            panic!("expected Completed");
        };
        assert_eq!(result["outcome"], "done");
        let used = &transcript.used.models["gpt-6.2-sol"];
        assert_eq!(
            (
                used.input_tokens,
                used.cache_read_tokens,
                used.output_tokens
            ),
            (16_477, 16_000, 153)
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_resumed_run_is_counted_for_what_it_used_and_not_for_the_whole_session() {
        let dir = scratch("usage");
        let spec = spec(Session::Fresh, &dir);
        let mut first = read(&spec, SUCCESS);
        Codex.settle(&spec, &dir.join("001-build.jsonl"), &mut first);
        assert_eq!(first.used.models["gpt-6.2-sol"].output_tokens, 153);

        // The same session, resumed: the stream reports the total so far.
        let later = SUCCESS
            .replace("\"input_tokens\":32477", "\"input_tokens\":52477")
            .replace(
                "\"cached_input_tokens\":16000",
                "\"cached_input_tokens\":30000",
            )
            .replace("\"output_tokens\":153", "\"output_tokens\":200");
        let mut second = read(&spec, &later);
        Codex.settle(&spec, &dir.join("002-fix.jsonl"), &mut second);
        let used = &second.used.models["gpt-6.2-sol"];
        assert_eq!(
            (
                used.input_tokens,
                used.cache_read_tokens,
                used.output_tokens
            ),
            (6_000, 14_000, 47)
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn each_way_a_run_can_end_is_told_apart() {
        let dir = scratch("ends");
        let spec = spec(Session::Fresh, &dir);
        let started = r#"{"type":"thread.started","thread_id":"t"}"#;
        assert_eq!(
            Codex.classify(&read(&spec, started), &exited(1)),
            Outcome::Crashed { code: Some(1) }
        );
        let failed = format!(
            "{started}\n{}",
            r#"{"type":"turn.failed","error":{"message":"You've hit your usage limit."}}"#
        );
        assert_eq!(
            Codex.classify(&read(&spec, &failed), &exited(1)),
            Outcome::ApiError {
                status: Some(429),
                message: "You've hit your usage limit.".to_string()
            }
        );
        let refused = format!(
            "{started}\n{}",
            r#"{"type":"error","message":"401 Unauthorized"}"#
        );
        assert!(matches!(
            Codex.classify(&read(&spec, &refused), &exited(1)),
            Outcome::AuthError(_)
        ));
        let prose = format!(
            "{started}\n{}\n{}",
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"All done."}}"#,
            r#"{"type":"turn.completed","usage":{}}"#
        );
        assert!(matches!(
            Codex.classify(&read(&spec, &prose), &exited(0)),
            Outcome::InvalidResult(_)
        ));
        let fenced = prose.replace(
            "All done.",
            "```json\\n{\\\"outcome\\\":\\\"done\\\"}\\n```",
        );
        assert!(matches!(
            Codex.classify(&read(&spec, &fenced), &exited(0)),
            Outcome::Completed(_)
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
