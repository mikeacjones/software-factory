//! Slack, as the factory uses it for one run: a channel per project, a thread
//! per run, and the owner's replies read by polling the thread.
//!
//! Code posts. No agent holds the token.

pub mod socket;

use std::time::Duration;

use serde_json::{Value, json};

#[derive(Debug, thiserror::Error)]
pub enum SlackError {
    #[error("Slack could not be reached: {0}")]
    Transport(String),
    #[error("Slack refused `{method}`: {error}")]
    Api { method: String, error: String },
    #[error("Slack's answer to `{method}` had no `{field}`")]
    Answer { method: String, field: String },
    #[error("`{0}` cannot be a channel name")]
    BadChannelName(String),
}

/// A message in a thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub ts: String,
    pub user: String,
    pub text: String,
}

#[derive(Clone)]
pub struct Slack {
    token: String,
    base_url: String,
    agent: ureq::Agent,
}

impl std::fmt::Debug for Slack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the token.
        f.debug_struct("Slack")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

/// A button under a message.
#[derive(Clone, Copy, Debug)]
pub struct Button<'a> {
    pub action_id: &'a str,
    pub label: &'a str,
    /// Returned with the click: which thing the button is about.
    pub value: &'a str,
    /// `primary` (green) or `danger` (red).
    pub style: Option<&'a str>,
}

/// Slack's limit on the text of one section block.
const SECTION_LIMIT: usize = 2900;

impl Slack {
    pub fn new(token: impl Into<String>) -> Self {
        Self::with_base_url(token, "https://slack.com/api")
    }

