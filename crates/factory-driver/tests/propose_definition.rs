//! The executive proposes a goal definition when none of the project's
//! suits the work, and the owner's Approve puts it in force and opens the
//! goal under it, with the scripted stand-ins for `claude` and Slack.

// Test code: a failed setup step should stop the test where it happens.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::world::World;
use factory_driver::goal::Supervisor;
use factory_slack::socket::Incoming;
use serde_json::{Value, json};

const MAIN: &str =
    "id = \"main\"\ndefault = true\n[director]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\n";
const WIDE: &str =
    "id = \"wide\"\n[director]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\nslots = 1\n";
const MAIN_CHANGED: &str = "id = \"main\"\ndefault = true\nflow = \"light\"\n[director]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\n";

fn front(reply: &str, open: &str, toml: &str, reason: &str, handoff: &str) -> Value {
    json!({
        "reply": reply,
        "close": [],
        "open_goal": {"definition": open, "handoff": ""},
        "propose_definition": {"toml": toml, "reason": reason, "handoff": handoff},
    })
}

#[test]
fn the_executive_proposes_a_definition_and_the_owners_approve_opens_the_goal_under_it() {
    let world = World::new("propose-definition", "true");
    let fake = common::FakeSlack::start_with(true, &[("100.0", "BOT", "root")], None);
    world.with_slack(&fake.url);
    // The toy project is the factory's own here, so the bead that commits
    // a definition to the release is filed in its tracker.
    let project = world.path("config/projects/toy.toml");
    let text = std::fs::read_to_string(&project).unwrap()
        + "\n[executive]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\nslots = 2\n\n[commands.release]\nargv = [\"true\"]\ntimeout_secs = 30\n\n[github]\nrepo = \"me/toy\"\nwrite = [\"me/toy\"]\n\n[self_deploy]\nbuild = \"release\"\n";
    std::fs::write(&project, text).unwrap();
    std::fs::create_dir_all(world.path("config/goals/toy")).unwrap();
    std::fs::write(world.path("config/goals/toy/main.toml"), MAIN).unwrap();
    // A second pipeline, which a change to `main` names.
    let pipeline = std::fs::read_to_string(world.path("config/flows/build-break-review.toml"))
        .unwrap()
        .replace("id = \"build-break-review\"", "id = \"light\"");
    std::fs::write(world.path("config/flows/light.toml"), pipeline).unwrap();
    std::fs::create_dir_all(world.path("bin/state")).unwrap();
    let say = |role: &str, n: u32, json: Value| {
        std::fs::write(
            world.path(&format!("bin/state/{role}-{n}.json")),
            json.to_string(),
        )
        .unwrap();
    };
    let brief =
        |n: usize| std::fs::read_to_string(world.path(&format!("bin/state/brief-{n}.md"))).unwrap();
    let calls = |role: &str| world.calls().iter().filter(|call| call[0] == role).count();
    let supervisor = || {
        Supervisor::new(
            world.config(),
            Some(factory_slack::Slack::with_base_url("xoxb-test", &fake.url)),
            vec!["OWNER".to_string()],
            false,
        )
        .unwrap()
    };
    let message = |text: &str, ts: &str, thread: Option<&str>| Incoming::Message {
        channel: "C0TOY".to_string(),
        user: "OWNER".to_string(),
        text: text.to_string(),
        ts: ts.to_string(),
        thread_ts: thread.map(str::to_string),
    };
    let press = |sup: &mut Supervisor, user: &str, action: &str, value: &str| {
        sup.incoming(Incoming::Action {
            channel: "C0TOY".to_string(),
            user: user.to_string(),
            action_id: action.to_string(),
            value: value.to_string(),
            message_ts: "100.0".to_string(),
        })
        .unwrap();
    };
    let posted = || fake.requests_to("chat.postMessage").join("\n");
    let store = |id: &str| world.path(&format!("state/goals/toy/{id}"));
    let filed = || -> Vec<String> {
        std::fs::read_dir(world.path("beads"))
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("filed-"))
            .collect()
    };
    let wakes = |sup: &Supervisor| sup.executives().next().unwrap().wakes;
    let mut sup = supervisor();

    // A TOML that does not read, then one whose set fails the check and
    // that opens a goal too: each goes back to the executive in the same
    // turn, and only the third, which passes, is posted.
    say(
        "executive",
        1,
        front("Proposing one.", "", "id = [", "", ""),
    );
    say(
        "executive",
        2,
        front(
            "Proposing one.",
            "main",
            &WIDE.replace("id = \"wide\"\n", "id = \"wide\"\nflow = \"nope\"\n"),
            "Nothing runs wide.",
            "",
        ),
    );
    say(
        "executive",
        3,
        front(
            "None of the kinds suits this; I propose one below.",
            "",
            WIDE,
            "`main` lands one bead at a time and this work is wide.",
            "The owner wants the docs rewritten across the project.",
        ),
    );
    sup.incoming(message("Rewrite all the docs.", "500.0", None))
        .unwrap();
    let executive_calls: Vec<Vec<String>> = world
        .calls()
        .into_iter()
        .filter(|call| call[0] == "executive")
        .collect();
    assert_eq!(executive_calls.len(), 3);
    assert!(
        executive_calls
            .iter()
            .all(|call| call[1] == executive_calls[0][1]),
        "the refusals went back to the same session: {executive_calls:?}"
    );
    let second = brief(2);
    assert!(
        second.contains("propose_definition: ")
            && second.contains("say in `reason` why no definition in force suits the work"),
        "{second}"
    );
    let third = brief(3);
    for expected in [
        "propose_definition: goal definition `wide` of `toy`: its pipeline cannot be used",
        "open_goal and propose_definition: do one or the other",
    ] {
        assert!(
            third.contains(expected),
            "missing {expected:?} in:\n{third}"
        );
    }
    let all = posted();
    assert!(!all.contains("nope") && !all.contains("id = ["), "{all}");
    assert!(!store("wide").exists());
    assert_eq!(sup.goals().count(), 0);

    // The valid one is posted in the thread with its whole text, being new,
    // and the two buttons naming the thread and the proposal.
    let proposal_post = fake
        .requests_to("chat.postMessage")
        .into_iter()
        .find(|request| request.contains("definition_approve"))
        .expect("the proposal was posted with its buttons");
    for expected in [
        r#""thread_ts":"500.0""#,
        "*Proposed goal definition* `wide` for `toy`: a new definition",
        "> `main` lands one bead at a time and this work is wide.",
        r#"id = \"wide\"\n[director]\nmodel = \"claude-opus-5-5\""#,
        r#""action_id":"definition_approve""#,
        r#""action_id":"definition_reject""#,
        r#""value":"500.0/1""#,
    ] {
        assert!(
            proposal_post.contains(expected),
            "missing {expected:?} in {proposal_post}"
        );
    }
    let recorded = sup.executives().next().unwrap().threads["500.0"].clone();
    assert_eq!(recorded.proposals.len(), 1);
    let wide = recorded.proposals[0].clone();
    assert_eq!(
        (
            wide.id.as_str(),
            wide.text.as_str(),
            wide.state.as_str(),
            wide.ts.as_str(),
            wide.change
        ),
        ("wide", WIDE, "waiting", "100.0", false)
    );
    assert_eq!(wide.hash, factory_config::goal_definition_hash(WIDE));
    assert_eq!(
        wide.handoff,
        "The owner wants the docs rewritten across the project."
    );
    assert_eq!(recorded.goal, None);

    // A change to a definition in force is posted as a diff against it.
    say(
        "executive",
        4,
        front(
            "`main` should run the light pipeline; I propose the change below.",
            "",
            MAIN_CHANGED,
            "The work needs the other pipeline.",
            "Use the light pipeline.",
        ),
    );
    sup.incoming(message("Make it lighter.", "600.0", None))
        .unwrap();
    let change_post = fake
        .requests_to("chat.postMessage")
        .into_iter()
        .find(|request| request.contains(r#""value":"600.0/1""#))
        .expect("the change was posted with its buttons");
    for expected in [
        "a change to `main`, the definition in force with that ID",
        "--- main.toml (in force)\\n+++ main.toml (proposed)\\n@@ -1,5 +1,6 @@\\n id = \\\"main\\\"\\n default = true\\n+flow = \\\"light\\\"\\n [director]",
    ] {
        assert!(
            change_post.contains(expected),
            "missing {expected:?} in {change_post}"
        );
    }
    assert!(
        !change_post.contains("\\nid = \\\"main\\\""),
        "{change_post}"
    );

    // Both are on the threads' records, and survive a restart.
    drop(sup);
    let mut sup = supervisor();
    let executive = sup.executives().next().unwrap().clone();
    assert_eq!(
        executive.threads["500.0"].proposals,
        std::slice::from_ref(&wide)
    );
    assert!(executive.threads["600.0"].proposals[0].change);
    let woken = wakes(&sup);
    assert_eq!(woken, 2);

    // Someone who is not the owner presses Approve: nothing happens.
    press(&mut sup, "SOMEONE", "definition_approve", "500.0/1");
    assert!(!store("wide").exists());
    assert_eq!(sup.goals().count(), 0);
    assert!(filed().is_empty());
    assert_eq!(
        sup.executives().next().unwrap().threads["500.0"].proposals[0].state,
        "waiting"
    );

    // The owner approves: the definition is stored under its hash, the
    // catch-up bead is filed, and the goal opens in the same thread pinned
    // to the stored copy, with the executive's handoff. The executive is
    // not woken.
    say(
        "intake",
        1,
        json!({"reply": "What should the docs say?", "ready": false, "objective": "", "stop_when": "", "plan": "", "paths": [], "edits": []}),
    );
    press(&mut sup, "OWNER", "definition_approve", "500.0/1");
    let stored = store("wide").join(format!("{}.toml", wide.hash));
    assert_eq!(std::fs::read_to_string(&stored).unwrap(), WIDE);
    let beads = filed();
    assert_eq!(beads.len(), 1, "{beads:?}");
    let bead = std::fs::read_to_string(world.path("beads").join(&beads[0])).unwrap();
    assert!(
        bead.contains("config/goals/toy/wide.toml") && bead.contains(&wide.hash),
        "{bead}"
    );
    let goals: Vec<_> = sup.goals().cloned().collect();
    assert_eq!(goals.len(), 1);
    let goal = &goals[0];
    assert_eq!(
        (
            goal.definition.as_str(),
            goal.definition_hash.as_deref(),
            goal.thread_ts.as_str(),
            goal.channel.as_str()
        ),
        ("wide", Some(wide.hash.as_str()), "500.0", "C0TOY")
    );
    let handoff = goal.handoff.clone().unwrap();
    assert!(
        handoff.starts_with("The owner wants the docs rewritten across the project.")
            && handoff.contains("owner:\n> Rewrite all the docs."),
        "{handoff}"
    );
    assert_eq!(wakes(&sup), woken, "no executive turn runs on Approve");
    assert_eq!(calls("executive"), 4);
    assert_eq!(calls("intake"), 1);
    let thread = sup.executives().next().unwrap().threads["500.0"].clone();
    assert_eq!(thread.goal.as_deref(), Some(goal.id.as_str()));
    let approved = thread.proposals[0].clone();
    assert_eq!(
        (approved.state.as_str(), approved.by.as_str()),
        ("approved", "OWNER")
    );
    assert!(
        approved
            .made
            .contains(&format!("goal `{}` is opened under it", goal.id)),
        "{}",
        approved.made
    );
    let updated = fake.requests_to("chat.update").join("\n");
    assert!(
        updated.contains("Approved by <@OWNER>: `wide` is in force"),
        "{updated}"
    );
    assert!(!updated.contains("definition_approve"), "{updated}");

    // A second press on a settled proposal changes nothing, and says so.
    let before = posted();
    press(&mut sup, "OWNER", "definition_approve", "500.0/1");
    assert_eq!(sup.goals().count(), 1);
    assert_eq!(filed().len(), 1);
    assert_eq!(calls("intake"), 1);
    let after = posted();
    assert!(
        after[before.len()..]
            .contains("The proposed definition `wide` was already approved by <@OWNER>"),
        "{after}"
    );
    assert_eq!(
        sup.executives().next().unwrap().threads["500.0"].proposals[0],
        approved
    );

    // The set changed under the change to `main`: the pipeline it names is
    // gone. Approve checks again, posts the problems and stores nothing.
    std::fs::remove_file(world.path("config/flows/light.toml")).unwrap();
    let before = posted();
    press(&mut sup, "OWNER", "definition_approve", "600.0/1");
    let after = posted();
    let said = &after[before.len()..];
    assert!(
        said.contains("cannot be put in force as things stand now, so nothing was stored")
            && said.contains("its pipeline cannot be used"),
        "{said}"
    );
    assert!(!store("main").exists());
    assert_eq!(sup.goals().count(), 1);
    assert_eq!(filed().len(), 1);
    assert_eq!(
        sup.executives().next().unwrap().threads["600.0"].proposals[0].state,
        "waiting"
    );
    assert_eq!(wakes(&sup), woken);
}
