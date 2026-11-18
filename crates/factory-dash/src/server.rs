//! The page and what it asks for, over HTTP.
//!
//! Only reading is offered: the page, the picture of the factory, and
//! parts of runs' logs; and the owner's acts, to those allowed. A
//! connection from an address outside the allowed networks is closed
//! before a byte of it is read.
//!
//! The page is sent the picture as a stream of events (`/api/stream`):
//! the whole picture once, then only what changed in it, when it changes.

use std::collections::hash_map::DefaultHasher;
use std::convert::Infallible;
use std::hash::{Hash, Hasher};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use http_body_util::Full;
use http_body_util::combinators::BoxBody;
use hyper::body::{Body, Bytes, Frame, Incoming};
use hyper::header::{self, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, mpsc, watch};

use http_body_util::{BodyExt, Limited};

use crate::act::{self, Act, Engine};
use crate::net::{Net, host_is_ours, may_connect};
use crate::records::{Links, Reader};
use crate::{patch, runlog, thread};

const PAGE: &str = include_str!("../assets/index.html");
const STYLE: &str = include_str!("../assets/app.css");
const SCRIPT: &str = include_str!("../assets/app.js");

/// How often the records are read while somebody is looking.
const LOOK_EVERY: Duration = Duration::from_secs(2);
/// How long after the last question the records go on being read.
const LOOKED_LATELY_SECS: u64 = 30;
/// How many connections are answered at once.
const MOST_CONNECTIONS: usize = 64;
/// How often a stream says it is still there when nothing has changed,
/// so the page can tell a quiet factory from a server that has gone.
const BEAT_EVERY: Duration = Duration::from_secs(10);

/// What the server is run with.
#[derive(Clone, Debug)]
pub struct Settings {
    /// The factory's state directory.
    pub state: PathBuf,
    /// The networks that may connect. None means nobody.
    pub allow: Vec<Net>,
    /// Host names the page may be asked for under, besides addresses.
    pub names: Vec<String>,
    pub links: Links,
    /// The factory's engine command, to hand the owner's acts to. None
    /// leaves the page as it was: reading only.
    pub engine: Option<Engine>,
    /// The networks whose people may act from the page, a part of those
    /// that may connect. None means nobody.
    pub act_allow: Vec<Net>,
}

/// The picture as it was last read.
#[derive(Default)]
struct Picture {
    value: serde_json::Value,
    body: Bytes,
    tag: String,
}

struct Shared {
    settings: Settings,
    reader: Mutex<Reader>,
    /// The latest picture; every stream open holds a receiver of it.
    pictures: watch::Sender<Arc<Picture>>,
    asked_at: AtomicU64,
    /// One act at a time is handed on, in the order they came.
    acting: tokio::sync::Mutex<()>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

impl Shared {
    /// Read the records afresh.
    fn look(&self) {
        let picture = match self.reader.lock() {
            Ok(mut reader) => reader.picture(now()),
            Err(_) => return,
        };
        let body = picture.to_string();
        let mut hasher = DefaultHasher::new();
        body.hash(&mut hasher);
        let tag = format!("\"{:016x}\"", hasher.finish());
        // The streams are woken only when something has changed.
        self.pictures.send_if_modified(|held| {
            if held.tag == tag {
                return false;
            }
            *held = Arc::new(Picture {
                value: picture,
                body: Bytes::from(body),
                tag,
            });
            true
        });
    }

    fn picture(&self) -> Arc<Picture> {
        self.pictures.borrow().clone()
    }

