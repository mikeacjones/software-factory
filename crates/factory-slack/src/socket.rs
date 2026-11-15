//! Socket Mode: Slack pushes events to the factory over a WebSocket the
//! factory opens, so nothing needs to be reachable from outside and nothing
//! is polled.
//!
//! A listener thread owns the socket. It acknowledges every envelope at
//! once, which is all Slack asks for within its deadline, and hands what the
//! factory reads to a channel: messages people wrote and buttons they pressed.
//! When the connection ends, for any reason, it opens another.

use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use serde_json::{Value, json};
use tungstenite::{Connector, Message, WebSocket, stream::MaybeTlsStream};

/// Something a person did in Slack.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Incoming {
    /// A message someone wrote. Messages from bots, edits and joins are left out.
    Message {
        channel: String,
        user: String,
        text: String,
        ts: String,
        /// The thread's root message, if the message is a reply.
        thread_ts: Option<String>,
    },
    /// A button someone pressed.
    Action {
        channel: String,
        user: String,
        action_id: String,
        value: String,
        /// The message the button is on.
        message_ts: String,
    },
}

/// Read one Socket Mode envelope: the acknowledgement it needs, what it
/// carries for the factory, and whether Slack is asking for a new connection.
pub fn read_envelope(text: &str) -> (Option<String>, Option<Incoming>, bool) {
    let Ok(envelope) = serde_json::from_str::<Value>(text) else {
        return (None, None, false);
    };
    let text_at = |value: &Value, pointer: &str| {
        value
            .pointer(pointer)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let ack = envelope
        .get("envelope_id")
        .and_then(Value::as_str)
        .map(|id| json!({ "envelope_id": id }).to_string());
    let kind = text_at(&envelope, "/type");
    let payload = envelope.get("payload").unwrap_or(&Value::Null);
    let incoming = match kind.as_str() {
        "events_api" => {
            let event = payload.get("event").unwrap_or(&Value::Null);
            let plain = text_at(event, "/type") == "message"
                && event.get("subtype").is_none()
                && event.get("bot_id").is_none();
            let user = text_at(event, "/user");
            (plain && !user.is_empty()).then(|| {
                let ts = text_at(event, "/ts");
                let thread = text_at(event, "/thread_ts");
                Incoming::Message {
                    channel: text_at(event, "/channel"),
                    user,
                    text: text_at(event, "/text"),
                    // A thread's root names itself as its thread: that is
                    // still a top-level message.
                    thread_ts: (!thread.is_empty() && thread != ts).then_some(thread),
                    ts,
                }
            })
        }
        "interactive" if text_at(payload, "/type") == "block_actions" => payload
            .pointer("/actions/0")
            .map(|action| Incoming::Action {
                channel: text_at(payload, "/channel/id"),
                user: text_at(payload, "/user/id"),
                action_id: text_at(action, "/action_id"),
                value: text_at(action, "/value"),
                message_ts: text_at(payload, "/message/ts"),
            }),
        _ => None,
    };
    (ack, incoming, kind == "disconnect")
}

/// A running listener. Dropping it does not stop the thread; `stop` does.
pub struct Listener {
    pub incoming: Receiver<Incoming>,
    connected: Arc<AtomicBool>,
    stopping: Arc<AtomicBool>,
}

impl Listener {
    /// Whether the socket is open right now.
    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    pub fn stop(&self) {
        self.stopping.store(true, Ordering::Relaxed);
    }
}

/// Start listening. `app_token` is the app-level token (`xapp-…`), and
/// `api_url` is `https://slack.com/api` outside tests. Problems are written
/// to stderr and retried for ever: the factory must not stop because Slack
/// did.
pub fn listen(app_token: String, api_url: String) -> Listener {
    let (sender, incoming) = mpsc::channel();
    let connected = Arc::new(AtomicBool::new(false));
    let stopping = Arc::new(AtomicBool::new(false));
    let (flag, stop) = (connected.clone(), stopping.clone());
    std::thread::spawn(move || {
        let mut wait = Duration::from_secs(1);
        while !stop.load(Ordering::Relaxed) {
            match serve_one_connection(&app_token, &api_url, &sender, &flag, &stop) {
                Ok(()) => wait = Duration::from_secs(1),
                Err(reason) => {
                    eprintln!(
                        "slack socket: {reason}; reconnecting in {}s",
                        wait.as_secs()
                    );
                    std::thread::sleep(wait);
                    wait = (wait * 2).min(Duration::from_secs(60));
                }
            }
            flag.store(false, Ordering::Relaxed);
        }
    });
    Listener {
        incoming,
        connected,
        stopping,
    }
}

/// Ask Slack where to connect.
fn connection_url(app_token: &str, api_url: &str) -> Result<String, String> {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .http_status_as_error(false)
        .build();
    let agent: ureq::Agent = config.into();
    let mut response = agent
        .post(&format!("{api_url}/apps.connections.open"))
        .header("Authorization", &format!("Bearer {app_token}"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send("")
        .map_err(|err| format!("could not reach Slack: {err}"))?;
    let answer: Value = response
        .body_mut()
        .read_json()
        .map_err(|err| format!("could not read Slack's answer: {err}"))?;
    answer
        .get("url")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            format!(
                "Slack refused apps.connections.open: {}",
                answer
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("no reason given")
            )
        })
}

fn open(url: &str) -> Result<WebSocket<MaybeTlsStream<TcpStream>>, String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or("the socket URL has no scheme")?;
    let authority = rest.split(['/', '?']).next().unwrap_or("");
    let tls = scheme == "wss";
    let address = if authority.contains(':') {
        authority.to_string()
    } else {
        format!("{authority}:{}", if tls { 443 } else { 80 })
    };
    let stream = TcpStream::connect(&address).map_err(|err| format!("connect {address}: {err}"))?;
    // A read that waits for ever could never notice a stop request.
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|err| err.to_string())?;
    let connector = if tls {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|err| err.to_string())?
        .with_root_certificates(roots)
        .with_no_client_auth();
        Connector::Rustls(Arc::new(config))
    } else {
        Connector::Plain
    };
    tungstenite::client_tls_with_config(url, stream, None, Some(connector))
        .map(|(socket, _)| socket)
        .map_err(|err| format!("WebSocket handshake: {err}"))
}

