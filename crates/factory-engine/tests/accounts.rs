//! The accounts workflow against a real Temporal server, with stand-in
//! accounts: a picture the test writes, and tokens that exist only in the
//! directory the scripted step runs from.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use common::Rig;
use factory_claude::Window;
use factory_claude::accounts::AccountState;
use factory_engine::accounts::{ACCOUNTS_ID, Accounts, Picture};
use factory_engine::engine::Pipeline;
use temporalio_client::{
    WorkflowFetchHistoryOptions, WorkflowGetResultOptions, WorkflowQueryOptions,
    WorkflowSignalOptions, WorkflowStartOptions,
};

/// Two agent steps on a model and a command between them.
const BUILD_AND_REVIEW: &str = r#"
id = "toy"
start = "build"

[[step]]
id = "build"
kind = "agent"
role = "builder"
model = "m"
effort = "medium"
writes = ["code"]
on.done = "test"

[[step]]
id = "test"
kind = "command"
command = "test"
on.pass = "review"

[[step]]
id = "review"
kind = "agent"
role = "reviewer"
model = "m"
effort = "medium"
workspace = "scratch"
on.approve = "end"
"#;

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// An account whose week ends in `week_secs`, at a limit for `limited_secs`.
fn account(week_secs: u64, limited_secs: u64) -> AccountState {
    let mut state = AccountState {
        seven_day: Some(Window {
            utilization: 0.4,
            resets_at: now() + week_secs,
            status: "allowed".to_string(),
        }),
        updated_at: now(),
        ..AccountState::default()
    };
    if limited_secs > 0 {
        state.blocked.insert("*".to_string(), now() + limited_secs);
    }
    state
}

fn picture(accounts: &[(&str, AccountState)]) -> Picture {
    Picture {
        order: accounts.iter().map(|(id, _)| id.to_string()).collect(),
        states: accounts
            .iter()
            .map(|(id, state)| (id.to_string(), state.clone()))
            .collect(),
        grace_secs: 0,
    }
}

/// The accounts each agent step of a unit ran under, in order.
fn ran_under(rig: &Rig, unit: &str) -> Vec<String> {
    rig.calls()
        .iter()
        .filter(|call| call.starts_with(&format!("{unit} agent ")))
        .map(|call| {
            let step = call.split(' ').nth(2).unwrap_or("?");
            let account = call
                .split(" account=")
                .nth(1)
                .map_or("none", |rest| rest.trim());
            format!("{step}:{account}")
        })
        .collect()
}

fn decode_base64(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut bits, mut have) = (0u32, 0u32);
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => continue,
            _ => return None,
        };
        bits = (bits << 6) | u32::from(value);
        have += 6;
        if have >= 8 {
            have -= 8;
            out.push((bits >> have) as u8);
            bits &= (1 << have) - 1;
        }
    }
    Some(out)
}

/// Every string in a history, as it is written and, where it is encoded
/// data, as what it holds. Payloads are base64 in a history's JSON, so a
/// search of the text alone would find nothing whatever was in them.
fn strings_of(value: &serde_json::Value, into: &mut Vec<String>) {
    match value {
        serde_json::Value::String(text) => {
            into.push(text.clone());
            if let Some(bytes) = decode_base64(text) {
                into.push(String::from_utf8_lossy(&bytes).into_owned());
            }
        }
        serde_json::Value::Array(items) => items.iter().for_each(|item| strings_of(item, into)),
        serde_json::Value::Object(fields) => {
            fields.values().for_each(|field| strings_of(field, into));
        }
        _ => {}
    }
}

/// Everything written in the history of a workflow's latest run.
async fn history_text(rig: &Rig, workflow: &str) -> String {
    let json = rig
        .client()
        .get_workflow_handle::<Pipeline>(workflow.to_string())
        .fetch_history(WorkflowFetchHistoryOptions::default())
        .to_json()
        .await
        .unwrap();
    let mut strings = Vec::new();
    strings_of(&serde_json::from_slice(&json).unwrap(), &mut strings);
    strings.join("\n")
}