    /// Whether somebody is looking: a stream is open, or the picture was
    /// asked for lately.
    fn watched(&self) -> bool {
        self.pictures.receiver_count() > 0
            || self.asked_at.load(Ordering::Relaxed) + LOOKED_LATELY_SECS >= now()
    }
}

/// Serve the page on a listener for as long as the process lives.
pub async fn serve(listener: TcpListener, settings: Settings) -> std::io::Result<()> {
    let reader = Reader::new(settings.state.clone(), settings.links.clone());
    let shared = Arc::new(Shared {
        settings,
        reader: Mutex::new(reader),
        pictures: watch::Sender::new(Arc::default()),
        asked_at: AtomicU64::new(0),
        acting: tokio::sync::Mutex::new(()),
    });
    // The first picture is there before the first question.
    let first = shared.clone();
    let _ = tokio::task::spawn_blocking(move || first.look()).await;
    // The records are read again and again, but only while somebody is
    // looking: an unwatched page costs nothing.
    let looker = shared.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(LOOK_EVERY);
        loop {
            tick.tick().await;
            if !looker.watched() {
                continue;
            }
            let again = looker.clone();
            let _ = tokio::task::spawn_blocking(move || again.look()).await;
        }
    });
    let room = Arc::new(Semaphore::new(MOST_CONNECTIONS));
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(_) => {
                // Out of files, most likely: give the others time to end.
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        if !may_connect(&shared.settings.allow, peer.ip()) {
            refused(peer);
            continue;
        }
        let Ok(place) = room.clone().try_acquire_owned() else {
            continue;
        };
        let shared = shared.clone();
        tokio::spawn(async move {
            let _place = place;
            let service = service_fn(move |request| {
                let shared = shared.clone();
                async move { Ok::<_, Infallible>(answer(&shared, request, peer).await) }
            });
            let _ = http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(Duration::from_secs(10))
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
}

/// Say that somebody outside the allowed networks tried, once a minute
/// at most for each address.
fn refused(peer: SocketAddr) {
    static LAST: Mutex<Option<(std::net::IpAddr, u64)>> = Mutex::new(None);
    let Ok(mut last) = LAST.lock() else {
        return;
    };
    let time = now();
    if last.is_none_or(|(address, at)| address != peer.ip() || at + 60 < time) {
        eprintln!(
            "refused a connection from {}: not an allowed network",
            peer.ip()
        );
        *last = Some((peer.ip(), time));
    }
}

type Answer = Response<BoxBody<Bytes, Infallible>>;

fn reply(status: StatusCode, kind: &'static str, body: impl Into<Bytes>) -> Answer {
    dressed(status, kind, BoxBody::new(Full::new(body.into())))
}

/// An answer with the headers every answer carries.
fn dressed(status: StatusCode, kind: &'static str, body: BoxBody<Bytes, Infallible>) -> Answer {
    let mut answer = Response::new(body);
    *answer.status_mut() = status;
    let headers = answer.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(kind));
    // The page loads nothing but its own files and talks to nobody but
    // this server, and no other page may show it in a frame.
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            // The graphs place their nodes with style attributes; no
            // style sheet but the page's own is loaded.
            "default-src 'none'; script-src 'self'; style-src 'self'; style-src-elem 'self'; \
             style-src-attr 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; \
             base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
        ),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        "cross-origin-resource-policy",
        HeaderValue::from_static("same-origin"),
    );
    if let Ok(time) = HeaderValue::from_str(&now().to_string()) {
        headers.insert("x-now", time);
    }
    answer
}

fn plain(status: StatusCode, text: &'static str) -> Answer {
    reply(status, "text/plain; charset=utf-8", text)
}

fn json_reply(status: StatusCode, body: &serde_json::Value) -> Answer {
    reply(status, "application/json", body.to_string())
}

/// Whether the page may act for whoever is asking, and if not why.
fn may_act(shared: &Shared, peer: SocketAddr) -> Result<&Engine, &'static str> {
    let Some(engine) = &shared.settings.engine else {
        return Err("The dashboard was started without the factory's engine, so it only reads.");
    };
    if !engine.command.is_file() {
        return Err("The factory's engine command is not where the dashboard was told it is.");
    }
    if !may_connect(&shared.settings.act_allow, peer.ip()) {
        return Err("Acting from the page is not allowed from this network (see --act-allow).");
    }
    Ok(engine)
}