fn serve_one_connection(
    app_token: &str,
    api_url: &str,
    sender: &Sender<Incoming>,
    connected: &AtomicBool,
    stopping: &AtomicBool,
) -> Result<(), String> {
    let mut socket = open(&connection_url(app_token, api_url)?)?;
    connected.store(true, Ordering::Relaxed);
    while !stopping.load(Ordering::Relaxed) {
        let message = match socket.read() {
            Ok(message) => message,
            // The read timed out with nothing to read: look again.
            Err(tungstenite::Error::Io(err))
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                return Ok(());
            }
            Err(err) => return Err(format!("read: {err}")),
        };
        let Message::Text(text) = message else {
            if message.is_close() {
                return Ok(());
            }
            continue;
        };
        let (ack, incoming, disconnect) = read_envelope(text.as_str());
        if let Some(ack) = ack {
            socket
                .send(Message::text(ack))
                .map_err(|err| format!("acknowledge: {err}"))?;
        }
        if let Some(incoming) = incoming
            && sender.send(incoming).is_err()
        {
            // Nobody is listening any more.
            return Ok(());
        }
        if disconnect {
            return Ok(());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    #[test]
    fn an_owner_message_and_a_button_press_are_read() {
        let message = r#"{"envelope_id":"e1","type":"events_api","payload":{"event":{"type":"message","channel":"C1","user":"U1","text":"reach parity","ts":"10.1"}}}"#;
        assert_eq!(
            read_envelope(message),
            (
                Some(r#"{"envelope_id":"e1"}"#.to_string()),
                Some(Incoming::Message {
                    channel: "C1".to_string(),
                    user: "U1".to_string(),
                    text: "reach parity".to_string(),
                    ts: "10.1".to_string(),
                    thread_ts: None,
                }),
                false
            )
        );
        let reply = message.replace(r#""ts":"10.1""#, r#""ts":"10.2","thread_ts":"10.1""#);
        let (_, incoming, _) = read_envelope(&reply);
        assert!(
            matches!(incoming, Some(Incoming::Message { thread_ts: Some(root), .. }) if root == "10.1")
        );

        let press = r#"{"envelope_id":"e2","type":"interactive","payload":{"type":"block_actions","user":{"id":"U1"},"channel":{"id":"C1"},"message":{"ts":"11.0"},"actions":[{"action_id":"goal_accept","value":"g-1"}]}}"#;
        assert_eq!(
            read_envelope(press).1,
            Some(Incoming::Action {
                channel: "C1".to_string(),
                user: "U1".to_string(),
                action_id: "goal_accept".to_string(),
                value: "g-1".to_string(),
                message_ts: "11.0".to_string(),
            })
        );
    }

    #[test]
    fn bots_edits_and_noise_are_acknowledged_and_dropped() {
        for event in [
            r#"{"type":"message","channel":"C1","bot_id":"B1","user":"U9","text":"status","ts":"1.0"}"#,
            r#"{"type":"message","subtype":"message_changed","channel":"C1","ts":"1.0"}"#,
            r#"{"type":"reaction_added","user":"U1"}"#,
        ] {
            let envelope = format!(
                r#"{{"envelope_id":"e","type":"events_api","payload":{{"event":{event}}}}}"#
            );
            let (ack, incoming, disconnect) = read_envelope(&envelope);
            assert!(ack.is_some());
            assert_eq!(incoming, None, "{event}");
            assert!(!disconnect);
        }
        assert_eq!(read_envelope(r#"{"type":"hello"}"#), (None, None, false));
        assert!(read_envelope(r#"{"type":"disconnect","reason":"refresh_requested"}"#).2);
        assert_eq!(read_envelope("not json"), (None, None, false));
    }

    /// A stand-in for Slack: one HTTP answer that says where the socket is,
    /// then a WebSocket that sends two envelopes and expects each acknowledged.
    #[test]
    fn the_listener_connects_acknowledges_and_delivers() {
        let socket_server = TcpListener::bind("127.0.0.1:0").unwrap();
        let socket_url = format!("ws://{}/link", socket_server.local_addr().unwrap());
        let http = TcpListener::bind("127.0.0.1:0").unwrap();
        let api_url = format!("http://{}", http.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in http.incoming() {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap() > 2 {
                    line.clear();
                }
                let body = format!(r#"{{"ok":true,"url":"{socket_url}"}}"#);
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        let (acks_out, acks) = mpsc::channel();
        std::thread::spawn(move || {
            let (stream, _) = socket_server.accept().unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            socket.send(Message::text(r#"{"type":"hello"}"#)).unwrap();
            for (id, text) in [("e1", "first"), ("e2", "second")] {
                let envelope = format!(
                    r#"{{"envelope_id":"{id}","type":"events_api","payload":{{"event":{{"type":"message","channel":"C1","user":"U1","text":"{text}","ts":"1.0"}}}}}}"#
                );
                socket.send(Message::text(envelope)).unwrap();
                acks_out.send(socket.read().unwrap().to_string()).unwrap();
            }
        });

        let listener = listen("xapp-test".to_string(), api_url);
        let wait = Duration::from_secs(10);
        for expected in ["first", "second"] {
            let Incoming::Message { text, .. } = listener.incoming.recv_timeout(wait).unwrap()
            else {
                panic!("expected a message");
            };
            assert_eq!(text, expected);
        }
        assert_eq!(acks.recv_timeout(wait).unwrap(), r#"{"envelope_id":"e1"}"#);
        assert_eq!(acks.recv_timeout(wait).unwrap(), r#"{"envelope_id":"e2"}"#);
        assert!(listener.connected());
        listener.stop();
    }
}
