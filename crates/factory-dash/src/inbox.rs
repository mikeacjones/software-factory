//! What is waiting on the owner, gathered from the picture into one list.
//!
//! Every item is something the factory has stopped to ask the owner, read
//! from the same records the rest of the page is: a charter to start, a
//! completion to accept, bead changes to approve, a question a director or
//! a step put, a goal a review proposed. Each says which of the owner's
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

/// Everything waiting on the owner in one project, newest first.
pub fn of_project(project: &Value, proposals: &[Value]) -> Vec<Value> {
    let id = text(&project["id"]);
    let mut items = Vec::new();
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
        let items = of_project(&project, &proposals);
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