/// An act the owner sends from the page. A browser sends the header and
/// the type only from the page itself: another site's page cannot without
/// asking first, and is never answered when it asks.
async fn act_answer(shared: &Arc<Shared>, request: Request<Incoming>, peer: SocketAddr) -> Answer {
    let engine = match may_act(shared, peer) {
        Ok(engine) => engine.clone(),
        Err(why) => return json_reply(StatusCode::FORBIDDEN, &json!({"ok": false, "error": why})),
    };
    let headers = request.headers();
    let ours = headers
        .get("x-factory-owner")
        .is_some_and(|value| value == "1")
        && headers
            .get(header::CONTENT_TYPE)
            .and_then(|kind| kind.to_str().ok())
            .is_some_and(|kind| kind.starts_with("application/json"));
    let host = headers
        .get(header::HOST)
        .and_then(|host| host.to_str().ok())
        .unwrap_or("");
    let same_origin = headers
        .get(header::ORIGIN)
        .and_then(|origin| origin.to_str().ok())
        .is_none_or(|origin| {
            origin
                .split_once("://")
                .is_some_and(|(_, rest)| rest == host)
        });
    if !ours || !same_origin {
        return json_reply(
            StatusCode::FORBIDDEN,
            &json!({"ok": false, "error": "Only the page itself may act."}),
        );
    }
    let Ok(body) = Limited::new(request.into_body(), 32 * 1024).collect().await else {
        return json_reply(
            StatusCode::PAYLOAD_TOO_LARGE,
            &json!({"ok": false, "error": "Too long."}),
        );
    };
    let asked: serde_json::Value = serde_json::from_slice(&body.to_bytes()).unwrap_or_default();
    let act = match Act::read(&asked) {
        Ok(act) => act,
        Err(why) => {
            return json_reply(
                StatusCode::BAD_REQUEST,
                &json!({"ok": false, "error": why.to_string()}),
            );
        }
    };
    let _turn = shared.acting.lock().await;
    let done = act::hand_on(&engine, &act).await;
    eprintln!(
        "{} from {}: {}",
        act.line(),
        peer.ip(),
        match &done {
            Ok(_) => "handed on".to_string(),
            Err(why) => why.to_string(),
        }
    );
    // The records change as the tiers take it in: read them soon.
    shared.asked_at.store(now(), Ordering::Relaxed);
    match done {
        Ok(said) => json_reply(StatusCode::OK, &json!({"ok": true, "said": said})),
        Err(why) => json_reply(
            StatusCode::BAD_GATEWAY,
            &json!({"ok": false, "error": why.to_string()}),
        ),
    }
}

/// A conversation with the owner, for `where=goal&id=…` or
/// `where=project&id=…&thread=…`.
fn conversation(state: &Path, query: &str) -> Option<String> {
    let field = |name: &str| {
        query
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value)
    };
    let id = field("id").filter(|id| plain_name(id))?;
    let found = match field("where")? {
        "goal" => thread::of_goal(&state.join("goals").join(id).join("events.jsonl"))?,
        "project" => {
            let ts = field("thread").filter(|ts| plain_name(ts))?;
            let text =
                std::fs::read_to_string(state.join("projects").join(id).join("executive.json"))
                    .ok()?;
            thread::of_executive(&serde_json::from_str(&text).ok()?, ts)?
        }
        _ => return None,
    };
    Some(found.to_string())
}

