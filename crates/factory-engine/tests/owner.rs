//! A button the owner presses on the dashboard reaches the project's tiers
//! as the same press in Slack does: `factory-engine owner <project> press`
//! builds its event with `owner_press`, and the worker's turn hands it to
//! the same handler, with the same effect. Each press here is taken through
//! the worker's own turn (`FactoryRunner`), on records of its own, with the
//! scripted stand-ins for `claude` and Slack.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[path = "../../factory-driver/tests/common/mod.rs"]
mod common;

use common::FakeSlack;
use common::world::World;
use factory_driver::goal::{Event, owner_press};
use factory_engine::factory::FactoryRunner;
use factory_engine::steps::StepRunner;
use factory_slack::socket::Incoming;
use serde_json::{Value, json};

const MAIN: &str =
    "id = \"main\"\ndefault = true\n[director]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\n";
const WIDE: &str =
    "id = \"wide\"\n[director]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\nslots = 1\n";

/// Where a press came from.
#[derive(Clone, Copy, Debug)]
enum Via {
    Slack,
    Dashboard,
}

/// What a press did, as far as the owner can see it.
#[derive(Debug, PartialEq, Eq)]
struct Effect {
    /// The proposal's state and who settled it.
    state: String,
    by: String,
    /// Whether the definition was stored, under the proposal's hash.
    stored: bool,
    /// The goal the thread became, by its definition.
    goal_definition: Option<String>,
    /// How often the executive was run, and whether it was told of the
    /// rejection.
    executive_calls: usize,
    rejection_told: bool,
    /// What the proposal's message in Slack was changed to say.
    updated: Vec<String>,
}

