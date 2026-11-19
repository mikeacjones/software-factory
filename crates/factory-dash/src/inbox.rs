//! What is waiting on the owner, gathered from the picture into one list.
//!
//! Every item is something the factory has stopped to ask the owner, read
//! from the same records the rest of the page is: a charter to start, a
//! completion to accept, bead changes to approve, a question a director or
//! a step put, a goal a review proposed, a goal definition the executive
//! proposed. Each says which of the owner's
//! acts would answer it: the buttons the factory posted with it, and
//! whether a reply in its thread is read. Those acts are the ones Slack
//! carries, so answering here or there is the same to the factory.

use serde_json::{Value, json};

/// A button the factory reads, as the owner would press it.
fn press(label: &str, tone: &str, action_id: &str, value: &str) -> Value {
    json!({"label": label, "tone": tone, "action_id": action_id, "value": value})
}

/// Stop is offered on every goal, and asks first.
fn stop(goal: &str) -> Value {
    json!({"label": "Stop goal", "tone": "danger", "action_id": "goal_stop", "value": goal, "confirm": "Stop this goal? A pipeline already running finishes by itself; nothing new starts."})
}

fn text(value: &Value) -> &str {
    value.as_str().unwrap_or("")
}

/// The first line of a text, cut short, for a title.
fn headline(value: &Value, most: usize) -> String {
    let line = text(value)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let line = line.trim_start_matches(['#', '*', '>', ' ']);
    if line.chars().count() > most {
        let mut short: String = line.chars().take(most).collect();
        short.push('…');
        short
    } else {
        line.to_string()
    }
}

/// The seconds of a Slack time stamp, which is what a thread is named by.
fn secs_of(ts: &Value) -> Option<u64> {
    text(ts).split('.').next()?.parse().ok()
}

/// What a proposal's message shows in its code block: the definition's
/// text, or its diff against the one in force, as Slack was given it. The
/// reason above it is quoted, so the first fence at a line's start opens
/// the block, and nothing after the block has one.
fn shown(message: &str) -> Option<String> {
    let (_, rest) = message.split_once("\n```\n")?;
    let (inside, _) = rest.rsplit_once("```")?;
    Some(
        inside
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&"),
    )
}

/// The goal definitions a project's executive proposed that the owner
/// has neither approved nor rejected, one item each, read from the
/// executive's record (`executive.json`). One that was approved, rejected
/// or superseded is settled, and gone from here.
fn of_executive(project: &str, executive: &Value) -> Vec<Value> {
    let mut items = Vec::new();
    for (root, thread) in executive["threads"].as_object().into_iter().flatten() {
        let proposals = thread["proposals"].as_array().map(Vec::as_slice);
        for (at, proposal) in proposals.unwrap_or_default().iter().enumerate() {
            if proposal["state"] != "waiting" {
                continue;
            }
            // The buttons name the thread and the proposal's place in it,
            // from 1, as the ones posted in Slack do.
            let value = format!("{root}/{}", at + 1);
            let definition = text(&proposal["id"]);
            let change = proposal["change"] == true;
            // A definition with a schedule runs by the clock: Approve puts
            // the schedule in force and opens no goal.
            let schedule = text(&proposal["schedule"]);
            let (summary, approve) = if schedule.is_empty() {
                (
                    "The executive proposes a goal definition, as none in force suits the work. Approve puts it in force and opens the goal under it in this thread; it is checked again first. Reject stores nothing, and the executive is told.".to_string(),
                    "Approve: put it in force and open the goal",
                )
            } else {
                (
                    format!(
                        "The executive proposes a goal definition with a schedule. Approve puts the schedule in force, and the factory runs it {schedule}; no goal is opened under it. It is checked again first. Reject stores nothing, and the executive is told."
                    ),
                    "Approve: put its schedule in force",
                )
            };
            let body = shown(text(&proposal["message"]))
                .unwrap_or_else(|| text(&proposal["text"]).to_string());
            let fence = if body.ends_with('\n') { "" } else { "\n" };
            let thread_at = json!(root);
            items.push(json!({
                "key": format!("definition:{value}"),
                "kind": "definition",
                "project": project,
                "goal": thread["goal"],
                "title": if change {
                    format!("Change goal definition `{definition}`")
                } else {
                    format!("New goal definition `{definition}`")
                },
                "summary": summary,
                "at": secs_of(&proposal["ts"]).or_else(|| secs_of(&thread_at)),
                "sections": [
                    {"label": "Why", "text": proposal["reason"]},
                    {
                        "label": if change { "The change, against the definition in force" } else { "The definition" },
                        "text": format!("```\n{body}{fence}```"),
                    },
                ],
                "actions": [
                    press(approve, "primary", "definition_approve", &value),
                    press("Reject", "plain", "definition_reject", &value),
                ],
                // A reply goes to the executive, and after a Reject it is
                // kept as why.
                "reply": {"channel": thread["channel"], "ts": root, "hint": "Goes to the executive in this thread. After rejecting, your next reply is kept as why."},
                "slack": {"channel": thread["channel"], "ts": root},
            }));
        }
    }
    items
}