    pub fn with_base_url(token: impl Into<String>, base_url: impl Into<String>) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(30)))
            // A refusal arrives as JSON with a 200 or a 4xx; read it either way.
            .http_status_as_error(false)
            .build();
        Self {
            token: token.into(),
            base_url: base_url.into(),
            agent: config.into(),
        }
    }

    fn call(&self, method: &str, body: Value) -> Result<Value, SlackError> {
        let url = format!("{}/{method}", self.base_url);
        for attempt in 0..3 {
            let mut response = self
                .agent
                .post(&url)
                .header("Authorization", &format!("Bearer {}", self.token))
                .header("Content-Type", "application/json; charset=utf-8")
                .send_json(&body)
                .map_err(|err| SlackError::Transport(err.to_string()))?;
            if response.status().as_u16() == 429 && attempt < 2 {
                let wait = response
                    .headers()
                    .get("retry-after")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<u64>().ok())
                    .unwrap_or(2);
                std::thread::sleep(Duration::from_secs(wait.min(30)));
                continue;
            }
            let answer: Value = response
                .body_mut()
                .read_json()
                .map_err(|err| SlackError::Transport(err.to_string()))?;
            if answer.get("ok").and_then(Value::as_bool) == Some(true) {
                return Ok(answer);
            }
            return Err(SlackError::Api {
                method: method.to_string(),
                error: answer
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("no reason given")
                    .to_string(),
            });
        }
        Err(SlackError::Api {
            method: method.to_string(),
            error: "rate limited".to_string(),
        })
    }

    /// Call a read method, with its arguments in the query string.
    fn get(&self, method: &str, params: &[(&str, &str)]) -> Result<Value, SlackError> {
        let url = format!("{}/{method}", self.base_url);
        let mut request = self
            .agent
            .get(&url)
            .header("Authorization", &format!("Bearer {}", self.token));
        for (key, value) in params {
            if !value.is_empty() {
                request = request.query(*key, *value);
            }
        }
        let mut response = request
            .call()
            .map_err(|err| SlackError::Transport(err.to_string()))?;
        let answer: Value = response
            .body_mut()
            .read_json()
            .map_err(|err| SlackError::Transport(err.to_string()))?;
        if answer.get("ok").and_then(Value::as_bool) == Some(true) {
            Ok(answer)
        } else {
            Err(SlackError::Api {
                method: method.to_string(),
                error: answer
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("no reason given")
                    .to_string(),
            })
        }
    }

    fn field(answer: &Value, method: &str, pointer: &str) -> Result<String, SlackError> {
        answer
            .pointer(pointer)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| SlackError::Answer {
                method: method.to_string(),
                field: pointer.to_string(),
            })
    }

    /// The bot's own user ID.
    pub fn whoami(&self) -> Result<String, SlackError> {
        let answer = self.call("auth.test", json!({}))?;
        Self::field(&answer, "auth.test", "/user_id")
    }

    /// Find a public channel by name, or create it. The bot joins it and the
    /// owners are invited. Returns the channel ID.
    pub fn ensure_channel(&self, name: &str, owners: &[String]) -> Result<String, SlackError> {
        let name = name.trim_start_matches('#');
        let valid = !name.is_empty()
            && name.len() <= 80
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_'));
        if !valid {
            return Err(SlackError::BadChannelName(name.to_string()));
        }
        let id = match self.find_channel(name)? {
            Some((id, archived)) => {
                if archived {
                    self.call("conversations.unarchive", json!({ "channel": id }))?;
                }
                id
            }
            None => {
                let answer = self.call("conversations.create", json!({ "name": name }))?;
                Self::field(&answer, "conversations.create", "/channel/id")?
            }
        };
        self.call("conversations.join", json!({ "channel": id }))?;
        if !owners.is_empty() {
            match self.call(
                "conversations.invite",
                json!({ "channel": id, "users": owners.join(",") }),
            ) {
                Ok(_) => {}
                // Already there is what we wanted.
                Err(SlackError::Api { error, .. })
                    if matches!(error.as_str(), "already_in_channel" | "cant_invite_self") => {}
                Err(err) => return Err(err),
            }
        }
        Ok(id)
    }

    fn find_channel(&self, name: &str) -> Result<Option<(String, bool)>, SlackError> {
        let mut cursor = String::new();
        loop {
            // Read methods take their arguments in the query, not as JSON.
            let answer = self.get(
                "conversations.list",
                &[
                    ("types", "public_channel"),
                    ("exclude_archived", "false"),
                    ("limit", "200"),
                    ("cursor", &cursor),
                ],
            )?;
            for channel in answer
                .get("channels")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default()
            {
                if channel.get("name").and_then(Value::as_str) == Some(name)
                    && let Some(id) = channel.get("id").and_then(Value::as_str)
                {
                    let archived = channel
                        .get("is_archived")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    return Ok(Some((id.to_string(), archived)));
                }
            }
            cursor = answer
                .pointer("/response_metadata/next_cursor")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if cursor.is_empty() {
                return Ok(None);
            }
        }
    }

    /// Post a message, in a thread if `thread_ts` is given. `text` is Slack
    /// mrkdwn. Returns the message's timestamp, which is its ID.
    pub fn post(
        &self,
        channel: &str,
        thread_ts: Option<&str>,
        text: &str,
    ) -> Result<String, SlackError> {
        let mut body = message_body(channel, text);
        if let Some(ts) = thread_ts {
            body["thread_ts"] = json!(ts);
        }
        let answer = self.call("chat.postMessage", body)?;
        Self::field(&answer, "chat.postMessage", "/ts")
    }

    /// Post a message that ends with buttons. A click arrives over the socket
    /// as an [`socket::Incoming::Action`] carrying the button's ID and value.
    pub fn post_with_buttons(
        &self,
        channel: &str,
        thread_ts: Option<&str>,
        text: &str,
        buttons: &[Button<'_>],
    ) -> Result<String, SlackError> {
        let mut body = message_body(channel, text);
        if let Some(ts) = thread_ts {
            body["thread_ts"] = json!(ts);
        }
        let elements: Vec<Value> = buttons
            .iter()
            .map(|button| {
                let mut element = json!({
                    "type": "button",
                    "action_id": button.action_id,
                    "value": button.value,
                    "text": { "type": "plain_text", "text": button.label },
                });
                if let Some(style) = button.style {
                    element["style"] = json!(style);
                }
                element
            })
            .collect();
        if let Some(blocks) = body["blocks"].as_array_mut() {
            blocks.push(json!({ "type": "actions", "elements": elements }));
        }
        let answer = self.call("chat.postMessage", body)?;
        Self::field(&answer, "chat.postMessage", "/ts")
    }

    /// A link to a message, for pointing from one thread at another.
    pub fn permalink(&self, channel: &str, ts: &str) -> Result<String, SlackError> {
        let answer = self.get(
            "chat.getPermalink",
            &[("channel", channel), ("message_ts", ts)],
        )?;
        Self::field(&answer, "chat.getPermalink", "/permalink")
    }

    /// Replace the text of a message the bot posted.
    pub fn update(&self, channel: &str, ts: &str, text: &str) -> Result<(), SlackError> {
        let mut body = message_body(channel, text);
        body["ts"] = json!(ts);
        self.call("chat.update", body).map(|_| ())
    }

    pub fn react(&self, channel: &str, ts: &str, emoji: &str) -> Result<(), SlackError> {
        match self.call(
            "reactions.add",
            json!({ "channel": channel, "timestamp": ts, "name": emoji }),
        ) {
            Err(SlackError::Api { error, .. }) if error == "already_reacted" => Ok(()),
            other => other.map(|_| ()),
        }
    }

    /// The replies in a thread after `after_ts`, oldest first. The thread's
    /// root message is never included.
    pub fn replies(
        &self,
        channel: &str,
        thread_ts: &str,
        after_ts: &str,
    ) -> Result<Vec<Message>, SlackError> {
        let mut messages = Vec::new();
        let mut cursor = String::new();
        loop {
            let answer = self.get(
                "conversations.replies",
                &[
                    ("channel", channel),
                    ("ts", thread_ts),
                    ("oldest", after_ts),
                    ("limit", "200"),
                    ("cursor", &cursor),
                ],
            )?;
            for message in answer
                .get("messages")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default()
            {
                let text = |key: &str| {
                    message
                        .get(key)
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string()
                };
                let ts = text("ts");
                if ts != thread_ts && ts.as_str() > after_ts {
                    messages.push(Message {
                        ts,
                        user: text("user"),
                        text: text("text"),
                    });
                }
            }
            cursor = answer
                .pointer("/response_metadata/next_cursor")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if cursor.is_empty() {
                break;
            }
        }
        messages.sort_by(|a, b| a.ts.cmp(&b.ts));
        Ok(messages)
    }
}