/// The executive proposes `wide` in thread 500.0, then the owner presses
/// `action` on it from `via`. Returns what that press did.
fn press_from(via: Via, action: &str) -> Effect {
    let world = World::new(&format!("owner-press-{action}-{via:?}"), "true");
    let fake = FakeSlack::start_with(true, &[("100.0", "BOT", "root")], None);
    world.with_slack(&fake.url);
    // The worker reads the Slack settings whole, as `factory serve` does.
    let env = world.path("slack.env");
    let text = std::fs::read_to_string(&env).unwrap() + "SLACK_APP_TOKEN=xapp-test\n";
    std::fs::write(&env, text).unwrap();
    let project = world.path("config/projects/toy.toml");
    let text = std::fs::read_to_string(&project).unwrap()
        + "\n[executive]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\nslots = 2\n";
    std::fs::write(&project, text).unwrap();
    std::fs::create_dir_all(world.path("config/goals/toy")).unwrap();
    std::fs::write(world.path("config/goals/toy/main.toml"), MAIN).unwrap();
    std::fs::create_dir_all(world.path("bin/state")).unwrap();
    let say = |role: &str, n: u32, json: Value| {
        std::fs::write(
            world.path(&format!("bin/state/{role}-{n}.json")),
            json.to_string(),
        )
        .unwrap();
    };
    say(
        "executive",
        1,
        json!({
            "reply": "None of the kinds suits this; I propose one below.",
            "close": [],
            "open_goal": {"definition": "", "handoff": ""},
            "propose_definition": {"toml": WIDE, "reason": "`main` lands one bead at a time.",
                                   "handoff": "The owner wants the docs rewritten."},
        }),
    );
    // What the executive says once told of a rejection, and what the
    // director of a goal opened by an approval says first.
    say(
        "executive",
        2,
        json!({"reply": "Then I will ask what you want.", "close": [],
               "open_goal": {"definition": "", "handoff": ""}}),
    );
    say(
        "intake",
        1,
        json!({"reply": "What should the docs say?", "ready": false, "objective": "", "stop_when": "", "plan": "", "paths": [], "edits": []}),
    );

    // Each turn is the worker's own: a supervisor read from the records.
    let runner = FactoryRunner {
        config: world.config(),
        slack: true,
    };
    let turned = runner
        .turn(
            "toy",
            &Event::Slack(Incoming::Message {
                channel: "C0TOY".to_string(),
                user: "OWNER".to_string(),
                text: "Rewrite all the docs.".to_string(),
                ts: "500.0".to_string(),
                thread_ts: None,
            }),
        )
        .unwrap();
    assert_eq!(turned.trouble, None);
    let record = || -> Value {
        serde_json::from_str(
            &std::fs::read_to_string(world.path("state/projects/toy/executive.json")).unwrap(),
        )
        .unwrap()
    };
    let proposal = record()["threads"]["500.0"]["proposals"][0].clone();
    assert_eq!(proposal["state"], "waiting", "{proposal}");
    let updates_before = fake.requests_to("chat.update").len();

    let event = match via {
        Via::Slack => Event::Slack(Incoming::Action {
            channel: "C0TOY".to_string(),
            user: "OWNER".to_string(),
            action_id: action.to_string(),
            value: "500.0/1".to_string(),
            message_ts: proposal["ts"].as_str().unwrap().to_string(),
        }),
        // As `factory-engine owner toy press <action> 500.0/1` builds it,
        // and as the engine carries it to the worker: in a signal's JSON.
        Via::Dashboard => {
            let built = owner_press(&world.config(), action, "500.0/1").unwrap();
            serde_json::from_value(serde_json::to_value(&built).unwrap()).unwrap()
        }
    };
    let turned = runner.turn("toy", &event).unwrap();
    assert_eq!(turned.trouble, None);

    let thread = record()["threads"]["500.0"].clone();
    let settled = &thread["proposals"][0];
    let goal_definition = thread["goal"].as_str().map(|goal| {
        let goal: Value = serde_json::from_str(
            &std::fs::read_to_string(world.path(&format!("state/goals/{goal}/goal.json"))).unwrap(),
        )
        .unwrap();
        goal["definition"].as_str().unwrap().to_string()
    });
    let executive: Vec<Vec<String>> = world
        .calls()
        .into_iter()
        .filter(|call| call[0] == "executive")
        .collect();
    let rejection_told = std::fs::read_dir(world.path("bin/state"))
        .unwrap()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("brief-"))
        .any(|entry| {
            std::fs::read_to_string(entry.path())
                .unwrap()
                .contains("The owner rejected your proposed goal definition `wide`")
        });
    Effect {
        state: settled["state"].as_str().unwrap().to_string(),
        by: settled["by"].as_str().unwrap().to_string(),
        stored: world
            .path(&format!(
                "state/goals/toy/wide/{}.toml",
                proposal["hash"].as_str().unwrap()
            ))
            .exists(),
        goal_definition,
        executive_calls: executive.len(),
        rejection_told,
        updated: fake.requests_to("chat.update")[updates_before..]
            .iter()
            .map(|update| {
                ["Approved by <@OWNER>", "Rejected by <@OWNER>"]
                    .into_iter()
                    .find(|said| update.contains(said))
                    .unwrap_or("something else")
                    .to_string()
            })
            .collect(),
    }
}

#[test]
fn a_definition_approved_from_the_dashboard_is_put_in_force_as_from_slack() {
    let slack = press_from(Via::Slack, "definition_approve");
    let dashboard = press_from(Via::Dashboard, "definition_approve");
    assert_eq!(dashboard, slack);
    assert_eq!(
        dashboard,
        Effect {
            state: "approved".to_string(),
            by: "OWNER".to_string(),
            stored: true,
            goal_definition: Some("wide".to_string()),
            executive_calls: 1,
            rejection_told: false,
            updated: vec!["Approved by <@OWNER>".to_string()],
        }
    );
}

#[test]
fn a_definition_rejected_from_the_dashboard_is_rejected_as_from_slack() {
    let slack = press_from(Via::Slack, "definition_reject");
    let dashboard = press_from(Via::Dashboard, "definition_reject");
    assert_eq!(dashboard, slack);
    assert_eq!(
        dashboard,
        Effect {
            state: "rejected".to_string(),
            by: "OWNER".to_string(),
            stored: false,
            goal_definition: None,
            executive_calls: 2,
            rejection_told: true,
            updated: vec!["Rejected by <@OWNER>".to_string()],
        }
    );
}
