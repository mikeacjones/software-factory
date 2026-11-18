//! The page as a browser meets it: who is answered, under what name, and
//! what the answers hold.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use factory_dash::net::Net;
use factory_dash::records::Links;
use factory_dash::server::{Settings, serve};

/// A factory's state directory with one goal, one unit running in it and
/// one account, as the factory would leave them.
fn state(name: &str) -> PathBuf {
    let state = std::env::temp_dir().join(format!("factory-dash-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&state);
    let put = |path: &str, text: &str| {
        let path = state.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    put(
        "accounts.json",
        r#"{"one":{"five_hour":{"utilization":0.4,"resets_at":99999999999,"status":"allowed"},"seven_day":{"utilization":0.1,"resets_at":99999999999,"status":"allowed"},"models":{"fable":{"utilization":0.6,"resets_at":99999999999,"status":"allowed"},"opus":{"utilization":0.9,"resets_at":2,"status":"allowed"}},"blocked":{},"updated_at":1,"usage_checked_at":7,"concern":null,"identity":"not for the page"}}"#,
    );
    put(
        "projects/toy/executive.json",
        r#"{"project":"toy","threads":{"1.1":{"waiting":true}},"wakes":2,"usage":{"cost_usd":1.5},"memos":[],"pending":[],"desk_retry_at":0}"#,
    );
    put(
        "goals/toy-g1/goal.json",
        r#"{"id":"toy-g1","project":"toy","phase":"running","charter":{"objective":"Ship the feature."},"wakes":3,
            "units":[{"bead":"toy-1","unit":"toy-1-17","state":"running","ship":false,"subgoal":"feature"}],
            "subgoals":[{"id":"feature","title":"The feature","state":"open","planned":["toy-1","toy-2"],"added":[],"foreman":{"wakes":1,"inbox":[]}}],
            "usage":{"cost_usd":2.0},"started_at":5}"#,
    );
    put(
        "units/toy-1-17/unit.json",
        r#"{"id":"toy-1-17","project":"toy","bead":"toy-1","title":"Add the feature","flow":"toy-flow","status":"running",
            "position":{"step":"build","rounds":{"build":1}},"usage":{"build":{"usage":{"cost_usd":0.25}}}}"#,
    );
    put(
        "units/toy-1-17/flow.toml",
        "id = \"toy-flow\"\nstart = \"build\"\n[[step]]\nid = \"build\"\nkind = \"agent\"\nrole = \"builder\"\n[[step]]\nid = \"test\"\nkind = \"command\"\n",
    );
    put(
        "units/toy-1-17/events.jsonl",
        &format!(
            "{}\n",
            r#"{"time_unix":10,"kind":"run.started","step":"build","data":{"account":"one","role":"builder","model":"claude-opus-5-5","effort":"high","run":1,"log":"/elsewhere/units/toy-1-17/runs/001-build.jsonl"}}"#
        ),
    );
    put(
        "units/toy-1-17/runs/001-build.jsonl",
        &format!(
            "{}\n{}\n",
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo test"}}]}}"#,
            r#"{"type":"rate_limit_event","rate_limit_info":{"unifiedWindows":{"five_hour":{"utilization":0.45,"resetsAt":99999999999},"seven_day":{"utilization":0.1,"resetsAt":99999999999}}}}"#
        ),
    );
    state
}

/// Serve a state directory on a port of this machine, to the given networks.
async fn open(state: &Path, allow: &[&str]) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let settings = Settings {
        state: state.to_path_buf(),
        allow: allow.iter().map(|net| Net::parse(net).unwrap()).collect(),
        names: vec!["factory.test".to_string()],
        links: Links {
            temporal_ui: Some("http://engine.test:8080".to_string()),
            namespace: "factory".to_string(),
        },
    };
    tokio::spawn(serve(listener, settings));
    address
}

/// Ask for a path as a browser would, and give back all that came.
async fn ask(address: SocketAddr, method: &str, path: &str, host: &str) -> String {
    let (method, path, host) = (method.to_string(), path.to_string(), host.to_string());
    tokio::task::spawn_blocking(move || {
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let _ = write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
        );
        let mut answer = String::new();
        let _ = stream.read_to_string(&mut answer);
        answer
    })
    .await
    .unwrap()
}

