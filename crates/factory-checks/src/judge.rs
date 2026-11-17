//! A classifier's reading of a failed check: what kind of failure its
//! output shows. It is a reading and nothing more. The factory records it
//! beside what its own checks found (the rerun, the run on the base, the
//! host patterns) and routes nothing by it.
//!
//! The classifier is a System One model behind TypeSafe's API: it is sent
//! the failure's lines and a question with fixed answers, and returns one
//! of them with a probability for each. On the factory's own records it
//! named the kind of a failure reliably; its guess at whether a failure
//! was a flake, and at whether the test or the code was wrong, did not
//! hold up, so neither is asked (owner's decision, 2026-11-17).

use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::{Value, json};

/// The kinds a failure may be said to be, each with what it means. The
/// order is part of the question: the model leans toward what comes first,
/// so the order is fixed and a change to it is a change to the readings.
pub const KINDS: [(&str, &str); 6] = [
    (
        "host",
        "The machine, network or CI service failed before the project's code was exercised: a download error, Docker not running, a full disk, a lost runner",
    ),
    (
        "timeout",
        "A test or command hung or ran past its time limit",
    ),
    (
        "crash",
        "A test process crashed: a signal, a panic, an abort, a segmentation fault",
    ),
    ("compile", "The code did not compile"),
    (
        "policy",
        "A formatter, a linter or a check of the repository's own rules refused the change",
    ),
    (
        "assertion",
        "A test ran to its end and an expectation in it was not met",
    ),
];

/// The most lines of a failure that are sent, and the most of each line.
/// The model's readings get worse as what it is sent grows with lines that
/// have nothing to do with the question.
const MOST_LINES: usize = 45;
const MOST_WIDTH: usize = 300;

#[derive(Debug, thiserror::Error)]
pub enum JudgeError {
    #[error("the request failed: {0}")]
    Http(String),
    #[error("the answer could not be read: {0}")]
    Shape(String),
}

/// What the classifier said of one failure.
#[derive(Clone, Debug, PartialEq)]
pub struct Judged {
    /// One of [`KINDS`].
    pub kind: String,
    pub confidence: f64,
    /// Every kind with its probability, in the order of [`KINDS`].
    pub probabilities: Vec<(String, f64)>,
    /// The model that answered, as the service names it.
    pub model: String,
    pub millis: u64,
}

/// The lines of a failed check's output that say what failed, for the
/// classifier: each line `patterns` or the usual words of a failure pick
/// out, with the line before it and the two after, or the last lines where
/// nothing matches. Colour codes are taken out, long lines are cut, and
/// `home` (the user's home directory) is written `~`, so that no more of
/// the machine than the failure needs leaves it.
pub fn failure_excerpt(output: &str, patterns: &[String], home: Option<&str>) -> String {
    let mut picks: Vec<Regex> = patterns
        .iter()
        .filter_map(|pattern| Regex::new(pattern).ok())
        .collect();
    if let Ok(usual) =
        Regex::new(r"(?i)error|fail|panic|timed out|timeout|signal|expected|deadlock|Build Summary")
    {
        picks.push(usual);
    }
    let colour = Regex::new(r"\x1b\[[0-9;]*[A-Za-z]").ok();
    let lines: Vec<String> = output
        .lines()
        .map(|line| {
            let plain = match &colour {
                Some(colour) => colour.replace_all(line, "").into_owned(),
                None => line.to_string(),
            };
            let plain = match home.filter(|home| !home.is_empty()) {
                Some(home) => plain.replace(home, "~"),
                None => plain,
            };
            plain.chars().take(MOST_WIDTH).collect()
        })
        .collect();
    let mut keep = vec![false; lines.len()];
    for (at, line) in lines.iter().enumerate() {
        // A warning that names an error is not the failure.
        if line.contains("(warn)") || !picks.iter().any(|pick| pick.is_match(line)) {
            continue;
        }
        let first = at.saturating_sub(1);
        for near in keep.iter_mut().take(at + 3).skip(first) {
            *near = true;
        }
    }
    let picked: Vec<&str> = lines
        .iter()
        .zip(&keep)
        .filter(|(line, keep)| **keep && !line.trim().is_empty())
        .map(|(line, _)| line.as_str())
        .collect();
    let shown: Vec<&str> = if picked.is_empty() {
        let meaningful: Vec<&str> = lines
            .iter()
            .map(String::as_str)
            .filter(|line| !line.trim().is_empty())
            .collect();
        meaningful[meaningful.len().saturating_sub(20)..].to_vec()
    } else {
        picked
    };
    shown[..shown.len().min(MOST_LINES)].join("\n")
}

