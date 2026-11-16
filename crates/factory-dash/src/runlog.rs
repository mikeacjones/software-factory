//! What an agent's run is doing, from the log the factory keeps of it.
//!
//! The factory writes every line a run prints to a file as it arrives.
//! This reads a part of such a file and says each line shortly: what the
//! agent wrote, the tool it called and with what, and what came back.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;
use serde_json::{Value, json};

/// How far back a log is read when no place in it is asked for.
const FIRST_LOOK: u64 = 300 * 1024;
/// The most that is read of a log for one answer.
const MOST: u64 = 1024 * 1024;

/// A part of a run's log.
pub struct Part {
    /// How long the log is now.
    pub size: u64,
    /// Where this part began.
    pub from: u64,
    /// Where the next part begins: after the last whole line read.
    pub next: u64,
    pub events: Vec<Value>,
}

/// Read a log from `from`, or from a little before its end. Only whole
/// lines are read: a line the run is still writing waits for next time.
pub fn read(path: &Path, from: Option<u64>) -> std::io::Result<Part> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    let asked = from.is_some();
    let mut start = from.unwrap_or(size.saturating_sub(FIRST_LOOK)).min(size);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(MOST).read_to_end(&mut bytes)?;
    // A look that begins in the middle of the log begins in the middle
    // of a line: what is left of that line is passed over.
    let mut skip = 0;
    if !asked && start > 0 {
        skip = bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(bytes.len(), |at| at + 1);
    }
    let whole = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |at| at + 1)
        .max(skip);
    let text = String::from_utf8_lossy(&bytes[skip..whole]);
    let events = text.lines().flat_map(said).collect();
    start += skip as u64;
    Ok(Part {
        size,
        from: start,
        next: start + (whole - skip) as u64,
        events,
    })
}

