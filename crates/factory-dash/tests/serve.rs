//! The page as a browser meets it: who is answered, under what name, and
//! what the answers hold.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use factory_dash::act::Engine;
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
    open_acting(state, allow, None, &[]).await
}

/// The same, with an engine the owner's acts are handed to, by those of
/// the networks `act_allow`.
async fn open_acting(
    state: &Path,
    allow: &[&str],
    engine: Option<Engine>,
    act_allow: &[&str],
) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let settings = Settings {
        state: state.to_path_buf(),
        allow: allow.iter().map(|net| Net::parse(net).unwrap()).collect(),
        names: vec!["factory.test".to_string()],
        links: Links {
            temporal_ui: Some("http://engine.test:8080".to_string()),
            namespace: "factory".to_string(),
            slack: None,
        },
        engine,
        act_allow: act_allow
            .iter()
            .map(|net| Net::parse(net).unwrap())
            .collect(),
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

/// Send an act as the page does, with whatever headers are given.
async fn post(address: SocketAddr, headers: &str, body: &str) -> String {
    let (headers, body) = (headers.to_string(), body.to_string());
    tokio::task::spawn_blocking(move || {
        let mut stream = TcpStream::connect(address).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        let _ = write!(
            stream,
            "POST /api/act HTTP/1.1\r\nHost: {address}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let mut answer = String::new();
        let _ = stream.read_to_string(&mut answer);
        answer
    })
    .await
    .unwrap()
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn the_owner_acts_from_the_page_through_the_engine_and_nobody_else_does() {
    use std::os::unix::fs::PermissionsExt;
    let state = state("acts");
    // An engine that says what it was asked to do.
    let heard = state.join("heard");
    let engine = state.join("factory-engine");
    std::fs::write(
        &engine,
        format!(
            "#!/bin/sh\nprintf '%s|' \"$@\" > {}\necho handed\n",
            heard.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&engine, std::fs::Permissions::from_mode(0o755)).unwrap();
    let with = Engine {
        command: engine.clone(),
        config: state.join("config"),
    };
    let address = open_acting(
        &state,
        &["127.0.0.0/8"],
        Some(with.clone()),
        &["127.0.0.0/8"],
    )
    .await;
    let page = "X-Factory-Owner: 1\r\nContent-Type: application/json\r\n";
    let press = r#"{"project":"toy","act":"press","action_id":"goal_start","value":"toy-g1"}"#;

    // Without the page's own header, or from another site, nothing is done.
    for headers in [
        "Content-Type: application/json\r\n".to_string(),
        "X-Factory-Owner: 1\r\nContent-Type: text/plain\r\n".to_string(),
        format!("{page}Origin: http://evil.example\r\n"),
    ] {
        let answer = post(address, &headers, press).await;
        assert!(answer.starts_with("HTTP/1.1 403"), "{headers}: {answer}");
    }
    assert!(!heard.exists());
    // A button the factory never posts is not pressed.
    let answer = post(
        address,
        page,
        r#"{"project":"toy","act":"press","action_id":"rm","value":"x"}"#,
    )
    .await;
    assert!(answer.starts_with("HTTP/1.1 400"), "{answer}");
    assert!(!heard.exists());

    let answer = post(
        address,
        &format!("{page}Origin: http://{address}\r\n"),
        press,
    )
    .await;
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    assert_eq!(body(&answer)["said"], "handed");
    assert_eq!(
        std::fs::read_to_string(&heard).unwrap(),
        format!(
            "--config|{}|owner|toy|press|goal_start|toy-g1|",
            state.join("config").display()
        )
    );
    // A reply never reaches the engine as an option.
    let say = r#"{"project":"toy","act":"say","channel":"C1","thread":"1.1","text":"--wait now"}"#;
    assert!(post(address, page, say).await.starts_with("HTTP/1.1 200"));
    assert!(
        std::fs::read_to_string(&heard)
            .unwrap()
            .ends_with("|say|C1|1.1|--|--wait now|")
    );
    let can = body(&ask(address, "GET", "/api/can-act", &address.to_string()).await);
    assert_eq!(can["act"], true);

    // Those who may read but not act are told so, and nothing is done.
    std::fs::remove_file(&heard).unwrap();
    let reader = open_acting(&state, &["127.0.0.0/8"], Some(with), &["192.168.101.0/24"]).await;
    let answer = post(reader, page, press).await;
    assert!(answer.starts_with("HTTP/1.1 403"), "{answer}");
    let can = body(&ask(reader, "GET", "/api/can-act", &reader.to_string()).await);
    assert_eq!(can["act"], false);
    assert!(!heard.exists());
    std::fs::remove_dir_all(&state).unwrap();
}

/// A definition the executive proposed is in the inbox from the
/// executive's record, and its buttons are handed to the engine as the
/// press in Slack is: the action and the value the Slack button carries.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_proposed_definition_is_read_from_the_executive_and_its_buttons_reach_the_engine() {
    use std::os::unix::fs::PermissionsExt;
    let state = state("definition");
    std::fs::write(
        state.join("projects/toy/executive.json"),
        r#"{"project":"toy","threads":{"500.0":{"channel":"C1","goal":null,"said":[],"proposals":[
            {"id":"wide","text":"id = \"wide\"\n","reason":"Nothing runs wide.","change":false,"state":"waiting",
             "message":"x\n```\nid = \"wide\"\n```\nApprove","ts":"501.0"}]}},
            "wakes":1,"usage":{"cost_usd":0.5},"memos":[],"pending":[]}"#,
    )
    .unwrap();
    let heard = state.join("heard");
    let engine = state.join("factory-engine");
    std::fs::write(
        &engine,
        format!(
            "#!/bin/sh\nprintf '%s|' \"$@\" > {}\necho handed\n",
            heard.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&engine, std::fs::Permissions::from_mode(0o755)).unwrap();
    let with = Engine {
        command: engine,
        config: state.join("config"),
    };
    let address = open_acting(&state, &["127.0.0.0/8"], Some(with), &["127.0.0.0/8"]).await;
    let picture = body(&ask(address, "GET", "/api/snapshot", &address.to_string()).await);
    let item = picture["inbox"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["kind"] == "definition")
        .unwrap_or_else(|| panic!("{}", picture["inbox"]));
    assert_eq!(item["title"], "New goal definition `wide`");
    assert_eq!(item["sections"][1]["text"], "```\nid = \"wide\"\n```");
    let page = "X-Factory-Owner: 1\r\nContent-Type: application/json\r\n";
    for action in item["actions"].as_array().unwrap() {
        let press = serde_json::json!({"project": "toy", "act": "press",
            "action_id": action["action_id"], "value": action["value"]});
        let answer = post(address, page, &press.to_string()).await;
        assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
        assert_eq!(
            std::fs::read_to_string(&heard).unwrap(),
            format!(
                "--config|{}|owner|toy|press|{}|500.0/1|",
                state.join("config").display(),
                action["action_id"].as_str().unwrap()
            )
        );
    }
    std::fs::remove_dir_all(&state).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn what_waits_on_the_owner_and_the_conversation_are_read_from_the_records() {
    let state = state("inbox");
    std::fs::write(
        state.join("goals/toy-g1/goal.json"),
        r#"{"id":"toy-g1","project":"toy","phase":"proposed_complete","channel":"C1","thread_ts":"5.5",
            "charter":{"objective":"Ship the feature.","stop_when":"It ships."},"evidence":"PR #1 merged","units":[],"subgoals":[],"started_at":5}"#,
    )
    .unwrap();
    std::fs::write(
        state.join("goals/toy-g1/events.jsonl"),
        [
            r#"{"time_unix":6,"kind":"owner.message","data":{"text":"Go"}}"#,
            r#"{"time_unix":7,"kind":"slack.prompt","data":{"text":"Done, accept?"}}"#,
        ]
        .join("\n"),
    )
    .unwrap();
    let address = open(&state, &["127.0.0.0/8"]).await;
    let host = address.to_string();
    let picture = body(&ask(address, "GET", "/api/snapshot", &host).await);
    let item = &picture["inbox"][0];
    assert_eq!(item["kind"], "completion");
    assert_eq!(item["actions"][0]["action_id"], "goal_accept");
    assert_eq!(
        item["reply"],
        serde_json::json!({"channel": "C1", "ts": "5.5"})
    );
    assert_eq!(picture["projects"][0]["pulse"]["waiting"], 1);
    assert_eq!(picture["projects"][0]["pulse"]["state"], "waiting");

    let thread = body(&ask(address, "GET", "/api/thread?where=goal&id=toy-g1", &host).await);
    assert_eq!(thread["lines"][0]["who"], "owner");
    assert_eq!(thread["lines"][1]["text"], "Done, accept?");
    let refused = ask(address, "GET", "/api/thread?where=goal&id=..", &host).await;
    assert!(refused.starts_with("HTTP/1.1 404"), "{refused}");
    std::fs::remove_dir_all(&state).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pipeline_a_director_started_in_no_sub_goal_is_drawn_under_its_goal() {
    let state = state("own-beads");
    std::fs::write(
        state.join("goals/toy-g1/goal.json"),
        r#"{"id":"toy-g1","project":"toy","phase":"running","charter":{"objective":"Audit it."},"wakes":1,
            "units":[{"bead":"toy-1","unit":"toy-1-17","state":"running","ship":false,"subgoal":null}],
            "subgoals":[],"usage":{"cost_usd":1.0},"started_at":5}"#,
    )
    .unwrap();
    let address = open(&state, &["127.0.0.0/8"]).await;
    let picture = body(&ask(address, "GET", "/api/snapshot", &address.to_string()).await);
    let project = &picture["projects"][0];
    let goal = &project["goals"][0];
    assert_eq!(goal["beads"][0]["unit"], "toy-1-17", "{goal}");
    assert_eq!(goal["beads"][0]["state"], "running");
    // It is the goal's, not a pipeline run by hand.
    assert_eq!(project["loose"], serde_json::json!([]), "{project}");
    assert_eq!(project["pulse"]["pipelines"], 1);
    std::fs::remove_dir_all(&state).unwrap();
}