fn body(answer: &str) -> serde_json::Value {
    let (_, body) = answer.split_once("\r\n\r\n").unwrap();
    serde_json::from_str(body).unwrap_or_else(|error| panic!("{error}: {answer}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn the_picture_shows_the_tiers_the_pipeline_the_account_and_the_run() {
    let state = state("picture");
    let address = open(&state, &["127.0.0.0/8"]).await;
    let host = address.to_string();
    let answer = ask(address, "GET", "/api/snapshot", &host).await;
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    assert!(
        answer.contains("content-security-policy: default-src 'none'"),
        "{answer}"
    );
    let picture = body(&answer);

    // The executive, the goal, its sub-goal and both beads, one of them
    // at a step of its pipeline with an agent in it.
    let project = &picture["projects"][0];
    assert_eq!(project["id"], "toy");
    assert_eq!(project["executive"]["waiting"], 1);
    assert_eq!(
        project["workflow"],
        "http://engine.test:8080/namespaces/factory/workflows/project-toy"
    );
    let goal = &project["goals"][0];
    assert_eq!(goal["objective"], "Ship the feature.");
    let beads = goal["subgoals"][0]["beads"].as_array().unwrap();
    assert_eq!(beads.len(), 2);
    assert_eq!(beads[0]["title"], "Add the feature");
    assert_eq!(beads[0]["state"], "running");
    assert_eq!(beads[0]["cost"], 0.25);
    let steps: Vec<(&str, u64)> = beads[0]["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| {
            (
                step["id"].as_str().unwrap(),
                step["rounds"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(steps, [("build", 1), ("test", 0)]);
    assert_eq!(beads[0]["run"]["role"], "builder");
    assert_eq!(beads[0]["run"]["account"], "one");
    assert_eq!(beads[0]["run"]["file"], "001-build.jsonl");
    assert_eq!(
        (&beads[1]["bead"], &beads[1]["state"]),
        (&"toy-2".into(), &"planned".into())
    );

    // The account: its windows, with what the run going has said of them
    // since the factory last wrote them down, and the run that is on it.
    let account = &picture["accounts"][0];
    assert_eq!(account["name"], "one");
    assert_eq!(account["five_hour"]["utilization"], 0.45);
    assert_eq!(account["seven_day"]["utilization"], 0.1);
    // A model's own week, apart from the account's two windows, as of when
    // the usage endpoint was last asked; one that has reset since is empty.
    assert_eq!(account["models"]["fable"]["utilization"], 0.6);
    assert_eq!(account["models"]["fable"]["resets_at"], 99999999999u64);
    assert_eq!(account["models"]["fable"]["read_at"], 7);
    assert_eq!(account["models"]["opus"]["utilization"], 0.0);
    assert!(account["models"]["opus"]["resets_at"].is_null());
    assert_eq!(account["runs"][0]["id"], "toy-1-17");
    assert_eq!(picture["runs"].as_array().unwrap().len(), 1);
    // Whose account it is stays in the factory's record.
    assert!(!answer.contains("not for the page"), "{answer}");

    // What has not changed is not sent again.
    let tag = answer
        .lines()
        .find_map(|line| line.strip_prefix("etag: "))
        .unwrap()
        .to_string();
    let again = tokio::task::spawn_blocking(move || {
        let mut stream = TcpStream::connect(address).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        write!(
            stream,
            "GET /api/snapshot HTTP/1.1\r\nHost: {address}\r\nIf-None-Match: {tag}\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut answer = String::new();
        let _ = stream.read_to_string(&mut answer);
        answer
    })
    .await
    .unwrap();
    assert!(again.starts_with("HTTP/1.1 304"), "{again}");

    // The run can be read, from where the reader left off.
    let run = body(
        &ask(
            address,
            "GET",
            "/api/run?where=unit&id=toy-1-17&file=001-build.jsonl&from=0",
            &host,
        )
        .await,
    );
    assert_eq!(run["events"][0]["name"], "Bash");
    assert_eq!(run["events"][0]["text"], "cargo test");
    assert_eq!(run["next"], run["size"]);
    std::fs::remove_dir_all(&state).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn only_reading_is_offered_and_only_under_a_name_that_is_ours() {
    let state = state("refusals");
    let address = open(&state, &["127.0.0.0/8"]).await;
    let host = address.to_string();
    for (method, path, name, status) in [
        ("GET", "/", host.as_str(), "200"),
        ("GET", "/app.js", "factory.test", "200"),
        ("HEAD", "/app.css", host.as_str(), "200"),
        // Another site's name pointed at this machine is not answered.
        ("GET", "/api/snapshot", "evil.example", "421"),
        ("POST", "/api/snapshot", host.as_str(), "405"),
        ("DELETE", "/", host.as_str(), "405"),
        (
            "GET",
            "/api/run?where=unit&id=toy-1-17&file=../unit.json",
            host.as_str(),
            "404",
        ),
        (
            "GET",
            "/api/run?where=unit&id=..&file=accounts.json",
            host.as_str(),
            "404",
        ),
        (
            "GET",
            "/api/run?where=unit&id=toy-1-17&file=001-build.jsonl&from=x",
            host.as_str(),
            "404",
        ),
        ("GET", "/accounts.json", host.as_str(), "404"),
        ("GET", "/../accounts.json", host.as_str(), "404"),
    ] {
        let answer = ask(address, method, path, name).await;
        assert!(
            answer.starts_with(&format!("HTTP/1.1 {status}")),
            "{method} {path} as {name}: {answer}"
        );
        assert!(
            !answer.contains("utilization") || status == "200",
            "{answer}"
        );
    }
    std::fs::remove_dir_all(&state).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_connection_from_outside_the_allowed_networks_gets_nothing() {
    let state = state("outside");
    // This machine's own address is outside the network that is allowed.
    let address = open(&state, &["192.168.101.0/24"]).await;
    let answer = ask(address, "GET", "/api/snapshot", &address.to_string()).await;
    assert_eq!(answer, "", "an address that is not allowed was answered");
    std::fs::remove_dir_all(&state).unwrap();
}

/// The dashboard is an observer: nothing in it can write, remove or move
/// a file, start a program, or open a connection of its own.
#[test]
fn nothing_in_the_dashboard_writes_or_reaches_out() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut read = 0;
    for entry in std::fs::read_dir(&src).unwrap() {
        let path = entry.unwrap().path();
        let text = std::fs::read_to_string(&path).unwrap();
        // The tests in a file may write what they then read.
        let code = text.split("#[cfg(test)]").next().unwrap();
        for forbidden in [
            "fs::write",
            "File::create",
            "OpenOptions",
            "create_dir",
            "remove_file",
            "remove_dir",
            "fs::rename",
            "fs::copy",
            "set_permissions",
            "Command::new",
            "TcpStream",
            "UdpSocket",
            "::connect(",
            ".connect(",
        ] {
            assert!(
                !code.contains(forbidden),
                "{} uses `{forbidden}`: the dashboard only reads",
                path.display()
            );
        }
        read += 1;
    }
    assert!(read >= 5, "the sources were not found in {}", src.display());
}