async fn answer(shared: &Arc<Shared>, request: Request<Incoming>, peer: SocketAddr) -> Answer {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|host| host.to_str().ok())
        .unwrap_or("");
    if !host_is_ours(host, &shared.settings.names) {
        return plain(
            StatusCode::MISDIRECTED_REQUEST,
            "This page is not served under that name.\n",
        );
    }
    if request.method() == Method::POST && request.uri().path() == "/api/act" {
        return act_answer(shared, request, peer).await;
    }
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return plain(
            StatusCode::METHOD_NOT_ALLOWED,
            "Only reading is offered, and the owner's acts at /api/act.\n",
        );
    }
    match request.uri().path() {
        "/" => reply(StatusCode::OK, "text/html; charset=utf-8", PAGE),
        "/app.css" => reply(StatusCode::OK, "text/css; charset=utf-8", STYLE),
        "/app.js" => reply(StatusCode::OK, "text/javascript; charset=utf-8", SCRIPT),
        "/healthz" => plain(StatusCode::OK, "ok\n"),
        "/api/can-act" => json_reply(
            StatusCode::OK,
            &match may_act(shared, peer) {
                Ok(_) => json!({"act": true}),
                Err(why) => json!({"act": false, "why": why}),
            },
        ),
        "/api/thread" => {
            let query = request.uri().query().unwrap_or("").to_string();
            let state = shared.settings.state.clone();
            match tokio::task::spawn_blocking(move || conversation(&state, &query)).await {
                Ok(Some(body)) => reply(StatusCode::OK, "application/json", body),
                _ => plain(StatusCode::NOT_FOUND, "There is no such conversation.\n"),
            }
        }
        "/api/stream" => stream(shared),
        "/api/snapshot" => {
            shared.asked_at.store(now(), Ordering::Relaxed);
            let picture = shared.picture();
            let known = request
                .headers()
                .get(header::IF_NONE_MATCH)
                .and_then(|tag| tag.to_str().ok());
            let mut answer = if known == Some(picture.tag.as_str()) {
                reply(StatusCode::NOT_MODIFIED, "application/json", Bytes::new())
            } else {
                reply(StatusCode::OK, "application/json", picture.body.clone())
            };
            if let Ok(tag) = HeaderValue::from_str(&picture.tag) {
                answer.headers_mut().insert(header::ETAG, tag);
            }
            answer
        }
        "/api/run" => {
            let query = request.uri().query().unwrap_or("").to_string();
            let state = shared.settings.state.clone();
            match tokio::task::spawn_blocking(move || run(&state, &query)).await {
                Ok(Some(body)) => reply(StatusCode::OK, "application/json", body),
                _ => plain(StatusCode::NOT_FOUND, "There is no such run.\n"),
            }
        }
        _ => plain(StatusCode::NOT_FOUND, "There is no such page.\n"),
    }
}

/// The picture as a stream of server-sent events: `picture` with the
/// whole of it first, then `patch` with what changed (see [`patch`])
/// each time it does, and `beat` with the server's clock when nothing
/// has for a while. Each stream works out its changes from what it sent
/// last, so a page that applies them in order holds the picture exactly.
fn stream(shared: &Arc<Shared>) -> Answer {
    let (sender, receiver) = mpsc::channel(8);
    let pictures = shared.pictures.subscribe();
    tokio::spawn(feed(pictures, sender));
    let mut answer = dressed(
        StatusCode::OK,
        "text/event-stream",
        BoxBody::new(Feed(receiver)),
    );
    answer
        .headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    answer
}

/// One event, as a stream carries it.
fn event(name: &str, data: &serde_json::Value) -> Bytes {
    Bytes::from(format!("event: {name}\ndata: {data}\n\n"))
}

/// Send a stream its events until the page goes away.
async fn feed(mut pictures: watch::Receiver<Arc<Picture>>, out: mpsc::Sender<Bytes>) {
    let mut sent = pictures.borrow_and_update().clone();
    // A page that loses the stream asks again after two seconds.
    let first = format!(
        "retry: 2000\nevent: picture\ndata: {{\"tag\":{},\"now\":{},\"picture\":",
        json!(sent.tag),
        now()
    );
    let mut whole = Vec::with_capacity(first.len() + sent.body.len() + 3);
    whole.extend_from_slice(first.as_bytes());
    whole.extend_from_slice(&sent.body);
    whole.extend_from_slice(b"}\n\n");
    if out.send(Bytes::from(whole)).await.is_err() {
        return;
    }
    let mut beat = tokio::time::interval(BEAT_EVERY);
    beat.tick().await;
    loop {
        let next = tokio::select! {
            changed = pictures.changed() => {
                if changed.is_err() {
                    return;
                }
                let fresh = pictures.borrow_and_update().clone();
                if fresh.tag == sent.tag {
                    continue;
                }
                let changes = patch::diff(&sent.value, &fresh.value);
                let said = event(
                    "patch",
                    &json!({"from": sent.tag, "tag": fresh.tag, "now": now(), "changes": changes}),
                );
                sent = fresh;
                beat.reset();
                said
            }
            _ = beat.tick() => event("beat", &json!({"now": now()})),
            () = out.closed() => return,
        };
        if out.send(next).await.is_err() {
            return;
        }
    }
}

