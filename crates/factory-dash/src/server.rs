//! The page and what it asks for, over HTTP.
//!
//! Only reading is offered: the page, the picture of the factory, and
//! parts of runs' logs. A connection from an address outside the allowed
//! networks is closed before a byte of it is read.

use std::collections::hash_map::DefaultHasher;
use std::convert::Infallible;
use std::hash::{Hash, Hasher};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::header::{self, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

use crate::net::{Net, host_is_ours, may_connect};
use crate::records::{Links, Reader};
use crate::runlog;

const PAGE: &str = include_str!("../assets/index.html");
const STYLE: &str = include_str!("../assets/app.css");
const SCRIPT: &str = include_str!("../assets/app.js");

/// How often the records are read while somebody is looking.
const LOOK_EVERY: Duration = Duration::from_secs(2);
/// How long after the last question the records go on being read.
const LOOKED_LATELY_SECS: u64 = 30;
/// How many connections are answered at once.
const MOST_CONNECTIONS: usize = 64;

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
}

/// The picture as it was last read.
#[derive(Default)]
struct Picture {
    body: Bytes,
    tag: String,
}

struct Shared {
    settings: Settings,
    reader: Mutex<Reader>,
    picture: RwLock<Arc<Picture>>,
    asked_at: AtomicU64,
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
        let fresh = Arc::new(Picture {
            tag: format!("\"{:016x}\"", hasher.finish()),
            body: Bytes::from(body),
        });
        if let Ok(mut held) = self.picture.write() {
            *held = fresh;
        }
    }

    fn picture(&self) -> Arc<Picture> {
        self.picture
            .read()
            .map(|held| held.clone())
            .unwrap_or_default()
    }
}

/// Serve the page on a listener for as long as the process lives.
pub async fn serve(listener: TcpListener, settings: Settings) -> std::io::Result<()> {
    let reader = Reader::new(settings.state.clone(), settings.links.clone());
    let shared = Arc::new(Shared {
        settings,
        reader: Mutex::new(reader),
        picture: RwLock::new(Arc::default()),
        asked_at: AtomicU64::new(0),
    });
    // The first picture is there before the first question.
    let first = shared.clone();
    let _ = tokio::task::spawn_blocking(move || first.look()).await;
    // The records are read again and again, but only while somebody
    // has asked lately: an unwatched page costs nothing.
    let looker = shared.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(LOOK_EVERY);
        loop {
            tick.tick().await;
            if looker.asked_at.load(Ordering::Relaxed) + LOOKED_LATELY_SECS < now() {
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
                async move { Ok::<_, Infallible>(answer(&shared, request).await) }
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

type Answer = Response<Full<Bytes>>;

fn reply(status: StatusCode, kind: &'static str, body: impl Into<Bytes>) -> Answer {
    let mut answer = Response::new(Full::new(body.into()));
    *answer.status_mut() = status;
    let headers = answer.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(kind));
    // The page loads nothing but its own files and talks to nobody but
    // this server, and no other page may show it in a frame.
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; \
             img-src 'self' data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
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

async fn answer(shared: &Arc<Shared>, request: Request<Incoming>) -> Answer {
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
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return plain(StatusCode::METHOD_NOT_ALLOWED, "Only reading is offered.\n");
    }
    match request.uri().path() {
        "/" => reply(StatusCode::OK, "text/html; charset=utf-8", PAGE),
        "/app.css" => reply(StatusCode::OK, "text/css; charset=utf-8", STYLE),
        "/app.js" => reply(StatusCode::OK, "text/javascript; charset=utf-8", SCRIPT),
        "/healthz" => plain(StatusCode::OK, "ok\n"),
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
