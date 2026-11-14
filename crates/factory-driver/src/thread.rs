//! The run's Slack thread: where the factory says what it is doing and reads
//! what the owner replies.
//!
//! Slack being down never stops a run. Every message is written to the unit's
//! event log first, so the log shows what was said, or would have been.

use factory_core::EventLog;
use factory_slack::{Message, Slack};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// What must be remembered to find the thread again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadState {
    pub channel: String,
    pub root_ts: String,
    /// The newest reply already read.
    pub last_seen: String,
}

pub struct Thread {
    slack: Option<Slack>,
    state: Option<ThreadState>,
    owners: Vec<String>,
    /// The fixed first lines of the root message.
    header: String,
    log: EventLog,
}

impl Thread {
    /// A thread that only writes to the event log.
    pub fn disabled(log: EventLog) -> Thread {
        Thread {
            slack: None,
            state: None,
            owners: Vec::new(),
            header: String::new(),
            log,
        }
    }

    /// Find or create the project's channel and start a thread for the run.
    /// If Slack cannot be reached the thread is disabled and the run goes on.
    pub fn open(
        slack: Slack,
        channel_name: &str,
        owners: Vec<String>,
        header: String,
        log: EventLog,
    ) -> Thread {
        let mut thread = Thread {
            slack: None,
            state: None,
            owners,
            header,
            log,
        };
        let opened = slack
            .ensure_channel(channel_name, &thread.owners)
            .and_then(|channel| {
                let text = format!("{}\n:hourglass_flowing_sand: starting", thread.header);
                let ts = slack.post(&channel, None, &text)?;
                Ok((channel, ts))
            });
        match opened {
            Ok((channel, root_ts)) => {
                thread.record(
                    "slack.thread_opened",
                    json!({ "channel": channel, "ts": root_ts }),
                );
                thread.state = Some(ThreadState {
                    channel,
                    last_seen: root_ts.clone(),
                    root_ts,
                });
                thread.slack = Some(slack);
            }
            Err(err) => thread.record(
                "slack.failed",
                json!({ "doing": "open", "error": err.to_string() }),
            ),
        }
        thread
    }

    /// Pick up a thread started by an earlier process.
    pub fn resume(
        slack: Slack,
        state: ThreadState,
        owners: Vec<String>,
        header: String,
        log: EventLog,
    ) -> Thread {
        Thread {
            slack: Some(slack),
            state: Some(state),
            owners,
            header,
            log,
        }
    }

    pub fn state(&self) -> Option<&ThreadState> {
        self.state.as_ref()
    }

    /// Whether the owner can be reached through this thread.
    pub fn live(&self) -> bool {
        self.slack.is_some() && self.state.is_some()
    }

    /// `<@U123>` for each owner, to draw their attention.
    pub fn owner_mentions(&self) -> String {
        self.owners
            .iter()
            .map(|id| format!("<@{id}>"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn record(&self, kind: &str, data: serde_json::Value) {
        // The log is the fallback for Slack; there is no fallback for the log.
        let _ = self.log.append(kind, None, data);
    }

    /// Post an update in the thread.
    pub fn say(&mut self, text: &str) {
        self.record("slack.say", json!({ "text": text }));
        if let (Some(slack), Some(state)) = (&self.slack, &self.state)
            && let Err(err) = slack.post(&state.channel, Some(&state.root_ts), text)
        {
            self.record(
                "slack.failed",
                json!({ "doing": "say", "error": err.to_string() }),
            );
        }
    }

    /// Replace the status line of the thread's root message.
    pub fn status(&mut self, line: &str) {
        self.record("slack.status", json!({ "text": line }));
        if let (Some(slack), Some(state)) = (&self.slack, &self.state) {
            let text = format!("{}\n{line}", self.header);
            if let Err(err) = slack.update(&state.channel, &state.root_ts, &text) {
                self.record(
                    "slack.failed",
                    json!({ "doing": "status", "error": err.to_string() }),
                );
            }
        }
    }

    /// What the owner has written in the thread since the last call. Each
    /// message is returned once, and only messages from configured owners.
    pub fn poll(&mut self) -> Vec<String> {
        let (Some(slack), Some(state)) = (&self.slack, &mut self.state) else {
            return Vec::new();
        };
        let replies = match slack.replies(&state.channel, &state.root_ts, &state.last_seen) {
            Ok(replies) => replies,
            Err(err) => {
                let data = json!({ "doing": "poll", "error": err.to_string() });
                let _ = self.log.append("slack.failed", None, data);
                return Vec::new();
            }
        };
        let mut texts = Vec::new();
        for Message { ts, user, text } in replies {
            if ts > state.last_seen {
                state.last_seen = ts.clone();
            }
            if !self.owners.contains(&user) || text.trim().is_empty() {
                continue;
            }
            let _ = slack.react(&state.channel, &ts, "eyes");
            let _ = self
                .log
                .append("owner.message", None, json!({ "ts": ts, "text": text }));
            texts.push(text);
        }
        texts
    }
}

/// Whether an owner message is the instruction to end the run.
pub fn is_stop(message: &str) -> bool {
    matches!(
        message
            .trim()
            .trim_start_matches('!')
            .to_lowercase()
            .as_str(),
        "stop" | "abort" | "cancel"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use factory_core::{ProjectId, UnitId};

    #[test]
    fn stop_is_recognised_and_ordinary_talk_is_not() {
        for text in ["stop", " Stop ", "!stop", "ABORT", "cancel"] {
            assert!(is_stop(text), "{text}");
        }
        for text in ["stop using tabs", "don't stop", ""] {
            assert!(!is_stop(text), "{text}");
        }
    }

    #[test]
    fn a_disabled_thread_writes_to_the_log_and_hears_nothing() {
        let path =
            std::env::temp_dir().join(format!("factory-thread-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let log = EventLog::new(&path, ProjectId::new("toy"), UnitId::new("u"));
        let mut thread = Thread::disabled(log.clone());
        thread.say("hello");
        thread.status("building");
        assert!(thread.poll().is_empty());
        assert!(!thread.live());
        let events = log.read().unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(events[0].kind, "slack.say");
        assert_eq!(events[0].data["text"], "hello");
        assert_eq!(events[1].kind, "slack.status");
    }

    #[test]
    fn an_unreachable_slack_disables_the_thread_and_logs_why() {
        let path =
            std::env::temp_dir().join(format!("factory-thread-down-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let log = EventLog::new(&path, ProjectId::new("toy"), UnitId::new("u"));
        let slack = Slack::with_base_url("token", "http://127.0.0.1:9");
        let mut thread = Thread::open(slack, "toy", Vec::new(), "header".to_string(), log.clone());
        thread.say("still running");
        let events = log.read().unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(!thread.live());
        assert_eq!(events[0].kind, "slack.failed");
        assert_eq!(events[1].kind, "slack.say");
    }
}