/// Everything waiting on the owner in one project, newest first.
/// `executive` is the project's executive's record, or null.
pub fn of_project(project: &Value, proposals: &[Value], executive: &Value) -> Vec<Value> {
    let id = text(&project["id"]);
    let mut items = of_executive(id, executive);
    for goal in project["goals"].as_array().into_iter().flatten() {
        items.extend(of_goal(id, goal));
    }
    // A pipeline run by hand, in no goal, has nobody above it but the owner.
    for unit in project["loose"].as_array().into_iter().flatten() {
        items.extend(of_unit(id, unit, None));
    }
    for proposal in proposals {
        let pid = text(&proposal["id"]);
        items.push(json!({
            "key": format!("proposal:{pid}"),
            "kind": "proposal",
            "project": id,
            "for_project": proposal["project"],
            "title": proposal["title"],
            "summary": format!("The nightly review proposes a goal for {}.", text(&proposal["project"])),
            "at": proposal["at"],
            "sections": [
                {"label": "Objective", "text": proposal["objective"]},
                {"label": "Done when", "text": proposal["stop_when"]},
                {"label": "The figures behind it", "text": proposal["figures"]},
            ],
            "actions": [
                press(if proposal["opens"] == "goal" { "Approve: open the goal" } else { "Approve: file a bead" }, "primary", "proposal_approve", pid),
                press("Decline", "plain", "proposal_decline", pid),
            ],
            // A reply under a declined proposal is kept as the reason.
            "reply": {"channel": proposal["channel"], "ts": proposal["ts"], "hint": "After declining, a reply here is kept as the reason."},
            "slack": {"channel": proposal["channel"], "ts": proposal["ts"]},
        }));
    }
    items.sort_by_key(|item| std::cmp::Reverse(item["at"].as_u64().unwrap_or(0)));
    items
}

