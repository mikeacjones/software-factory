//! A conversation between the owner and the factory, from the records.
//!
//! A goal's log keeps everything the factory said to the owner and every
//! message and button of the owner's, with the goal's milestones among
//! them. An executive keeps each of its threads with the owner as the
//! lines that were said. Read in order, either is the conversation the
//! Slack thread holds, without Slack.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::{Value, json};

/// The most of a goal's log that is read for its conversation.
const MOST: u64 = 6 * 1024 * 1024;
/// The most lines of a conversation that are given.
const LINES: usize = 400;

fn text(value: &Value) -> String {
    value.as_str().unwrap_or("").to_string()
}

/// What a button the owner pressed is called on the page.
fn pressed(action: &str) -> &str {
    match action {
        "goal_start" => "Started the goal",
        "goal_accept" => "Accepted the goal as done",
        "goal_stop" => "Stopped the goal",
        "request_approve" => "Approved the bead changes",
        "request_reject" => "Rejected the bead changes",
        _ => action,
    }
}

/// One line of a goal's log, as a line of its conversation, if it is one.
fn line(event: &Value) -> Option<Value> {
    let at = event["time_unix"].as_u64().unwrap_or(0);
    let data = &event["data"];
    let (who, kind, said) = match event["kind"].as_str()? {
        "owner.message" => ("owner", "message", text(&data["text"])),
        "owner.action" => (
            "owner",
            "action",
            pressed(data["action"].as_str().unwrap_or("")).to_string(),
        ),
        "slack.say" => ("factory", "message", text(&data["text"])),
        "slack.need" => ("factory", "need", text(&data["text"])),
        "slack.prompt" => ("factory", "prompt", text(&data["text"])),
        "goal.opened" => ("event", "opened", "The goal was opened.".to_string()),
        "goal.queued" => ("event", "queued", "The goal waits its turn.".to_string()),
        "goal.unqueued" => ("event", "started", "The goal's turn came.".to_string()),
        "goal.complete" => ("event", "complete", "The goal is complete.".to_string()),
        "subgoal.opened" => (
            "event",
            "subgoal",
            format!("Sub-goal `{}` opened.", text(&data["id"])),
        ),
        "subgoal.shipping" => (
            "event",
            "ship",
            format!("Sub-goal `{}` is shipping.", text(&data["id"])),
        ),
        "pipeline.started" => (
            "event",
            "pipeline",
            format!("Pipeline started for `{}`.", text(&data["bead"])),
        ),
        _ => return None,
    };
    Some(json!({"who": who, "kind": kind, "text": said, "at": at}))
}

/// A goal's conversation, oldest first.
pub fn of_goal(log: &Path) -> Option<Value> {
    let mut file = fs::File::open(log).ok()?;
    let size = file.metadata().ok()?.len();
    let start = size.saturating_sub(MOST);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.take(MOST).read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<Value> = text
        .lines()
        .skip(usize::from(start > 0))
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|event| line(&event))
        .collect();
    let cut = lines.len().saturating_sub(LINES);
    lines.drain(..cut);
    Some(json!({"lines": lines, "earlier": cut > 0 || start > 0}))
}

/// One of an executive's threads with the owner, oldest first. It keeps
/// what was said and not when, so only the thread's start has a time.
pub fn of_executive(record: &Value, thread: &str) -> Option<Value> {
    let said = record["threads"][thread]["said"].as_array()?;
    let at = thread
        .split('.')
        .next()
        .and_then(|secs| secs.parse::<u64>().ok());
    let lines: Vec<Value> = said
        .iter()
        .filter_map(|pair| {
            let who = pair[0].as_str()?;
            Some(json!({
                "who": if who == "owner" { "owner" } else { "factory" },
                "kind": "message",
                "text": text(&pair[1]),
                "at": Value::Null,
            }))
        })
        .collect();
    Some(json!({"lines": lines, "earlier": false, "at": at}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_goal_log_reads_as_the_conversation_with_the_owner() {
        let said = [
            r#"{"time_unix":1,"kind":"goal.opened","data":{}}"#,
            r#"{"time_unix":2,"kind":"slack.say","data":{"text":"Here is a charter"}}"#,
            r#"{"time_unix":3,"kind":"owner.action","data":{"action":"goal_start"}}"#,
            r#"{"time_unix":4,"kind":"director.ran","data":{"usage":{}}}"#,
            r#"{"time_unix":5,"kind":"slack.need","data":{"text":"Which way?"}}"#,
            r#"{"time_unix":6,"kind":"owner.message","data":{"text":"Left"}}"#,
        ]
        .map(|said| line(&serde_json::from_str::<Value>(said).unwrap_or_default()));
        let whos: Vec<_> = said
            .iter()
            .map(|line| {
                line.as_ref()
                    .map(|line| line["who"].as_str().unwrap_or("").to_string())
            })
            .collect();
        assert_eq!(
            whos,
            [
                Some("event"),
                Some("factory"),
                Some("owner"),
                None,
                Some("factory"),
                Some("owner")
            ]
            .map(|who| who.map(str::to_string))
        );
        assert_eq!(
            said[2].as_ref().map(|line| line["text"].clone()),
            Some(json!("Started the goal"))
        );

        let record =
            json!({"threads": {"17.5": {"said": [["owner", "hi"], ["executive", "hello"]]}}});
        let thread = of_executive(&record, "17.5").unwrap_or_default();
        assert_eq!(thread["lines"][1]["who"], "factory");
        assert_eq!(thread["at"], 17);
        assert!(of_executive(&record, "18.0").is_none());
    }
}