/// The request that asks what kind of failure `excerpt` shows. `preface`
/// is what the factory knows that the lines do not say, such as that the
/// command reached its time limit, or which CI jobs failed.
pub fn kind_request(model: &str, preface: &str, excerpt: &str) -> Value {
    let criteria: serde_json::Map<String, Value> = KINDS
        .iter()
        .map(|(kind, means)| ((*kind).to_string(), json!(means)))
        .collect();
    let state = if preface.trim().is_empty() {
        excerpt.to_string()
    } else {
        format!("{}\n{excerpt}", preface.trim())
    };
    json!({
        "model": model,
        "state": state,
        "questions": {
            "kind": {
                "type": "choice",
                "instructions": "What kind of failure do these build or test output lines show",
                "criteria": criteria,
            }
        }
    })
}

/// What the service's answer to [`kind_request`] says.
pub fn parse_kind(answer: &Value, millis: u64) -> Result<Judged, JudgeError> {
    let kind = &answer["answers"]["kind"];
    let choice = kind["choice"]
        .as_str()
        .ok_or_else(|| JudgeError::Shape("it names no choice".to_string()))?;
    if !KINDS.iter().any(|(known, _)| *known == choice) {
        return Err(JudgeError::Shape(format!(
            "`{choice}` is not a kind that was asked for"
        )));
    }
    let probabilities = KINDS
        .iter()
        .map(|(known, _)| {
            (
                (*known).to_string(),
                kind["probabilities"][*known].as_f64().unwrap_or(0.0),
            )
        })
        .collect();
    Ok(Judged {
        kind: choice.to_string(),
        confidence: kind["confidence"].as_f64().unwrap_or(0.0),
        probabilities,
        model: answer["model"].as_str().unwrap_or("").to_string(),
        millis,
    })
}

/// The classifier, as the factory reaches it.
pub struct Judge {
    url: String,
    model: String,
    key: String,
    agent: ureq::Agent,
}

impl std::fmt::Debug for Judge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the key.
        f.debug_struct("Judge")
            .field("url", &self.url)
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