fn of_goal(project: &str, goal: &Value) -> Vec<Value> {
    let id = text(&goal["id"]);
    let thread = &goal["thread"];
    let objective = &goal["objective"];
    let phase = text(&goal["phase"]);
    let base = |kind: &str, title: String, summary: &str, at: &Value| {
        json!({
            "key": format!("goal:{id}:{kind}"),
            "kind": kind,
            "project": project,
            "goal": id,
            "title": title,
            "summary": summary,
            "at": at,
            "reply": thread,
            "slack": thread,
        })
    };
    let mut items = Vec::new();
    let charter = &goal["charter"];
    match phase {
        "intake" if charter.is_object() => {
            let mut item = base(
                "charter",
                format!("Start: {}", headline(&charter["objective"], 110)),
                "The director proposes this charter. Nothing runs until you start it; a reply goes back to the director instead.",
                &goal["said"]["at"],
            );
            let mut sections = vec![
                json!({"label": "Objective", "text": charter["objective"]}),
                json!({"label": "Done when", "text": charter["stop_when"]}),
                json!({"label": "Plan", "text": charter["plan"]}),
            ];
            if let Some(paths) = charter["paths"]
                .as_array()
                .filter(|paths| !paths.is_empty())
            {
                let paths: Vec<&str> = paths.iter().filter_map(Value::as_str).collect();
                sections.push(json!({"label": "Paths", "text": paths.join("\n")}));
            }
            item["sections"] = json!(sections);
            item["edits"] = charter["edits"].clone();
            item["actions"] = json!([press("Start goal", "primary", "goal_start", id), stop(id)]);
            items.push(item);
        }
        "intake" => {
            // The director is agreeing the goal with the owner: its turn
            // is over when it has said something since the owner did.
            let said_at = goal["said"]["at"].as_u64().unwrap_or(0);
            if said_at > goal["owner_at"].as_u64().unwrap_or(0) {
                let mut item = base(
                    "intake",
                    format!("Shape the goal: {}", headline(&goal["said"]["text"], 100)),
                    "The director is agreeing what this goal is with you and waits for your reply.",
                    &goal["said"]["at"],
                );
                item["sections"] =
                    json!([{"label": "The director said", "text": goal["said"]["text"]}]);
                item["actions"] = json!([stop(id)]);
                items.push(item);
            }
        }
        "proposed_complete" => {
            let mut item = base(
                "completion",
                format!("Accept as done: {}", headline(objective, 100)),
                "The director says the stop condition is met. Accept ends the goal; a reply instead is guidance and the goal goes on.",
                &goal["said"]["at"],
            );
            item["sections"] = json!([
                {"label": "Evidence", "text": goal["evidence"]},
                {"label": "Done when", "text": goal["stop_when"]},
            ]);
            item["actions"] = json!([press("Accept", "primary", "goal_accept", id), stop(id)]);
            items.push(item);
        }
        _ => {}
    }
    let request = &goal["request"];
    if phase == "running" && request.is_object() {
        let files: Vec<String> = request["files"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|file| {
                format!(
                    "File a {} (P{}): {}",
                    text(&file["type"]),
                    file["priority"].as_i64().unwrap_or(2),
                    text(&file["title"])
                )
            })
            .collect();
        let mut item = base(
            "request",
            format!("Approve bead changes: {}", headline(objective, 90)),
            "The director asks to change the tracker in ways only you may approve.",
            &goal["said"]["at"],
        );
        item["sections"] = json!(if files.is_empty() {
            vec![]
        } else {
            vec![json!({"label": "Beads to file", "text": files.join("\n")})]
        });
        item["edits"] = request["edits"].clone();
        item["actions"] = json!([
            press("Approve", "primary", "request_approve", id),
            press("Reject", "plain", "request_reject", id),
        ]);
        items.push(item);
    }
    // A question the director put, unanswered since.
    if matches!(phase, "running" | "queued") && goal["need"].is_object() && !request.is_object() {
        let mut item = base(
            "question",
            format!("Question: {}", headline(objective, 100)),
            "The goal needs an answer from you. Your reply wakes its director.",
            &goal["need"]["at"],
        );
        item["sections"] = json!([{"label": "Asked", "text": goal["need"]["text"]}]);
        item["actions"] = json!([stop(id)]);
        items.push(item);
    }
    for sub in goal["subgoals"].as_array().into_iter().flatten() {
        for unit in sub["beads"]
            .as_array()
            .into_iter()
            .flatten()
            .chain(sub["ship"].as_object().map(|_| &sub["ship"]))
        {
            items.extend(of_unit(project, unit, Some(id)));
        }
    }
    // The pipelines a director started itself, in no sub-goal.
    for unit in goal["beads"].as_array().into_iter().flatten() {
        items.extend(of_unit(project, unit, Some(id)));
    }
    items
}