/// A message as blocks, so long text is not cut off, with a plain fallback.
fn message_body(channel: &str, text: &str) -> Value {
    let blocks: Vec<Value> = chunks(text, SECTION_LIMIT)
        .into_iter()
        .map(|chunk| json!({ "type": "section", "text": { "type": "mrkdwn", "text": chunk } }))
        .collect();
    json!({
        "channel": channel,
        "text": truncate(text, 3000),
        "blocks": blocks,
        "unfurl_links": false,
        "unfurl_media": false,
    })
}

/// Split text into pieces of at most `limit` characters, at line ends where
/// possible. At most ten pieces are kept; the rest is noted as left out.
fn chunks(text: &str, limit: usize) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut current = String::new();
    for line in text.split_inclusive('\n') {
        if current.chars().count() + line.chars().count() > limit && !current.is_empty() {
            pieces.push(std::mem::take(&mut current));
        }
        let mut rest = line.to_string();
        while rest.chars().count() > limit {
            let head: String = rest.chars().take(limit).collect();
            rest = rest.chars().skip(limit).collect();
            pieces.push(head);
        }
        current.push_str(&rest);
    }
    if !current.trim().is_empty() {
        pieces.push(current);
    }
    if pieces.len() > 10 {
        pieces.truncate(9);
        pieces.push("_…the rest was left out._".to_string());
    }
    if pieces.is_empty() {
        pieces.push(" ".to_string());
    }
    pieces
}

pub fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Make text safe to place inside mrkdwn: Slack reads `&`, `<` and `>` as
/// control characters.
pub fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Text as a quoted block, one `>` per line.
pub fn quote(text: &str) -> String {
    text.lines()
        .map(|line| format!("> {}", escape(line)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Text as a code block. Backticks inside would end the block early.
pub fn code_block(text: &str) -> String {
    format!("```\n{}\n```", escape(&text.replace("```", "'''")))
}

/// A link with a label.
pub fn link(url: &str, label: &str) -> String {
    format!("<{url}|{}>", escape(label).replace('|', "/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_characters_are_escaped() {
        assert_eq!(escape("a < b && c > d"), "a &lt; b &amp;&amp; c &gt; d");
        assert_eq!(quote("one\n<two>"), "> one\n> &lt;two&gt;");
        assert_eq!(code_block("x ``` y"), "```\nx ''' y\n```");
        assert_eq!(
            link("https://e.com", "a|b <c>"),
            "<https://e.com|a/b &lt;c&gt;>"
        );
    }

    #[test]
    fn long_text_is_split_at_line_ends() {
        let text = format!("{}\n{}\n", "a".repeat(60), "b".repeat(60));
        let pieces = chunks(&text, 100);
        assert_eq!(pieces.len(), 2);
        assert!(pieces[0].starts_with('a') && pieces[1].starts_with('b'));
        assert_eq!(chunks(&"c".repeat(250), 100).len(), 3);
        assert_eq!(chunks(&"d\n".repeat(5000), 100).len(), 10);
    }

    #[test]
    fn a_message_carries_blocks_and_a_plain_fallback() {
        let body = message_body("C1", "*hello*");
        assert_eq!(body["channel"], "C1");
        assert_eq!(body["text"], "*hello*");
        assert_eq!(body["blocks"][0]["text"]["type"], "mrkdwn");
        assert_eq!(body["unfurl_links"], false);
    }

    #[test]
    fn truncation_marks_the_cut() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 6), "hello…");
    }

    #[test]
    fn a_bad_channel_name_is_refused_before_any_call() {
        let slack = Slack::with_base_url("token", "http://127.0.0.1:9");
        assert!(matches!(
            slack.ensure_channel("Bad Name", &[]),
            Err(SlackError::BadChannelName(_))
        ));
    }

    #[test]
    fn an_unreachable_slack_is_a_transport_error() {
        let slack = Slack::with_base_url("token", "http://127.0.0.1:9");
        assert!(matches!(
            slack.post("C1", None, "hi"),
            Err(SlackError::Transport(_))
        ));
    }

    #[test]
    fn the_token_is_never_printed() {
        let slack = Slack::new("xoxb-secret");
        assert!(!format!("{slack:?}").contains("secret"));
    }
}
