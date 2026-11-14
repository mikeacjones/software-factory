//! The run's Slack thread against a local stand-in for Slack's Web API: the
//! channel is found or created, the owner invited, and the owner's replies are
//! read once each and from nobody else.

// Test code: a failed setup step should stop the test where it happens.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::path::PathBuf;

use common::FakeSlack;
use factory_core::{EventLog, ProjectId, UnitId};
use factory_driver::thread::Thread;
use factory_slack::Slack;

fn log(name: &str) -> (EventLog, PathBuf) {
    let path =
        std::env::temp_dir().join(format!("factory-slack-{name}-{}.jsonl", std::process::id()));
    let _ = std::fs::remove_file(&path);
    (
        EventLog::new(&path, ProjectId::new("toy"), UnitId::new("u")),
        path,
    )
}

fn open(fake: &FakeSlack, name: &str) -> (Thread, EventLog, PathBuf) {
    let (log, path) = log(name);
    let thread = Thread::open(
        Slack::with_base_url("xoxb-test", &fake.url),
        "toy",
        vec!["OWNER".to_string()],
        "*toy-1* · Add the feature".to_string(),
        log.clone(),
    );
    (thread, log, path)
}

#[test]
fn a_missing_channel_is_created_and_the_owner_invited() {
    let fake = FakeSlack::start(false);
    let (thread, _, path) = open(&fake, "create");
    std::fs::remove_file(path).unwrap();
    assert!(thread.live());
    assert_eq!(thread.state().unwrap().channel, "C0NEW");
    assert_eq!(
        fake.methods(),
        [
            "conversations.list",
            "conversations.create",
            "conversations.join",
            "conversations.invite",
            "chat.postMessage"
        ]
    );
    assert!(fake.requests_to("conversations.create")[0].contains(r#""name":"toy""#));
    assert!(fake.requests_to("conversations.invite")[0].contains(r#""users":"OWNER""#));
    // The run's thread starts with the header.
    assert!(fake.requests_to("chat.postMessage")[0].contains("Add the feature"));
}

#[test]
fn an_existing_channel_is_used_and_an_owner_already_there_is_fine() {
    let fake = FakeSlack::start(true);
    let (thread, _, path) = open(&fake, "existing");
    std::fs::remove_file(path).unwrap();
    assert!(thread.live());
    assert_eq!(thread.state().unwrap().channel, "C0TOY");
    assert!(!fake.methods().contains(&"conversations.create".to_string()));
}

#[test]
fn owner_replies_are_heard_once_and_strangers_never() {
    let fake = FakeSlack::start(true);
    let (mut thread, log, path) = open(&fake, "poll");

    assert_eq!(
        thread.poll(),
        ["use the short name", "and keep the old flag"]
    );
    // Each owner message was acknowledged with a reaction; nothing else was.
    let reactions = fake.requests_to("reactions.add");
    assert_eq!(reactions.len(), 2);
    assert!(reactions[0].contains(r#""timestamp":"101.0""#));
    assert!(reactions[1].contains(r#""timestamp":"104.0""#));
    assert_eq!(thread.state().unwrap().last_seen, "104.0");

    // Asked again, there is nothing new: the second request starts after the
    // newest message already read.
    assert!(thread.poll().is_empty());
    assert!(fake.requests_to("conversations.replies")[1].contains("oldest=104.0"));

    // What the owner said is in the audit log; what the stranger said is not.
    let heard: Vec<String> = log
        .read()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "owner.message")
        .map(|event| event.data["text"].as_str().unwrap().to_string())
        .collect();
    std::fs::remove_file(path).unwrap();
    assert_eq!(heard, ["use the short name", "and keep the old flag"]);
}

#[test]
fn updates_go_to_the_thread_and_status_replaces_the_root() {
    let fake = FakeSlack::start(true);
    let (mut thread, _, path) = open(&fake, "say");
    thread.say("*build* done");
    thread.status("running tests");
    std::fs::remove_file(path).unwrap();
    let posts = fake.requests_to("chat.postMessage");
    assert!(posts[1].contains(r#""thread_ts":"100.0""#));
    assert!(posts[1].contains("*build* done"));
    let update = &fake.requests_to("chat.update")[0];
    assert!(update.contains(r#""ts":"100.0""#));
    assert!(update.contains("Add the feature") && update.contains("running tests"));
}