async fn wait_until_waiting(rig: &Rig, unit: &str) {
    let handle = rig
        .client()
        .get_workflow_handle::<Pipeline>(unit.to_string());
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let standing = handle
            .query(Pipeline::standing, (), WorkflowQueryOptions::default())
            .await
            .unwrap();
        if standing.limited_secs.is_some() {
            return;
        }
        assert!(Instant::now() < deadline, "it never waited: {standing:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn recheck(rig: &Rig) {
    rig.client()
        .get_workflow_handle::<Accounts>(ACCOUNTS_ID.to_string())
        .signal(Accounts::recheck, (), WorkflowSignalOptions::default())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn accounts_are_handed_out_by_the_rule_and_only_their_names_travel() {
    let Some(mut rig) = Rig::start("accounts-rule").await else {
        return;
    };
    // Three accounts. The third would be spent first, its week ending
    // soonest, but it is at a limit. Of the others the second's week ends
    // sooner, so it is the one to spend.
    rig.picture(&picture(&[
        ("first", account(5 * 86_400, 0)),
        ("second", account(86_400, 0)),
        ("third", account(7200, 3600)),
    ]));
    for id in ["first", "second", "third"] {
        std::fs::write(
            rig.dir.join(format!("state/token-{id}")),
            format!("tok-secret-{id}\n"),
        )
        .unwrap();
    }
    rig.worker("b1", "unversioned");
    rig.accounts(3600, 0).await;
    let client = rig.client();
    let start = |unit: &str| WorkflowStartOptions::new(rig.queue.clone(), unit.to_string()).build();
    let end = client
        .start_workflow(
            Pipeline::run,
            rig.input_under_accounts("toy-1", BUILD_AND_REVIEW),
            start("toy-1"),
        )
        .await
        .unwrap()
        .get_result(WorkflowGetResultOptions::default())
        .await
        .unwrap();
    assert!(end.ended);
    assert_eq!(ran_under(&rig, "toy-1"), ["build:second", "review:second"]);
    // A command step is not run under an account at all.
    assert!(
        rig.calls()
            .iter()
            .any(|call| call.starts_with("toy-1 command test ") && !call.contains("account="))
    );

    // The account in use stops a run at a usage limit. The accounts
    // workflow is told, the step is handed the next account, and the run
    // carries on in the session it had.
    rig.scenario("limits-second");
    let end = client
        .start_workflow(
            Pipeline::run,
            rig.input_under_accounts("toy-2", BUILD_AND_REVIEW),
            start("toy-2"),
        )
        .await
        .unwrap()
        .get_result(WorkflowGetResultOptions::default())
        .await
        .unwrap();
    assert!(end.ended);
    assert_eq!(
        ran_under(&rig, "toy-2"),
        ["build:second", "build:first", "review:first"]
    );
    let resumed = rig
        .calls()
        .into_iter()
        .find(|call| call.starts_with("toy-2 agent build ") && call.contains("account=first"))
        .unwrap();
    assert!(
        resumed.contains("session=sess-toy-2-build-1"),
        "the run under the next account did not resume the session: {resumed}"
    );
    let board = rig.board().await;
    assert!(
        board.picture.states["second"]
            .blocked
            .get("m")
            .is_some_and(|until| *until > now()),
        "the limit a run reported is not on the board: {board:?}"
    );
    assert_eq!(rig.checks(), 1, "a reported limit needs no new check");

    // The steps had their accounts' tokens, read where they ran. No
    // history holds one: what travelled was the account's name.
    let used = std::fs::read_to_string(rig.dir.join("state/tokens-used")).unwrap();
    assert!(used.contains("tok-secret-second") && used.contains("tok-secret-first"));
    for workflow in [ACCOUNTS_ID, "toy-1", "toy-2"] {
        let text = history_text(&rig, workflow).await;
        assert!(
            text.contains("second"),
            "the search of `{workflow}`'s history does not see inside it"
        );
        assert!(
            !text.contains("tok-secret"),
            "a token is in the history of `{workflow}`"
        );
        // A run stopped at a limit, and the accounts were told: that
        // too says what it is for.
        rig.assert_steps_described(workflow).await;
    }
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pipeline_waits_for_an_account_with_nothing_running_until_a_recheck_finds_one() {
    let Some(mut rig) = Rig::start("accounts-recheck").await else {
        return;
    };
    rig.picture(&picture(&[
        ("first", account(5 * 86_400, 3600)),
        ("second", account(86_400, 3600)),
    ]));
    let worker = rig.worker("b1", "unversioned");
    rig.accounts(3600, 0).await;
    let client = rig.client();
    let handle = client
        .start_workflow(
            Pipeline::run,
            rig.input_under_accounts("toy-1", BUILD_AND_REVIEW),
            rig.options(
                "toy-1",
                &rig.input_under_accounts("toy-1", BUILD_AND_REVIEW),
            ),
        )
        .await
        .unwrap();
    wait_until_waiting(&rig, "toy-1").await;
    let board = rig.board().await;
    assert_eq!(board.waiting.len(), 1, "{board:?}");
    assert_eq!(board.waiting[0].who, "toy-1");
    assert!(
        rig.calls().is_empty(),
        "a step ran with no account to run under"
    );

    // Nothing has to run while it waits: the worker goes away. The owner
    // then resets usage on an account, a worker comes back, and the
    // accounts are told to look again. They believe what they find over
    // the limit they knew, and the pipeline is released at once, an hour
    // before the limit it was waiting on would have ended.
    rig.kill(worker);
    tokio::time::sleep(Duration::from_secs(2)).await;
    rig.picture(&picture(&[
        ("first", account(5 * 86_400, 3600)),
        ("second", account(86_400, 0)),
    ]));
    rig.worker("b1", "unversioned");
    recheck(&rig).await;
    let end = handle
        .get_result(WorkflowGetResultOptions::default())
        .await
        .unwrap();
    assert!(end.ended);
    assert_eq!(ran_under(&rig, "toy-1"), ["build:second", "review:second"]);
    let board = rig.board().await;
    assert!(board.waiting.is_empty(), "{board:?}");
    assert!(
        !board.picture.states["second"].blocked.contains_key("*"),
        "{board:?}"
    );
    assert!(
        board.picture.states["first"].blocked.contains_key("*"),
        "{board:?}"
    );
    // Someone reading either workflow on the server is told what each
    // thing in it is for: the wait, the check, the word that an account
    // is back, and every step.
    rig.assert_described("toy-1").await;
    rig.assert_described(ACCOUNTS_ID).await;
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_limit_that_ends_releases_those_waiting_without_anyone_asking() {
    let Some(mut rig) = Rig::start("accounts-reset").await else {
        return;
    };
    rig.picture(&picture(&[("only", account(86_400, 6))]));
    rig.worker("b1", "unversioned");
    rig.accounts(3600, 0).await;
    let started = Instant::now();
    let handle = rig
        .client()
        .start_workflow(
            Pipeline::run,
            rig.input_under_accounts("toy-1", BUILD_AND_REVIEW),
            WorkflowStartOptions::new(rig.queue.clone(), "toy-1".to_string()).build(),
        )
        .await
        .unwrap();
    wait_until_waiting(&rig, "toy-1").await;
    let end = handle
        .get_result(WorkflowGetResultOptions::default())
        .await
        .unwrap();
    assert!(end.ended);
    assert_eq!(ran_under(&rig, "toy-1"), ["build:only", "review:only"]);
    assert!(
        started.elapsed() >= Duration::from_secs(4),
        "it did not wait for the limit to end"
    );
    assert!(
        started.elapsed() < Duration::from_secs(40),
        "it was not released when the limit ended"
    );
    // The accounts workflow woke for the reset by itself: it knew when
    // the limit ended and had no need to ask anything again.
    assert_eq!(rig.checks(), 1);
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn those_waiting_are_carried_over_when_the_accounts_continue_as_new() {
    let Some(mut rig) = Rig::start("accounts-new").await else {
        return;
    };
    rig.picture(&picture(&[("only", account(86_400, 3600))]));
    rig.worker("b1", "unversioned");
    // A check every second, and a new run after every two.
    rig.accounts(1, 2).await;
    let handle = rig
        .client()
        .start_workflow(
            Pipeline::run,
            rig.input_under_accounts("toy-1", BUILD_AND_REVIEW),
            WorkflowStartOptions::new(rig.queue.clone(), "toy-1".to_string()).build(),
        )
        .await
        .unwrap();
    wait_until_waiting(&rig, "toy-1").await;
    rig.wait_for("the accounts never continued as new", || {
        let (_, said) = rig.temporal(&[
            "workflow",
            "list",
            "--query",
            "WorkflowId = 'accounts' AND ExecutionStatus = 'ContinuedAsNew'",
        ]);
        said.contains("accounts")
    });
    let board = rig.board().await;
    assert_eq!(board.waiting.len(), 1, "{board:?}");
    assert!(board.picture.states["only"].blocked.contains_key("*"));

    rig.picture(&picture(&[("only", account(86_400, 0))]));
    recheck(&rig).await;
    let end = handle
        .get_result(WorkflowGetResultOptions::default())
        .await
        .unwrap();
    assert!(end.ended);
    assert_eq!(ran_under(&rig, "toy-1"), ["build:only", "review:only"]);
    rig.stop().await;
}