/// What one line of a run's log says, shortly. Most lines say nothing
/// worth showing.
pub fn said(line: &str) -> Vec<Value> {
    let Ok(entry) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    let kind = entry["type"].as_str().unwrap_or("");
    let parts = entry["message"]["content"].as_array();
    match (kind, parts) {
        ("system", _) if entry["subtype"] == "init" => vec![json!({
            "k": "start",
            "text": format!(
                "{} in {}",
                entry["model"].as_str().unwrap_or("a model"),
                entry["cwd"].as_str().unwrap_or("its directory")
            ),
        })],
        ("assistant", Some(parts)) => parts
            .iter()
            .filter_map(|part| match part["type"].as_str() {
                Some("text") => {
                    let text = part["text"].as_str().unwrap_or("").trim();
                    (!text.is_empty()).then(|| json!({"k": "say", "text": short(text, 6000)}))
                }
                Some("tool_use") => Some(json!({
                    "k": "tool",
                    "name": part["name"].as_str().unwrap_or("tool"),
                    "text": short(&asked_for(&part["input"]), 1200),
                })),
                _ => None,
            })
            .collect(),
        ("user", Some(parts)) => parts
            .iter()
            .filter(|part| part["type"] == "tool_result")
            .map(|part| {
                let text = match &part["content"] {
                    Value::String(text) => text.clone(),
                    Value::Array(pieces) => pieces
                        .iter()
                        .filter_map(|piece| piece["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => String::new(),
                };
                json!({
                    "k": "out",
                    "text": short(text.trim(), 1500),
                    "err": part["is_error"].as_bool().unwrap_or(false),
                })
            })
            .collect(),
        ("result", _) => vec![json!({
            "k": "end",
            "text": short(entry["result"].as_str().unwrap_or("").trim(), 6000),
            "outcome": entry["subtype"].as_str().unwrap_or(""),
            "cost": entry["total_cost_usd"].as_f64(),
            "turns": entry["num_turns"].as_u64(),
        })],
        _ => Vec::new(),
    }
}

/// What a tool was asked for, as the one thing about it worth reading.
fn asked_for(input: &Value) -> String {
    for key in [
        "command",
        "file_path",
        "pattern",
        "description",
        "query",
        "url",
        "skill",
    ] {
        if let Some(text) = input[key].as_str() {
            return match (key, input["path"].as_str()) {
                ("pattern", Some(path)) => format!("{text}  in {path}"),
                _ => text.to_string(),
            };
        }
    }
    match input {
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// A text cut to a length, with what looks like a secret taken out. The
/// logs are an agent's own words and what its tools printed, and a token
/// printed by mistake should not be read off a page.
pub fn short(text: &str, most: usize) -> String {
    static SECRET: OnceLock<Option<Regex>> = OnceLock::new();
    let secret = SECRET.get_or_init(|| {
        Regex::new(
            r"sk-ant-[A-Za-z0-9_-]{8,}|xox[abeprs]-[A-Za-z0-9-]{8,}|xapp-[A-Za-z0-9-]{8,}|gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}",
        )
        .ok()
    });
    let mut cut: String = text.chars().take(most).collect();
    if cut.len() < text.len() {
        cut.push_str(&format!(
            "\n… {} more characters",
            text.chars().count().saturating_sub(most)
        ));
    }
    match secret {
        Some(secret) => secret.replace_all(&cut, "[secret removed]").into_owned(),
        None => cut,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_says_what_the_agent_wrote_called_and_was_answered() {
        let wrote = said(
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":""},{"type":"text","text":"Now the bridge."},{"type":"tool_use","name":"Bash","input":{"command":"zig build test"}}]}}"#,
        );
        assert_eq!(wrote.len(), 2, "{wrote:?}");
        assert_eq!(wrote[0], json!({"k": "say", "text": "Now the bridge."}));
        assert_eq!(
            wrote[1],
            json!({"k": "tool", "name": "Bash", "text": "zig build test"})
        );
        let answered = said(
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":[{"type":"text","text":"ok"}],"is_error":true}]}}"#,
        );
        assert_eq!(
            answered,
            vec![json!({"k": "out", "text": "ok", "err": true})]
        );
        let ended = said(
            r#"{"type":"result","subtype":"success","result":"Done.","total_cost_usd":1.5,"num_turns":3}"#,
        );
        assert_eq!(ended[0]["k"], "end");
        assert_eq!(ended[0]["cost"], 1.5);
        // Lines that are not for reading say nothing, and neither does
        // one that is not whole.
        assert!(said(r#"{"type":"system","subtype":"thinking_tokens"}"#).is_empty());
        assert!(said(r#"{"type":"rate_limit_event"}"#).is_empty());
        assert!(said(r#"{"type":"assistant","mess"#).is_empty());
    }

    #[test]
    fn a_search_says_where_it_looked() {
        let called = said(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Grep","input":{"pattern":"fn connect","path":"src"}}]}}"#,
        );
        assert_eq!(called[0]["text"], "fn connect  in src");
    }

    #[test]
    fn a_long_text_is_cut_and_a_token_is_taken_out() {
        let cut = short(&"é".repeat(50), 10);
        assert!(cut.starts_with(&"é".repeat(10)), "{cut}");
        assert!(cut.ends_with("… 40 more characters"), "{cut}");
        let token = format!("sk-ant-{}", "a1B2".repeat(8));
        let kept = short(&format!("export KEY={token} and xoxb-123456789-abc"), 500);
        assert_eq!(kept, "export KEY=[secret removed] and [secret removed]");
    }

    #[test]
    fn a_log_is_read_in_whole_lines_from_where_the_last_look_ended() {
        let dir = std::env::temp_dir().join(format!("factory-dash-runlog-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("001-build.jsonl");
        let line = |text: &str| {
            format!(
                r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"{text}"}}]}}}}"#
            )
        };
        std::fs::write(
            &path,
            format!("{}\n{}\n{}", line("one"), line("two"), &line("three")[..20]),
        )
        .unwrap();
        let first = read(&path, Some(0)).unwrap();
        assert_eq!(first.events.len(), 2);
        assert_eq!(
            first.next as usize,
            line("one").len() + line("two").len() + 2
        );
        // The line being written is read once it is whole.
        std::fs::write(
            &path,
            format!("{}\n{}\n{}\n", line("one"), line("two"), line("three")),
        )
        .unwrap();
        let second = read(&path, Some(first.next)).unwrap();
        assert_eq!(second.events, vec![json!({"k": "say", "text": "three"})]);
        assert_eq!(second.next, second.size);
        assert!(read(&path, Some(second.next)).unwrap().events.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