/// The body of a stream: whatever its feed sends, for as long as it does.
struct Feed(mpsc::Receiver<Bytes>);

impl Body for Feed {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        self.0
            .poll_recv(context)
            .map(|said| said.map(|bytes| Ok(Frame::data(bytes))))
    }
}

/// A name the factory would give a unit, a goal, a project or a log:
/// nothing in it can lead out of the directory it is looked for in.
fn plain_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && !name.starts_with('.')
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Where the log of a run is, if what was asked for names one.
pub fn log_path(state: &Path, place: &str, id: &str, file: &str) -> Option<PathBuf> {
    if !plain_name(id) || !plain_name(file) || !file.ends_with(".jsonl") {
        return None;
    }
    let dir = match place {
        "unit" => state.join("units").join(id).join("runs"),
        "goal" => state.join("goals").join(id).join("runs"),
        "project" => state
            .join("projects")
            .join(id)
            .join("executive")
            .join("runs"),
        _ => return None,
    };
    // A link in the state directory that leads out of it is not followed.
    let path = dir.join(file).canonicalize().ok()?;
    path.starts_with(state.canonicalize().ok()?).then_some(path)
}

/// A part of a run's log, for `where=…&id=…&file=…[&from=…]`.
fn run(state: &Path, query: &str) -> Option<String> {
    let field = |name: &str| {
        query
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value)
    };
    let path = log_path(state, field("where")?, field("id")?, field("file")?)?;
    let from = match field("from") {
        Some(from) => Some(from.parse::<u64>().ok()?),
        None => None,
    };
    let part = runlog::read(&path, from).ok()?;
    Some(
        json!({
            "size": part.size,
            "from": part.from,
            "next": part.next,
            "events": part.events,
        })
        .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_log_inside_the_state_directory_is_found() {
        let state = std::env::temp_dir().join(format!("factory-dash-paths-{}", std::process::id()));
        let runs = state.join("units").join("toy-1-17").join("runs");
        std::fs::create_dir_all(&runs).unwrap();
        std::fs::write(runs.join("001-build.jsonl"), "").unwrap();
        std::fs::write(state.join("accounts.json"), "{}").unwrap();
        let outside = state.with_extension("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.jsonl"), "").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.join("secret.jsonl"), runs.join("002-link.jsonl"))
            .unwrap();

        assert!(log_path(&state, "unit", "toy-1-17", "001-build.jsonl").is_some());
        for (place, id, file) in [
            ("unit", "toy-1-17", "../unit.json"),
            ("unit", "toy-1-17", "..%2f..%2faccounts.json"),
            ("unit", "..", "001-build.jsonl"),
            ("unit", "toy-1-17/runs/..", "001-build.jsonl"),
            ("unit", "toy-1-17", "001-build.json"),
            ("unit", "toy-1-17", "missing.jsonl"),
            ("unit", "toy-1-17", ".hidden.jsonl"),
            ("units", "toy-1-17", "001-build.jsonl"),
            ("goal", "toy-1-17", "001-build.jsonl"),
            ("unit", "", "001-build.jsonl"),
        ] {
            assert_eq!(
                log_path(&state, place, id, file),
                None,
                "{place} {id} {file}"
            );
        }
        // A link that leads out of the state directory is not followed.
        #[cfg(unix)]
        assert_eq!(log_path(&state, "unit", "toy-1-17", "002-link.jsonl"), None);
        std::fs::remove_dir_all(&state).unwrap();
        std::fs::remove_dir_all(&outside).unwrap();
    }
}