/// Read server-sent events from a stream until `want` of them have come,
/// as (name, data) pairs.
fn events(
    stream: &mut TcpStream,
    buffer: &mut String,
    want: usize,
) -> Vec<(String, serde_json::Value)> {
    let mut found = Vec::new();
    let mut chunk = [0u8; 65536];
    loop {
        while let Some(end) = buffer.find("\n\n") {
            let block: String = buffer.drain(..end + 2).collect();
            let name = block.lines().find_map(|line| line.strip_prefix("event: "));
            let data = block.lines().find_map(|line| line.strip_prefix("data: "));
            if let (Some(name), Some(data)) = (name, data) {
                found.push((name.to_string(), serde_json::from_str(data).unwrap()));
                if found.len() == want {
                    return found;
                }
            }
        }
        let read = stream.read(&mut chunk).unwrap();
        assert!(read > 0, "the stream ended: {buffer}");
        buffer.push_str(std::str::from_utf8(&chunk[..read]).unwrap());
        // The answer's head comes first, once.
        if buffer.starts_with("HTTP/") {
            let Some((head, rest)) = buffer.split_once("\r\n\r\n") else {
                continue;
            };
            assert!(
                head.starts_with("HTTP/1.0 200") || head.starts_with("HTTP/1.1 200"),
                "{head}"
            );
            assert!(head.contains("content-type: text/event-stream"), "{head}");
            *buffer = rest.to_string();
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_stream_sends_the_picture_once_and_then_only_what_changed() {
    let state = state("stream");
    let address = open(&state, &["127.0.0.0/8"]).await;
    let goal = state.join("goals/toy-g1/goal.json");
    let (first, patched, whole) = tokio::task::spawn_blocking(move || {
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        // HTTP/1.0, so that the body comes as it is, not in chunks.
        write!(
            stream,
            "GET /api/stream HTTP/1.0\r\nHost: {address}\r\n\r\n"
        )
        .unwrap();
        let mut buffer = String::new();
        let first = events(&mut stream, &mut buffer, 1).remove(0);

        // A record changes: only that change comes, and nothing else.
        let text = std::fs::read_to_string(&goal).unwrap();
        std::fs::write(
            &goal,
            text.replace("Ship the feature.", "Ship the whole feature."),
        )
        .unwrap();
        let patched = events(&mut stream, &mut buffer, 1).remove(0);
        let whole = {
            let mut asked = TcpStream::connect(address).unwrap();
            write!(
                asked,
                "GET /api/snapshot HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            let mut answer = String::new();
            asked.read_to_string(&mut answer).unwrap();
            body(&answer)
        };
        (first, patched, whole)
    })
    .await
    .unwrap();

    assert_eq!(first.0, "picture");
    let picture = first.1["picture"].clone();
    assert_eq!(
        picture["projects"][0]["goals"][0]["objective"],
        "Ship the feature."
    );
    assert_eq!(patched.0, "patch");
    assert_eq!(patched.1["from"], first.1["tag"]);
    let changes = patched.1["changes"].as_array().unwrap();
    assert!(
        changes.contains(&serde_json::json!([
            "s",
            ["projects", 0, "goals", 0, "objective"],
            "Ship the whole feature."
        ])),
        "{changes:?}"
    );
    // Applied in order, the changes give the picture the server now holds.
    assert_eq!(factory_dash::patch::apply(picture, changes), Some(whole));
    std::fs::remove_dir_all(&state).unwrap();
}

/// The dashboard is an observer: nothing in it can write, remove or move
/// a file or open a connection of its own, and the one program it may
/// start is the factory's engine, to hand on the owner's acts.
#[test]
fn nothing_in_the_dashboard_writes_or_reaches_out() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut read = 0;
    for entry in std::fs::read_dir(&src).unwrap() {
        let path = entry.unwrap().path();
        let text = std::fs::read_to_string(&path).unwrap();
        // The tests in a file may write what they then read.
        let code = text.split("#[cfg(test)]").next().unwrap();
        let starts = code.matches("Command::new").count();
        if path.ends_with("act.rs") {
            assert_eq!(starts, 1, "act.rs starts one program, the engine");
        } else {
            assert_eq!(starts, 0, "{} starts a program", path.display());
        }
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
            "std::process::Command",
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

/// The page the program embeds is built from the page's sources as they
/// are: a change to `web` without `npm run build` fails here, with the
/// same hash of the sources the build writes beside what it built.
#[test]
fn the_embedded_page_is_built_from_its_sources() {
    fn files(root: &Path, at: &str, into: &mut Vec<String>) {
        let path = root.join(at);
        if path.is_dir() {
            for entry in std::fs::read_dir(&path).unwrap() {
                let name = entry.unwrap().file_name().to_string_lossy().into_owned();
                files(root, &format!("{at}/{name}"), into);
            }
        } else {
            into.push(at.to_string());
        }
    }
    let web = Path::new(env!("CARGO_MANIFEST_DIR")).join("web");
    let mut all = Vec::new();
    for at in [
        "index.html",
        "package.json",
        "package-lock.json",
        "svelte.config.js",
        "vite.config.ts",
        "tsconfig.json",
        "src",
    ] {
        files(&web, at, &mut all);
    }
    all.sort();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        for byte in bytes {
            hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3);
        }
    };
    for name in &all {
        eat(name.as_bytes());
        eat(&[0]);
        eat(&std::fs::read(web.join(name)).unwrap());
        eat(&[0]);
    }
    let built = std::fs::read_to_string(web.join("../assets/source.txt")).unwrap();
    assert_eq!(
        built.trim(),
        format!("{hash:016x}"),
        "the page was not rebuilt after its sources changed: run `npm ci && npm run build` in crates/factory-dash/web"
    );
}