impl Judge {
    /// `timeout` bounds the whole call: a reading that is late is not
    /// worth holding a step up for.
    pub fn new(
        url: impl Into<String>,
        model: impl Into<String>,
        key: impl Into<String>,
        timeout: Duration,
    ) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            // A refusal arrives as JSON with a 4xx; read it either way.
            .http_status_as_error(false)
            .build();
        Self {
            url: url.into(),
            model: model.into(),
            key: key.into(),
            agent: config.into(),
        }
    }

    /// Ask what kind of failure `excerpt` shows.
    pub fn kind(&self, preface: &str, excerpt: &str) -> Result<Judged, JudgeError> {
        let started = Instant::now();
        let mut response = self
            .agent
            .post(&self.url)
            .header("Authorization", &format!("Bearer {}", self.key))
            .send_json(kind_request(&self.model, preface, excerpt))
            .map_err(|error| JudgeError::Http(error.to_string()))?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .read_to_string()
            .map_err(|error| JudgeError::Http(error.to_string()))?;
        if !(200..300).contains(&status) {
            let said: String = body.chars().take(200).collect();
            return Err(JudgeError::Http(format!("status {status}: {said}")));
        }
        let answer: Value =
            serde_json::from_str(&body).map_err(|error| JudgeError::Shape(error.to_string()))?;
        let millis = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        parse_kind(&answer, millis)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// A server that answers one request with `status` and `body`, and
    /// hands back what it was sent.
    fn serve_once(status: &str, body: &str) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
        let (status, body) = (status.to_string(), body.to_string());
        let served = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let read = stream.read(&mut chunk).unwrap();
                request.extend_from_slice(&chunk[..read]);
                let text = String::from_utf8_lossy(&request).to_string();
                if let Some(head) = text.find("\r\n\r\n") {
                    let length = text
                        .to_ascii_lowercase()
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:").map(str::to_string))
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if request.len() >= head + 4 + length || read == 0 {
                        break;
                    }
                }
                if read == 0 {
                    break;
                }
            }
            write!(
                stream,
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            String::from_utf8_lossy(&request).to_string()
        });
        (url, served)
    }

    const ANSWER: &str = r#"{"model":"jev-1.13.0","answers":{"kind":{"type":"choice","choice":"timeout","confidence":0.97,"probabilities":{"host":0.01,"timeout":0.97,"crash":0.0,"compile":0.0,"policy":0.0,"assertion":0.02}}},"usage":{"input_tokens":400,"output_tokens":70}}"#;

    #[test]
    fn a_failures_lines_are_picked_out_cut_down_and_cleared_of_the_home_directory() {
        let mut output = String::new();
        for line in 0..200 {
            output.push_str(&format!("compiling step {line}\n"));
        }
        output.push_str("[temporal] (warn): Activity failed Error=\"name is empty\"\n");
        output.push_str("\x1b[31merror: 'worker.test.cancel' timed out after 10m\x1b[0m\n");
        output.push_str("    at /Users/someone/work/src/worker.zig:12\n");
        output.push_str(&format!("{}\n", "x".repeat(900)));
        output.push_str("later noise\n");
        let excerpt = failure_excerpt(&output, &[], Some("/Users/someone"));
        let lines: Vec<&str> = excerpt.lines().collect();
        // The line before, the failure with its colour gone, and two after.
        assert_eq!(
            lines[0],
            "[temporal] (warn): Activity failed Error=\"name is empty\""
        );
        assert_eq!(lines[1], "error: 'worker.test.cancel' timed out after 10m");
        assert_eq!(lines[2], "    at ~/work/src/worker.zig:12");
        assert_eq!(lines[3].len(), 300);
        assert_eq!(lines.len(), 4, "{excerpt}");
        // The project's own patterns pick lines too.
        let named = failure_excerpt(
            "a\nb\nFLAKY test_x\nc\nd\ne\n",
            &["^FLAKY ".to_string()],
            None,
        );
        assert_eq!(named, "b\nFLAKY test_x\nc\nd");
        // Nothing that looks like a failure: the last lines.
        let quiet: String = (0..30).map(|line| format!("line {line}\n")).collect();
        let tail = failure_excerpt(&quiet, &[], None);
        assert_eq!(tail.lines().count(), 20);
        assert!(tail.ends_with("line 29"));
        // Never more than the limit.
        let loud: String = (0..500).map(|line| format!("error {line}\n")).collect();
        assert_eq!(failure_excerpt(&loud, &[], None).lines().count(), 45);
    }

    #[test]
    fn the_question_has_fixed_answers_in_a_fixed_order() {
        let request = kind_request(
            "jev-1.13.0",
            "The command reached its time limit.",
            "error: x",
        );
        assert_eq!(request["model"], "jev-1.13.0");
        assert_eq!(
            request["state"],
            "The command reached its time limit.\nerror: x"
        );
        let question = &request["questions"]["kind"];
        assert_eq!(question["type"], "choice");
        let kinds: Vec<&String> = question["criteria"].as_object().unwrap().keys().collect();
        assert_eq!(kinds.len(), 6);
        // Only the kind is asked: not whether it is a flake, and not
        // whether the test or the code is wrong.
        assert_eq!(request["questions"].as_object().unwrap().len(), 1);
        assert_eq!(kind_request("m", " ", "error: x")["state"], "error: x");
    }

    #[test]
    fn the_classifier_is_asked_once_and_its_answer_is_read() {
        let (url, served) = serve_once("200 OK", ANSWER);
        let judge = Judge::new(url, "jev-1.13.0", "secret-key", Duration::from_secs(5));
        let judged = judge.kind("", "error: 'x' timed out after 10m").unwrap();
        assert_eq!(judged.kind, "timeout");
        assert!((judged.confidence - 0.97).abs() < 1e-9);
        assert_eq!(judged.model, "jev-1.13.0");
        assert_eq!(judged.probabilities.len(), 6);
        assert_eq!(judged.probabilities[1], ("timeout".to_string(), 0.97));
        let request = served.join().unwrap();
        assert!(request.starts_with("POST /v1/systemone "), "{request}");
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer secret-key"),
            "{request}"
        );
        assert!(request.contains("timed out after 10m"), "{request}");
        // The key is never in what is printed of it.
        assert!(!format!("{judge:?}").contains("secret-key"));
    }

    #[test]
    fn a_refusal_an_unknown_kind_and_a_dead_server_are_errors_and_nothing_more() {
        let (url, served) = serve_once("401 Unauthorized", r#"{"error":"bad key"}"#);
        let judge = Judge::new(url, "m", "k", Duration::from_secs(5));
        let refused = judge.kind("", "error").unwrap_err().to_string();
        assert!(refused.contains("status 401"), "{refused}");
        served.join().unwrap();

        let odd = r#"{"model":"m","answers":{"kind":{"choice":"gremlins","confidence":1.0,"probabilities":{}}}}"#;
        let (url, served) = serve_once("200 OK", odd);
        let judge = Judge::new(url, "m", "k", Duration::from_secs(5));
        let unknown = judge.kind("", "error").unwrap_err().to_string();
        assert!(unknown.contains("`gremlins` is not a kind"), "{unknown}");
        served.join().unwrap();

        // Nothing listens here.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
        drop(listener);
        let judge = Judge::new(url, "m", "k", Duration::from_secs(2));
        assert!(matches!(judge.kind("", "error"), Err(JudgeError::Http(_))));
    }
}
