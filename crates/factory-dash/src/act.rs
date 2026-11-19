//! The owner's acts from the page: a reply in a thread, or a button.
//!
//! The dashboard does not act itself. It hands what the owner did to the
//! factory's own engine command, `factory-engine owner`, which gives it to
//! the project's tiers as the same event Slack would have carried, so a
//! press here and a press in Slack are one thing to the factory. Nothing
//! else is ever started: the command, its arguments and every value in
//! them are checked here first, and no shell is involved.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;

/// The buttons the factory posts to the owner, and so the only ones the
/// page may press.
pub const BUTTONS: [&str; 9] = [
    "goal_start",
    "goal_accept",
    "goal_stop",
    "request_approve",
    "request_reject",
    "proposal_approve",
    "proposal_decline",
    "definition_approve",
    "definition_reject",
];

/// The longest reply the page hands on.
const MOST_TEXT: usize = 8000;
/// How long the engine's command may take to hand an act on.
const WAIT: Duration = Duration::from_secs(45);

/// Where the factory's engine command and its config are.
#[derive(Clone, Debug)]
pub struct Engine {
    pub command: PathBuf,
    pub config: PathBuf,
}

/// What the owner did, once checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Act {
    Press {
        project: String,
        action_id: String,
        value: String,
    },
    Say {
        project: String,
        channel: String,
        thread: String,
        text: String,
    },
    /// A new message, as at the top of the project's channel.
    New { project: String, text: String },
}

/// Why an act was not handed on.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The page asked for something it never offers.
    Malformed(String),
    /// The engine's command could not be run, or said no.
    Engine(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Malformed(why) => write!(f, "{why}"),
            Refusal::Engine(why) => write!(f, "the factory did not take it: {why}"),
        }
    }
}