/// A step's question is answered in the unit's own thread, which its
/// driver reads from Slack: the page shows it and points there. A parked
/// unit outside any goal waits on the owner too.
fn of_unit(project: &str, unit: &Value, goal: Option<&str>) -> Vec<Value> {
    let id = text(&unit["unit"]);
    let mut items = Vec::new();
    let name = if unit["title"].is_string() {
        text(&unit["title"])
    } else {
        text(&unit["bead"])
    };
    if unit["asked"].is_object() {
        items.push(json!({
            "key": format!("unit:{id}:asked"),
            "kind": "step_question",
            "project": project,
            "goal": goal,
            "unit": id,
            "title": format!("Step {} asks: {}", text(&unit["asked"]["step"]), headline(&unit["asked"]["question"], 90)),
            "summary": format!("The pipeline of “{name}” waits for your answer. Its driver reads replies in its Slack thread, so answer there."),
            "at": unit["asked"]["at"],
            "sections": [{"label": "Question", "text": unit["asked"]["question"]}],
            "actions": [],
            "reply": Value::Null,
            "slack": unit["thread"],
        }));
    }
    if goal.is_none() && unit["status"] == "parked" {
        items.push(json!({
            "key": format!("unit:{id}:parked"),
            "kind": "parked",
            "project": project,
            "unit": id,
            "title": format!("Parked: {name}"),
            "summary": format!("A pipeline run by hand has parked. Continue it with `factory run --unit {id}`, or from a step with `--from <step>`."),
            "at": unit["changed_at"],
            "sections": [{"label": "Why it parked", "text": unit["parked"]}],
            "actions": [],
            "reply": Value::Null,
            "slack": unit["thread"],
        }));
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_question_from_a_pipeline_the_director_started_itself_is_in_the_inbox() {
        let project = json!({
            "id": "toy",
            "goals": [{"id": "g1", "phase": "running", "objective": "Audit it", "subgoals": [],
                "beads": [{"unit": "u1", "bead": "toy-1", "title": "A spike", "asked": {"step": "build", "question": "Which API?", "at": 80}, "status": "running", "thread": {"channel": "C1", "ts": "9.9"}}]}],
        });
        let items = of_project(&project, &[], &Value::Null);
        assert_eq!(items.len(), 1, "{items:?}");
        assert_eq!(items[0]["goal"], "g1");
    }

    #[test]
    fn a_definition_the_executive_proposed_waits_with_its_diff_and_both_buttons_until_settled() {
        let project = json!({"id": "toy", "goals": [], "loose": []});
        let diff = "--- main.toml (in force)\n+++ main.toml (proposed)\n@@ -1,2 +1,3 @@\n id = \"main\"\n+flow = \"light\"\n+note = \"a &lt; b &amp;&amp; c\"\n default = true\n";
        let message = format!(
            ":scroll: *Proposed goal definition* `main` for `toy`: a change to `main`, the definition in force with that ID\n> ```\n> `main` is one at a time\n```\n{diff}```\nApprove puts it in force and opens the goal under it in this thread. It is checked again first."
        );
        let mut executive = json!({
            "project": "toy",
            "threads": {
                "500.0": {"channel": "C1", "goal": null, "said": [], "proposals": [
                    {"id": "wide", "text": "id = \"wide\"\n", "state": "superseded", "superseded_by": 2, "message": "old", "ts": "501.0"},
                    {"id": "main", "text": "id = \"main\"\nflow = \"light\"\n", "reason": "`main` is one at a time", "change": true,
                     "state": "waiting", "message": message, "ts": "502.5"},
                ]},
                "600.0": {"channel": "C1", "goal": null, "said": [], "proposals": [
                    {"id": "narrow", "text": "id = \"narrow\"\n", "state": "rejected", "by": "OWNER", "message": "x", "ts": "601.0"},
                ]},
            },
        });
        let items = of_project(&project, &[], &executive);
        assert_eq!(items.len(), 1, "{items:?}");
        let item = &items[0];
        assert_eq!(item["kind"], "definition");
        assert_eq!(item["key"], "definition:500.0/2");
        assert_eq!(item["title"], "Change goal definition `main`");
        assert_eq!(item["at"], 502);
        assert_eq!(item["sections"][0]["text"], "`main` is one at a time");
        // The diff as the owner reads it in Slack, in a code block.
        assert_eq!(
            item["sections"][1]["text"],
            format!(
                "```\n{}```",
                diff.replace("&lt;", "<").replace("&amp;", "&")
            )
        );
        // The buttons Slack carries, naming the thread and the proposal.
        let actions: Vec<(&str, &str)> = item["actions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| {
                (
                    a["action_id"].as_str().unwrap(),
                    a["value"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            actions,
            [
                ("definition_approve", "500.0/2"),
                ("definition_reject", "500.0/2")
            ]
        );
        assert_eq!(item["reply"]["ts"], "500.0");
        assert_eq!(item["reply"]["channel"], "C1");

        // A new definition is shown whole, under its own title.
        executive["threads"]["600.0"]["proposals"][0]["state"] = json!("waiting");
        let items = of_project(&project, &[], &executive);
        assert_eq!(items.len(), 2, "{items:?}");
        assert_eq!(items[0]["title"], "New goal definition `narrow`");
        assert_eq!(items[0]["sections"][1]["text"], "```\nid = \"narrow\"\n```");

        // Once settled, by either button, it is gone.
        for settled in ["approved", "rejected"] {
            executive["threads"]["500.0"]["proposals"][1]["state"] = json!(settled);
            executive["threads"]["600.0"]["proposals"][0]["state"] = json!(settled);
            assert!(of_project(&project, &[], &executive).is_empty());
        }
    }

    #[test]
    fn a_proposed_definition_with_a_schedule_says_approve_puts_it_in_force_and_opens_no_goal() {
        let project = json!({"id": "toy", "goals": [], "loose": []});
        let executive = json!({
            "project": "toy",
            "threads": {
                "500.0": {"channel": "C1", "goal": null, "said": [], "proposals": [
                    {"id": "upkeep", "text": "id = \"upkeep\"\n", "reason": "Weekly upkeep.",
                     "schedule": "every 7 day(s) at 04:00 UTC", "state": "waiting", "ts": "501.0"},
                    {"id": "wide", "text": "id = \"wide\"\n", "reason": "Wide.", "state": "waiting", "ts": "502.0"},
                ]},
            },
        });
        let items = of_project(&project, &[], &executive);
        assert_eq!(items.len(), 2, "{items:?}");
        let of = |key: &str| {
            items
                .iter()
                .find(|item| item["key"] == key)
                .unwrap()
                .clone()
        };
        let scheduled = of("definition:500.0/1");
        let summary = scheduled["summary"].as_str().unwrap();
        assert!(
            summary.contains("Approve puts the schedule in force, and the factory runs it every 7 day(s) at 04:00 UTC; no goal is opened under it."),
            "{summary}"
        );
        assert_eq!(
            scheduled["actions"][0]["label"],
            "Approve: put its schedule in force"
        );
        // One without a schedule is offered as before.
        let wide = of("definition:500.0/2");
        assert!(
            wide["summary"]
                .as_str()
                .unwrap()
                .contains("Approve puts it in force and opens the goal under it in this thread"),
            "{wide}"
        );
        assert_eq!(
            wide["actions"][0]["label"],
            "Approve: put it in force and open the goal"
        );
    }

    #[test]
    fn each_thing_the_factory_waits_for_is_one_item_with_the_acts_that_answer_it() {
        let project = json!({
            "id": "toy",
            "goals": [
                {"id": "g1", "phase": "intake", "thread": {"channel": "C1", "ts": "1.1"},
                 "charter": {"objective": "## Make it fast\nmore", "stop_when": "p99 < 10ms", "plan": "1. profile", "paths": ["src"], "edits": []},
                 "said": {"at": 50, "text": "Here is a charter"}, "owner_at": 40, "subgoals": []},
                {"id": "g2", "phase": "intake", "thread": {"channel": "C1", "ts": "2.2"},
                 "charter": null, "said": {"at": 10, "text": "what?"}, "owner_at": 20, "subgoals": []},
                {"id": "g3", "phase": "proposed_complete", "objective": "Ship it", "evidence": "PR #1 merged", "said": {"at": 60}, "owner_at": 0, "subgoals": []},
                {"id": "g4", "phase": "running", "objective": "Grow", "request": {"files": [{"title": "Fix x", "type": "bug", "priority": 1}], "edits": []},
                 "need": {"at": 70, "text": "Approve?"}, "said": {"at": 75}, "owner_at": 0,
                 "subgoals": [{"beads": [{"unit": "u1", "title": "A bead", "asked": {"step": "build", "question": "Which API?", "at": 80}, "status": "running", "thread": {"channel": "C1", "ts": "9.9"}}]}]},
                {"id": "g5", "phase": "running", "objective": "Answer me", "request": null, "need": {"at": 90, "text": "Which way?"}, "subgoals": []},
            ],
            "loose": [{"unit": "u2", "bead": "toy-2", "status": "parked", "parked": "flaky", "changed_at": 30}],
        });
        let proposals = vec![
            json!({"id": "r-1-1", "project": "toy", "title": "Do a thing", "at": 100, "opens": "bead", "channel": "C9", "ts": "5.5"}),
        ];
        let items = of_project(&project, &proposals, &Value::Null);
        let kinds: Vec<&str> = items
            .iter()
            .map(|item| item["kind"].as_str().unwrap())
            .collect();
        // Newest first; the intake whose director has not spoken since the
        // owner did is not waiting on the owner.
        assert_eq!(
            kinds,
            [
                "proposal",
                "question",
                "step_question",
                "request",
                "completion",
                "charter",
                "parked"
            ]
        );
        let charter = &items[5];
        assert_eq!(charter["title"], "Start: Make it fast");
        assert_eq!(charter["actions"][0]["action_id"], "goal_start");
        assert_eq!(charter["actions"][0]["value"], "g1");
        assert_eq!(charter["reply"]["ts"], "1.1");
        assert_eq!(items[3]["actions"][1]["action_id"], "request_reject");
        // A step's question is answered where its driver listens.
        assert!(items[2]["reply"].is_null());
        assert_eq!(items[0]["actions"][0]["action_id"], "proposal_approve");
    }
}