/// A name of the factory's: a project, a goal, a proposal.
fn plain(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && !name.starts_with(['.', '-'])
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// A Slack channel, or none.
fn channel(name: &str) -> bool {
    name.len() <= 40 && name.chars().all(|c| c.is_ascii_alphanumeric())
}

/// A Slack message's time stamp, as a thread is named by: digits and a dot.
fn stamp(ts: &str) -> bool {
    !ts.is_empty() && ts.len() <= 40 && ts.chars().all(|c| c.is_ascii_digit() || c == '.')
}

/// What a button names: one of the factory's names, or a definition the
/// executive proposed, as its thread and its place there (`<ts>/<n>`).
fn named(value: &str) -> bool {
    plain(value)
        || value.split_once('/').is_some_and(|(thread, n)| {
            stamp(thread) && !n.is_empty() && n.len() <= 6 && n.chars().all(|c| c.is_ascii_digit())
        })
}

impl Act {
    /// Read an act from what the page sent, or say why it is not one.
    pub fn read(body: &Value) -> Result<Act, Refusal> {
        let field = |name: &str| body[name].as_str().unwrap_or("").to_string();
        let project = field("project");
        if !plain(&project) {
            return Err(Refusal::Malformed(
                "no project, or not one of the factory's names".into(),
            ));
        }
        match body["act"].as_str() {
            Some("press") => {
                let action_id = field("action_id");
                let value = field("value");
                if !BUTTONS.contains(&action_id.as_str()) {
                    return Err(Refusal::Malformed(format!(
                        "`{action_id}` is not a button the factory posts"
                    )));
                }
                if !named(&value) {
                    return Err(Refusal::Malformed(
                        "the button names nothing the factory knows".into(),
                    ));
                }
                Ok(Act::Press {
                    project,
                    action_id,
                    value,
                })
            }
            Some("say") => {
                let channel_id = field("channel");
                let thread = field("thread");
                let text = field("text").trim().to_string();
                if !channel(&channel_id) || !stamp(&thread) {
                    return Err(Refusal::Malformed("the reply names no thread".into()));
                }
                if text.is_empty() || text.chars().count() > MOST_TEXT {
                    return Err(Refusal::Malformed(format!(
                        "a reply is 1 to {MOST_TEXT} characters"
                    )));
                }
                Ok(Act::Say {
                    project,
                    channel: channel_id,
                    thread,
                    text,
                })
            }
            Some("new") => {
                let text = field("text").trim().to_string();
                if text.is_empty() || text.chars().count() > MOST_TEXT {
                    return Err(Refusal::Malformed(format!(
                        "a message is 1 to {MOST_TEXT} characters"
                    )));
                }
                Ok(Act::New { project, text })
            }
            _ => Err(Refusal::Malformed(
                "an act is `press`, `say` or `new`".into(),
            )),
        }
    }

    /// The engine command's arguments for this act, after `--config`.
    pub fn args(&self) -> Vec<String> {
        match self {
            Act::Press {
                project,
                action_id,
                value,
            } => {
                vec![
                    "owner".into(),
                    project.clone(),
                    "press".into(),
                    action_id.clone(),
                    value.clone(),
                ]
            }
            Act::Say {
                project,
                channel,
                thread,
                text,
            } => vec![
                "owner".into(),
                project.clone(),
                "say".into(),
                if channel.is_empty() {
                    "-".into()
                } else {
                    channel.clone()
                },
                thread.clone(),
                // Whatever the owner wrote is never read as an option.
                "--".into(),
                text.clone(),
            ],
            Act::New { project, text } => vec![
                "owner".into(),
                project.clone(),
                "new".into(),
                "--".into(),
                text.clone(),
            ],
        }
    }

    /// What the act was, in a line, for the server's log.
    pub fn line(&self) -> String {
        match self {
            Act::New { project, text } => format!(
                "{project}: wrote a new message ({} characters)",
                text.chars().count()
            ),
            Act::Press {
                project,
                action_id,
                value,
            } => format!("{project}: pressed {action_id} on {value}"),
            Act::Say {
                project,
                thread,
                text,
                ..
            } => {
                format!(
                    "{project}: replied in {thread} ({} characters)",
                    text.chars().count()
                )
            }
        }
    }
}

/// Hand an act to the factory, and say what its engine said.
pub async fn hand_on(engine: &Engine, act: &Act) -> Result<String, Refusal> {
    let mut command = tokio::process::Command::new(&engine.command);
    command
        .arg("--config")
        .arg(&engine.config)
        .args(act.args())
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let ran = tokio::time::timeout(WAIT, command.output())
        .await
        .map_err(|_| Refusal::Engine(format!("no answer in {} s", WAIT.as_secs())))?
        .map_err(|err| {
            Refusal::Engine(format!(
                "{} could not be run: {err}",
                engine.command.display()
            ))
        })?;
    let out = String::from_utf8_lossy(&ran.stdout).trim().to_string();
    if ran.status.success() {
        Ok(out)
    } else {
        let err = String::from_utf8_lossy(&ran.stderr).trim().to_string();
        Err(Refusal::Engine(if err.is_empty() { out } else { err }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_what_the_page_offers_is_handed_on() {
        let press = Act::read(
            &json!({"project": "toy", "act": "press", "action_id": "goal_start", "value": "toy-g1"}),
        );
        assert_eq!(
            press.map(|act| act.args()),
            Ok(["owner", "toy", "press", "goal_start", "toy-g1"]
                .map(String::from)
                .to_vec())
        );
        // A proposed definition is named by its thread and its place there.
        for action in ["definition_approve", "definition_reject"] {
            let press = Act::read(
                &json!({"project": "toy", "act": "press", "action_id": action, "value": "1700000000.123456/2"}),
            );
            assert_eq!(
                press.map(|act| act.args()),
                Ok(["owner", "toy", "press", action, "1700000000.123456/2"]
                    .map(String::from)
                    .to_vec())
            );
        }
        let say = Act::read(
            &json!({"project": "toy", "act": "say", "channel": "", "thread": "17.5", "text": "  --config /tmp/x  "}),
        );
        assert_eq!(
            say.map(|act| act.args()),
            Ok(
                ["owner", "toy", "say", "-", "17.5", "--", "--config /tmp/x"]
                    .map(String::from)
                    .to_vec()
            )
        );
        let new = Act::read(&json!({"project": "toy", "act": "new", "text": "What is parked?"}));
        assert_eq!(
            new.map(|act| act.args()),
            Ok(["owner", "toy", "new", "--", "What is parked?"]
                .map(String::from)
                .to_vec())
        );
        for refused in [
            json!({"project": "toy", "act": "press", "action_id": "rm_rf", "value": "g"}),
            json!({"project": "toy", "act": "press", "action_id": "definition_delete", "value": "500.0/1"}),
            json!({"project": "toy", "act": "press", "action_id": "definition_approve", "value": "500.0/"}),
            json!({"project": "toy", "act": "press", "action_id": "definition_approve", "value": "/1"}),
            json!({"project": "toy", "act": "press", "action_id": "definition_reject", "value": "500.0/1/2"}),
            json!({"project": "toy", "act": "press", "action_id": "definition_reject", "value": "../x/1"}),
            json!({"project": "toy", "act": "press", "action_id": "definition_reject", "value": "-5/1"}),
            json!({"project": "toy", "act": "press", "action_id": "goal_start", "value": "--config"}),
            json!({"project": "../x", "act": "press", "action_id": "goal_start", "value": "g"}),
            json!({"project": "toy", "act": "say", "channel": "C 1", "thread": "1.1", "text": "hi"}),
            json!({"project": "toy", "act": "say", "channel": "C1", "thread": "1.1; ls", "text": "hi"}),
            json!({"project": "toy", "act": "say", "channel": "C1", "thread": "1.1", "text": "   "}),
            json!({"project": "toy", "act": "say", "channel": "C1", "thread": "1.1", "text": "x".repeat(MOST_TEXT + 1)}),
            json!({"project": "toy", "act": "run"}),
            json!({"project": "toy", "act": "new", "text": " "}),
        ] {
            assert!(Act::read(&refused).is_err(), "{refused}");
        }
    }
}
