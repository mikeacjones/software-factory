//! Goals: an objective the owner wants reached on a project, and a directing
//! agent that runs pipelines toward it.
//!
//! A goal starts as a conversation. The owner says what they want in the
//! project's channel; that message becomes the goal's thread, and the
//! director talks with them until it can propose a charter. Nothing runs
//! until the owner presses Start.
//!
//! From then on the goal moves only on events: a pipeline ended, the owner
//! wrote in the thread, the owner answered a request. Each event wakes the
//! director in a fresh session, briefed by code. It returns decisions, and
//! this code checks every one before carrying it out. Between events no
//! model runs.
//!
//! The director can propose that the goal is complete. Ending it is the
//! owner's act and involves no agent: they press Accept, or reply with what
//! is still missing.
//!
//! A goal definition may put a foreman under the director. The director
//! then opens sub-goals and ships them, and each sub-goal's foreman, woken
//! the same way by its own events, starts that sub-goal's beads, rules on
//! their findings and reports to the director when the sub-goal is ready or
//! when it needs an answer. Both are the same mechanism at two levels: an
//! inbox of events from below, a fresh session per wake, and decisions that
//! code checks.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use factory_beads::{Change, NewBead, Source, Summary, readiness};
use factory_claude::{Outcome, RunSpec, Session, Usage};
use factory_config::{
    ConfigDir, Factory, GoalDefinition, Permission, Project, Tier, read_env_file,
};
use factory_core::{EventLog, ProjectId, UnitId, now_unix};
use factory_slack::socket::{Incoming, Listener};
use factory_slack::{Button, Slack, escape, quote, truncate};
use factory_supervise::Control;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{Driver, DriverError, Options, Ship, Status, Within, bead_source, under};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// The owner and the director are still agreeing what the goal is.
    Intake,
    /// Started by the owner, and waiting for a running goal of its project
    /// to end.
    Queued,
    Running,
    /// The director says the stop condition is met. The owner decides.
    ProposedComplete,
    Complete,
    Stopped,
}

/// What the owner agreed to when they pressed Start.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Charter {
    pub objective: String,
    pub stop_when: String,
    pub plan: String,
    /// The files and directories the goal's work will change, when its
    /// director could bound them. With them the goal may run beside another
    /// whose paths do not overlap; without them it runs alone.
    #[serde(default)]
    pub paths: Vec<String>,
}

/// A change to a bead that already exists.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BeadEdit {
    pub bead: String,
    /// Empty leaves the criteria alone.
    #[serde(default)]
    pub acceptance_criteria: String,
    /// Below zero leaves the priority alone.
    #[serde(default = "unchanged_priority")]
    pub priority: i64,
    /// `unchanged`, `defer` or `undefer`.
    #[serde(default = "unchanged")]
    pub defer: String,
    #[serde(default)]
    pub reason: String,
}

fn unchanged_priority() -> i64 {
    -1
}

fn unchanged() -> String {
    "unchanged".to_string()
}

impl BeadEdit {
    fn change(&self) -> Change {
        Change {
            acceptance_criteria: (!self.acceptance_criteria.trim().is_empty())
                .then(|| self.acceptance_criteria.clone()),
            priority: (self.priority >= 0).then_some(self.priority),
            deferred: match self.defer.as_str() {
                "defer" => Some(true),
                "undefer" => Some(false),
                _ => None,
            },
        }
    }

    fn describe(&self) -> String {
        let mut parts = Vec::new();
        if !self.acceptance_criteria.trim().is_empty() {
            parts.push(format!(
                "acceptance criteria:\n{}",
                quote(self.acceptance_criteria.trim())
            ));
        }
        if self.priority >= 0 {
            parts.push(format!("priority P{}", self.priority));
        }
        if self.defer != "unchanged" {
            parts.push(self.defer.clone());
        }
        format!("• `{}`: {}", escape(&self.bead), parts.join("; "))
    }
}

/// A part of the goal that lands as one pull request: a branch of its own
/// that beads are absorbed into, shipped when the director says it is whole.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubGoal {
    /// A short slug, which also names its branch.
    pub id: String,
    pub title: String,
    pub objective: String,
    /// The beads the director meant it to hold when it was opened.
    pub planned: Vec<String>,
    pub branch: String,
    pub worktree: PathBuf,
    /// The commit of the default branch it started from.
    pub base: String,
    /// `open`, `queued` (waiting its turn to ship), `shipping`, `parked` or
    /// `landed`.
    pub state: String,
    /// The beads whose work is on its branch, with the commit each became.
    pub absorbed: Vec<(String, String)>,
    pub ship_unit: Option<String>,
    pub pull_request: Option<String>,
    /// Its foreman, when the goal's definition had one as it was opened.
    /// Without one the director starts its beads itself.
    #[serde(default)]
    pub foreman: Option<Foreman>,
    /// Beads its foreman filed to run in it, beyond what the director planned.
    #[serde(default)]
    pub added: Vec<String>,
    /// The files and directories set aside for it while other sub-goals are
    /// worked on beside it. Empty when nothing was declared.
    #[serde(default)]
    pub paths: Vec<String>,
    /// When the director shipped it while another sub-goal of the project
    /// was shipping: it waits its turn from then.
    #[serde(default)]
    pub queued_at: Option<u64>,
}

/// The factory's record of the agent that runs one sub-goal.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Foreman {
    /// What it has not been told yet.
    pub inbox: Vec<Happening>,
    pub wakes: u32,
    pub failures: u32,
    pub retry_at: Option<u64>,
    /// `ready` or `escalated` while it waits for the director. Otherwise it
    /// is working: a pipeline is running, or an event is on its way to it.
    pub waiting: Option<String>,
    /// What it last said to the director.
    pub message: String,
    /// Messages between it and the director since a pipeline last started.
    pub exchanges: u32,
}

/// How many messages a director and a foreman may pass each other without
/// a pipeline starting, before the director must ship or ask the owner.
const MAX_EXCHANGES: u32 = 6;

/// One pipeline the goal started.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GoalUnit {
    pub bead: String,
    pub unit: Option<String>,
    /// `running`, `absorbed`, `landed`, `published`, `parked`, `refused` or `failed`.
    pub state: String,
    pub detail: String,
    /// Whether this pipeline ships a sub-goal; `bead` is then the sub-goal's ID.
    #[serde(default)]
    pub ship: bool,
    /// The sub-goal a bead's pipeline runs in.
    #[serde(default)]
    pub subgoal: Option<String>,
    /// The files and directories set aside for it while it runs beside
    /// other beads. Empty when nothing was declared.
    #[serde(default)]
    pub paths: Vec<String>,
}

/// Something a director or a foreman has not been told yet.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Happening {
    Started,
    /// A fact from the factory itself, such as a pipeline lost in a restart.
    Note {
        text: String,
    },
    /// To a foreman: the director opened its sub-goal.
    Opened,
    /// To a foreman: the director wrote to it.
    DirectorSaid {
        text: String,
    },
    /// To a foreman: the beads it filed to run in its sub-goal now exist.
    Filed {
        beads: Vec<String>,
    },
    /// To the director: a sub-goal's foreman has stopped and waits. `report`
    /// is `ready` or `escalated`.
    ForemanReported {
        subgoal: String,
        report: String,
        message: String,
    },
    OwnerSaid {
        text: String,
    },
    PipelineEnded {
        bead: String,
        unit: Option<String>,
        outcome: String,
        detail: String,
        pull_request: Option<String>,
        /// What its tester and reviewer found that was not fixed.
        findings: Vec<Value>,
        /// The commit the bead became on its sub-goal's branch, if absorbed.
        #[serde(default)]
        commit: Option<String>,
        /// Whether this was a sub-goal being shipped; `bead` is then its ID.
        #[serde(default)]
        ship: bool,
        /// Files it changed outside the paths declared for it.
        #[serde(default)]
        strayed: Vec<String>,
    },
    RequestAnswered {
        approved: bool,
        what: String,
    },
}

/// Bead changes the director wants and the owner has not ruled on.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Request {
    pub files: Vec<Value>,
    pub edits: Vec<BeadEdit>,
}

/// The factory's record of one goal.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Goal {
    pub id: String,
    pub project: String,
    pub definition: String,
    pub phase: Phase,
    pub channel: String,
    /// The owner's message that opened the goal: the root of its thread.
    pub thread_ts: String,
    pub opened_by: String,
    /// The one session of the intake conversation.
    pub intake_session: Option<String>,
    pub charter: Option<Charter>,
    /// Bead changes shown in the charter, applied when the owner starts the goal.
    pub charter_edits: Vec<BeadEdit>,
    pub units: Vec<GoalUnit>,
    #[serde(default)]
    pub subgoals: Vec<SubGoal>,
    pub inbox: Vec<Happening>,
    pub request: Option<Request>,
    /// The message that carries buttons right now, and its text.
    pub prompt: Option<(String, String)>,
    pub evidence: Option<String>,
    pub wakes: u32,
    pub failures: u32,
    pub retry_at: Option<u64>,
    pub usage: Usage,
    pub started_at: u64,
    /// When the owner started it while its project was at its limit.
    #[serde(default)]
    pub queued_at: Option<u64>,
    /// What the project's executive passed on when it opened the goal: its
    /// note to the director, and what was said before.
    #[serde(default)]
    pub handoff: Option<String>,
}

/// The factory's record of a project's executive: the agent the owner
/// talks to in the project's channel, above every goal's director.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Executive {
    pub project: String,
    /// Its conversations with the owner, by the message that opened each.
    pub threads: BTreeMap<String, Front>,
    pub wakes: u32,
    pub usage: Usage,
}

/// One conversation between the owner and a project's executive.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Front {
    pub channel: String,
    pub opened_by: String,
    /// The one session of the conversation.
    pub session: Option<String>,
    /// What was said, in order: who, then what.
    pub said: Vec<(String, String)>,
    /// The goal it became. From then the thread is that goal's.
    pub goal: Option<String>,
}

/// Where an agent of the supervisor sits: the directory its runs, its
/// checkout and its log are kept in, and the names its log carries.
struct Seat {
    project: String,
    id: String,
    dir: PathBuf,
}

impl Goal {
    /// The first sub-goal that has not landed yet, if there is one. A goal
    /// whose director has one slot never has a second.
    pub fn current(&self) -> Option<&SubGoal> {
        self.subgoals.iter().find(|sub| sub.state != "landed")
    }

    /// The sub-goals that have not landed: the ones the director's slots count.
    fn in_flight(&self) -> Vec<&SubGoal> {
        self.subgoals
            .iter()
            .filter(|sub| sub.state != "landed")
            .collect()
    }

    /// The beads the current sub-goal has absorbed: open in the tracker, but
    /// done as far as the blockers of its other beads go.
    fn absorbed(&self) -> Vec<String> {
        self.current()
            .map(|sub| sub.absorbed.iter().map(|(bead, _)| bead.clone()).collect())
            .unwrap_or_default()
    }

    fn running(&self) -> usize {
        self.units
            .iter()
            .filter(|unit| unit.state == "running")
            .count()
    }

    /// The bead pipelines running in one sub-goal.
    fn running_in(&self, sub: &str) -> usize {
        self.units
            .iter()
            .filter(|unit| {
                unit.state == "running"
                    && !unit.ship
                    && unit.subgoal.as_deref().is_none_or(|own| own == sub)
            })
            .count()
    }
}

/// What a pipeline thread tells the supervisor.
enum Internal {
    Began {
        goal: String,
        bead: String,
        unit: String,
    },
    Ended {
        goal: String,
        happening: Happening,
    },
}

const BEAD_TYPES: [&str; 6] = ["bug", "feature", "task", "chore", "spike", "decision"];

/// Make a pipeline's report agree with the goal's record of what that
/// pipeline was. A pipeline that ships a sub-goal must be settled as one
/// even if the report does not say so, or the sub-goal would never be marked
/// landed and nothing after it could start.
fn mark_shipping(goal: &Goal, happening: &mut Happening) {
    if let Happening::PipelineEnded { bead, ship, .. } = happening
        && goal
            .units
            .iter()
            .rev()
            .find(|unit| &unit.bead == bead && unit.state == "running")
            .is_some_and(|unit| unit.ship)
    {
        *ship = true;
    }
}

fn text_schema(description: &str) -> Value {
    json!({ "type": "string", "description": description })
}

fn object_schema(properties: Value) -> Value {
    let required: Vec<String> = properties
        .as_object()
        .map(|map| map.keys().cloned().collect())
        .unwrap_or_default();
    json!({ "type": "object", "additionalProperties": false, "required": required, "properties": properties })
}

fn edit_schema() -> Value {
    object_schema(json!({
        "bead": text_schema("The ID of a bead that exists."),
        "acceptance_criteria": text_schema("The bead's new acceptance criteria, or empty to leave them alone."),
        "priority": { "type": "integer", "description": "0 (most urgent) to 4, or -1 to leave it alone." },
        "defer": { "enum": ["unchanged", "defer", "undefer"] },
        "reason": text_schema("Why, in a sentence."),
    }))
}

/// What the director returns during the opening conversation.
pub fn intake_schema() -> Value {
    object_schema(json!({
        "reply": text_schema("What you say to the owner in the thread: a question, or a short note introducing the charter."),
        "ready": { "type": "boolean", "description": "True when you are proposing the charter for the owner to start. False while you still need something from them." },
        "objective": text_schema("The goal, in a sentence or two the owner would recognise as theirs. Empty until ready."),
        "stop_when": text_schema("The condition that ends the goal, checkable against the tracker and the repository. Empty until ready."),
        "plan": text_schema("How you intend to go about it: the first beads in order and why, what you will file or ask about. Empty until ready."),
        "paths": { "type": "array", "items": { "type": "string" }, "description": "The files and directories this goal's work will change, relative to the repository, when you can bound them with confidence. A goal with paths may run at the same time as another goal whose paths do not overlap; a goal without them runs alone. Empty until ready, and empty for a goal that may touch anything." },
        "edits": { "type": "array", "items": edit_schema(), "description": "Changes to existing beads the owner agreed to in this conversation, such as acceptance criteria. They are shown in the charter and applied when the owner starts the goal." },
    }))
}

fn start_schema(foreman: bool) -> Value {
    let mut properties = json!({
        "bead": text_schema("The ID of a launchable bead."),
        "reason": text_schema("Why this one next."),
    });
    if foreman {
        properties["paths"] = json!({ "type": "array", "items": { "type": "string" }, "description": "The files and directories this bead will change, relative to the repository, such as `src/worker` or `docs/timers.md`. Required for every bead that runs while another does, and no two may overlap. Leave empty for a bead that runs alone." });
    }
    json!({ "type": "array", "description": "Pipelines to start now.", "items": object_schema(properties) })
}

/// A new bead to file. A foreman's may also join its sub-goal.
fn file_schema(foreman: bool) -> Value {
    let mut properties = json!({
        "title": text_schema("What the bead is, as a short title."),
        "type": { "enum": BEAD_TYPES },
        "priority": { "type": "integer", "minimum": 0, "maximum": 4 },
        "description": text_schema("Why it exists and what needs doing, for someone who has not seen this goal."),
        "acceptance_criteria": text_schema("How anyone will know it is done."),
        "parent": text_schema("The epic it belongs under, or empty."),
        "discovered_from": text_schema("The bead whose work turned this up, or empty."),
        "deferred": { "type": "boolean", "description": "True to file it into the icebox: kept, but not ready work." },
    });
    if foreman {
        properties["run_here"] = json!({ "type": "boolean", "description": "True when this sub-goal should not ship without it: it joins the sub-goal, and you are woken again at once to start it. False for work that can land later by itself." });
    }
    json!({ "type": "array", "description": "New beads to file.", "items": object_schema(properties) })
}

fn dropped_schema() -> Value {
    json!({ "type": "array", "description": "Findings from an ended pipeline that you decided not to file.", "items": object_schema(json!({
        "finding": text_schema("Which finding, by its label."),
        "reason": text_schema("Why it is not worth a bead."),
    }))})
}

/// What a sub-goal's foreman returns each time an event wakes it.
pub fn foreman_schema() -> Value {
    object_schema(json!({
        "summary": text_schema("What you concluded and decided, for the owner and the director, in a few plain sentences."),
        "start": start_schema(true),
        "file": file_schema(true),
        "dropped_findings": dropped_schema(),
        "report": { "enum": ["working", "ready", "escalate"], "description": "`working` while a pipeline of this sub-goal is running or being started. `ready` when everything meant for it is absorbed and the director should ship it. `escalate` when you need the director: a bead that cannot run, a question, work that does not belong." },
        "message": text_schema("To the director, when `report` is `ready` or `escalate`: what the sub-goal holds and what they should know, or what you need from them. Otherwise empty."),
        "journal_entry": text_schema("What you want to remember at the next wake of this sub-goal. You start fresh each time; this is all that carries over."),
    }))
}

/// What the director returns each time an event wakes it.
pub fn wake_schema() -> Value {
    object_schema(json!({
        "summary": text_schema("What you concluded and decided, for the owner, in a few plain sentences."),
        "start": start_schema(false),
        "file": file_schema(false),
        "edit": { "type": "array", "description": "Changes to beads that exist.", "items": edit_schema() },
        "dropped_findings": dropped_schema(),
        "open_subgoals": { "type": "array", "description": "Sub-goals to open now. Usually none or one.", "items": object_schema(json!({
            "id": text_schema("A short slug for the sub-goal, such as `signals`: lowercase letters, digits and dashes. It names the branch."),
            "title": text_schema("What the sub-goal delivers, as the title its one pull request could carry."),
            "objective": text_schema("What will be true when it has landed."),
            "beads": { "type": "array", "items": { "type": "string" }, "description": "The beads you mean it to hold, in the order you would run them." },
            "paths": { "type": "array", "items": { "type": "string" }, "description": "The files and directories its beads will change, relative to the repository. Required for every sub-goal that is worked on while another has not landed, and no two may overlap. Leave empty for a sub-goal worked on alone." },
        }))},
        "ship": { "type": "array", "items": { "type": "string" }, "description": "The IDs of sub-goals to ship now, in the order they should land. Each gets the whole verification once on its branch, one pull request, and lands; they ship one at a time. Only a sub-goal whose beads you meant for it are absorbed and in which no pipeline is running." },
        "tell_foreman": { "type": "array", "description": "Messages to foremen: an answer to what one escalated, or an instruction. Always empty on a goal whose sub-goals have no foreman.", "items": object_schema(json!({
            "subgoal": text_schema("The ID of an open sub-goal."),
            "text": text_schema("What you tell its foreman."),
        }))},
        "ask_owner": text_schema("One question only the owner can answer, or empty."),
        "complete": { "type": "boolean", "description": "True to propose that the stop condition is met. The owner decides." },
        "evidence": text_schema("When complete: how the stop condition is met, point by point, with what you checked. Otherwise empty."),
        "journal_entry": text_schema("What you want to remember at the next wake of this goal: decisions made, what you are waiting on, what you would do next. You start fresh each time; this is all that carries over."),
        "project_memory": text_schema("The whole new text of this project's memory, which every goal on the project reads, or empty to leave it as it is. Keep only durable facts about the project."),
    }))
}

fn strings<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

fn items<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn edits_of(value: &Value, key: &str) -> Vec<BeadEdit> {
    items(value, key)
        .iter()
        .filter_map(|edit| serde_json::from_value(edit.clone()).ok())
        .collect()
}

fn edit_problems(edits: &[BeadEdit], tracker: &[Summary]) -> Vec<String> {
    let mut problems = Vec::new();
    for edit in edits {
        if !tracker.iter().any(|bead| bead.id == edit.bead) {
            problems.push(format!(
                "edit: `{}` is not an open bead of this project",
                edit.bead
            ));
        } else if edit.change() == Change::default() {
            problems.push(format!("edit: nothing would change on `{}`", edit.bead));
        }
        if edit.priority > 4 {
            problems.push(format!("edit: priority {} is not 0 to 4", edit.priority));
        }
    }
    problems
}

/// Why a director's decisions may not be carried out. Empty means they may.
///
/// `launchable` answers, for a bead ID, why the readiness check would refuse
/// it (empty: it would not).
pub fn decision_problems(
    definition: &GoalDefinition,
    goal: &Goal,
    tracker: &[Summary],
    launchable: &dyn Fn(&str) -> Vec<String>,
    result: &Value,
) -> Vec<String> {
    let mut problems = Vec::new();
    let known = |id: &str| tracker.iter().any(|bead| bead.id == id);
    let complete = result.get("complete").and_then(Value::as_bool) == Some(true);
    let ships = ships_of(goal, result);
    let tells = tells_of(goal, result);
    let batched = definition.ship_flow.is_some();
    if strings(result, "summary").trim().is_empty() {
        problems.push("`summary` is empty".to_string());
    }

    // Sub-goals: each lands as one pull request, and the director may have
    // so many of them not yet landed at once.
    let openings = openings_of(result);
    let opening = openings.first();
    let current = goal.current();
    let flight = goal.in_flight();
    let slots = definition.director.slots.max(1) as usize;
    for (index, sub) in openings.iter().enumerate() {
        let id = strings(sub, "id").trim();
        let slug = id.len() <= 40
            && id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !slug {
            problems.push(format!(
                "open_subgoal: `{id}` is not a slug of lowercase letters, digits and dashes"
            ));
        }
        let twice = openings[..index]
            .iter()
            .any(|earlier| strings(earlier, "id").trim() == id);
        if twice || goal.subgoals.iter().any(|earlier| earlier.id == id) {
            problems.push(format!(
                "open_subgoal: this goal already had a sub-goal `{id}`"
            ));
        }
        if strings(sub, "title").trim().is_empty() || strings(sub, "objective").trim().is_empty() {
            problems.push("open_subgoal: it needs a title and an objective".to_string());
        }
        // A goal that may run beside another keeps to its charter's paths.
        let own = goal
            .charter
            .as_ref()
            .map(|charter| charter.paths.as_slice())
            .unwrap_or_default();
        for path in paths_of(sub)
            .iter()
            .filter(|path| !own.is_empty() && !under(path, own))
        {
            problems.push(format!(
                "open_subgoal: `{path}`, declared for `{id}`, is outside this goal's own paths ({}): another goal may be running beside this one",
                own.join(", ")
            ));
        }
    }
    if !openings.is_empty() {
        let waiting: Vec<String> = flight.iter().map(|sub| format!("`{}`", sub.id)).collect();
        if !batched {
            problems.push("open_subgoal: this goal lands every bead as its own pull request and has no sub-goals".to_string());
        } else if flight.len() + openings.len() > slots {
            problems.push(match current {
                Some(open) if slots == 1 => format!(
                    "open_subgoal: sub-goal `{}` has not landed yet: ship it before opening another",
                    open.id
                ),
                _ => format!(
                    "open_subgoal: that would make {} sub-goals that have not landed, and this goal's director may have {slots}{}",
                    flight.len() + openings.len(),
                    if waiting.is_empty() {
                        String::new()
                    } else {
                        format!(": {} still to land", waiting.join(", "))
                    }
                ),
            });
        } else {
            // Within the limit: may these be worked on beside each other?
            let mut claims: Vec<(String, Vec<String>, bool)> = flight
                .iter()
                .map(|sub| (sub.id.clone(), sub.paths.clone(), true))
                .collect();
            claims.extend(
                openings
                    .iter()
                    .map(|sub| (strings(sub, "id").trim().to_string(), paths_of(sub), false)),
            );
            problems.extend(claim_problems(&SUBGOAL_CLAIMS, &claims));
        }
    }
    let absorbed = goal.absorbed();
    // Whether the sub-goal in question is run by a foreman: the open one as
    // it was opened, or one opened now under this definition.
    let foreman = match current {
        Some(_) => flight.iter().any(|sub| sub.foreman.is_some()),
        None => definition.foreman.is_some(),
    };

    let mut starting: Vec<&str> = Vec::new();
    if foreman && !items(result, "start").is_empty() {
        problems.push(
            "start: each sub-goal of this goal is run by its foreman, which starts its beads: name them in `open_subgoal.beads`, or say what you want in `tell_foreman`"
                .to_string(),
        );
    }
    for start in items(result, "start") {
        let bead = strings(start, "bead");
        if starting.contains(&bead) {
            problems.push(format!("start: `{bead}` is listed twice"));
            continue;
        }
        starting.push(bead);
        if absorbed.iter().any(|done| done == bead) {
            problems.push(format!(
                "start: `{bead}` is already absorbed in the open sub-goal"
            ));
        } else if !known(bead) {
            problems.push(format!(
                "start: `{bead}` is not an open bead of this project"
            ));
        } else {
            let reasons = launchable(bead);
            if !reasons.is_empty() {
                problems.push(format!(
                    "start: `{bead}` may not be launched: {}",
                    reasons.join("; ")
                ));
            }
        }
    }
    if batched && !starting.is_empty() {
        match current {
            None if opening.is_none() => problems.push(
                "start: a bead runs inside a sub-goal: open one with `open_subgoal` in the same result".to_string(),
            ),
            Some(open) if open.state != "open" => problems.push(format!(
                "start: sub-goal `{}` is being shipped: nothing more can join it",
                open.id
            )),
            _ => {}
        }
    }
    let opened_now = |id: &str| openings.iter().any(|sub| strings(sub, "id").trim() == id);
    for (target, _) in &tells {
        match flight.iter().find(|sub| &sub.id == target) {
            None if opened_now(target) || (target.is_empty() && !openings.is_empty()) => problems.push(
                "tell_foreman: a sub-goal you open now is briefed by its title, objective and beads: say it there"
                    .to_string(),
            ),
            None if target.is_empty() => {
                problems.push("tell_foreman: no sub-goal is open".to_string());
            }
            None => problems.push(format!(
                "tell_foreman: `{target}` is not a sub-goal that is open"
            )),
            Some(open) if open.state != "open" => problems.push(format!(
                "tell_foreman: sub-goal `{}` is being shipped, and its foreman has finished",
                open.id
            )),
            Some(open) => match &open.foreman {
                None => problems.push(format!(
                    "tell_foreman: sub-goal `{}` has no foreman: you start its beads yourself",
                    open.id
                )),
                Some(foreman) if foreman.exchanges >= MAX_EXCHANGES => problems.push(format!(
                    "tell_foreman: you and the foreman of `{}` have passed {} messages without a pipeline starting: ship what is absorbed, or ask the owner",
                    open.id, foreman.exchanges
                )),
                Some(_) => {}
            },
        }
        if ships.contains(target) {
            problems.push(
                "tell_foreman: ship the sub-goal or write to its foreman, not both".to_string(),
            );
        }
    }
    // A foreman that is not waiting for the director will move the goal by
    // itself: a pipeline of its sub-goal is running, or an event is on its
    // way to it. So will a sub-goal that waits its turn to ship.
    let moving = flight
        .iter()
        .filter(|sub| {
            sub.state == "queued"
                || (sub.state == "open"
                    && sub
                        .foreman
                        .as_ref()
                        .is_some_and(|foreman| foreman.waiting.is_none()))
        })
        .count()
        + tells.len()
        + if definition.foreman.is_some() {
            openings.len()
        } else {
            0
        };
    let in_flight = goal.running() + starting.len() + ships.len() + moving;
    if goal.running() + starting.len() > definition.max_pipelines as usize {
        problems.push(format!(
            "start: that would make {} pipelines at once, and this goal may run {}",
            goal.running() + starting.len(),
            definition.max_pipelines
        ));
    }
    for (index, id) in ships.iter().enumerate() {
        if !batched {
            problems.push("ship: this goal has no sub-goals to ship".to_string());
            continue;
        }
        if ships[..index].contains(id) {
            problems.push(format!("ship: `{id}` is listed twice"));
            continue;
        }
        let Some(open) = flight.iter().find(|sub| &sub.id == id) else {
            problems.push(if id.is_empty() {
                "ship: no sub-goal is open".to_string()
            } else {
                format!("ship: `{id}` is not a sub-goal waiting to land")
            });
            continue;
        };
        if open.absorbed.is_empty() {
            problems.push(format!(
                "ship: sub-goal `{}` has absorbed no bead yet",
                open.id
            ));
        } else if matches!(open.state.as_str(), "shipping" | "queued") {
            problems.push(format!(
                "ship: sub-goal `{}` is already being shipped",
                open.id
            ));
        }
        // The first form of this rule spoke of one sub-goal; it still holds
        // for each: nothing of its own may be running or starting.
        let own_start = !starting.is_empty() && current.is_some_and(|first| &first.id == id);
        if goal.running_in(id) > 0 || own_start || opened_now(id) {
            problems.push(
                "ship: a sub-goal ships when nothing else is running or starting".to_string(),
            );
        }
    }

    problems.extend(file_problems(items(result, "file"), launchable));
    let edits = edits_of(result, "edit");
    if edits.len() != items(result, "edit").len() {
        problems.push("edit: an entry could not be read".to_string());
    }
    problems.extend(edit_problems(&edits, tracker));

    if complete {
        if strings(result, "evidence").trim().is_empty() {
            problems.push(
                "complete: say in `evidence` how the stop condition is met, point by point"
                    .to_string(),
            );
        }
        if in_flight > 0 {
            problems.push(
                "complete: a goal cannot be complete while work is running or being started"
                    .to_string(),
            );
        }
        if let Some(open) = flight.iter().find(|open| !open.absorbed.is_empty()) {
            problems.push(format!(
                "complete: sub-goal `{}` holds work that has not landed: ship it first",
                open.id
            ));
        }
    }
    // A goal must never be left with nothing that can move it.
    let asks_owner = !strings(result, "ask_owner").trim().is_empty();
    let needs_approval = (definition.permissions.edit_beads == Permission::Ask
        && !edits.is_empty())
        || (definition.permissions.file_beads == Permission::Ask
            && !items(result, "file").is_empty());
    if in_flight == 0 && !asks_owner && !complete && !needs_approval {
        problems.push(
            if foreman {
                "nothing would be running and nobody would be asked: open a sub-goal, answer the foreman with `tell_foreman`, ship the sub-goal, ask the owner, or propose that the goal is complete"
            } else {
                "nothing would be running and nobody would be asked: start a pipeline, ship the sub-goal, ask the owner, or propose that the goal is complete"
            }
            .to_string(),
        );
    }
    problems
}

/// Why beads may not be filed as asked.
fn file_problems(files: &[Value], launchable: &dyn Fn(&str) -> Vec<String>) -> Vec<String> {
    let mut problems = Vec::new();
    for file in files {
        let title = strings(file, "title");
        if title.trim().is_empty() || strings(file, "description").trim().is_empty() {
            problems.push("file: a bead needs a title and a description".to_string());
        }
        if !BEAD_TYPES.contains(&strings(file, "type")) {
            problems.push(format!("file: `{title}` has no valid type"));
        }
        let priority = file.get("priority").and_then(Value::as_i64).unwrap_or(-1);
        if !(0..=4).contains(&priority) {
            problems.push(format!("file: `{title}` needs a priority from 0 to 4"));
        }
        let from = strings(file, "discovered_from");
        if !from.is_empty() && launchable(from) == ["unknown"] {
            problems.push(format!(
                "file: `{title}` was discovered from `{from}`, which does not exist"
            ));
        }
    }
    problems
}

/// The paths declared for a piece of work, tidied.
fn paths_of(work: &Value) -> Vec<String> {
    items(work, "paths")
        .iter()
        .filter_map(Value::as_str)
        .map(|path| path.trim().trim_end_matches('/').to_string())
        .collect()
}

/// Whether a declared path names a file or directory inside the repository,
/// with no pattern in it.
fn plain_path(path: &str) -> bool {
    !path.is_empty()
        && path != "."
        && !path.starts_with('/')
        && !path.split('/').any(|part| part == ".." || part.is_empty())
        && !path.contains(['*', '?', '[', '\\'])
}

/// How the rules about paths are worded for one kind of work.
struct Claims {
    /// The field of the result the problems are about.
    field: &'static str,
    /// What happens to work declared without paths that would run beside other work.
    undeclared_new: &'static str,
    /// What is said of work already going that has no paths.
    undeclared_old: &'static str,
    /// What becomes of work whose paths overlap.
    overlap: &'static str,
}

const BEAD_CLAIMS: Claims = Claims {
    field: "start",
    undeclared_new: "would run beside another bead, so it needs `paths`: the files and directories it will change",
    undeclared_old: "is running with no paths declared, so nothing may start beside it: wait for it to end",
    overlap: "beads whose paths overlap run one after the other",
};

const SUBGOAL_CLAIMS: Claims = Claims {
    field: "open_subgoal",
    undeclared_new: "would be worked on beside another sub-goal, so it needs `paths`: the files and directories its beads will change",
    undeclared_old: "has not landed and has no paths declared, so nothing may open beside it: wait for it to land",
    overlap: "sub-goals whose paths overlap are worked on one after the other",
};

const GOAL_CLAIMS: Claims = Claims {
    field: "start",
    undeclared_new: "would run beside another goal, and its charter names no paths, so it runs alone: leave it waiting",
    undeclared_old: "is running with no paths in its charter, so nothing may start beside it",
    overlap: "goals whose paths overlap run one after the other",
};

/// What a project's executive returns in a conversation with the owner.
pub fn front_schema() -> Value {
    object_schema(json!({
        "reply": text_schema("What you say to the owner in the thread, written to them directly."),
        "open_goal": object_schema(json!({
            "definition": text_schema("To open a goal: the ID of the goal definition it should run under. Empty while there is no goal to open: you are still finding out what the owner wants, or they asked something you can answer yourself."),
            "handoff": text_schema("What the goal's director should know that the owner's own words do not say: what you found out, what else is running on the project and how this goal relates to it. Empty when opening none."),
        })),
    }))
}

/// What a project's executive returns when goals are waiting to run.
pub fn schedule_schema() -> Value {
    object_schema(json!({
        "summary": text_schema("What you decided and why, for the owner, in a sentence or two."),
        "start": { "type": "array", "description": "Waiting goals to start now, beside what is running. Empty to leave them waiting.", "items": object_schema(json!({
            "goal": text_schema("The ID of a waiting goal."),
            "reason": text_schema("Why it is safe to run beside what is running."),
        }))},
    }))
}

/// Why an executive's choice of goals to start may not be carried out.
/// `running` and `waiting` are the project's goals in those states.
pub fn schedule_problems(
    slots: usize,
    running: &[&Goal],
    waiting: &[&Goal],
    result: &Value,
) -> Vec<String> {
    let mut problems = Vec::new();
    if strings(result, "summary").trim().is_empty() {
        problems.push("`summary` is empty".to_string());
    }
    let paths = |goal: &Goal| {
        goal.charter
            .as_ref()
            .map(|charter| charter.paths.clone())
            .unwrap_or_default()
    };
    let mut claims: Vec<(String, Vec<String>, bool)> = running
        .iter()
        .map(|goal| (goal.id.clone(), paths(goal), true))
        .collect();
    let mut starting: Vec<&str> = Vec::new();
    for start in items(result, "start") {
        let id = strings(start, "goal").trim();
        if starting.contains(&id) {
            problems.push(format!("start: `{id}` is listed twice"));
            continue;
        }
        starting.push(id);
        match waiting.iter().find(|goal| goal.id == id) {
            Some(goal) => claims.push((goal.id.clone(), paths(goal), false)),
            None => problems.push(format!("start: `{id}` is not a goal that is waiting")),
        }
    }
    if running.len() + starting.len() > slots {
        problems.push(format!(
            "start: that would make {} goals running at once, and this project may run {slots}",
            running.len() + starting.len()
        ));
    } else if !starting.is_empty() {
        problems.extend(claim_problems(&GOAL_CLAIMS, &claims));
    }
    problems
}

/// Why pieces of work may not go on at the same time. Work runs side by
/// side only when each piece has declared the paths it will change and no
/// two declarations overlap: that is what makes it safe before any of it
/// runs. Each claim is a name, its paths, and whether it is already going;
/// `starting` says whether anything new is among them.
fn claim_problems(words: &Claims, claims: &[(String, Vec<String>, bool)]) -> Vec<String> {
    let mut problems = Vec::new();
    let field = words.field;
    for (name, paths, old) in claims {
        for path in paths.iter().filter(|_| !old) {
            if !plain_path(path) {
                problems.push(format!(
                    "{field}: `{path}`, declared for `{name}`, is not a plain path inside the repository: name files and directories, with no patterns"
                ));
            }
        }
    }
    if claims.len() < 2 || claims.iter().all(|(_, _, old)| *old) {
        return problems;
    }
    for (name, paths, old) in claims {
        if paths.is_empty() {
            problems.push(format!(
                "{field}: `{name}` {}",
                if *old {
                    words.undeclared_old
                } else {
                    words.undeclared_new
                }
            ));
        }
    }
    for (index, (name, paths, _)) in claims.iter().enumerate() {
        for (other, theirs, _) in &claims[index + 1..] {
            let clash = paths.iter().find_map(|path| {
                theirs
                    .iter()
                    .find(|their| {
                        under(path, std::slice::from_ref(their))
                            || under(their, std::slice::from_ref(path))
                    })
                    .map(|their| (path, their))
            });
            if let Some((path, their)) = clash {
                problems.push(format!(
                    "{field}: `{name}` (`{path}`) and `{other}` (`{their}`) would change the same files: {}",
                    words.overlap
                ));
            }
        }
    }
    problems
}

/// Why the beads a foreman wants to start may not run beside each other and
/// beside what is already running in the sub-goal.
fn path_problems(goal: &Goal, sub: &SubGoal, starts: &[Value]) -> Vec<String> {
    let mut claims: Vec<(String, Vec<String>, bool)> = goal
        .units
        .iter()
        .filter(|unit| {
            unit.state == "running"
                && !unit.ship
                && unit.subgoal.as_deref().is_none_or(|own| own == sub.id)
        })
        .map(|unit| (unit.bead.clone(), unit.paths.clone(), true))
        .collect();
    let mut problems = Vec::new();
    for start in starts {
        let bead = strings(start, "bead");
        let paths = paths_of(start);
        // A sub-goal worked on beside others keeps to its own paths.
        for path in paths
            .iter()
            .filter(|path| !sub.paths.is_empty() && !under(path, &sub.paths))
        {
            problems.push(format!(
                "start: `{path}`, declared for `{bead}`, is outside this sub-goal's own paths ({}): another sub-goal is being worked on beside this one",
                sub.paths.join(", ")
            ));
        }
        claims.push((bead.to_string(), paths, false));
    }
    if !starts.is_empty() {
        problems.extend(claim_problems(&BEAD_CLAIMS, &claims));
    }
    problems
}

/// The sub-goals a director wants to open, from either form of its result.
fn openings_of(result: &Value) -> Vec<&Value> {
    result
        .get("open_subgoal")
        .into_iter()
        .chain(items(result, "open_subgoals"))
        .filter(|sub| !strings(sub, "id").trim().is_empty())
        .collect()
}

/// The sub-goals a director wants shipped, in the order it gave: by ID, or
/// `true` for the only one there is.
fn ships_of(goal: &Goal, result: &Value) -> Vec<String> {
    match result.get("ship") {
        Some(Value::Bool(true)) => {
            vec![goal.current().map(|sub| sub.id.clone()).unwrap_or_default()]
        }
        Some(Value::Array(ids)) => ids
            .iter()
            .filter_map(Value::as_str)
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

/// What a director tells foremen, as sub-goal and text. A bare text goes to
/// the only sub-goal there is.
fn tells_of(goal: &Goal, result: &Value) -> Vec<(String, String)> {
    match result.get("tell_foreman") {
        Some(Value::String(text)) if !text.trim().is_empty() => vec![(
            goal.current().map(|sub| sub.id.clone()).unwrap_or_default(),
            text.trim().to_string(),
        )],
        Some(Value::Array(tells)) => tells
            .iter()
            .map(|tell| {
                (
                    strings(tell, "subgoal").trim().to_string(),
                    strings(tell, "text").trim().to_string(),
                )
            })
            .filter(|(_, text)| !text.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

fn flag(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool) == Some(true)
}

/// Why a foreman's decisions may not be carried out. Empty means they may.
///
/// The rule that matters most: a sub-goal is never left with nothing that
/// can move it. After every decision a pipeline is running in it, or the
/// director has been told.
pub fn foreman_problems(
    definition: &GoalDefinition,
    goal: &Goal,
    sub: &SubGoal,
    tracker: &[Summary],
    launchable: &dyn Fn(&str) -> Vec<String>,
    result: &Value,
) -> Vec<String> {
    let mut problems = Vec::new();
    if strings(result, "summary").trim().is_empty() {
        problems.push("`summary` is empty".to_string());
    }
    let report = strings(result, "report");
    let slots = definition.foreman.as_ref().map_or(1, |tier| tier.slots) as usize;
    let running = goal.running_in(&sub.id);

    let mut starting: Vec<&str> = Vec::new();
    for start in items(result, "start") {
        let bead = strings(start, "bead");
        if starting.contains(&bead) {
            problems.push(format!("start: `{bead}` is listed twice"));
            continue;
        }
        starting.push(bead);
        let is_running = |unit: &GoalUnit| unit.bead == bead && unit.state == "running";
        if sub.absorbed.iter().any(|(done, _)| done == bead) {
            problems.push(format!(
                "start: `{bead}` is already absorbed in this sub-goal"
            ));
        } else if goal.units.iter().any(is_running) {
            problems.push(format!("start: a pipeline for `{bead}` is already running"));
        } else if !sub.planned.iter().chain(&sub.added).any(|own| own == bead) {
            problems.push(format!(
                "start: `{bead}` is not one of this sub-goal's beads. The director planned {}; a bead you file with `run_here` joins them. If another bead belongs here, escalate",
                if sub.planned.is_empty() {
                    "none".to_string()
                } else {
                    sub.planned.join(", ")
                }
            ));
        } else if !tracker.iter().any(|open| open.id == bead) {
            problems.push(format!(
                "start: `{bead}` is not an open bead of this project"
            ));
        } else {
            let reasons = launchable(bead);
            if !reasons.is_empty() {
                problems.push(format!(
                    "start: `{bead}` may not be launched: {}",
                    reasons.join("; ")
                ));
            }
        }
    }
    if running + starting.len() > slots {
        problems.push(format!(
            "start: that would make {} pipelines at once in this sub-goal, and its foreman may run {slots}",
            running + starting.len()
        ));
    } else if goal.running() + starting.len() > definition.max_pipelines as usize {
        problems.push(format!(
            "start: that would make {} pipelines at once, and the goal may run {}",
            goal.running() + starting.len(),
            definition.max_pipelines
        ));
    } else {
        // Within the limits: may these run beside each other?
        problems.extend(path_problems(goal, sub, items(result, "start")));
    }

    let files = items(result, "file");
    if !files.is_empty() && definition.permissions.file_beads == Permission::Ask {
        problems.push(
            "file: on this goal a new bead is put to the owner, which is the director's to do: describe it in `message` and escalate"
                .to_string(),
        );
    }
    problems.extend(file_problems(files, launchable));
    let here = files.iter().filter(|file| flag(file, "run_here")).count();
    if files
        .iter()
        .any(|file| flag(file, "run_here") && flag(file, "deferred"))
    {
        problems.push("file: a bead that is to run here cannot be filed deferred".to_string());
    }
    // A bead filed to run here wakes the foreman again to start it. Filing
    // another before that one has started would go round for ever.
    let unstarted: Vec<&str> = sub
        .added
        .iter()
        .map(String::as_str)
        .filter(|bead| {
            !sub.absorbed.iter().any(|(done, _)| done == bead)
                && !starting.contains(bead)
                && !goal.units.iter().any(|unit| &unit.bead == bead)
        })
        .collect();
    if here > 0 && !unstarted.is_empty() {
        problems.push(format!(
            "file: you already filed {} to run here and have not started it: start it, or escalate, before filing another to run here",
            unstarted.join(", ")
        ));
    }

    let message = strings(result, "message").trim();
    match report {
        "ready" => {
            if running > 0 || !starting.is_empty() || here > 0 {
                problems.push(
                    "ready: a sub-goal is ready when nothing in it is running, starting or waiting to run"
                        .to_string(),
                );
            }
            if sub.absorbed.is_empty() {
                problems.push(
                    "ready: this sub-goal has absorbed nothing, so there is nothing to ship: escalate instead"
                        .to_string(),
                );
            }
            if message.is_empty() {
                problems.push(
                    "ready: say in `message` what the sub-goal now holds and what the director should know before shipping it"
                        .to_string(),
                );
            }
        }
        "escalate" => {
            if message.is_empty() {
                problems
                    .push("escalate: say in `message` what you need from the director".to_string());
            }
        }
        "working" => {
            if running + starting.len() + here == 0 {
                problems.push(
                    "working: nothing would be running in this sub-goal and the director would not be told: start a bead, report `ready`, or `escalate`"
                        .to_string(),
                );
            }
        }
        other => problems.push(format!(
            "`report` is `{other}`, not `working`, `ready` or `escalate`"
        )),
    }
    problems
}

fn table(tracker: &[Summary], done: &[String]) -> String {
    let mut out = String::from("id | P | type | state | title\n");
    for bead in tracker.iter().take(300) {
        let state = if done.contains(&bead.id) {
            "absorbed in the open sub-goal".to_string()
        } else if bead.launchable_given(done) {
            "launchable".to_string()
        } else if bead.status != "open" {
            bead.status.clone()
        } else if bead.blocked {
            format!("blocked by {}", bead.blockers.join(", "))
        } else if !bead.has_criteria {
            "no acceptance criteria".to_string()
        } else {
            format!("assigned to {}", bead.assignee)
        };
        out.push_str(&format!(
            "{} | P{} | {} | {state} | {}\n",
            bead.id, bead.priority, bead.issue_type, bead.title
        ));
    }
    if tracker.len() > 300 {
        out.push_str(&format!("… and {} more\n", tracker.len() - 300));
    }
    out
}

fn permission_text(definition: &GoalDefinition) -> String {
    let say = |permission: Permission| match permission {
        Permission::Allow => "is carried out at once",
        Permission::Ask => "is put to the owner, who approves or rejects it",
    };
    if definition.foreman.is_some() {
        let slots = definition.director.slots;
        return format!(
            "- Sub-goals: each is a part of the goal that lands as one pull request. {} Opening one (`open_subgoals`) hands it to its foreman, a separate agent that starts its beads through the light pipeline, rules on their findings, and reports to you when the sub-goal is ready to ship or when it needs you. You do not start beads: `start` is refused.\n- Answering a foreman: `tell_foreman` reaches the foreman of an open sub-goal, and wakes it.\n- Shipping: yours to decide, by naming the sub-goal in `ship` once nothing is running in it. It runs the whole verification once, opens one pull request for all of it and lands it, which closes its beads. Sub-goals ship one at a time, in the order you name them, and each takes in what landed before it first.\n- Filing a new bead {}.\n- Changing a bead that exists (criteria, priority, deferral) {}.\n- Ending the goal: you propose, the owner decides.",
            if slots > 1 {
                format!(
                    "Up to {slots} may be waiting to land at once. That is a limit, not a target: sub-goals are worked on side by side only when each has `paths`, the files and directories its beads will change, and no two overlap, which code checks before anything opens. Open two together only when you have read the code and are certain neither needs the other's result or files; otherwise one after the other."
                )
            } else {
                "One is open at a time.".to_string()
            },
            say(definition.permissions.file_beads),
            say(definition.permissions.edit_beads),
        );
    }
    format!(
        "- Starting a pipeline: carried out at once, on a launchable bead, up to {} at a time.{}\n- Filing a new bead {}.\n- Changing a bead that exists (criteria, priority, deferral) {}.\n- Ending the goal: you propose, the owner decides.",
        definition.max_pipelines,
        if definition.ship_flow.is_some() {
            "\n- Sub-goals: a bead runs inside a sub-goal, one sub-goal at a time. Its pipeline is light and ends by absorbing the bead's one commit into the sub-goal's branch; the bead stays open. Shipping the sub-goal runs the whole verification once, opens one pull request for all of it, and lands it, which closes its beads. A bead blocked only by beads the open sub-goal has absorbed is launchable."
        } else {
            ""
        },
        say(definition.permissions.file_beads),
        say(definition.permissions.edit_beads),
    )
}

/// What an agent has not been told yet, as the list its brief opens with.
fn told(inbox: &[Happening]) -> String {
    let mut out = String::new();
    let mut label = 0;
    for happening in inbox {
        match happening {
            Happening::Started => out.push_str("- The owner started the goal.\n"),
            Happening::OwnerSaid { text } => {
                out.push_str(&format!(
                    "- The owner wrote in the goal's thread:\n\n{}\n\n",
                    quote_plain(text)
                ));
            }
            Happening::Note { text } => out.push_str(&format!("- {text}\n")),
            Happening::Opened => out.push_str(
                "- The director opened this sub-goal and handed it to you. Nothing has run in it yet.\n",
            ),
            Happening::DirectorSaid { text } => out.push_str(&format!(
                "- The director wrote to you:\n\n{}\n\n",
                quote_plain(text)
            )),
            Happening::Filed { beads } => out.push_str(&format!(
                "- The beads you filed to run here now exist and are part of this sub-goal: {}.\n",
                beads.join(", ")
            )),
            Happening::ForemanReported {
                subgoal,
                report,
                message,
            } => out.push_str(&format!(
                "- The foreman of sub-goal `{subgoal}` {} and waits for you:\n\n{}\n\n",
                if report == "ready" {
                    "reports it **ready** to ship"
                } else {
                    "has **escalated**"
                },
                quote_plain(message)
            )),
            Happening::RequestAnswered { approved, what } => out.push_str(&format!(
                "- The owner {} your request: {what}\n",
                if *approved { "approved" } else { "rejected" }
            )),
            Happening::PipelineEnded {
                bead,
                outcome,
                detail,
                pull_request,
                findings,
                ship,
                strayed,
                ..
            } => {
                out.push_str(&format!(
                    "- {} `{bead}` ended: **{outcome}**. {detail}",
                    if *ship {
                        "Shipping the sub-goal"
                    } else {
                        "The pipeline for"
                    }
                ));
                if let Some(url) = pull_request {
                    out.push_str(&format!(" Pull request: {url}"));
                }
                out.push('\n');
                if !strayed.is_empty() {
                    out.push_str(&format!(
                        "  It changed files outside the paths declared for it: {}. Declare wider next time, or run such beads alone.\n",
                        strayed.join(", ")
                    ));
                }
                if !findings.is_empty() {
                    out.push_str("  Findings its tester and reviewer left unfixed. Decide for each: file a bead (with the right type and priority, deferred if it can wait) or drop it.\n");
                }
                for finding in findings {
                    label += 1;
                    out.push_str(&format!(
                        "  - F{label} ({}, {}): {} at `{}`\n",
                        strings(finding, "step"),
                        strings(finding, "severity"),
                        strings(finding, "title"),
                        strings(finding, "location"),
                    ));
                }
            }
        }
    }
    out
}

/// The briefing for one wake of the director. The same inputs give the same text.
pub fn wake_brief(
    definition: &GoalDefinition,
    goal: &Goal,
    tracker: &[Summary],
    journal: &str,
    memory: &str,
) -> String {
    let charter = goal.charter.clone().unwrap_or_default();
    let mut out = format!(
        "# Goal `{}` on project `{}`, wake {}\n\n## Objective\n\n{}\n\n## Stop when\n\n{}\n\n## The plan the owner agreed to\n\n{}\n\n",
        goal.id,
        goal.project,
        goal.wakes + 1,
        charter.objective,
        charter.stop_when,
        charter.plan
    );
    out.push_str("## What happened since you last ran\n\n");
    out.push_str(&told(&goal.inbox));
    if definition.ship_flow.is_some() {
        out.push_str("\n## Sub-goals\n\n");
        if goal.subgoals.is_empty() {
            out.push_str("None opened yet.\n");
        }
        for sub in &goal.subgoals {
            let absorbed: Vec<&str> = sub.absorbed.iter().map(|(bead, _)| bead.as_str()).collect();
            let waiting: Vec<&str> = sub
                .planned
                .iter()
                .map(String::as_str)
                .filter(|bead| !absorbed.contains(bead))
                .collect();
            let foreman = match &sub.foreman {
                Some(foreman) if sub.state == "open" => match foreman.waiting.as_deref() {
                    Some("ready") => format!(
                        " Its foreman reports it ready to ship and waits for you: {}",
                        foreman.message
                    ),
                    Some(_) => format!(
                        " Its foreman has escalated and waits for you: {}",
                        foreman.message
                    ),
                    None => " Its foreman is working on it and will report.".to_string(),
                },
                _ => String::new(),
            };
            let foreman = if sub.paths.is_empty() {
                foreman
            } else {
                format!(" Its paths: {}.{foreman}", sub.paths.join(", "))
            };
            out.push_str(&format!(
                "- `{}` ({}): {}. Branch `{}`. Absorbed: {}. Planned and not yet absorbed: {}.{}{foreman}\n",
                sub.id,
                sub.state,
                sub.title,
                sub.branch,
                if absorbed.is_empty() {
                    "none".to_string()
                } else {
                    absorbed.join(", ")
                },
                if waiting.is_empty() {
                    "none".to_string()
                } else {
                    waiting.join(", ")
                },
                sub.pull_request
                    .as_ref()
                    .map(|url| format!(" Pull request: {url}"))
                    .unwrap_or_default(),
            ));
        }
    }
    out.push_str(&format!(
        "\n## Pipelines of this goal\n\n{} running; this goal may run {} at a time.\n",
        goal.running(),
        definition.max_pipelines
    ));
    for unit in &goal.units {
        out.push_str(&format!(
            "- `{}`: {} {}\n",
            unit.bead, unit.state, unit.detail
        ));
    }
    out.push_str(&format!(
        "\n## The tracker: open work, most urgent first\n\n{}\nOnly a `launchable` bead can be started. Look at any bead in full with `bd show <id>`.\n",
        table(tracker, &goal.absorbed())
    ));
    for (title, text) in [
        ("Your journal for this goal", journal),
        ("Project memory", memory),
    ] {
        out.push_str(&format!(
            "\n## {title}\n\n{}\n",
            if text.trim().is_empty() {
                "(empty)"
            } else {
                text.trim()
            }
        ));
    }
    out.push_str(&format!(
        "\n## What you may decide\n\n{}\n\nReturn the structured result. Code checks every decision against the tracker before carrying it out, and sends back what it refuses.\n",
        permission_text(definition)
    ));
    out
}

/// The briefing for one wake of a sub-goal's foreman. The same inputs give
/// the same text.
pub fn foreman_brief(
    definition: &GoalDefinition,
    goal: &Goal,
    sub: &SubGoal,
    tracker: &[Summary],
    journal: &str,
    memory: &str,
) -> String {
    let foreman = sub.foreman.clone().unwrap_or_default();
    let charter = goal.charter.clone().unwrap_or_default();
    let slots = definition.foreman.as_ref().map_or(1, |tier| tier.slots);
    let mut out = format!(
        "# Sub-goal `{}` of goal `{}` on project `{}`, wake {}\n\n## What this sub-goal delivers\n\n{}\n\n{}\n\nIts branch is `{}`, and your working directory is a checkout of it as it is now.{}\n\n## The goal it is part of\n\n{}\n\n",
        sub.id,
        goal.id,
        goal.project,
        foreman.wakes + 1,
        sub.title,
        sub.objective,
        sub.branch,
        if sub.paths.is_empty() {
            String::new()
        } else {
            format!(
                "\n\nAnother sub-goal is being worked on beside this one, so this one has paths of its own: {}. Its beads keep to them, and any paths you declare for a bead lie inside them. If a bead cannot be done within them, escalate.",
                sub.paths.join(", ")
            )
        },
        charter.objective,
    );
    out.push_str("## What happened since you last ran\n\n");
    out.push_str(&told(&foreman.inbox));

    let done: Vec<String> = sub.absorbed.iter().map(|(bead, _)| bead.clone()).collect();
    let list = |beads: &[String]| {
        if beads.is_empty() {
            "none".to_string()
        } else {
            beads.join(", ")
        }
    };
    let waiting: Vec<String> = sub
        .planned
        .iter()
        .chain(&sub.added)
        .filter(|bead| !done.contains(bead))
        .cloned()
        .collect();
    out.push_str(&format!(
        "\n## Its beads\n\n- Planned by the director, in the order it would run them: {}.\n- Filed by you to run here: {}.\n- Absorbed, one commit each on the branch: {}.\n- Not yet absorbed: {}.\n\n## Its pipelines\n\n{} running; you may run {slots} at a time.\n",
        list(&sub.planned),
        list(&sub.added),
        list(&done),
        list(&waiting),
        goal.running_in(&sub.id),
    ));
    for unit in goal
        .units
        .iter()
        .filter(|unit| !unit.ship && unit.subgoal.as_deref() == Some(sub.id.as_str()))
    {
        out.push_str(&format!(
            "- `{}`: {} {}{}\n",
            unit.bead,
            unit.state,
            unit.detail,
            if unit.state == "running" && !unit.paths.is_empty() {
                format!("(paths: {})", unit.paths.join(", "))
            } else {
                String::new()
            }
        ));
    }
    let own: Vec<Summary> = tracker
        .iter()
        .filter(|bead| {
            sub.planned
                .iter()
                .chain(&sub.added)
                .any(|id| id == &bead.id)
        })
        .cloned()
        .collect();
    out.push_str(&format!(
        "\n## The tracker: this sub-goal's beads\n\n{}\nOnly a `launchable` bead can be started. Look at any bead in full with `bd show <id>`.\n",
        table(&own, &done)
    ));
    for (title, text) in [
        ("Your journal for this sub-goal", journal),
        ("Project memory", memory),
    ] {
        out.push_str(&format!(
            "\n## {title}\n\n{}\n",
            if text.trim().is_empty() {
                "(empty)"
            } else {
                text.trim()
            }
        ));
    }
    out.push_str(&format!(
        "\n## What you may decide\n\n- Starting a pipeline: carried out at once, on a launchable bead of this sub-goal, up to {slots} at a time. A bead blocked only by beads this sub-goal has absorbed is launchable.{}\n- Filing a new bead: {}\n- Reporting: `working` while a pipeline runs; `ready` hands the sub-goal to the director to ship; `escalate` asks the director, and you are woken with the answer.\n- Not yours: shipping, changing a bead that exists, and talking to the owner. Those are the director's; escalate.\n\nReturn the structured result. Code checks every decision against the tracker before carrying it out, and sends back what it refuses.\n",
        if slots > 1 {
            "\n- Side by side: beads may run at the same time only when each has `paths`, the files and directories it will change, and no two declarations overlap. Code checks that before anything starts. The count above is a maximum, not a target: start beads together only when you have read the code each will touch and are certain they cannot collide, and one after the other whenever you are not. Each is built from the branch as it is now; the one absorbed second has the first merged into it and its checks run again, and a conflict goes back to its builder."
        } else {
            ""
        },
        match definition.permissions.file_beads {
            Permission::Allow =>
                "carried out at once. One filed with `run_here` joins this sub-goal, and you are woken again to start it.",
            Permission::Ask =>
                "not yours on this goal. Describe it in `message` and escalate.",
        },
    ));
    out
}

/// The most goals a project runs at once: its executive's slots, or its
/// fixed limit when it has no executive.
fn goal_limit(project: &Project) -> usize {
    project
        .executive
        .as_ref()
        .map_or(project.max_goals, |tier| tier.slots)
        .max(1) as usize
}

/// One line on a goal, for an executive.
fn goal_line(goal: &Goal) -> String {
    let charter = goal.charter.clone().unwrap_or_default();
    let subgoals: Vec<String> = goal
        .subgoals
        .iter()
        .map(|sub| format!("{} ({})", sub.id, sub.state))
        .collect();
    format!(
        "- `{}` ({}): {}\n  Paths: {}. Sub-goals: {}.\n",
        goal.id,
        match goal.phase {
            Phase::Intake => "being agreed with the owner",
            Phase::Queued => "waiting to run",
            Phase::Running => "running",
            Phase::ProposedComplete => "proposed as complete, waiting for the owner",
            Phase::Complete => "complete",
            Phase::Stopped => "stopped",
        },
        if charter.objective.trim().is_empty() {
            "(no charter yet)".to_string()
        } else {
            truncate(&charter.objective.replace('\n', " "), 400)
        },
        if charter.paths.is_empty() {
            "none declared, so it runs alone".to_string()
        } else {
            charter.paths.join(", ")
        },
        if subgoals.is_empty() {
            "none".to_string()
        } else {
            subgoals.join(", ")
        },
    )
}

/// The briefing that opens an executive's conversation with the owner.
pub fn front_brief(
    project: &str,
    text: &str,
    definitions: &[GoalDefinition],
    goals: &[&Goal],
    tracker: &[Summary],
    memory: &str,
) -> String {
    let mut out = format!(
        "# The owner wrote in the channel of project `{project}`\n\n{}\n\n## The kinds of goal you can open\n\n",
        quote_plain(text)
    );
    for definition in definitions {
        out.push_str(&format!(
            "- `{}`{}: {}\n",
            definition.id,
            if definition.default {
                " (the usual one)"
            } else {
                ""
            },
            match (&definition.objective, &definition.stop_when) {
                (Some(objective), Some(stop)) => format!("{objective} Stop when: {stop}"),
                (Some(objective), None) => objective.clone(),
                _ => "no standing objective; the owner says what it is for".to_string(),
            }
        ));
    }
    out.push_str("\n## Goals of this project that have not ended\n\n");
    let live: Vec<&&Goal> = goals
        .iter()
        .filter(|goal| !matches!(goal.phase, Phase::Complete | Phase::Stopped))
        .collect();
    if live.is_empty() {
        out.push_str("None.\n");
    }
    for goal in live {
        out.push_str(&goal_line(goal));
    }
    out.push_str(&format!(
        "\n## The tracker: open work, most urgent first\n\n{}\n## Project memory\n\n{}\n\nReturn the structured result. Your reply is posted in the thread as written.\n",
        table(tracker, &[]),
        if memory.trim().is_empty() {
            "(empty)"
        } else {
            memory.trim()
        },
    ));
    out
}

/// The briefing for an executive deciding which waiting goals may start.
/// The same inputs give the same text.
pub fn schedule_brief(project: &str, slots: usize, running: &[&Goal], waiting: &[&Goal]) -> String {
    let mut out = format!(
        "# Goals are waiting to run on project `{project}`\n\nThis project may have {slots} goals running at once. That is a limit, not a target. A waiting goal may start beside a running one only when both have paths in their charters and none overlap, which code checks, and only when you judge that they cannot collide.\n\n## Running\n\n"
    );
    for goal in running {
        out.push_str(&goal_line(goal));
    }
    out.push_str("\n## Waiting, longest first\n\n");
    for goal in waiting {
        out.push_str(&goal_line(goal));
        if let Some(charter) = &goal.charter {
            out.push_str(&format!(
                "  Its plan: {}\n",
                truncate(&charter.plan.replace('\n', " "), 800)
            ));
        }
    }
    out.push_str("\nDecide which waiting goals, if any, start now. One you leave waiting starts by itself when a running goal ends. Return the structured result; code checks it and sends back what it refuses.\n");
    out
}

fn quote_plain(text: &str) -> String {
    text.lines()
        .map(|line| format!("> {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The words that act without any agent, when the goal is waiting for them.
fn command(text: &str) -> String {
    text.trim().trim_end_matches(['.', '!']).to_lowercase()
}

/// How `run_director` says that every account is at a limit: this, then
/// the time to try again.
const LIMITED: &str = "limited until ";

/// One run of a director or a foreman: who it is, which prompt it works to,
/// where it reads the code, what it must return, and what it is told.
struct Turn {
    /// `director` or `foreman`, as the goal's log names the run.
    who: &'static str,
    role: &'static str,
    tier: Tier,
    /// The checkout it works in: a name under the goal's directory, and the
    /// revision it shows. `None` is the project's default branch, fetched.
    place: Option<(String, String)>,
    schema: Value,
    session: Session,
    prompt: String,
}

/// Check every project's goal definitions against what they name: the
/// pipeline, the director's model and its prompts. Returns one line per
/// definition, or the first problem.
pub fn check_definitions(config: &ConfigDir) -> Result<Vec<String>, DriverError> {
    let factory = config.factory()?;
    let mut lines = Vec::new();
    for id in config.projects()? {
        let project = config.project(&id)?;
        if let Some(tier) = &project.executive {
            if !factory.allowed_models.contains(&tier.model) {
                return Err(DriverError::Preflight(format!(
                    "project `{id}`: its executive's model `{}` is not allowed",
                    tier.model
                )));
            }
            config.role_prompt("executive")?;
            lines.push(format!(
                "{id}: executive {} ({}), {} goal(s) at a time",
                tier.model, tier.effort, tier.slots
            ));
        }
        for definition in config.goal_definitions(&id)? {
            let refuse = |reason: String| {
                DriverError::Preflight(format!(
                    "goal definition `{}` of `{id}`: {reason}",
                    definition.id
                ))
            };
            let mut with_flow = project.clone();
            if let Some(flow) = &definition.flow {
                with_flow.flow = flow.clone();
            }
            let (flow, _) = crate::load_flow(config, &factory, &with_flow)
                .map_err(|err| refuse(format!("its pipeline cannot be used: {err}")))?;
            if let Some(ship) = &definition.ship_flow {
                let ends_with = |flow: &factory_flow::Flow, action: &str| {
                    flow.steps.iter().any(|step| {
                        matches!(&step.kind, factory_flow::StepKind::Action { action: named } if named == action)
                    })
                };
                let reconciles = flow.steps.iter().any(|step| {
                    matches!(&step.kind, factory_flow::StepKind::Action { action } if action == "absorb")
                        && step.on.contains_key("moved")
                        && step.on.contains_key("conflict")
                });
                if definition
                    .foreman
                    .as_ref()
                    .is_some_and(|tier| tier.slots > 1)
                    && !reconciles
                {
                    return Err(refuse(format!(
                        "its foreman may run beads side by side, so the `absorb` step of its bead pipeline `{}` must route `moved` and `conflict`",
                        flow.id
                    )));
                }
                if !ends_with(&flow, "absorb") {
                    return Err(refuse(format!(
                        "it ships sub-goals, so its bead pipeline `{}` must end by absorbing into one (action `absorb`)",
                        flow.id
                    )));
                }
                let mut with_ship = project.clone();
                with_ship.flow = ship.clone();
                let (ship_flow, _) = crate::load_flow(config, &factory, &with_ship)
                    .map_err(|err| refuse(format!("its ship pipeline cannot be used: {err}")))?;
                if definition.director.slots > 1 && !ends_with(&ship_flow, "sync") {
                    return Err(refuse(format!(
                        "its director may have sub-goals side by side, so its ship pipeline `{}` must take in the default branch before it verifies (action `sync`)",
                        ship_flow.id
                    )));
                }
                if !ends_with(&ship_flow, "land") {
                    return Err(refuse(format!(
                        "its ship pipeline `{}` never lands (action `land`)",
                        ship_flow.id
                    )));
                }
            }
            if !factory.allowed_models.contains(&definition.director.model) {
                return Err(refuse(format!(
                    "the director's model `{}` is not allowed",
                    definition.director.model
                )));
            }
            for role in ["director", "director-intake"] {
                config.role_prompt(role)?;
            }
            let mut foreman = String::new();
            if let Some(tier) = &definition.foreman {
                if !factory.allowed_models.contains(&tier.model) {
                    return Err(refuse(format!(
                        "the foreman's model `{}` is not allowed",
                        tier.model
                    )));
                }
                config.role_prompt("foreman")?;
                foreman = format!(", foreman {} ({})", tier.model, tier.effort);
            }
            lines.push(format!(
                "{id}/{}: director {} ({}){foreman}, pipeline `{}`, {} at a time{}",
                definition.id,
                definition.director.model,
                definition.director.effort,
                flow.id,
                definition.max_pipelines,
                if definition.default { ", default" } else { "" }
            ));
        }
    }
    Ok(lines)
}

pub struct Supervisor {
    config: ConfigDir,
    factory: Factory,
    slack: Option<Slack>,
    owners: Vec<String>,
    /// Channel ID to project ID, for projects that have goal definitions.
    channels: BTreeMap<String, String>,
    goals: BTreeMap<String, Goal>,
    /// Each project's executive, for the projects that have one.
    executives: BTreeMap<String, Executive>,
    /// Whether pipelines post to Slack threads of their own.
    unit_slack: bool,
    sender: Sender<Internal>,
    receiver: Receiver<Internal>,
}

impl Supervisor {
    /// Load the registry and every goal already on disk. `slack` is where
    /// the supervisor posts; `None` keeps everything in the goals' logs.
    pub fn new(
        config: ConfigDir,
        slack: Option<Slack>,
        owners: Vec<String>,
        unit_slack: bool,
    ) -> Result<Supervisor, DriverError> {
        let factory = config.factory()?;
        // A definition that cannot run must not be able to open a goal.
        check_definitions(&config)?;
        let mut channels = BTreeMap::new();
        for id in config.projects()? {
            let project = config.project(&id)?;
            if config.goal_definitions(&id)?.is_empty() {
                continue;
            }
            if let (Some(slack), Some(name)) = (&slack, &project.slack_channel) {
                match slack.ensure_channel(name, &owners) {
                    Ok(channel) => drop(channels.insert(channel, id)),
                    Err(err) => eprintln!("goals: project {id}: channel #{name}: {err}"),
                }
            }
        }
        let mut goals = BTreeMap::new();
        let dir = factory.state_dir.join("goals");
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path().join("goal.json");
            if let Ok(text) = std::fs::read_to_string(&path) {
                let goal: Goal =
                    serde_json::from_str(&text).map_err(|source| DriverError::Json {
                        path: path.clone(),
                        source,
                    })?;
                goals.insert(goal.id.clone(), goal);
            }
        }
        let mut executives = BTreeMap::new();
        let dir = factory.state_dir.join("projects");
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path().join("executive.json");
            if let Ok(text) = std::fs::read_to_string(&path) {
                let executive: Executive =
                    serde_json::from_str(&text).map_err(|source| DriverError::Json {
                        path: path.clone(),
                        source,
                    })?;
                executives.insert(executive.project.clone(), executive);
            }
        }
        let (sender, receiver) = mpsc::channel();
        Ok(Supervisor {
            config,
            factory,
            slack,
            owners,
            channels,
            goals,
            executives,
            unit_slack,
            sender,
            receiver,
        })
    }

    /// Build a supervisor from the factory's Slack settings, with its socket.
    pub fn from_config(config: ConfigDir) -> Result<(Supervisor, Listener), DriverError> {
        let factory = config.factory()?;
        let settings = factory.slack.clone().ok_or_else(|| {
            DriverError::Preflight("factory.toml has no [slack] table".to_string())
        })?;
        let env = read_env_file(&settings.env_file)?;
        let need = |key: &str| {
            env.get(key).cloned().ok_or_else(|| {
                DriverError::Preflight(format!("{} has no {key}", settings.env_file.display()))
            })
        };
        let api = env
            .get("SLACK_API_URL")
            .cloned()
            .unwrap_or_else(|| "https://slack.com/api".to_string());
        let owners: Vec<String> = need("SLACK_OWNER_USER_ID")?
            .split(',')
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty())
            .collect();
        let slack = Slack::with_base_url(need("SLACK_BOT_TOKEN")?, api.clone());
        let listener = factory_slack::socket::listen(need("SLACK_APP_TOKEN")?, api);
        Ok((
            Supervisor::new(config, Some(slack), owners, true)?,
            listener,
        ))
    }

    pub fn goals(&self) -> impl Iterator<Item = &Goal> {
        self.goals.values()
    }

    pub fn projects_listening(&self) -> Vec<String> {
        self.channels.values().cloned().collect()
    }

    fn dir(&self, goal: &str) -> PathBuf {
        self.factory.state_dir.join("goals").join(goal)
    }

    fn seat(&self, goal: &Goal) -> Seat {
        Seat {
            project: goal.project.clone(),
            id: goal.id.clone(),
            dir: self.dir(&goal.id),
        }
    }

    /// Where a project's executive sits: beside the project's memory.
    fn executive_seat(&self, project: &str) -> Seat {
        Seat {
            project: project.to_string(),
            id: format!("{project}-executive"),
            dir: self
                .factory
                .state_dir
                .join("projects")
                .join(project)
                .join("executive"),
        }
    }

    fn log_at(&self, seat: &Seat, kind: &str, data: Value) {
        let log = EventLog::new(
            seat.dir.join("events.jsonl"),
            ProjectId::new(seat.project.clone()),
            UnitId::new(seat.id.clone()),
        );
        let _ = log.append(kind, None, data);
    }

    fn log(&self, goal: &Goal, kind: &str, data: Value) {
        self.log_at(&self.seat(goal), kind, data);
    }

    fn save_executive(&mut self, executive: Executive) -> Result<(), DriverError> {
        let seat = self.executive_seat(&executive.project);
        std::fs::create_dir_all(seat.dir.join("runs")).map_err(crate::io(&seat.dir))?;
        let path = seat.dir.with_file_name("executive.json");
        let text =
            serde_json::to_string_pretty(&executive).map_err(|source| DriverError::Json {
                path: path.clone(),
                source,
            })?;
        let temp = seat.dir.with_file_name("executive.json.tmp");
        std::fs::write(&temp, text).map_err(crate::io(&temp))?;
        std::fs::rename(&temp, &path).map_err(crate::io(&path))?;
        self.executives.insert(executive.project.clone(), executive);
        Ok(())
    }

    /// Post in a thread that is not, or not yet, a goal's.
    fn post(&self, channel: &str, thread: &str, text: &str) {
        if let Some(slack) = &self.slack
            && let Err(err) = slack.post(channel, Some(thread), text)
        {
            eprintln!("goals: slack: {err}");
        }
    }

    fn save(&mut self, goal: Goal) -> Result<(), DriverError> {
        let dir = self.dir(&goal.id);
        std::fs::create_dir_all(dir.join("runs")).map_err(crate::io(&dir))?;
        let path = dir.join("goal.json");
        let text = serde_json::to_string_pretty(&goal).map_err(|source| DriverError::Json {
            path: path.clone(),
            source,
        })?;
        let temp = dir.join("goal.json.tmp");
        std::fs::write(&temp, text).map_err(crate::io(&temp))?;
        std::fs::rename(&temp, &path).map_err(crate::io(&path))?;
        self.goals.insert(goal.id.clone(), goal);
        Ok(())
    }

    fn say(&self, goal: &Goal, text: &str) {
        self.log(goal, "slack.say", json!({ "text": text }));
        if let Some(slack) = &self.slack
            && let Err(err) = slack.post(&goal.channel, Some(&goal.thread_ts), text)
        {
            self.log(goal, "slack.failed", json!({ "error": err.to_string() }));
        }
    }

    /// Post a message the owner answers with a button, and remember it.
    fn prompt(&self, goal: &mut Goal, text: &str, buttons: &[Button<'_>]) {
        self.log(
            goal,
            "slack.prompt",
            json!({ "text": text, "buttons": buttons.iter().map(|b| b.action_id).collect::<Vec<_>>() }),
        );
        goal.prompt = None;
        if let Some(slack) = &self.slack {
            match slack.post_with_buttons(&goal.channel, Some(&goal.thread_ts), text, buttons) {
                Ok(ts) => goal.prompt = Some((ts, text.to_string())),
                Err(err) => self.log(goal, "slack.failed", json!({ "error": err.to_string() })),
            }
        }
    }

    /// Take the buttons off the last prompt and say how it was answered.
    fn settle_prompt(&self, goal: &mut Goal, outcome: &str) {
        if let (Some(slack), Some((ts, text))) = (&self.slack, goal.prompt.take()) {
            let _ = slack.update(&goal.channel, &ts, &format!("{text}\n_{outcome}_"));
        }
    }

    fn mentions(&self) -> String {
        self.owners
            .iter()
            .map(|id| format!("<@{id}>"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn definition(&self, goal: &Goal) -> Result<GoalDefinition, DriverError> {
        self.config
            .goal_definitions(&goal.project)?
            .into_iter()
            .find(|definition| definition.id == goal.definition)
            .ok_or_else(|| {
                DriverError::Preflight(format!(
                    "goal `{}` uses the definition `{}`, which project `{}` no longer has",
                    goal.id, goal.definition, goal.project
                ))
            })
    }

    /// Something a person did in Slack.
    pub fn incoming(&mut self, incoming: Incoming) -> Result<(), DriverError> {
        match incoming {
            Incoming::Message {
                channel,
                user,
                text,
                ts,
                thread_ts,
            } => {
                // Only the owner starts or steers a goal.
                if !self.owners.contains(&user) || text.trim().is_empty() {
                    return Ok(());
                }
                match thread_ts {
                    None => {
                        let Some(project) = self.channels.get(&channel).cloned() else {
                            return Ok(());
                        };
                        // A project with an executive is spoken to through it.
                        if self.config.project(&project)?.executive.is_some() {
                            self.front_turn(&project, &channel, &ts, &user, &text)?;
                        } else {
                            self.open_goal(&project, &channel, &ts, &user, &text)?;
                        }
                        Ok(())
                    }
                    Some(root) => {
                        let goal = self
                            .goals
                            .values()
                            .find(|goal| goal.channel == channel && goal.thread_ts == root)
                            .map(|goal| goal.id.clone());
                        // A conversation with an executive that has not
                        // become a goal goes on with the executive.
                        let front = self
                            .executives
                            .values()
                            .find(|executive| {
                                executive.threads.get(&root).is_some_and(|front| {
                                    front.channel == channel && front.goal.is_none()
                                })
                            })
                            .map(|executive| executive.project.clone());
                        match (goal, front) {
                            (Some(id), _) => self.owner_said(&id, &text),
                            (None, Some(project)) => self
                                .front_turn(&project, &channel, &root, &user, &text)
                                .map(|_| ()),
                            // A pipeline's own thread: its driver reads it.
                            (None, None) => Ok(()),
                        }
                    }
                }
            }
            Incoming::Action {
                user,
                action_id,
                value,
                ..
            } => {
                if !self.owners.contains(&user) || !self.goals.contains_key(&value) {
                    return Ok(());
                }
                self.act(&value, &action_id, &format!("<@{user}>"))
            }
        }
    }

    /// Open a goal as an owner's message would, without Slack: for driving a
    /// goal from somewhere other than a channel. Returns its ID.
    pub fn open_directly(&mut self, project: &str, text: &str) -> Result<String, DriverError> {
        let owner = self.owners.first().cloned().unwrap_or_default();
        self.open_goal(project, "", &now_unix().to_string(), &owner, text)?
            .ok_or_else(|| {
                DriverError::Preflight(format!("project `{project}` has no goal definition"))
            })
    }

    fn open_goal(
        &mut self,
        project: &str,
        channel: &str,
        ts: &str,
        user: &str,
        text: &str,
    ) -> Result<Option<String>, DriverError> {
        let definitions = self.config.goal_definitions(project)?;
        // "name: what I want" picks a definition by name.
        let named = text
            .split_once(':')
            .and_then(|(name, _)| definitions.iter().find(|d| d.id == name.trim()));
        let Some(definition) = named.or_else(|| {
            definitions
                .iter()
                .find(|d| d.default)
                .or(definitions.first())
        }) else {
            return Ok(None);
        };
        let id = self.create_goal(project, channel, ts, user, &definition.id, text, None)?;
        self.intake_turn(&id, text)?;
        Ok(Some(id))
    }

    /// Record a new goal, still to be agreed, in the thread of the message
    /// that opened it. Returns its ID.
    #[allow(clippy::too_many_arguments)]
    fn create_goal(
        &mut self,
        project: &str,
        channel: &str,
        ts: &str,
        user: &str,
        definition: &str,
        text: &str,
        handoff: Option<String>,
    ) -> Result<String, DriverError> {
        // Two goals opened in the same second must not share a name.
        let mut name = format!("{project}-g{}", now_unix());
        let mut copy = 1;
        while self.goals.contains_key(&name) {
            copy += 1;
            name = format!("{project}-g{}-{copy}", now_unix());
        }
        let goal = Goal {
            id: name,
            project: project.to_string(),
            definition: definition.to_string(),
            phase: Phase::Intake,
            channel: channel.to_string(),
            thread_ts: ts.to_string(),
            opened_by: user.to_string(),
            intake_session: None,
            charter: None,
            charter_edits: Vec::new(),
            units: Vec::new(),
            subgoals: Vec::new(),
            inbox: Vec::new(),
            request: None,
            prompt: None,
            evidence: None,
            wakes: 0,
            failures: 0,
            retry_at: None,
            usage: Usage::default(),
            started_at: now_unix(),
            queued_at: None,
            handoff,
        };
        let id = goal.id.clone();
        self.save(goal.clone())?;
        self.log(
            &goal,
            "goal.opened",
            json!({ "text": text, "definition": goal.definition, "by_executive": goal.handoff.is_some() }),
        );
        Ok(id)
    }

    /// Say something to a project's executive as the owner would in its
    /// channel, without Slack: `thread` continues a conversation, `None`
    /// opens one. Returns the conversation's thread and the goal it became,
    /// if it has.
    pub fn tell_executive(
        &mut self,
        project: &str,
        thread: Option<&str>,
        text: &str,
    ) -> Result<(String, Option<String>), DriverError> {
        let owner = self.owners.first().cloned().unwrap_or_default();
        let opened = format!(
            "{}.{}",
            now_unix(),
            self.executives.len() + self.goals.len()
        );
        let thread = thread.map(str::to_string).unwrap_or(opened);
        let goal = self.front_turn(project, "", &thread, &owner, text)?;
        Ok((thread, goal))
    }

    pub fn executives(&self) -> impl Iterator<Item = &Executive> {
        self.executives.values()
    }

    /// One turn of the owner's conversation with a project's executive. It
    /// answers, and when it knows what goal the owner wants it opens one:
    /// from then the thread belongs to that goal and its director. Returns
    /// the goal, if this turn opened it.
    fn front_turn(
        &mut self,
        project_id: &str,
        channel: &str,
        root: &str,
        user: &str,
        text: &str,
    ) -> Result<Option<String>, DriverError> {
        let project = self.config.project(project_id)?;
        let Some(tier) = project.executive.clone() else {
            return Ok(None);
        };
        let definitions = self.config.goal_definitions(project_id)?;
        let mut executive = self
            .executives
            .get(project_id)
            .cloned()
            .unwrap_or_else(|| Executive {
                project: project_id.to_string(),
                ..Executive::default()
            });
        let mut front = executive
            .threads
            .get(root)
            .cloned()
            .unwrap_or_else(|| Front {
                channel: channel.to_string(),
                opened_by: user.to_string(),
                ..Front::default()
            });
        front.said.push(("owner".to_string(), text.to_string()));
        // Its seat has to exist before it runs.
        executive.threads.insert(root.to_string(), front.clone());
        self.save_executive(executive.clone())?;
        let seat = self.executive_seat(project_id);
        self.log_at(
            &seat,
            "owner.message",
            json!({ "thread": root, "text": text }),
        );

        let (session, prompt) = match &front.session {
            Some(session) => (
                Session::Resume(session.clone()),
                format!(
                    "The owner replied in the thread:\n\n{}\n",
                    quote_plain(text)
                ),
            ),
            None => {
                let tracker = bead_source(&project).list_open()?;
                let memory =
                    std::fs::read_to_string(self.memory_path(project_id)).unwrap_or_default();
                let goals: Vec<&Goal> = self
                    .goals
                    .values()
                    .filter(|goal| goal.project == project_id)
                    .collect();
                (
                    Session::Fresh,
                    front_brief(project_id, text, &definitions, &goals, &tracker, &memory),
                )
            }
        };
        let known: Vec<String> = definitions.iter().map(|d| d.id.clone()).collect();
        let check = |result: &Value| {
            let mut problems = Vec::new();
            if strings(result, "reply").trim().is_empty() {
                problems.push("`reply` is empty".to_string());
            }
            let wanted = result
                .get("open_goal")
                .map(|open| strings(open, "definition").trim())
                .unwrap_or("");
            if !wanted.is_empty() && !known.iter().any(|id| id == wanted) {
                problems.push(format!(
                    "open_goal: `{wanted}` is not a goal definition of this project: {}",
                    known.join(", ")
                ));
            }
            problems
        };
        let turn = Turn {
            who: "executive",
            role: "executive",
            tier,
            place: None,
            schema: front_schema(),
            session,
            prompt,
        };
        let (result, session) = match self.run_agent(&seat, &mut executive.usage, turn, &check) {
            Ok(done) => done,
            Err(reason) => {
                self.post(
                    channel,
                    root,
                    &format!(
                        ":warning: I could not answer ({}). Reply again to retry.",
                        escape(&reason)
                    ),
                );
                self.save_executive(executive)?;
                return Ok(None);
            }
        };
        executive.wakes += 1;
        front.session = Some(session);
        let reply = strings(&result, "reply").to_string();
        front.said.push(("executive".to_string(), reply.clone()));
        self.post(
            channel,
            root,
            &format!(":office: *Executive*\n{}", escape(&reply)),
        );
        let open = result.get("open_goal").cloned().unwrap_or_default();
        let definition = strings(&open, "definition").trim().to_string();
        if definition.is_empty() {
            executive.threads.insert(root.to_string(), front);
            self.save_executive(executive)?;
            return Ok(None);
        }
        // The goal takes over the thread. Its director is told what was
        // said here, since it shares no session with the executive.
        let conversation = front
            .said
            .iter()
            .map(|(who, said)| format!("{who}:\n{}", quote_plain(said)))
            .collect::<Vec<_>>()
            .join("\n\n");
        let handoff = format!(
            "{}\n\nWhat was said before the goal was opened:\n\n{conversation}",
            strings(&open, "handoff").trim()
        );
        let asked = front
            .said
            .iter()
            .filter(|(who, _)| who == "owner")
            .map(|(_, said)| said.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        let opened_by = front.opened_by.clone();
        let goal = self.create_goal(
            project_id,
            channel,
            root,
            &opened_by,
            &definition,
            &asked,
            Some(handoff.trim().to_string()),
        )?;
        front.goal = Some(goal.clone());
        executive.threads.insert(root.to_string(), front);
        self.save_executive(executive)?;
        self.log_at(
            &seat,
            "goal.opened",
            json!({ "thread": root, "goal": goal, "definition": definition }),
        );
        self.intake_turn(&goal, &asked)?;
        Ok(Some(goal))
    }

    /// Ask a project's executive which waiting goals may start beside the
    /// ones running. Called only when that is a judgement: something is
    /// running, something waits, and there is room. If the executive cannot
    /// be run, the goals stay in line and start when a running one ends.
    fn schedule(&mut self, project_id: &str) -> Result<(), DriverError> {
        let project = self.config.project(project_id)?;
        let Some(tier) = project.executive.clone() else {
            return Ok(());
        };
        let slots = tier.slots.max(1) as usize;
        let running: Vec<Goal> = self.active(project_id).into_iter().cloned().collect();
        let mut waiting: Vec<Goal> = self
            .goals
            .values()
            .filter(|goal| goal.project == project_id && goal.phase == Phase::Queued)
            .cloned()
            .collect();
        waiting.sort_by_key(|goal| (goal.queued_at.unwrap_or(u64::MAX), goal.id.clone()));
        if waiting.is_empty() || running.is_empty() || running.len() >= slots {
            return Ok(());
        }
        let mut executive = self
            .executives
            .get(project_id)
            .cloned()
            .unwrap_or_else(|| Executive {
                project: project_id.to_string(),
                ..Executive::default()
            });
        self.save_executive(executive.clone())?;
        let seat = self.executive_seat(project_id);
        let (now, later): (Vec<&Goal>, Vec<&Goal>) =
            (running.iter().collect(), waiting.iter().collect());
        let check = |result: &Value| schedule_problems(slots, &now, &later, result);
        let turn = Turn {
            who: "executive",
            role: "executive",
            tier,
            place: None,
            schema: schedule_schema(),
            session: Session::Fresh,
            prompt: schedule_brief(project_id, slots, &now, &later),
        };
        let result = match self.run_agent(&seat, &mut executive.usage, turn, &check) {
            Ok((result, _)) => result,
            Err(reason) => {
                self.log_at(&seat, "executive.failed", json!({ "reason": reason }));
                return self.save_executive(executive);
            }
        };
        executive.wakes += 1;
        self.save_executive(executive)?;
        let starts: Vec<String> = items(&result, "start")
            .iter()
            .map(|start| strings(start, "goal").trim().to_string())
            .collect();
        self.log_at(
            &seat,
            "executive.decided",
            json!({ "start": starts, "waiting": waiting.iter().map(|goal| &goal.id).collect::<Vec<_>>() }),
        );
        let summary = quote(&truncate(strings(&result, "summary"), 1500));
        for goal in &waiting {
            if starts.contains(&goal.id) {
                self.start_goal(
                    &goal.id,
                    &format!(":office: *Executive*\n{summary}\n:rocket: *Goal started*, beside what is already running. It now moves on events: a pipeline ending, or a message from you here. `stop` ends it."),
                )?;
            } else {
                self.say(
                    goal,
                    &format!(":office: *Executive*\n{summary}\nThis goal stays in line and starts when a running one ends."),
                );
            }
        }
        Ok(())
    }

    /// Start a goal that was waiting in line.
    fn start_goal(&mut self, id: &str, announcement: &str) -> Result<(), DriverError> {
        let Some(mut goal) = self.goals.get(id).cloned() else {
            return Ok(());
        };
        goal.phase = Phase::Running;
        goal.inbox.insert(0, Happening::Started);
        self.log(
            &goal,
            "goal.unqueued",
            json!({ "queued_at": goal.queued_at }),
        );
        self.say(&goal, announcement);
        self.save(goal)?;
        self.wake(id)
    }

    /// The goals of a project that hold one of its places: running, or
    /// proposed as complete and not yet accepted.
    fn active(&self, project: &str) -> Vec<&Goal> {
        self.goals
            .values()
            .filter(|goal| {
                goal.project == project
                    && matches!(goal.phase, Phase::Running | Phase::ProposedComplete)
            })
            .collect()
    }

    /// Start the goals that have waited longest, while their project has a
    /// place free. Called whenever a goal ends.
    fn start_queued(&mut self, project: &str) -> Result<(), DriverError> {
        let config = self.config.project(project)?;
        let limit = goal_limit(&config);
        loop {
            let active = self.active(project).len();
            if active >= limit {
                return Ok(());
            }
            // Beside a goal that is running, a waiting one starts only on
            // the executive's word.
            if active > 0 && config.executive.is_some() {
                return self.schedule(project);
            }
            let next = self
                .goals
                .values()
                .filter(|goal| goal.project == project && goal.phase == Phase::Queued)
                .min_by_key(|goal| (goal.queued_at.unwrap_or(u64::MAX), goal.id.clone()))
                .map(|goal| goal.id.clone());
            let Some(id) = next else {
                return Ok(());
            };
            self.start_goal(&id, ":rocket: *Goal started.* The goal ahead of it has ended. It now moves on events: a pipeline ending, or a message from you here. `stop` ends it.")?;
        }
    }

    /// The owner wrote in a goal's thread.
    fn owner_said(&mut self, id: &str, text: &str) -> Result<(), DriverError> {
        let mut goal = self.goals[id].clone();
        self.log(&goal, "owner.message", json!({ "text": text }));
        let word = command(text);
        if word == "stop" && !matches!(goal.phase, Phase::Complete | Phase::Stopped) {
            return self.act(id, "goal_stop", "the owner");
        }
        match goal.phase {
            Phase::Intake if word == "start" && goal.charter.is_some() => {
                self.act(id, "goal_start", "the owner")
            }
            Phase::Intake => self.intake_turn(id, text),
            Phase::ProposedComplete if word == "accept" => self.act(id, "goal_accept", "the owner"),
            Phase::Running if goal.request.is_some() && word == "approve" => {
                self.act(id, "request_approve", "the owner")
            }
            Phase::Running if goal.request.is_some() && word == "reject" => {
                self.act(id, "request_reject", "the owner")
            }
            Phase::ProposedComplete | Phase::Running => {
                if goal.phase == Phase::ProposedComplete {
                    // Guidance instead of Accept: the goal goes on.
                    self.settle_prompt(&mut goal, "Not accepted: the owner replied with guidance.");
                    goal.phase = Phase::Running;
                    goal.evidence = None;
                }
                goal.inbox.push(Happening::OwnerSaid {
                    text: text.to_string(),
                });
                self.save(goal)?;
                self.wake(id)
            }
            Phase::Queued => {
                // Kept for the director, who reads it when the goal starts.
                goal.inbox.push(Happening::OwnerSaid {
                    text: text.to_string(),
                });
                self.say(
                    &goal,
                    "Noted. The director reads this when the goal starts.",
                );
                self.save(goal)
            }
            Phase::Complete | Phase::Stopped => {
                self.say(
                    &goal,
                    "This goal has ended. Start a new one with a new message in the channel.",
                );
                Ok(())
            }
        }
    }

    /// A deterministic act by the owner: a button, or its word. No agent runs
    /// to decide any of these.
    fn act(&mut self, id: &str, action: &str, who: &str) -> Result<(), DriverError> {
        let mut goal = self.goals[id].clone();
        self.log(&goal, "owner.action", json!({ "action": action }));
        match (action, goal.phase.clone()) {
            ("goal_start", Phase::Intake) if goal.charter.is_some() => {
                let project = self.config.project(&goal.project)?;
                let source = bead_source(&project);
                for edit in goal.charter_edits.clone() {
                    if let Err(err) = source.change(&edit.bead, &edit.change(), &project.bead_actor)
                    {
                        self.say(
                            &goal,
                            &format!(
                                ":x: Could not change `{}`: {}. The goal has not started.",
                                escape(&edit.bead),
                                escape(&err.to_string())
                            ),
                        );
                        return Ok(());
                    }
                    self.log(&goal, "bead.changed", json!({ "edit": edit }));
                }
                self.settle_prompt(&mut goal, &format!("Started by {who}."));
                // A project runs only so many goals at once. Beyond that
                // the goal waits its turn, and starts by itself.
                let ahead: Vec<String> = self
                    .active(&goal.project)
                    .iter()
                    .map(|other| format!("`{}`", other.id))
                    .collect();
                let limit = goal_limit(&project);
                let full = ahead.len() >= limit;
                // With room left and another goal running, whether this one
                // may run beside it is the executive's to judge.
                let judged = !full && !ahead.is_empty() && project.executive.is_some();
                if full || judged {
                    goal.phase = Phase::Queued;
                    goal.queued_at = Some(now_unix());
                    self.log(
                        &goal,
                        "goal.queued",
                        json!({ "behind": ahead, "judged": judged }),
                    );
                    self.say(&goal, &if judged {
                        format!(
                            ":hourglass_flowing_sand: *Goal waiting.* {} is running. The project's executive is deciding whether this one can safely run beside it; if not, it starts by itself when that ends. What you write here meanwhile is kept for its director; `stop` takes it out of the line.",
                            ahead.join(", ")
                        )
                    } else {
                        format!(
                            ":hourglass_flowing_sand: *Goal queued.* This project runs {limit} goal(s) at a time, and {} is running. This one starts by itself when that ends. What you write here meanwhile is kept for its director; `stop` takes it out of the queue.",
                            ahead.join(", ")
                        )
                    });
                    let project = goal.project.clone();
                    self.save(goal)?;
                    return if judged {
                        self.schedule(&project)
                    } else {
                        Ok(())
                    };
                }
                goal.phase = Phase::Running;
                goal.inbox.push(Happening::Started);
                self.say(&goal, ":rocket: *Goal started.* It now moves on events: a pipeline ending, or a message from you here. `stop` ends it.");
                self.save(goal)?;
                self.wake(id)
            }
            ("goal_accept", Phase::ProposedComplete) => {
                self.settle_prompt(&mut goal, &format!("Accepted by {who}."));
                goal.phase = Phase::Complete;
                self.log(&goal, "goal.complete", json!({ "evidence": goal.evidence }));
                self.say(
                    &goal,
                    &format!(":checkered_flag: *Goal complete*, accepted by {who}."),
                );
                let project = goal.project.clone();
                self.save(goal)?;
                self.start_queued(&project)
            }
            ("request_approve" | "request_reject", Phase::Running) if goal.request.is_some() => {
                let approved = action == "request_approve";
                let request = goal.request.take().unwrap_or_default();
                let mut what = Vec::new();
                if approved {
                    let project = self.config.project(&goal.project)?;
                    let source = bead_source(&project);
                    for file in &request.files {
                        what.push(self.file_bead(&goal, &source, &project, file).1);
                    }
                    for edit in &request.edits {
                        what.push(self.edit_bead(&goal, &source, &project, edit));
                    }
                } else {
                    what.extend(
                        request
                            .files
                            .iter()
                            .map(|file| format!("file `{}`", strings(file, "title"))),
                    );
                    what.extend(
                        request
                            .edits
                            .iter()
                            .map(|edit| format!("change `{}`", edit.bead)),
                    );
                }
                self.settle_prompt(
                    &mut goal,
                    &format!(
                        "{} by {who}.",
                        if approved { "Approved" } else { "Rejected" }
                    ),
                );
                goal.inbox.push(Happening::RequestAnswered {
                    approved,
                    what: what.join("; "),
                });
                self.save(goal)?;
                self.wake(id)
            }
            ("goal_stop", _) => {
                self.settle_prompt(&mut goal, "The goal was stopped.");
                goal.phase = Phase::Stopped;
                self.say(&goal, ":octagonal_sign: *Goal stopped.* A pipeline already running finishes by itself; nothing new starts.");
                let project = goal.project.clone();
                self.save(goal)?;
                self.start_queued(&project)
            }
            // A stale button: the goal has moved on.
            _ => Ok(()),
        }
    }

    fn model(&self, model: &str) -> String {
        let map = &self.factory.model_override;
        map.get(model)
            .or_else(|| map.get("*"))
            .cloned()
            .unwrap_or_else(|| model.to_string())
    }

    /// A checkout for an agent to read, kept apart from the owner's own
    /// checkout and from every pipeline's worktree: the default branch for
    /// the director, a sub-goal's branch for its foreman.
    fn checkout(
        &self,
        home: &Path,
        project: &Project,
        place: &Option<(String, String)>,
    ) -> Result<PathBuf, DriverError> {
        let (dir, head) = match place {
            Some((name, rev)) => (home.join(name), rev.clone()),
            None => {
                factory_git::fetch(&project.repo, &project.remote)?;
                (
                    home.join("checkout"),
                    format!("{}/{}", project.remote, project.default_branch),
                )
            }
        };
        if dir.exists() {
            factory_git::checkout_detached(&dir, &head)?;
        } else {
            factory_git::worktree_add_detached(&project.repo, &dir, &head)?;
        }
        Ok(dir)
    }

    /// Run a director or a foreman once, sending a result that code refuses
    /// back to the same session up to twice. Returns the result and the
    /// session.
    fn run_agent(
        &mut self,
        seat: &Seat,
        spent: &mut Usage,
        turn: Turn,
        check: &dyn Fn(&Value) -> Vec<String>,
    ) -> Result<(Value, String), String> {
        let Turn {
            who,
            role,
            tier,
            place,
            schema,
            mut session,
            mut prompt,
        } = turn;
        let project = self
            .config
            .project(&seat.project)
            .map_err(|err| err.to_string())?;
        let cwd = self
            .checkout(&seat.dir, &project, &place)
            .map_err(|err| err.to_string())?;
        let system_prompt = self
            .config
            .role_prompt(role)
            .map_err(|err| err.to_string())?;
        let dir = seat.dir.join("runs");
        for attempt in 1..=3 {
            let run = std::fs::read_dir(&dir)
                .map(|entries| entries.count())
                .unwrap_or(0)
                / 2
                + 1;
            let _ = std::fs::write(dir.join(format!("{run:03}-{role}-brief.md")), &prompt);
            let model = self.model(&tier.model);
            let (account, oauth_token) = match crate::pick_account(&self.factory, &model) {
                Ok(crate::AccountPick::Use { id, token }) => (id, token),
                Ok(crate::AccountPick::Wait { until }) => return Err(format!("{LIMITED}{until}")),
                Err(err) => return Err(err.to_string()),
            };
            let spec = RunSpec {
                claude_bin: self.factory.claude_bin.clone(),
                model,
                effort: tier.effort.clone(),
                session: session.clone(),
                system_prompt: system_prompt.clone(),
                schema: schema.clone(),
                cwd: cwd.clone(),
                prompt: prompt.clone(),
                budget_usd: project.limits.step_budget_usd,
                timeout: Duration::from_secs(tier.timeout_secs),
                oauth_token,
            };
            let finished = factory_claude::run(
                &spec,
                &dir.join(format!("{run:03}-{role}.jsonl")),
                &mut |_| Control::Continue,
            )
            .map_err(|err| err.to_string())?;
            let usage = finished.transcript.usage();
            crate::observe_account(
                &self.factory,
                &account,
                usage.rate_limit.as_ref(),
                &spec.model,
            );
            spent.add(&usage);
            self.log_at(
                seat,
                &format!("{who}.ran"),
                json!({ "role": role, "run": run, "account": account, "outcome": finished.outcome.name(), "session": finished.transcript.session_id, "usage": usage }),
            );
            let id = finished.transcript.session_id.clone().unwrap_or_default();
            match finished.outcome {
                Outcome::Completed(result) => {
                    let problems = check(&result);
                    if problems.is_empty() {
                        return Ok((result, id));
                    }
                    self.log_at(
                        seat,
                        &format!("{who}.refused"),
                        json!({ "problems": problems }),
                    );
                    if attempt == 3 {
                        return Err(format!(
                            "its decisions were refused three times: {}",
                            problems.join("; ")
                        ));
                    }
                    prompt = format!(
                        "Code checked your result and refused it:\n\n{}\n\nReturn the whole result again with these put right.",
                        problems
                            .iter()
                            .map(|p| format!("- {p}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    );
                    session = Session::Resume(id);
                }
                // The tracker has taken that account out: the next attempt
                // picks another and continues the session, or waits.
                Outcome::Limited { .. } => {
                    if !id.is_empty() {
                        session = Session::Resume(id);
                    }
                }
                other => return Err(format!("the {who}'s run ended as `{}`", other.name())),
            }
        }
        Err(format!("the {who} did not return a result"))
    }

    /// What to do when the director could not be run: try again later, and
    /// after five failures in a row stop and tell the owner.
    fn director_failed(&mut self, mut goal: Goal, reason: &str) -> Result<(), DriverError> {
        if let Some(until) = reason
            .strip_prefix(LIMITED)
            .and_then(|at| at.parse::<u64>().ok())
        {
            // Waiting for a usage window is not a failure.
            goal.retry_at = Some(until);
            self.log(&goal, "accounts.limited", json!({ "until": until }));
            self.say(&goal, &format!(
                ":hourglass: Every account is at a usage limit. The director wakes at {:02}:{:02} UTC.",
                (until % 86_400) / 3600,
                (until % 3600) / 60
            ));
            return self.save(goal);
        }
        goal.failures += 1;
        self.log(
            &goal,
            "director.failed",
            json!({ "reason": reason, "failures": goal.failures }),
        );
        if goal.failures >= 5 {
            goal.phase = Phase::Stopped;
            self.say(&goal, &format!(":x: {} *Goal stopped*: the director failed five times in a row. The last reason: {}", self.mentions(), escape(reason)));
        } else {
            let retry = goal
                .retry_at
                .unwrap_or_else(|| now_unix() + 300)
                .max(now_unix() + 1);
            goal.retry_at = Some(retry);
            self.say(
                &goal,
                &format!(
                    ":warning: The director could not run ({}). Trying again in {} minutes.",
                    escape(reason),
                    (retry - now_unix()).div_ceil(60)
                ),
            );
        }
        // A goal that stopped has given up its place on the project.
        let (project, stopped) = (goal.project.clone(), goal.phase == Phase::Stopped);
        self.save(goal)?;
        if stopped {
            self.start_queued(&project)?;
        }
        Ok(())
    }

    /// One turn of the opening conversation.
    fn intake_turn(&mut self, id: &str, owner_text: &str) -> Result<(), DriverError> {
        let mut goal = self.goals[id].clone();
        let definition = self.definition(&goal)?;
        let project = self.config.project(&goal.project)?;
        let tracker = bead_source(&project).list_open()?;
        let (session, prompt) = match &goal.intake_session {
            Some(session) => (
                Session::Resume(session.clone()),
                format!(
                    "The owner replied in the thread:\n\n{}\n",
                    quote_plain(owner_text)
                ),
            ),
            None => {
                let memory =
                    std::fs::read_to_string(self.memory_path(&goal.project)).unwrap_or_default();
                let handed = goal
                    .handoff
                    .as_deref()
                    .map(|handoff| {
                        format!(
                            "\n## From the project's executive, who took the owner's message and opened this goal\n\n{handoff}\n"
                        )
                    })
                    .unwrap_or_default();
                let standing = match (&definition.objective, &definition.stop_when) {
                    (None, None) => String::new(),
                    (objective, stop) => format!(
                        "\n## This kind of goal's standing objective\n\n{}\n\nStop when: {}\n",
                        objective.as_deref().unwrap_or("(none)"),
                        stop.as_deref().unwrap_or("(none)")
                    ),
                };
                (
                    Session::Fresh,
                    format!(
                        "# A new goal on project `{}`\n\nThe owner opened it with this message:\n\n{}\n{handed}{standing}\n## The tracker: open work, most urgent first\n\n{}\n## Project memory\n\n{}\n\n## What the director of this goal may decide once it runs\n\n{}\n",
                        goal.project,
                        quote_plain(owner_text),
                        table(&tracker, &[]),
                        if memory.trim().is_empty() {
                            "(empty)"
                        } else {
                            memory.trim()
                        },
                        permission_text(&definition),
                    ),
                )
            }
        };
        let check = |result: &Value| {
            let mut problems = Vec::new();
            if strings(result, "reply").trim().is_empty() {
                problems.push("`reply` is empty".to_string());
            }
            let edits = edits_of(result, "edits");
            problems.extend(edit_problems(&edits, &tracker));
            if result.get("ready").and_then(Value::as_bool) == Some(true) {
                for key in ["objective", "stop_when", "plan"] {
                    if strings(result, key).trim().is_empty() {
                        problems.push(format!("ready, but `{key}` is empty"));
                    }
                }
                for path in paths_of(result).iter().filter(|path| !plain_path(path)) {
                    problems.push(format!(
                        "paths: `{path}` is not a plain path inside the repository: name files and directories, with no patterns"
                    ));
                }
            }
            problems
        };
        let turn = Turn {
            who: "director",
            role: "director-intake",
            tier: definition.director.clone(),
            place: None,
            schema: intake_schema(),
            session,
            prompt,
        };
        let seat = self.seat(&goal);
        let result = self.run_agent(&seat, &mut goal.usage, turn, &check);
        let (result, session) = match result {
            Ok(done) => done,
            Err(reason) => {
                self.say(
                    &goal,
                    &format!(
                        ":warning: I could not answer ({}). Reply again to retry.",
                        escape(&reason)
                    ),
                );
                return self.save(goal);
            }
        };
        goal.intake_session = Some(session);
        goal.failures = 0;
        self.settle_prompt(&mut goal, "Superseded by the conversation below.");
        if result.get("ready").and_then(Value::as_bool) == Some(true) {
            goal.charter = Some(Charter {
                objective: strings(&result, "objective").to_string(),
                stop_when: strings(&result, "stop_when").to_string(),
                plan: strings(&result, "plan").to_string(),
                paths: paths_of(&result),
            });
            goal.charter_edits = edits_of(&result, "edits");
            let mut text = format!(
                "{}\n\n:dart: *Objective*\n{}\n\n:checkered_flag: *Stop when*\n{}\n\n:compass: *Plan*\n{}",
                escape(strings(&result, "reply")),
                quote(strings(&result, "objective")),
                quote(strings(&result, "stop_when")),
                quote(&truncate(strings(&result, "plan"), 2500)),
            );
            let paths = paths_of(&result);
            if !paths.is_empty() {
                text.push_str(&format!(
                    "\n\n:file_folder: *Paths*\n{}\n_Its work keeps to these, so it may run beside another goal that touches none of them._",
                    quote(&paths.join("\n"))
                ));
            }
            if !goal.charter_edits.is_empty() {
                text.push_str(&format!(
                    "\n\n:pencil2: *Starting also makes these changes to beads, as we agreed*\n{}",
                    goal.charter_edits
                        .iter()
                        .map(BeadEdit::describe)
                        .collect::<Vec<_>>()
                        .join("\n")
                ));
            }
            text.push_str(&format!(
                "\n\n_Director: `{}`{} · pipeline: `{}` · {} at a time. Press *Start*, or reply to change anything._",
                self.model(&definition.director.model),
                definition
                    .foreman
                    .as_ref()
                    .map(|tier| format!(" · foreman of each sub-goal: `{}`", self.model(&tier.model)))
                    .unwrap_or_default(),
                definition.flow.as_deref().unwrap_or(&project.flow),
                definition.max_pipelines
            ));
            self.prompt(
                &mut goal,
                &text,
                &[Button {
                    action_id: "goal_start",
                    label: "Start",
                    value: id,
                    style: Some("primary"),
                }],
            );
        } else {
            goal.charter = None;
            goal.charter_edits.clear();
            self.say(&goal, &escape(strings(&result, "reply")));
        }
        self.save(goal)
    }

    fn memory_path(&self, project: &str) -> PathBuf {
        self.factory
            .state_dir
            .join("projects")
            .join(project)
            .join("memory.md")
    }

    /// File a bead. Returns its ID, if it was filed, and a line saying what
    /// happened.
    fn file_bead(
        &self,
        goal: &Goal,
        source: &Source,
        project: &Project,
        file: &Value,
    ) -> (Option<String>, String) {
        let optional = |key: &str| {
            let value = strings(file, key).trim();
            (!value.is_empty()).then(|| value.to_string())
        };
        let new = NewBead {
            title: strings(file, "title").to_string(),
            issue_type: strings(file, "type").to_string(),
            priority: file.get("priority").and_then(Value::as_i64).unwrap_or(2),
            description: strings(file, "description").to_string(),
            acceptance_criteria: strings(file, "acceptance_criteria").to_string(),
            parent: optional("parent"),
            discovered_from: optional("discovered_from"),
            deferred: file.get("deferred").and_then(Value::as_bool) == Some(true),
        };
        match source.create(&new, &project.bead_actor) {
            Ok(bead) => {
                // A bead the factory files has a GitHub issue from the start.
                match crate::ensure_issue(&self.factory, project, source, &bead) {
                    Ok(Some(url)) => {
                        self.log(
                            goal,
                            "issue.created",
                            json!({ "bead": bead.id, "url": url }),
                        );
                        let mut linked = bead.clone();
                        linked.external_ref = url;
                        self.sync_issue(goal, project, &linked);
                    }
                    Ok(None) => {}
                    Err(err) => self.log(
                        goal,
                        "issue.failed",
                        json!({ "bead": bead.id, "error": err.to_string() }),
                    ),
                }
                self.log(
                    goal,
                    "bead.filed",
                    json!({ "bead": bead.id, "request": file }),
                );
                let line = format!(
                    "filed `{}` ({}, P{}{}): {}",
                    bead.id,
                    new.issue_type,
                    new.priority,
                    if new.deferred { ", deferred" } else { "" },
                    new.title
                );
                (Some(bead.id), line)
            }
            Err(err) => {
                self.log(
                    goal,
                    "bead.failed",
                    json!({ "request": file, "error": err.to_string() }),
                );
                (None, format!("could not file `{}`: {err}", new.title))
            }
        }
    }

    /// Set a bead's issue to the bead's type and priority, and record how
    /// that went. It never holds anything else up.
    fn sync_issue(&self, goal: &Goal, project: &Project, bead: &factory_beads::Bead) {
        match crate::sync_issue_fields(&self.factory, project, bead) {
            Ok(changed) if changed.is_empty() => {}
            Ok(changed) => self.log(
                goal,
                "issue.fields",
                json!({ "bead": bead.id, "set": changed }),
            ),
            Err(err) => self.log(
                goal,
                "issue.fields_failed",
                json!({ "bead": bead.id, "error": err.to_string() }),
            ),
        }
    }

    fn edit_bead(
        &self,
        goal: &Goal,
        source: &Source,
        project: &Project,
        edit: &BeadEdit,
    ) -> String {
        match source.change(&edit.bead, &edit.change(), &project.bead_actor) {
            Ok(_) => {
                self.log(goal, "bead.changed", json!({ "edit": edit }));
                // A new priority shows on the bead's issue too.
                if edit.priority >= 0
                    && let Ok(bead) = source.read(&edit.bead)
                {
                    self.sync_issue(goal, project, &bead);
                }
                format!("changed `{}`", edit.bead)
            }
            Err(err) => {
                self.log(
                    goal,
                    "bead.failed",
                    json!({ "edit": edit, "error": err.to_string() }),
                );
                format!("could not change `{}`: {err}", edit.bead)
            }
        }
    }

    /// Wake the director with everything it has not been told, and carry out
    /// what it decides.
    fn wake(&mut self, id: &str) -> Result<(), DriverError> {
        let mut goal = self.goals[id].clone();
        if goal.phase != Phase::Running || goal.inbox.is_empty() {
            return Ok(());
        }
        goal.retry_at = None;
        let definition = self.definition(&goal)?;
        let project = self.config.project(&goal.project)?;
        let source = bead_source(&project);
        let tracker = source.list_open()?;
        let journal_path = self.dir(id).join("journal.md");
        let journal = std::fs::read_to_string(&journal_path).unwrap_or_default();
        let memory = std::fs::read_to_string(self.memory_path(&goal.project)).unwrap_or_default();
        let brief = wake_brief(&definition, &goal, &tracker, &journal, &memory);

        let done = goal.absorbed();
        let launchable = |bead: &str| match source.read(bead) {
            Ok(mut bead) => {
                // Work the open sub-goal has absorbed counts as done.
                bead.open_blockers.retain(|blocker| !done.contains(blocker));
                readiness(&bead, &project.bead_actor)
            }
            Err(_) => vec!["unknown".to_string()],
        };
        let snapshot = goal.clone();
        let check = |result: &Value| {
            decision_problems(&definition, &snapshot, &tracker, &launchable, result)
        };
        let turn = Turn {
            who: "director",
            role: "director",
            tier: definition.director.clone(),
            place: None,
            schema: wake_schema(),
            session: Session::Fresh,
            prompt: brief,
        };
        let seat = self.seat(&goal);
        let result = self.run_agent(&seat, &mut goal.usage, turn, &check);
        let result = match result {
            Ok((result, _)) => result,
            Err(reason) => return self.director_failed(goal, &reason),
        };
        goal.failures = 0;
        goal.wakes += 1;
        goal.inbox.clear();

        // Beads first: a bead the director files may be one it starts next time.
        let mut did: Vec<String> = Vec::new();
        let mut request = Request::default();
        for file in items(&result, "file") {
            match definition.permissions.file_beads {
                Permission::Allow => did.push(self.file_bead(&goal, &source, &project, file).1),
                Permission::Ask => request.files.push(file.clone()),
            }
        }
        for edit in edits_of(&result, "edit") {
            match definition.permissions.edit_beads {
                Permission::Allow => did.push(self.edit_bead(&goal, &source, &project, &edit)),
                Permission::Ask => request.edits.push(edit),
            }
        }
        for dropped in items(&result, "dropped_findings") {
            self.log(&goal, "finding.dropped", dropped.clone());
            did.push(format!(
                "dropped finding {}: {}",
                strings(dropped, "finding"),
                strings(dropped, "reason")
            ));
        }
        let entry = strings(&result, "journal_entry").trim();
        if !entry.is_empty() {
            let text = format!("{journal}\n## Wake {}\n\n{entry}\n", goal.wakes);
            let _ = std::fs::write(&journal_path, text.trim_start());
        }
        let memory = strings(&result, "project_memory").trim();
        if !memory.is_empty() {
            let path = self.memory_path(&goal.project);
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&path, truncate(memory, 12_000));
            self.log(
                &goal,
                "memory.updated",
                json!({ "chars": memory.chars().count() }),
            );
        }

        let mut text = format!(
            ":brain: *Director* · wake {}\n{}",
            goal.wakes,
            quote(&truncate(strings(&result, "summary"), 2000))
        );
        if !did.is_empty() {
            text.push_str(&format!(
                "\n{}",
                did.iter()
                    .map(|line| format!("• {}", escape(line)))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        self.say(&goal, &text);

        if !request.files.is_empty() || !request.edits.is_empty() {
            let mut lines: Vec<String> = request
                .files
                .iter()
                .map(|file| {
                    format!(
                        "• file a new {} (P{}): {}",
                        strings(file, "type"),
                        file.get("priority").and_then(Value::as_i64).unwrap_or(2),
                        escape(strings(file, "title"))
                    )
                })
                .collect();
            lines.extend(
                request
                    .edits
                    .iter()
                    .map(|edit| format!("{}\n   _{}_", edit.describe(), escape(&edit.reason))),
            );
            let text = format!(
                ":pencil2: {} *The director asks to change beads.* Approve, reject, or reply with guidance.\n{}",
                self.mentions(),
                lines.join("\n")
            );
            goal.request = Some(request);
            self.prompt(
                &mut goal,
                &text,
                &[
                    Button {
                        action_id: "request_approve",
                        label: "Approve",
                        value: id,
                        style: Some("primary"),
                    },
                    Button {
                        action_id: "request_reject",
                        label: "Reject",
                        value: id,
                        style: Some("danger"),
                    },
                ],
            );
        }
        let question = strings(&result, "ask_owner").trim();
        if !question.is_empty() {
            self.say(
                &goal,
                &format!(
                    ":question: {} The director needs an answer:\n{}\n_Reply in this thread._",
                    self.mentions(),
                    quote(question)
                ),
            );
        }
        if result.get("complete").and_then(Value::as_bool) == Some(true) {
            let evidence = strings(&result, "evidence").to_string();
            let text = format!(
                ":checkered_flag: {} *The director says the stop condition is met.*\n{}\nPress *Accept* to mark the goal complete, or reply with what is still missing.",
                self.mentions(),
                quote(&truncate(&evidence, 2500))
            );
            goal.evidence = Some(evidence);
            goal.phase = Phase::ProposedComplete;
            self.prompt(
                &mut goal,
                &text,
                &[Button {
                    action_id: "goal_accept",
                    label: "Accept",
                    value: id,
                    style: Some("primary"),
                }],
            );
        }
        // What the decisions name, as they were checked: before any of
        // them changes which sub-goals there are.
        let tells = tells_of(&goal, &result);
        let ships = ships_of(&goal, &result);
        let open = goal
            .current()
            .filter(|_| definition.ship_flow.is_some())
            .map(|sub| sub.id.clone());
        // A new sub-goal gets its branch before anything can start in it.
        let openings: Vec<Value> = openings_of(&result).into_iter().cloned().collect();
        let first_opened = openings
            .first()
            .map(|sub| strings(sub, "id").trim().to_string());
        for sub in &openings {
            match self.open_subgoal(&goal, &project, sub) {
                Ok(sub) => {
                    self.say(&goal, &format!(
                        ":seedling: *Sub-goal opened:* {} (`{}`)\n{}\nBranch `{}`. Its beads are absorbed there and it lands as one pull request.{}",
                        escape(&sub.title),
                        sub.id,
                        quote(&truncate(&sub.objective, 800)),
                        escape(&sub.branch),
                        if sub.paths.is_empty() {
                            String::new()
                        } else {
                            format!(" Its paths: `{}`.", escape(&sub.paths.join("`, `")))
                        }
                    ));
                    self.log(&goal, "subgoal.opened", json!({ "id": sub.id, "branch": sub.branch, "base": sub.base, "planned": sub.planned, "paths": sub.paths, "foreman": definition.foreman.is_some() }));
                    let mut sub = sub;
                    if definition.foreman.is_some() {
                        // Its foreman takes it from here, woken below.
                        sub.foreman = Some(Foreman {
                            inbox: vec![Happening::Opened],
                            ..Foreman::default()
                        });
                    }
                    goal.subgoals.push(sub);
                }
                Err(err) => {
                    // Nothing can start without the branch: tell the director.
                    self.say(
                        &goal,
                        &format!(
                            ":x: Could not open the sub-goal `{}`: {}",
                            escape(strings(sub, "id")),
                            escape(&err.to_string())
                        ),
                    );
                    goal.inbox.push(Happening::Note {
                        text: format!(
                            "The factory could not open the sub-goal `{}` ({err}). Nothing was started in it.",
                            strings(sub, "id")
                        ),
                    });
                    goal.retry_at = Some(now_unix() + 60);
                    self.save(goal)?;
                    return self.wake_foremen(id);
                }
            }
        }
        for (target, text) in &tells {
            let told = goal
                .subgoals
                .iter_mut()
                .find(|sub| &sub.id == target && sub.state == "open")
                .and_then(|sub| sub.foreman.as_mut())
                .map(|foreman| {
                    foreman
                        .inbox
                        .push(Happening::DirectorSaid { text: text.clone() });
                    foreman.waiting = None;
                    foreman.exchanges += 1;
                })
                .is_some();
            if told {
                self.log(
                    &goal,
                    "foreman.told",
                    json!({ "subgoal": target, "text": text }),
                );
            }
        }
        // A bead the director starts itself goes into the one sub-goal
        // there is: the one it just opened, or the one already open.
        let into = open.or(first_opened);
        for start in items(&result, "start") {
            self.start_bead(
                &mut goal,
                &definition,
                into.as_deref(),
                strings(start, "bead"),
                strings(start, "reason"),
                Vec::new(),
            )?;
        }
        for sub in &ships {
            self.ship(&mut goal, &definition, &source, sub)?;
        }
        self.save(goal)?;
        self.wake_foremen(id)
    }

    /// Start one bead's pipeline: inside a sub-goal, from that sub-goal's
    /// tip, or by itself when the goal has no sub-goals.
    fn start_bead(
        &self,
        goal: &mut Goal,
        definition: &GoalDefinition,
        subgoal: Option<&str>,
        bead: &str,
        reason: &str,
        paths: Vec<String>,
    ) -> Result<(), DriverError> {
        let mut within = None;
        let mut paths = paths;
        if let Some(sub) = goal
            .subgoals
            .iter_mut()
            .find(|sub| Some(sub.id.as_str()) == subgoal)
        {
            if paths.is_empty() {
                // With none of its own, a bead keeps to its sub-goal's.
                paths = sub.paths.clone();
            }
            within = Some(Within::Bead {
                tip: factory_git::rev_parse(&sub.worktree, "HEAD")?,
                worktree: sub.worktree.clone(),
                absorbed: sub.absorbed.iter().map(|(bead, _)| bead.clone()).collect(),
                paths: paths.clone(),
            });
            if let Some(foreman) = sub.foreman.as_mut() {
                // Work has started: the talking has got somewhere.
                foreman.exchanges = 0;
            }
        }
        goal.units.push(GoalUnit {
            bead: bead.to_string(),
            unit: None,
            state: "running".to_string(),
            detail: String::new(),
            ship: false,
            subgoal: subgoal.map(str::to_string),
            paths: paths.clone(),
        });
        self.log(
            goal,
            "pipeline.started",
            json!({ "bead": bead, "reason": reason, "subgoal": subgoal, "paths": paths }),
        );
        self.launch(
            goal,
            definition.flow.clone(),
            bead.to_string(),
            within,
            None,
        );
        Ok(())
    }

    /// Wake every foreman of a goal that is owed an event and is not
    /// waiting for the director.
    fn wake_foremen(&mut self, id: &str) -> Result<(), DriverError> {
        let owed: Vec<String> = self
            .goals
            .get(id)
            .map(|goal| {
                goal.subgoals
                    .iter()
                    .filter(|sub| {
                        sub.state == "open"
                            && sub.foreman.as_ref().is_some_and(|foreman| {
                                foreman.waiting.is_none() && !foreman.inbox.is_empty()
                            })
                    })
                    .map(|sub| sub.id.clone())
                    .collect()
            })
            .unwrap_or_default();
        for sub in owed {
            self.wake_foreman(id, &sub)?;
        }
        Ok(())
    }

    /// Wake the foreman of one sub-goal with everything it has not been
    /// told, and carry out what it decides.
    fn wake_foreman(&mut self, id: &str, sub_id: &str) -> Result<(), DriverError> {
        let mut goal = self.goals[id].clone();
        let Some(index) = goal.subgoals.iter().position(|sub| sub.id == sub_id) else {
            return Ok(());
        };
        let sub = goal.subgoals[index].clone();
        let Some(mut foreman) = sub.foreman.clone() else {
            return Ok(());
        };
        if goal.phase != Phase::Running || sub.state != "open" || foreman.inbox.is_empty() {
            return Ok(());
        }
        let definition = self.definition(&goal)?;
        let project = self.config.project(&goal.project)?;
        let source = bead_source(&project);
        let tracker = source.list_open()?;
        let journal_path = self.dir(id).join(format!("foreman-{sub_id}.md"));
        let journal = std::fs::read_to_string(&journal_path).unwrap_or_default();
        let memory = std::fs::read_to_string(self.memory_path(&goal.project)).unwrap_or_default();
        let brief = foreman_brief(&definition, &goal, &sub, &tracker, &journal, &memory);

        let done: Vec<String> = sub.absorbed.iter().map(|(bead, _)| bead.clone()).collect();
        let launchable = |bead: &str| match source.read(bead) {
            Ok(mut bead) => {
                // Work this sub-goal has absorbed counts as done.
                bead.open_blockers.retain(|blocker| !done.contains(blocker));
                readiness(&bead, &project.bead_actor)
            }
            Err(_) => vec!["unknown".to_string()],
        };
        let snapshot = goal.clone();
        let check = |result: &Value| {
            foreman_problems(&definition, &snapshot, &sub, &tracker, &launchable, result)
        };
        let turn = Turn {
            who: "foreman",
            role: "foreman",
            // A definition that lost its foreman mid-goal still has to
            // finish the sub-goals that were opened with one.
            tier: definition
                .foreman
                .clone()
                .unwrap_or_else(|| definition.director.clone()),
            place: Some((format!("foreman-{sub_id}"), sub.branch.clone())),
            schema: foreman_schema(),
            session: Session::Fresh,
            prompt: brief,
        };
        let seat = self.seat(&goal);
        let result = match self.run_agent(&seat, &mut goal.usage, turn, &check) {
            Ok((result, _)) => result,
            Err(reason) => return self.foreman_failed(goal, index, &reason),
        };
        foreman.failures = 0;
        foreman.retry_at = None;
        foreman.wakes += 1;
        foreman.inbox.clear();

        let mut did: Vec<String> = Vec::new();
        let mut here: Vec<String> = Vec::new();
        for file in items(&result, "file") {
            let (bead, line) = self.file_bead(&goal, &source, &project, file);
            did.push(line);
            if let Some(bead) = bead.filter(|_| flag(file, "run_here")) {
                here.push(bead);
            }
        }
        for dropped in items(&result, "dropped_findings") {
            self.log(&goal, "finding.dropped", dropped.clone());
            did.push(format!(
                "dropped finding {}: {}",
                strings(dropped, "finding"),
                strings(dropped, "reason")
            ));
        }
        let entry = strings(&result, "journal_entry").trim();
        if !entry.is_empty() {
            let text = format!("{journal}\n## Wake {}\n\n{entry}\n", foreman.wakes);
            let _ = std::fs::write(&journal_path, text.trim_start());
        }

        let message = strings(&result, "message").trim().to_string();
        foreman.waiting = match strings(&result, "report") {
            "ready" => Some("ready".to_string()),
            "escalate" => Some("escalated".to_string()),
            _ => None,
        };
        let mut text = format!(
            ":construction_worker: *Foreman* of `{sub_id}` · wake {}\n{}",
            foreman.wakes,
            quote(&truncate(strings(&result, "summary"), 2000))
        );
        if !did.is_empty() {
            text.push_str(&format!(
                "\n{}",
                did.iter()
                    .map(|line| format!("• {}", escape(line)))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        if let Some(waiting) = &foreman.waiting {
            foreman.message = message.clone();
            foreman.exchanges += 1;
            text.push_str(&format!(
                "\n:arrow_up: *{}*\n{}",
                if waiting == "ready" {
                    "Ready to ship: over to the director."
                } else {
                    "Escalated to the director."
                },
                quote(&truncate(&message, 1500))
            ));
            goal.inbox.push(Happening::ForemanReported {
                subgoal: sub_id.to_string(),
                report: waiting.clone(),
                message,
            });
        }
        if !here.is_empty() {
            // The beads it filed to run here join the sub-goal, and it is
            // woken again at once to start them.
            foreman.inbox.push(Happening::Filed {
                beads: here.clone(),
            });
            goal.subgoals[index].added.extend(here);
        }
        self.log(
            &goal,
            "foreman.decided",
            json!({ "subgoal": sub_id, "wake": foreman.wakes, "report": strings(&result, "report") }),
        );
        let again = !foreman.inbox.is_empty();
        goal.subgoals[index].foreman = Some(foreman);
        self.say(&goal, &text);
        for start in items(&result, "start") {
            self.start_bead(
                &mut goal,
                &definition,
                Some(sub_id),
                strings(start, "bead"),
                strings(start, "reason"),
                paths_of(start),
            )?;
        }
        self.save(goal)?;
        if again {
            self.wake_foreman(id, sub_id)?;
        }
        // The director, if this left something for it.
        self.wake(id)
    }

    /// What to do when a foreman could not be run: try again later, and
    /// after five failures in a row hand the sub-goal to the director.
    fn foreman_failed(
        &mut self,
        mut goal: Goal,
        index: usize,
        reason: &str,
    ) -> Result<(), DriverError> {
        let id = goal.id.clone();
        let sub_id = goal.subgoals[index].id.clone();
        let Some(mut foreman) = goal.subgoals[index].foreman.clone() else {
            return self.save(goal);
        };
        let mut gave_up = false;
        if let Some(until) = reason
            .strip_prefix(LIMITED)
            .and_then(|at| at.parse::<u64>().ok())
        {
            // Waiting for a usage window is not a failure.
            foreman.retry_at = Some(until);
            self.log(
                &goal,
                "accounts.limited",
                json!({ "until": until, "subgoal": sub_id }),
            );
            self.say(&goal, &format!(
                ":hourglass: Every account is at a usage limit. The foreman of `{sub_id}` wakes at {:02}:{:02} UTC.",
                (until % 86_400) / 3600,
                (until % 3600) / 60
            ));
        } else {
            foreman.failures += 1;
            self.log(
                &goal,
                "foreman.failed",
                json!({ "subgoal": sub_id, "reason": reason, "failures": foreman.failures }),
            );
            gave_up = foreman.failures >= 5;
            if gave_up {
                foreman.failures = 0;
                foreman.retry_at = None;
                foreman.waiting = Some("escalated".to_string());
                foreman.message = format!(
                    "The foreman could not be run five times in a row, so the sub-goal is yours to rule on. The last reason: {reason}"
                );
                foreman.exchanges += 1;
                goal.inbox.push(Happening::ForemanReported {
                    subgoal: sub_id.clone(),
                    report: "escalated".to_string(),
                    message: foreman.message.clone(),
                });
                self.say(&goal, &format!(
                    ":x: The foreman of `{sub_id}` failed five times in a row ({}). Over to the director.",
                    escape(reason)
                ));
            } else {
                foreman.retry_at = Some(now_unix() + 300);
                self.say(&goal, &format!(
                    ":warning: The foreman of `{sub_id}` could not run ({}). Trying again in 5 minutes.",
                    escape(reason)
                ));
            }
        }
        goal.subgoals[index].foreman = Some(foreman);
        self.save(goal)?;
        if gave_up {
            self.wake(&id)?;
        }
        Ok(())
    }

    /// Give a new sub-goal its branch and worktree, from the default branch
    /// as it is now, and run the project's setup in it.
    fn open_subgoal(
        &self,
        goal: &Goal,
        project: &Project,
        spec: &Value,
    ) -> Result<SubGoal, DriverError> {
        let id = strings(spec, "id").trim().to_string();
        let repo = &project.repo;
        factory_git::fetch(repo, &project.remote)?;
        let base = factory_git::rev_parse(
            repo,
            &format!("{}/{}", project.remote, project.default_branch),
        )?;
        let branch = format!("{}/{id}", project.branch_prefix("feature"));
        if factory_git::branch_exists(repo, &branch)? {
            return Err(DriverError::Preflight(format!(
                "the branch `{branch}` already exists in {}",
                repo.display()
            )));
        }
        let worktree = self
            .factory
            .state_dir
            .join("worktrees")
            .join(format!("{}-{id}", goal.id));
        if let Some(parent) = worktree.parent() {
            std::fs::create_dir_all(parent).map_err(crate::io(parent))?;
        }
        factory_git::worktree_add(repo, &worktree, &branch, &base)?;
        for name in &project.setup {
            let command = &project.commands[name];
            let ran = factory_checks::run_command(
                &command.argv,
                &worktree,
                Duration::from_secs(command.timeout_secs),
            );
            if ran.verdict != factory_checks::Verdict::Passed {
                return Err(DriverError::Preflight(format!(
                    "setup command `{name}` did not pass in the sub-goal's worktree ({:?})",
                    ran.verdict
                )));
            }
        }
        Ok(SubGoal {
            id,
            title: strings(spec, "title").trim().to_string(),
            objective: strings(spec, "objective").trim().to_string(),
            planned: items(spec, "beads")
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
            branch,
            worktree,
            base,
            state: "open".to_string(),
            absorbed: Vec::new(),
            ship_unit: None,
            pull_request: None,
            foreman: None,
            added: Vec::new(),
            paths: match paths_of(spec) {
                own if !own.is_empty() => own,
                _ => goal
                    .charter
                    .as_ref()
                    .map(|charter| charter.paths.clone())
                    .unwrap_or_default(),
            },
            queued_at: None,
        })
    }

    /// Ship a sub-goal, or put it in line. Sub-goals of a project ship one
    /// at a time, so that each is verified on top of the one that landed
    /// before it: its ship pipeline begins by taking in the default branch.
    fn ship(
        &mut self,
        goal: &mut Goal,
        definition: &GoalDefinition,
        source: &Source,
        id: &str,
    ) -> Result<(), DriverError> {
        let Some(index) = goal.subgoals.iter().position(|sub| sub.id == id) else {
            return Ok(());
        };
        let others = self
            .goals
            .values()
            .filter(|other| other.project == goal.project && other.id != goal.id);
        let project: Vec<&Goal> = others.chain(std::iter::once(&*goal)).collect();
        let ahead = project
            .iter()
            .flat_map(|goal| goal.units.iter())
            .find(|unit| unit.ship && unit.state == "running")
            .map(|unit| unit.bead.clone());
        let Some(ahead) = ahead else {
            return self.start_ship(goal, definition, source, index);
        };
        // After the last one already in line, in the order they were asked for.
        let last = project
            .iter()
            .flat_map(|goal| goal.subgoals.iter())
            .filter(|sub| sub.state == "queued")
            .filter_map(|sub| sub.queued_at)
            .max();
        let ticket = last.map_or_else(now_unix, |last| last + 1);
        let sub = &mut goal.subgoals[index];
        sub.state = "queued".to_string();
        sub.queued_at = Some(ticket);
        let sub = sub.id.clone();
        self.log(
            goal,
            "subgoal.queued",
            json!({ "id": sub, "behind": ahead, "ticket": ticket }),
        );
        self.say(goal, &format!(
            ":hourglass_flowing_sand: *Sub-goal* `{sub}` *is next to ship.* `{}` is shipping now; sub-goals land one at a time, each verified on top of the last.",
            escape(&ahead)
        ));
        Ok(())
    }

    /// Start the sub-goal that has waited longest to ship, if none of its
    /// project is shipping. Called whenever a ship pipeline ends.
    fn start_next_ship(&mut self, project: &str) -> Result<(), DriverError> {
        let of_project = || self.goals.values().filter(|goal| goal.project == project);
        if of_project()
            .flat_map(|goal| goal.units.iter())
            .any(|unit| unit.ship && unit.state == "running")
        {
            return Ok(());
        }
        let next = of_project()
            .filter(|goal| goal.phase == Phase::Running)
            .flat_map(|goal| {
                goal.subgoals
                    .iter()
                    .filter(|sub| sub.state == "queued")
                    .map(|sub| {
                        (
                            sub.queued_at.unwrap_or(u64::MAX),
                            goal.id.clone(),
                            sub.id.clone(),
                        )
                    })
            })
            .min();
        let Some((_, goal, sub)) = next else {
            return Ok(());
        };
        let mut goal = self.goals[&goal].clone();
        let definition = self.definition(&goal)?;
        let source = bead_source(&self.config.project(&goal.project)?);
        let Some(index) = goal.subgoals.iter().position(|own| own.id == sub) else {
            return Ok(());
        };
        self.start_ship(&mut goal, &definition, &source, index)?;
        self.save(goal)
    }

    /// Run the heavy pipeline, once, on a sub-goal's branch. A sub-goal
    /// whose shipping parked is continued where it stopped.
    fn start_ship(
        &mut self,
        goal: &mut Goal,
        definition: &GoalDefinition,
        source: &Source,
        index: usize,
    ) -> Result<(), DriverError> {
        let sub = goal.subgoals[index].clone();
        let closes: Vec<String> = sub.absorbed.iter().map(|(bead, _)| bead.clone()).collect();
        let mut criteria = String::new();
        let mut list = String::new();
        for bead in &closes {
            if let Ok(bead) = source.read(bead) {
                list.push_str(&format!("- {}: {}\n", bead.id, bead.title));
                criteria.push_str(&format!(
                    "### {}: {}\n\n{}\n\n",
                    bead.id,
                    bead.title,
                    bead.acceptance_criteria.trim()
                ));
            }
        }
        let project = self.config.project(&goal.project)?;
        let issues: Vec<u64> = closes
            .iter()
            .filter_map(|bead| source.read(bead).ok())
            .filter_map(|bead| {
                let github = project.github.as_ref()?;
                factory_github::issue_number(&bead.external_ref, &github.repo)
            })
            .collect();
        let ship = Ship {
            issues,
            id: format!("{}-{}", goal.project, sub.id),
            title: sub.title.clone(),
            description: format!(
                "{}\n\nThis is a sub-goal of the goal `{}`. Its branch holds one commit for each of these beads, each already built, adversarially tested and reviewed on its own:\n\n{list}",
                sub.objective, goal.id
            ),
            acceptance_criteria: criteria,
            worktree: sub.worktree.clone(),
            branch: sub.branch.clone(),
            base: sub.base.clone(),
            closes,
        };
        goal.units.push(GoalUnit {
            bead: sub.id.clone(),
            unit: None,
            state: "running".to_string(),
            detail: String::new(),
            ship: true,
            subgoal: Some(sub.id.clone()),
            paths: Vec::new(),
        });
        goal.subgoals[index].state = "shipping".to_string();
        goal.subgoals[index].queued_at = None;
        self.log(
            goal,
            "subgoal.shipping",
            json!({ "id": sub.id, "beads": ship.closes, "resumes": sub.ship_unit }),
        );
        self.say(goal, &format!(
            ":package: *Shipping sub-goal* `{}`: {} bead(s), one pull request. The whole verification runs once on its branch.",
            sub.id,
            ship.closes.len()
        ));
        let flow = definition.ship_flow.clone();
        self.launch(
            goal,
            flow,
            sub.id.clone(),
            Some(Within::Ship(ship)),
            sub.ship_unit.clone(),
        );
        Ok(())
    }

    /// Run one pipeline on a thread of its own, and report back when it ends.
    /// `name` is the bead, or the sub-goal when `within` ships one.
    fn launch(
        &self,
        goal: &Goal,
        flow: Option<String>,
        name: String,
        within: Option<Within>,
        resume: Option<String>,
    ) {
        // A pipeline continued after a restart carries no `within`: what it
        // is comes from the goal's own record of it.
        let recorded = goal
            .units
            .iter()
            .rev()
            .find(|unit| unit.bead == name && unit.state == "running")
            .is_some_and(|unit| unit.ship);
        let (config, sender) = (self.config.clone(), self.sender.clone());
        let (goal_id, project) = (goal.id.clone(), goal.project.clone());
        let ship = recorded || matches!(within, Some(Within::Ship(_)));
        let options = Options {
            slack: self.unit_slack,
            flow,
            within,
        };
        std::thread::spawn(move || {
            let ended =
                |outcome: &str, detail: String, driver: Option<&Driver>| Happening::PipelineEnded {
                    bead: name.clone(),
                    unit: driver.map(|driver| driver.unit().id.to_string()),
                    outcome: outcome.to_string(),
                    detail,
                    pull_request: driver.and_then(|driver| driver.unit().pull_request.clone()),
                    findings: driver
                        .map(|driver| {
                            driver
                                .unit()
                                .results
                                .iter()
                                .flat_map(|(step, result)| {
                                    items(result, "findings").iter().map(move |finding| {
                                        let mut finding = finding.clone();
                                        finding["step"] = json!(step);
                                        finding
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                    commit: driver.and_then(|driver| driver.unit().absorbed.clone()),
                    ship,
                    strayed: driver
                        .map(|driver| driver.unit().strayed.clone())
                        .unwrap_or_default(),
                };
            let started = match &resume {
                Some(unit) => Driver::resume(config, unit, &options, None),
                None => Driver::start(config, &project, &name, &options),
            };
            let happening = match started {
                Err(err) => ended("refused", err.to_string(), None),
                Ok(mut driver) => {
                    let _ = sender.send(Internal::Began {
                        goal: goal_id.clone(),
                        bead: name.clone(),
                        unit: driver.unit().id.to_string(),
                    });
                    match driver.run() {
                        Ok(Status::Ended) if driver.unit().merged.is_some() => ended(
                            "landed",
                            if ship {
                                "Its pull request is merged and its beads are closed.".to_string()
                            } else {
                                "Its change is merged and its bead is closed.".to_string()
                            },
                            Some(&driver),
                        ),
                        Ok(Status::Ended) if driver.unit().absorbed.is_some() => ended(
                            "absorbed",
                            "Its change is one commit on the sub-goal's branch. The bead stays open until the sub-goal lands.".to_string(),
                            Some(&driver),
                        ),
                        Ok(Status::Ended) => ended(
                            "published",
                            "Its pull request is open.".to_string(),
                            Some(&driver),
                        ),
                        Ok(Status::Parked(reason)) => {
                            ended("parked", format!("Parked: {reason}"), Some(&driver))
                        }
                        Ok(Status::Running) => ended(
                            "failed",
                            "It stopped without ending.".to_string(),
                            Some(&driver),
                        ),
                        Err(err) => ended("failed", err.to_string(), Some(&driver)),
                    }
                }
            };
            let _ = sender.send(Internal::Ended {
                goal: goal_id,
                happening,
            });
        });
    }

    /// Handle what pipelines reported, waiting up to `wait` for the first
    /// report, and retry directors whose time has come. Returns whether
    /// anything happened.
    pub fn pump(&mut self, wait: Duration) -> Result<bool, DriverError> {
        let mut happened = false;
        let due: Vec<String> = self
            .goals
            .values()
            .filter(|goal| goal.retry_at.is_some_and(|at| at <= now_unix()))
            .map(|goal| goal.id.clone())
            .collect();
        for id in due {
            happened = true;
            self.wake(&id)?;
        }
        let due: Vec<(String, String)> = self
            .goals
            .values()
            .filter(|goal| goal.phase == Phase::Running)
            .flat_map(|goal| {
                goal.subgoals
                    .iter()
                    .filter(|sub| {
                        sub.state == "open"
                            && sub
                                .foreman
                                .as_ref()
                                .and_then(|foreman| foreman.retry_at)
                                .is_some_and(|at| at <= now_unix())
                    })
                    .map(|sub| (goal.id.clone(), sub.id.clone()))
            })
            .collect();
        for (goal, sub) in due {
            happened = true;
            self.wake_foreman(&goal, &sub)?;
        }
        let Ok(first) = self.receiver.recv_timeout(wait) else {
            return Ok(happened);
        };
        let mut reports = vec![first];
        reports.extend(self.receiver.try_iter());
        for report in reports {
            match report {
                Internal::Began { goal, bead, unit } => {
                    let Some(mut goal) = self.goals.get(&goal).cloned() else {
                        continue;
                    };
                    let mut ship = false;
                    if let Some(entry) = goal
                        .units
                        .iter_mut()
                        .rev()
                        .find(|u| u.bead == bead && u.state == "running")
                    {
                        entry.unit = Some(unit.clone());
                        ship = entry.ship;
                    }
                    if ship && let Some(sub) = goal.subgoals.iter_mut().find(|sub| sub.id == bead) {
                        // Remembered, so that shipping can be continued if it parks.
                        sub.ship_unit = Some(unit.clone());
                    }
                    self.say(&goal, &format!(
                        ":arrow_forward: {} `{}` started (run `{unit}`). It reports in its own thread in this channel.",
                        if ship { "Shipping of sub-goal" } else { "Pipeline for" },
                        escape(&bead)
                    ));
                    self.save(goal)?;
                }
                Internal::Ended { goal, happening } => {
                    let Some(mut goal) = self.goals.get(&goal).cloned() else {
                        continue;
                    };
                    let id = goal.id.clone();
                    let mut happening = happening;
                    mark_shipping(&goal, &mut happening);
                    let shipped = matches!(&happening, Happening::PipelineEnded { ship: true, .. });
                    let project = goal.project.clone();
                    // The sub-goal a bead's pipeline ran in.
                    let mut belongs: Option<String> = None;
                    if let Happening::PipelineEnded {
                        bead,
                        outcome,
                        detail,
                        unit,
                        pull_request,
                        commit,
                        ship,
                        ..
                    } = &happening
                    {
                        if let Some(entry) = goal
                            .units
                            .iter_mut()
                            .rev()
                            .find(|u| &u.bead == bead && u.state == "running")
                        {
                            entry.state = outcome.clone();
                            entry.detail = truncate(detail, 300);
                            entry.unit = unit.clone().or(entry.unit.take());
                            belongs = entry.subgoal.clone().filter(|_| !*ship);
                        }
                        if !*ship && belongs.is_none() {
                            // A record from before pipelines named their sub-goal.
                            belongs = goal
                                .subgoals
                                .iter()
                                .find(|sub| sub.state == "open")
                                .map(|sub| sub.id.clone());
                        }
                        if *ship {
                            if let Some(sub) = goal.subgoals.iter_mut().find(|sub| &sub.id == bead)
                            {
                                sub.state = if outcome == "landed" {
                                    "landed"
                                } else {
                                    "parked"
                                }
                                .to_string();
                                sub.pull_request = pull_request.clone().or(sub.pull_request.take());
                            }
                            // A landed sub-goal's foreman has nothing left to read.
                            let reading = self.dir(&goal.id).join(format!("foreman-{bead}"));
                            if outcome == "landed"
                                && reading.exists()
                                && let Ok(project) = self.config.project(&goal.project)
                            {
                                let _ = factory_git::worktree_remove(&project.repo, &reading);
                            }
                        } else if let (Some(commit), Some(sub)) = (
                            commit,
                            goal.subgoals
                                .iter_mut()
                                .find(|sub| Some(&sub.id) == belongs.as_ref()),
                        ) {
                            sub.absorbed.push((bead.clone(), commit.clone()));
                        }
                        self.say(
                            &goal,
                            &format!(
                                "{} {} `{}` ended: *{outcome}*. {}{}",
                                if matches!(outcome.as_str(), "landed" | "absorbed") {
                                    ":white_check_mark:"
                                } else {
                                    ":double_vertical_bar:"
                                },
                                if *ship {
                                    "Shipping of sub-goal"
                                } else {
                                    "Pipeline for"
                                },
                                escape(bead),
                                escape(&truncate(detail, 500)),
                                pull_request
                                    .as_ref()
                                    .map(|url| format!(" <{url}|Pull request>"))
                                    .unwrap_or_default()
                            ),
                        );
                    }
                    // A bead's pipeline reports to its sub-goal's foreman,
                    // when there is one; everything else to the director.
                    let foreman = goal
                        .subgoals
                        .iter_mut()
                        .find(|sub| Some(&sub.id) == belongs.as_ref())
                        .and_then(|sub| sub.foreman.as_mut());
                    match (foreman, belongs) {
                        (Some(foreman), Some(sub)) => {
                            foreman.inbox.push(happening);
                            self.save(goal)?;
                            self.wake_foreman(&id, &sub)?;
                        }
                        _ => {
                            goal.inbox.push(happening);
                            self.save(goal)?;
                            if shipped {
                                // Landed or parked, its turn is over.
                                self.start_next_ship(&project)?;
                            }
                            self.wake(&id)?;
                        }
                    }
                }
            }
        }
        Ok(true)
    }

    /// Pick up where a previous process stopped: continue pipelines that were
    /// running and wake directors that were owed an event.
    pub fn resume(&mut self) -> Result<(), DriverError> {
        let ids: Vec<String> = self.goals.keys().cloned().collect();
        for id in ids {
            let mut goal = self.goals[&id].clone();
            if !matches!(goal.phase, Phase::Running | Phase::Stopped) {
                continue;
            }
            let definition = self.definition(&goal)?;
            let mut lost: Vec<(String, Option<String>)> = Vec::new();
            for unit in goal.units.iter().filter(|unit| unit.state == "running") {
                match &unit.unit {
                    Some(unit_id) => {
                        let flow = if unit.ship {
                            definition.ship_flow.clone()
                        } else {
                            definition.flow.clone()
                        };
                        self.launch(&goal, flow, unit.bead.clone(), None, Some(unit_id.clone()));
                    }
                    // It never got as far as having a unit. Whoever started
                    // it is told, and decides again with the tracker as it
                    // is now.
                    None => lost.push((
                        unit.bead.clone(),
                        unit.subgoal.clone().filter(|_| !unit.ship),
                    )),
                }
            }
            for (bead, subgoal) in lost {
                if let Some(entry) = goal
                    .units
                    .iter_mut()
                    .find(|u| u.bead == bead && u.state == "running")
                {
                    entry.state = "failed".to_string();
                }
                if let Some(sub) = goal
                    .subgoals
                    .iter_mut()
                    .find(|sub| sub.id == bead && sub.state == "shipping")
                {
                    sub.state = "parked".to_string();
                }
                let note = Happening::Note {
                    text: format!(
                        "The factory restarted before `{bead}` had started. It did not run."
                    ),
                };
                match goal
                    .subgoals
                    .iter_mut()
                    .find(|sub| Some(&sub.id) == subgoal.as_ref())
                    .and_then(|sub| sub.foreman.as_mut())
                {
                    Some(foreman) => foreman.inbox.push(note),
                    None => goal.inbox.push(note),
                }
            }
            self.save(goal)?;
            self.wake(&id)?;
            self.wake_foremen(&id)?;
        }
        // A goal may have ended just before the restart.
        let projects: std::collections::BTreeSet<String> = self
            .goals
            .values()
            .map(|goal| goal.project.clone())
            .collect();
        for project in projects {
            self.start_queued(&project)?;
            self.start_next_ship(&project)?;
        }
        Ok(())
    }

    /// Serve for ever: Slack events as they arrive, pipelines as they end.
    pub fn serve(&mut self, listener: &Listener) {
        loop {
            while let Ok(incoming) = listener.incoming.try_recv() {
                if let Err(err) = self.incoming(incoming) {
                    eprintln!("goals: {err}");
                }
            }
            if let Err(err) = self.pump(Duration::from_millis(500)) {
                eprintln!("goals: {err}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definition(max: u32, edit: &str) -> GoalDefinition {
        toml::from_str(&format!(
            "id = \"d\"\nmax_pipelines = {max}\n[director]\nmodel = \"m\"\neffort = \"high\"\n[permissions]\nedit_beads = \"{edit}\"\n"
        ))
        .unwrap()
    }

    fn goal() -> Goal {
        serde_json::from_value(json!({
            "id": "toy-g1", "project": "toy", "definition": "d", "phase": "running",
            "channel": "C", "thread_ts": "1.0", "opened_by": "U", "intake_session": null,
            "charter": {"objective": "Ship it", "stop_when": "It shipped", "plan": "toy-1 first"},
            "charter_edits": [], "units": [], "inbox": [{"kind": "started"}], "request": null,
            "prompt": null, "evidence": null, "wakes": 0, "failures": 0, "retry_at": null,
            "usage": Usage::default(), "started_at": 0
        }))
        .unwrap()
    }

    fn tracker() -> Vec<Summary> {
        let bead = |id: &str, criteria: bool| Summary {
            id: id.to_string(),
            title: format!("Bead {id}"),
            status: "open".to_string(),
            issue_type: "task".to_string(),
            priority: 1,
            assignee: String::new(),
            has_criteria: criteria,
            blocked: false,
            blockers: Vec::new(),
        };
        vec![bead("toy-1", true), bead("toy-2", false)]
    }

    fn launchable(bead: &str) -> Vec<String> {
        match bead {
            "toy-1" => Vec::new(),
            "toy-2" => vec!["it has no acceptance criteria".to_string()],
            _ => vec!["unknown".to_string()],
        }
    }

    fn result(extra: Value) -> Value {
        let mut result = json!({
            "summary": "Decided.", "start": [], "file": [], "edit": [], "dropped_findings": [],
            "ask_owner": "", "complete": false, "evidence": "", "journal_entry": "", "project_memory": ""
        });
        for (key, value) in extra.as_object().unwrap() {
            result[key] = value.clone();
        }
        result
    }

    fn problems(definition: &GoalDefinition, goal: &Goal, extra: Value) -> Vec<String> {
        decision_problems(definition, goal, &tracker(), &launchable, &result(extra))
    }

    #[test]
    fn starting_a_launchable_bead_within_the_cap_is_allowed() {
        let start = json!({"start": [{"bead": "toy-1", "reason": "first"}]});
        assert_eq!(
            problems(&definition(1, "ask"), &goal(), start),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_start_that_is_not_launchable_unknown_or_over_the_cap_is_refused() {
        let d = definition(1, "ask");
        assert_eq!(
            problems(
                &d,
                &goal(),
                json!({"start": [{"bead": "toy-2", "reason": ""}]})
            ),
            ["start: `toy-2` may not be launched: it has no acceptance criteria"]
        );
        assert_eq!(
            problems(
                &d,
                &goal(),
                json!({"start": [{"bead": "toy-9", "reason": ""}]})
            ),
            ["start: `toy-9` is not an open bead of this project"]
        );
        let mut busy = goal();
        busy.units.push(GoalUnit {
            bead: "toy-0".into(),
            unit: None,
            state: "running".into(),
            detail: String::new(),
            ship: false,
            subgoal: None,
            paths: Vec::new(),
        });
        assert_eq!(
            problems(
                &d,
                &busy,
                json!({"start": [{"bead": "toy-1", "reason": ""}]})
            ),
            ["start: that would make 2 pipelines at once, and this goal may run 1"]
        );
        // The same decision is fine for a goal allowed two.
        assert!(
            problems(
                &definition(2, "ask"),
                &busy,
                json!({"start": [{"bead": "toy-1", "reason": ""}]})
            )
            .is_empty()
        );
    }

    /// A definition that works in sub-goals.
    fn batched() -> GoalDefinition {
        toml::from_str("id = \"d\"\nflow = \"bead\"\nship_flow = \"ship\"\n[director]\nmodel = \"m\"\neffort = \"high\"\n").unwrap()
    }

    fn with_subgoal(state: &str, absorbed: &[&str]) -> Goal {
        let mut goal = goal();
        goal.subgoals.push(SubGoal {
            id: "signals".into(),
            title: "Signals".into(),
            objective: "Signals work.".into(),
            planned: vec!["toy-1".into(), "toy-2".into()],
            branch: "feat/signals".into(),
            worktree: PathBuf::from("/tmp/signals"),
            base: "abc".into(),
            state: state.into(),
            absorbed: absorbed
                .iter()
                .map(|bead| (bead.to_string(), "c0ffee".to_string()))
                .collect(),
            ship_unit: None,
            pull_request: None,
            foreman: None,
            added: Vec::new(),
            paths: Vec::new(),
            queued_at: None,
        });
        goal
    }

    /// A definition whose sub-goals are each run by a foreman.
    fn tiered(file: &str) -> GoalDefinition {
        toml::from_str(&format!(
            "id = \"d\"\nflow = \"bead\"\nship_flow = \"ship\"\n[director]\nmodel = \"m\"\neffort = \"high\"\n[foreman]\nmodel = \"f\"\neffort = \"high\"\n[permissions]\nfile_beads = \"{file}\"\n"
        ))
        .unwrap()
    }

    fn with_foreman(absorbed: &[&str], waiting: Option<&str>) -> Goal {
        let mut goal = with_subgoal("open", absorbed);
        goal.subgoals[0].foreman = Some(Foreman {
            waiting: waiting.map(str::to_string),
            message: "Both are in.".into(),
            ..Foreman::default()
        });
        goal
    }

    fn foreman_says(definition: &GoalDefinition, goal: &Goal, extra: Value) -> Vec<String> {
        let mut result = json!({
            "summary": "Decided.", "start": [], "file": [], "dropped_findings": [],
            "report": "working", "message": "", "journal_entry": ""
        });
        for (key, value) in extra.as_object().unwrap() {
            result[key] = value.clone();
        }
        foreman_problems(
            definition,
            goal,
            &goal.subgoals[0],
            &tracker(),
            &launchable,
            &result,
        )
    }

    #[test]
    fn a_director_with_foremen_opens_answers_and_ships_but_starts_no_bead() {
        let d = tiered("allow");
        let open = json!({"id": "signals", "title": "Signals", "objective": "They work.", "beads": ["toy-1"]});
        // Opening a sub-goal is enough: its foreman takes it from there.
        assert!(problems(&d, &goal(), json!({"open_subgoal": open.clone()})).is_empty());
        let both = problems(
            &d,
            &goal(),
            json!({"open_subgoal": open, "start": [{"bead": "toy-1", "reason": ""}]}),
        );
        assert_eq!(both.len(), 1, "{both:?}");
        assert!(both[0].starts_with("start: each sub-goal of this goal is run by its foreman"));

        // While the foreman works, the director may simply answer the owner.
        assert!(problems(&d, &with_foreman(&[], None), json!({})).is_empty());
        // Once the foreman waits, the director has to move the goal itself.
        let waiting = with_foreman(&["toy-1"], Some("ready"));
        let idle = problems(&d, &waiting, json!({}));
        assert_eq!(idle.len(), 1, "{idle:?}");
        assert!(idle[0].contains("answer the foreman with `tell_foreman`"));
        assert!(problems(&d, &waiting, json!({"ship": true})).is_empty());
        assert!(problems(&d, &waiting, json!({"tell_foreman": "Run toy-2 too."})).is_empty());
        assert!(problems(&d, &waiting, json!({"ask_owner": "Ship without toy-2?"})).is_empty());
        assert_eq!(
            problems(
                &d,
                &waiting,
                json!({"ship": true, "tell_foreman": "And another thing."})
            ),
            ["tell_foreman: ship the sub-goal or write to its foreman, not both"]
        );

        // Talk that starts nothing is cut off.
        let mut circling = with_foreman(&["toy-1"], Some("escalated"));
        circling.subgoals[0].foreman.as_mut().unwrap().exchanges = MAX_EXCHANGES;
        let cut = problems(&d, &circling, json!({"tell_foreman": "Try again."}));
        assert!(
            cut[0].contains("have passed 6 messages without a pipeline starting"),
            "{cut:?}"
        );

        // There has to be a foreman to tell.
        assert!(
            problems(
                &d,
                &goal(),
                json!({"tell_foreman": "Hello?", "ask_owner": "?"})
            )
            .contains(&"tell_foreman: no sub-goal is open".to_string())
        );
        assert_eq!(
            problems(
                &d,
                &with_subgoal("open", &[]),
                json!({"tell_foreman": "Hello?"})
            ),
            ["tell_foreman: sub-goal `signals` has no foreman: you start its beads yourself"]
        );
        // A sub-goal opened before the definition had a foreman stays the
        // director's to run.
        assert!(
            problems(
                &d,
                &with_subgoal("open", &[]),
                json!({"start": [{"bead": "toy-1", "reason": ""}]})
            )
            .is_empty()
        );
    }

    #[test]
    fn a_foreman_starts_its_own_beads_and_never_leaves_its_sub_goal_idle() {
        let d = tiered("allow");
        let fresh = with_foreman(&[], None);
        let start = |bead: &str| json!({"start": [{"bead": bead, "reason": ""}]});
        assert!(foreman_says(&d, &fresh, start("toy-1")).is_empty());
        // Planned, but it cannot run.
        assert_eq!(
            foreman_says(&d, &fresh, start("toy-2")),
            ["start: `toy-2` may not be launched: it has no acceptance criteria"]
        );
        // Not this sub-goal's.
        let stray = foreman_says(&d, &fresh, start("toy-9"));
        assert!(stray[0].starts_with("start: `toy-9` is not one of this sub-goal's beads. The director planned toy-1, toy-2"), "{stray:?}");
        // A bead it filed to run here is.
        let mut grown = with_foreman(&[], None);
        grown.subgoals[0].planned = vec!["toy-2".into()];
        assert!(
            foreman_says(&d, &grown, start("toy-1"))[0]
                .contains("not one of this sub-goal's beads")
        );
        grown.subgoals[0].added = vec!["toy-1".into()];
        assert!(foreman_says(&d, &grown, start("toy-1")).is_empty());

        // Nothing running and nobody told: refused.
        let idle = foreman_says(&d, &fresh, json!({}));
        assert!(
            idle[0].starts_with("working: nothing would be running in this sub-goal"),
            "{idle:?}"
        );
        // One at a time.
        let mut busy = with_foreman(&[], None);
        busy.units.push(GoalUnit {
            bead: "toy-0".into(),
            unit: None,
            state: "running".into(),
            detail: String::new(),
            ship: false,
            subgoal: Some("signals".into()),
            paths: Vec::new(),
        });
        assert!(foreman_says(&d, &busy, json!({})).is_empty());
        assert_eq!(
            foreman_says(&d, &busy, start("toy-1")),
            [
                "start: that would make 2 pipelines at once in this sub-goal, and its foreman may run 1"
            ]
        );

        // Ready means absorbed work, nothing running, and a word to the director.
        let ready = |message: &str| json!({"report": "ready", "message": message});
        assert_eq!(
            foreman_says(&d, &fresh, ready("Done.")),
            [
                "ready: this sub-goal has absorbed nothing, so there is nothing to ship: escalate instead"
            ]
        );
        let absorbed = with_foreman(&["toy-1"], None);
        assert!(foreman_says(&d, &absorbed, ready("toy-1 is in; toy-2 cannot run.")).is_empty());
        assert!(foreman_says(&d, &absorbed, ready(""))[0].starts_with("ready: say in `message`"));
        assert_eq!(
            foreman_says(&d, &absorbed, start("toy-1")),
            ["start: `toy-1` is already absorbed in this sub-goal"]
        );
        let mut both = ready("Done.");
        both["start"] = start("toy-2")["start"].clone();
        assert!(foreman_says(&d, &absorbed, both).contains(
            &"ready: a sub-goal is ready when nothing in it is running, starting or waiting to run"
                .to_string()
        ));
        // Escalating needs the question.
        assert_eq!(
            foreman_says(&d, &fresh, json!({"report": "escalate"})),
            ["escalate: say in `message` what you need from the director"]
        );
        assert!(
            foreman_says(
                &d,
                &fresh,
                json!({"report": "escalate", "message": "toy-2 has no criteria."})
            )
            .is_empty()
        );
        assert_eq!(
            foreman_says(&d, &fresh, json!({"report": "done"})),
            ["`report` is `done`, not `working`, `ready` or `escalate`"]
        );

        // A bead filed to run here keeps the sub-goal moving by itself.
        let file = |here: bool, deferred: bool| {
            json!({"file": [{"title": "Fix the race", "type": "bug", "priority": 1,
                "description": "The tester found it.", "acceptance_criteria": "- fixed",
                "parent": "", "discovered_from": "", "deferred": deferred, "run_here": here}]})
        };
        assert!(foreman_says(&d, &fresh, file(true, false)).is_empty());
        assert!(foreman_says(&d, &fresh, file(false, false))[0].starts_with("working: nothing"));
        assert_eq!(
            foreman_says(&d, &busy, file(true, true)),
            ["file: a bead that is to run here cannot be filed deferred"]
        );
        // One at a time: a bead filed to run here is started before the next.
        let mut owed = with_foreman(&[], None);
        owed.subgoals[0].added = vec!["toy-1".into()];
        assert_eq!(
            foreman_says(&d, &owed, file(true, false)),
            [
                "file: you already filed toy-1 to run here and have not started it: start it, or escalate, before filing another to run here"
            ]
        );
        let mut and_start = file(true, false);
        and_start["start"] = start("toy-1")["start"].clone();
        assert!(foreman_says(&d, &owed, and_start).is_empty());
        // Where filing is the owner's to approve, it goes through the director.
        assert!(
            foreman_says(&tiered("ask"), &busy, file(false, false))[0]
                .starts_with("file: on this goal a new bead is put to the owner")
        );
    }

    #[test]
    fn beads_run_side_by_side_only_on_declared_paths_that_do_not_overlap() {
        // A foreman with two slots, on a goal allowed two pipelines.
        let d: GoalDefinition = toml::from_str(
            "id = \"d\"\nflow = \"bead\"\nship_flow = \"ship\"\nmax_pipelines = 2\n[director]\nmodel = \"m\"\neffort = \"high\"\n[foreman]\nmodel = \"f\"\neffort = \"high\"\nslots = 2\n",
        )
        .unwrap();
        // Two beads that can both be launched.
        let both = |goal: &Goal, first: Value, second: Value| {
            let result = json!({
                "summary": "Decided.", "file": [], "dropped_findings": [],
                "report": "working", "message": "", "journal_entry": "",
                "start": [
                    {"bead": "toy-1", "reason": "", "paths": first},
                    {"bead": "toy-3", "reason": "", "paths": second},
                ],
            });
            let mut tracker = tracker();
            tracker.push(Summary {
                id: "toy-3".into(),
                ..tracker[0].clone()
            });
            foreman_problems(
                &d,
                goal,
                &goal.subgoals[0],
                &tracker,
                &|_| Vec::new(),
                &result,
            )
        };
        let mut goal = with_foreman(&[], None);
        goal.subgoals[0].planned = vec!["toy-1".into(), "toy-3".into()];

        assert!(
            both(
                &goal,
                json!(["src/worker"]),
                json!(["src/client", "docs/client.md"])
            )
            .is_empty()
        );
        // Without paths nothing is known about either, so they take turns.
        assert_eq!(
            both(&goal, json!(["src/worker"]), json!([])),
            [
                "start: `toy-3` would run beside another bead, so it needs `paths`: the files and directories it will change"
            ]
        );
        // A directory holds the files under it; the same name further along does not count.
        assert_eq!(
            both(
                &goal,
                json!(["src/worker/"]),
                json!(["src/worker/poll.zig"])
            ),
            [
                "start: `toy-1` (`src/worker`) and `toy-3` (`src/worker/poll.zig`) would change the same files: beads whose paths overlap run one after the other"
            ]
        );
        assert!(both(&goal, json!(["src/worker"]), json!(["src/worker_pool"])).is_empty());
        // Paths are plain: inside the repository, and no patterns.
        for bad in ["../other", "/etc", "src/**", "."] {
            let problems = both(&goal, json!([bad]), json!(["docs"]));
            assert!(
                problems
                    .iter()
                    .any(|problem| problem.contains("is not a plain path")),
                "{bad}: {problems:?}"
            );
        }

        // What is already running counts: by its paths, or by having none.
        let start_one = |goal: &Goal, paths: Value| {
            let result = json!({
                "summary": "Decided.", "file": [], "dropped_findings": [],
                "report": "working", "message": "", "journal_entry": "",
                "start": [{"bead": "toy-1", "reason": "", "paths": paths}],
            });
            foreman_problems(
                &d,
                goal,
                &goal.subgoals[0],
                &tracker(),
                &launchable,
                &result,
            )
        };
        let running = |paths: &[&str]| {
            let mut goal = goal.clone();
            goal.units.push(GoalUnit {
                bead: "toy-0".into(),
                unit: None,
                state: "running".into(),
                detail: String::new(),
                ship: false,
                subgoal: Some("signals".into()),
                paths: paths.iter().map(|path| path.to_string()).collect(),
            });
            goal
        };
        assert!(start_one(&running(&["src/client"]), json!(["src/worker"])).is_empty());
        assert_eq!(
            start_one(&running(&["src"]), json!(["src/worker"])),
            [
                "start: `toy-0` (`src`) and `toy-1` (`src/worker`) would change the same files: beads whose paths overlap run one after the other"
            ]
        );
        assert_eq!(
            start_one(&running(&[]), json!(["src/worker"])),
            [
                "start: `toy-0` is running with no paths declared, so nothing may start beside it: wait for it to end"
            ]
        );
        // Alone, a bead needs no paths at all.
        assert!(start_one(&goal, json!([])).is_empty());
        // And two slots are a limit like any other.
        let mut full = running(&["src/client"]);
        full.units.push(GoalUnit {
            bead: "toy-9".into(),
            paths: vec!["docs".into()],
            ..full.units[0].clone()
        });
        assert_eq!(
            start_one(&full, json!(["src/worker"])),
            [
                "start: that would make 3 pipelines at once in this sub-goal, and its foreman may run 2"
            ]
        );
    }

    #[test]
    fn sub_goals_are_worked_on_side_by_side_only_on_paths_that_do_not_overlap() {
        // A director with two slots, each sub-goal run by a foreman.
        let d: GoalDefinition = toml::from_str(
            "id = \"d\"\nflow = \"bead\"\nship_flow = \"ship\"\nmax_pipelines = 2\n[director]\nmodel = \"m\"\neffort = \"high\"\nslots = 2\n[foreman]\nmodel = \"f\"\neffort = \"high\"\n",
        )
        .unwrap();
        let sub = |id: &str, paths: Value| json!({"id": id, "title": "A part", "objective": "It works.", "beads": ["toy-1"], "paths": paths});
        let open = |subs: Vec<Value>| json!({"open_subgoals": subs});

        // Two at once, each on its own paths.
        assert!(
            problems(
                &d,
                &goal(),
                open(vec![
                    sub("left", json!(["src/a"])),
                    sub("right", json!(["src/b"]))
                ])
            )
            .is_empty()
        );
        // One alone needs none.
        assert!(problems(&d, &goal(), open(vec![sub("left", json!([]))])).is_empty());
        assert_eq!(
            problems(
                &d,
                &goal(),
                open(vec![sub("left", json!(["src/a"])), sub("right", json!([]))])
            ),
            [
                "open_subgoal: `right` would be worked on beside another sub-goal, so it needs `paths`: the files and directories its beads will change"
            ]
        );
        assert_eq!(
            problems(
                &d,
                &goal(),
                open(vec![
                    sub("left", json!(["src"])),
                    sub("right", json!(["src/b"]))
                ])
            ),
            [
                "open_subgoal: `left` (`src`) and `right` (`src/b`) would change the same files: sub-goals whose paths overlap are worked on one after the other"
            ]
        );
        assert_eq!(
            problems(
                &d,
                &goal(),
                open(vec![sub("left", json!(["a"])), sub("left", json!(["b"]))])
            ),
            ["open_subgoal: this goal already had a sub-goal `left`"]
        );
        // The slots are a limit, counting every sub-goal that has not landed.
        assert_eq!(
            problems(
                &d,
                &goal(),
                open(vec![
                    sub("a", json!(["a"])),
                    sub("b", json!(["b"])),
                    sub("c", json!(["c"]))
                ])
            ),
            [
                "open_subgoal: that would make 3 sub-goals that have not landed, and this goal's director may have 2"
            ]
        );
        // One already there counts, by its paths or by having none.
        let mut one = with_foreman(&[], None);
        assert_eq!(
            problems(&d, &one, open(vec![sub("right", json!(["src/b"]))])),
            [
                "open_subgoal: `signals` has not landed and has no paths declared, so nothing may open beside it: wait for it to land"
            ]
        );
        one.subgoals[0].paths = vec!["src/a".into()];
        assert!(problems(&d, &one, open(vec![sub("right", json!(["src/b"]))])).is_empty());
        assert!(
            problems(&d, &one, open(vec![sub("right", json!(["src/a/x.zig"]))]))[0]
                .contains("would change the same files")
        );
        let full = problems(
            &d,
            &one,
            open(vec![sub("b", json!(["b"])), sub("c", json!(["c"]))]),
        );
        assert_eq!(
            full,
            [
                "open_subgoal: that would make 3 sub-goals that have not landed, and this goal's director may have 2: `signals` still to land"
            ]
        );

        // Two waiting on the director: each is shipped or answered by name.
        let mut two = with_foreman(&["toy-1"], Some("ready"));
        let mut second = two.subgoals[0].clone();
        second.id = "timers".into();
        two.subgoals.push(second);
        assert!(problems(&d, &two, json!({"ship": ["signals", "timers"]})).is_empty());
        assert!(
            problems(
                &d,
                &two,
                json!({"ship": ["timers"], "tell_foreman": [{"subgoal": "signals", "text": "Add toy-2."}]})
            )
            .is_empty()
        );
        assert_eq!(
            problems(&d, &two, json!({"ship": ["signals", "signals"]})),
            ["ship: `signals` is listed twice"]
        );
        assert_eq!(
            problems(&d, &two, json!({"ship": ["signals", "nowhere"]})),
            ["ship: `nowhere` is not a sub-goal waiting to land"]
        );
        assert_eq!(
            problems(
                &d,
                &two,
                json!({"ship": ["signals"], "tell_foreman": [{"subgoal": "nowhere", "text": "Hello?"}]})
            ),
            ["tell_foreman: `nowhere` is not a sub-goal that is open"]
        );
        // One in line to ship will move the goal by itself; one being
        // shipped cannot be shipped again.
        two.subgoals[0].state = "queued".into();
        two.subgoals[1].state = "landed".into();
        assert!(problems(&d, &two, json!({})).is_empty());
        assert_eq!(
            problems(&d, &two, json!({"ship": ["signals"]})),
            ["ship: sub-goal `signals` is already being shipped"]
        );

        // A bead of a sub-goal that has paths keeps inside them.
        let mut fenced = with_foreman(&[], None);
        fenced.subgoals[0].paths = vec!["src/a".into()];
        let start =
            |paths: Value| json!({"start": [{"bead": "toy-1", "reason": "", "paths": paths}]});
        assert!(
            foreman_says(&tiered("allow"), &fenced, start(json!(["src/a/poll.zig"]))).is_empty()
        );
        assert!(foreman_says(&tiered("allow"), &fenced, start(json!([]))).is_empty());
        assert_eq!(
            foreman_says(&tiered("allow"), &fenced, start(json!(["src/b"]))),
            [
                "start: `src/b`, declared for `toy-1`, is outside this sub-goal's own paths (src/a): another sub-goal is being worked on beside this one"
            ]
        );
    }

    #[test]
    fn goals_run_side_by_side_only_on_charters_whose_paths_do_not_overlap() {
        let with_paths = |id: &str, phase: Phase, paths: &[&str]| {
            let mut goal = goal();
            goal.id = id.into();
            goal.phase = phase;
            goal.charter.as_mut().unwrap().paths =
                paths.iter().map(|path| path.to_string()).collect();
            goal
        };
        let running = with_paths("toy-g1", Phase::Running, &["src/worker"]);
        let apart = with_paths("toy-g2", Phase::Queued, &["src/client"]);
        let clashing = with_paths("toy-g3", Phase::Queued, &["src/worker/poll.zig"]);
        let unbounded = with_paths("toy-g4", Phase::Queued, &[]);
        let waiting = [&apart, &clashing, &unbounded];
        let says = |slots: usize, running: &[&Goal], start: &[&str]| {
            let start: Vec<Value> = start
                .iter()
                .map(|goal| json!({"goal": goal, "reason": ""}))
                .collect();
            schedule_problems(
                slots,
                running,
                &waiting,
                &json!({"summary": "Decided.", "start": start}),
            )
        };
        // Apart from what is running: it may start. Leaving all waiting is
        // always allowed, since they start when a running goal ends.
        assert!(says(2, &[&running], &["toy-g2"]).is_empty());
        assert!(says(2, &[&running], &[]).is_empty());
        assert_eq!(
            says(2, &[&running], &["toy-g3"]),
            [
                "start: `toy-g1` (`src/worker`) and `toy-g3` (`src/worker/poll.zig`) would change the same files: goals whose paths overlap run one after the other"
            ]
        );
        // A goal that could touch anything runs alone, whichever side it is on.
        assert_eq!(
            says(2, &[&running], &["toy-g4"]),
            [
                "start: `toy-g4` would run beside another goal, and its charter names no paths, so it runs alone: leave it waiting"
            ]
        );
        let loose = with_paths("toy-g1", Phase::Running, &[]);
        assert_eq!(
            says(2, &[&loose], &["toy-g2"]),
            [
                "start: `toy-g1` is running with no paths in its charter, so nothing may start beside it"
            ]
        );
        // The limit, and goals that are not waiting.
        assert_eq!(
            says(2, &[&running], &["toy-g2", "toy-g4"]),
            ["start: that would make 3 goals running at once, and this project may run 2"]
        );
        assert_eq!(
            says(3, &[&running], &["toy-g9"]),
            ["start: `toy-g9` is not a goal that is waiting"]
        );
        // The executive is shown each goal's paths, and the same text each time.
        let brief = schedule_brief("toy", 2, &[&running], &[&apart, &unbounded]);
        assert_eq!(
            brief,
            schedule_brief("toy", 2, &[&running], &[&apart, &unbounded])
        );
        for expected in [
            "This project may have 2 goals running at once. That is a limit, not a target.",
            "- `toy-g1` (running): Ship it\n  Paths: src/worker.",
            "- `toy-g4` (waiting to run): Ship it\n  Paths: none declared, so it runs alone.",
            "  Its plan: toy-1 first",
        ] {
            assert!(
                brief.contains(expected),
                "missing {expected:?} in:\n{brief}"
            );
        }

        // A goal with paths keeps its sub-goals inside them.
        let d = tiered("allow");
        let fenced = with_paths("toy-g1", Phase::Running, &["src/worker"]);
        let open = |paths: Value| json!({"open_subgoals": [{"id": "polling", "title": "Polling", "objective": "It polls.", "beads": ["toy-1"], "paths": paths}]});
        assert!(problems(&d, &fenced, open(json!(["src/worker/poll.zig"]))).is_empty());
        assert!(problems(&d, &fenced, open(json!([]))).is_empty());
        assert_eq!(
            problems(&d, &fenced, open(json!(["src/client"]))),
            [
                "open_subgoal: `src/client`, declared for `polling`, is outside this goal's own paths (src/worker): another goal may be running beside this one"
            ]
        );
    }

    #[test]
    fn each_tier_is_briefed_on_the_other() {
        let d = tiered("allow");
        let mut goal = with_foreman(&["toy-1"], None);
        let foreman = goal.subgoals[0].foreman.as_mut().unwrap();
        foreman.inbox = vec![
            Happening::Opened,
            Happening::DirectorSaid {
                text: "Leave toy-2 out.".into(),
            },
            Happening::PipelineEnded {
                bead: "toy-1".into(),
                unit: Some("u1".into()),
                outcome: "absorbed".into(),
                detail: String::new(),
                pull_request: None,
                findings: vec![
                    json!({"step": "review", "severity": "minor", "title": "The doc omits the timeout", "location": "a.zig:1"}),
                ],
                commit: Some("c0ffee".into()),
                ship: false,
                strayed: vec!["build.zig".into()],
            },
        ];
        let sub = goal.subgoals[0].clone();
        let brief = foreman_brief(&d, &goal, &sub, &tracker(), "Earlier: started toy-1.", "");
        assert_eq!(
            brief,
            foreman_brief(&d, &goal, &sub, &tracker(), "Earlier: started toy-1.", ""),
            "the same inputs give the same text"
        );
        for expected in [
            "# Sub-goal `signals` of goal `toy-g1` on project `toy`, wake 1",
            "- The director opened this sub-goal and handed it to you.",
            "- The director wrote to you:\n\n> Leave toy-2 out.",
            "- The pipeline for `toy-1` ended: **absorbed**.",
            "  - F1 (review, minor): The doc omits the timeout at `a.zig:1`",
            "  It changed files outside the paths declared for it: build.zig.",
            "- Absorbed, one commit each on the branch: toy-1.",
            "- Not yet absorbed: toy-2.",
            "toy-1 | P1 | task | absorbed in the open sub-goal",
            "Earlier: started toy-1.",
            "- Not yours: shipping, changing a bead that exists, and talking to the owner.",
        ] {
            assert!(
                brief.contains(expected),
                "missing {expected:?} in:\n{brief}"
            );
        }

        // The director is told what the foreman reported, and sees it wait.
        let mut waiting = with_foreman(&["toy-1"], Some("ready"));
        waiting.inbox = vec![Happening::ForemanReported {
            subgoal: "signals".into(),
            report: "ready".into(),
            message: "Both are in.".into(),
        }];
        let brief = wake_brief(&d, &waiting, &tracker(), "", "");
        for expected in [
            "- The foreman of sub-goal `signals` reports it **ready** to ship and waits for you:\n\n> Both are in.",
            "Its foreman reports it ready to ship and waits for you: Both are in.",
            "You do not start beads: `start` is refused.",
        ] {
            assert!(
                brief.contains(expected),
                "missing {expected:?} in:\n{brief}"
            );
        }
    }

    const OPEN: &str = r#"{"id": "signals", "title": "Signals", "objective": "Signals work.", "beads": ["toy-1"]}"#;

    #[test]
    fn a_bead_runs_inside_a_sub_goal_and_one_sub_goal_is_open_at_a_time() {
        let d = batched();
        let start = json!({"start": [{"bead": "toy-1", "reason": "first"}]});
        // No sub-goal: the start is refused. Opening one in the same result is the way.
        assert_eq!(
            problems(&d, &goal(), start.clone()),
            [
                "start: a bead runs inside a sub-goal: open one with `open_subgoal` in the same result"
            ]
        );
        let mut both = start.clone();
        both["open_subgoal"] = serde_json::from_str(OPEN).unwrap();
        assert_eq!(problems(&d, &goal(), both.clone()), Vec::<String>::new());
        // With one open, the start needs nothing more, and a second cannot be opened.
        assert_eq!(
            problems(&d, &with_subgoal("open", &[]), start.clone()),
            Vec::<String>::new()
        );
        let mut second = both.clone();
        second["open_subgoal"]["id"] = json!("timers");
        assert_eq!(
            problems(&d, &with_subgoal("open", &[]), second),
            ["open_subgoal: sub-goal `signals` has not landed yet: ship it before opening another"]
        );
        // Once it has landed, the next may open.
        let mut next = both;
        next["open_subgoal"]["id"] = json!("timers");
        assert_eq!(
            problems(&d, &with_subgoal("landed", &["toy-0"]), next),
            Vec::<String>::new()
        );
        // A goal without a ship pipeline has no sub-goals at all.
        let plain =
            json!({"ask_owner": "q", "open_subgoal": serde_json::from_str::<Value>(OPEN).unwrap()});
        assert_eq!(problems(&definition(1, "ask"), &goal(), plain).len(), 1);
        // An absorbed bead is not run again, and nothing joins a sub-goal being shipped.
        assert_eq!(
            problems(&d, &with_subgoal("open", &["toy-1"]), start.clone()),
            ["start: `toy-1` is already absorbed in the open sub-goal"]
        );
        assert_eq!(
            problems(&d, &with_subgoal("parked", &["toy-0"]), start),
            ["start: sub-goal `signals` is being shipped: nothing more can join it"]
        );
    }

    #[test]
    fn a_sub_goal_ships_when_it_holds_work_and_nothing_else_is_running() {
        let d = batched();
        let ship = json!({"ship": true});
        assert_eq!(
            problems(&d, &with_subgoal("open", &["toy-1"]), ship.clone()),
            Vec::<String>::new()
        );
        // Shipping again after it parked continues it.
        assert_eq!(
            problems(&d, &with_subgoal("parked", &["toy-1"]), ship.clone()),
            Vec::<String>::new()
        );
        assert_eq!(
            problems(&d, &with_subgoal("open", &[]), ship.clone()),
            ["ship: sub-goal `signals` has absorbed no bead yet"]
        );
        assert_eq!(
            problems(&d, &goal(), ship.clone()),
            ["ship: no sub-goal is open"]
        );
        let mut busy = with_subgoal("open", &["toy-1"]);
        busy.units.push(GoalUnit {
            bead: "toy-2".into(),
            unit: None,
            state: "running".into(),
            detail: String::new(),
            ship: false,
            subgoal: None,
            paths: Vec::new(),
        });
        assert_eq!(
            problems(&d, &busy, ship),
            ["ship: a sub-goal ships when nothing else is running or starting"]
        );
        // The goal is not complete while absorbed work has not landed.
        let done = json!({"complete": true, "evidence": "All of it."});
        assert_eq!(
            problems(&d, &with_subgoal("open", &["toy-1"]), done.clone()),
            ["complete: sub-goal `signals` holds work that has not landed: ship it first"]
        );
        assert_eq!(
            problems(&d, &with_subgoal("landed", &["toy-1"]), done),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_shipping_pipeline_is_settled_as_one_whatever_its_report_says() {
        // After a restart the continued pipeline reported as an ordinary one,
        // and the sub-goal stayed "shipping" for ever.
        let mut goal = with_subgoal("shipping", &["toy-1"]);
        goal.units.push(GoalUnit {
            bead: "signals".into(),
            unit: Some("u9".into()),
            state: "running".into(),
            detail: String::new(),
            ship: true,
            subgoal: None,
            paths: Vec::new(),
        });
        let ended = |bead: &str| Happening::PipelineEnded {
            bead: bead.into(),
            unit: Some("u9".into()),
            outcome: "landed".into(),
            detail: String::new(),
            pull_request: None,
            findings: Vec::new(),
            commit: None,
            ship: false,
            strayed: Vec::new(),
        };
        let mut report = ended("signals");
        mark_shipping(&goal, &mut report);
        assert!(matches!(
            report,
            Happening::PipelineEnded { ship: true, .. }
        ));
        // A bead's pipeline is left as it is.
        let mut other = ended("toy-2");
        mark_shipping(&goal, &mut other);
        assert!(matches!(
            other,
            Happening::PipelineEnded { ship: false, .. }
        ));
    }

    #[test]
    fn the_brief_shows_each_sub_goal_and_what_it_still_waits_for() {
        let goal = with_subgoal("open", &["toy-1"]);
        let brief = wake_brief(&batched(), &goal, &tracker(), "", "");
        assert!(brief.contains("- `signals` (open): Signals. Branch `feat/signals`. Absorbed: toy-1. Planned and not yet absorbed: toy-2."), "{brief}");
        assert!(
            brief.contains("toy-1 | P1 | task | absorbed in the open sub-goal | Bead toy-1"),
            "{brief}"
        );
        assert!(
            brief.contains("Shipping the sub-goal runs the whole verification once"),
            "{brief}"
        );
        assert!(
            !wake_brief(&definition(1, "ask"), &goal, &tracker(), "", "").contains("## Sub-goals")
        );
    }

    #[test]
    fn a_goal_is_never_left_with_nothing_that_can_move_it() {
        let d = definition(1, "ask");
        assert_eq!(problems(&d, &goal(), json!({})).len(), 1);
        assert!(problems(&d, &goal(), json!({"ask_owner": "Which first?"})).is_empty());
        // An edit that waits for the owner's approval counts as asking.
        let edit = json!({"edit": [{"bead": "toy-2", "acceptance_criteria": "- works", "priority": -1, "defer": "unchanged", "reason": "r"}]});
        assert!(problems(&d, &goal(), edit.clone()).is_empty());
        // Applied at once, it leaves nothing running.
        assert_eq!(problems(&definition(1, "allow"), &goal(), edit).len(), 1);
    }

    #[test]
    fn completion_needs_evidence_and_no_running_pipeline() {
        let d = definition(1, "ask");
        assert_eq!(
            problems(&d, &goal(), json!({"complete": true})),
            ["complete: say in `evidence` how the stop condition is met, point by point"]
        );
        assert!(
            problems(
                &d,
                &goal(),
                json!({"complete": true, "evidence": "toy-1 landed."})
            )
            .is_empty()
        );
        let both =
            json!({"complete": true, "evidence": "e", "start": [{"bead": "toy-1", "reason": ""}]});
        assert_eq!(problems(&d, &goal(), both).len(), 1);
    }

    #[test]
    fn a_bead_to_file_needs_a_type_a_priority_and_a_description() {
        let d = definition(1, "ask");
        let file = |kind: &str, priority: i64| {
            json!({"ask_owner": "q", "file": [{"title": "Doc", "type": kind, "priority": priority, "description": "d",
                "acceptance_criteria": "", "parent": "", "discovered_from": "toy-1", "deferred": true}]})
        };
        assert!(problems(&d, &goal(), file("task", 4)).is_empty());
        assert_eq!(
            problems(&d, &goal(), file("epic", 4)),
            ["file: `Doc` has no valid type"]
        );
        assert_eq!(
            problems(&d, &goal(), file("bug", 7)),
            ["file: `Doc` needs a priority from 0 to 4"]
        );
    }

    #[test]
    fn the_brief_tells_the_director_what_happened_and_what_it_remembers() {
        let mut goal = goal();
        goal.inbox = vec![
            Happening::PipelineEnded {
                bead: "toy-1".into(),
                unit: Some("u1".into()),
                outcome: "landed".into(),
                detail: "Merged.".into(),
                pull_request: Some("https://example.com/pr/7".into()),
                findings: vec![
                    json!({"step": "review", "severity": "minor", "title": "Doc gap", "location": "a.zig:4"}),
                ],
                commit: None,
                ship: false,
                strayed: Vec::new(),
            },
            Happening::OwnerSaid {
                text: "do toy-2 next".into(),
            },
        ];
        let brief = wake_brief(
            &definition(1, "ask"),
            &goal,
            &tracker(),
            "Started with toy-1.",
            "Toy uses make.",
        );
        assert_eq!(
            brief,
            wake_brief(
                &definition(1, "ask"),
                &goal,
                &tracker(),
                "Started with toy-1.",
                "Toy uses make."
            )
        );
        for expected in [
            "## Objective\n\nShip it",
            "The pipeline for `toy-1` ended: **landed**. Merged. Pull request: https://example.com/pr/7",
            "F1 (review, minor): Doc gap at `a.zig:4`",
            "> do toy-2 next",
            "toy-1 | P1 | task | launchable | Bead toy-1",
            "toy-2 | P1 | task | no acceptance criteria | Bead toy-2",
            "Started with toy-1.",
            "Toy uses make.",
            "is put to the owner, who approves or rejects it",
        ] {
            assert!(
                brief.contains(expected),
                "missing `{expected}` in:\n{brief}"
            );
        }
    }

    #[test]
    fn only_the_exact_words_act_without_an_agent() {
        assert_eq!(command(" Accept. "), "accept");
        assert_eq!(command("START!"), "start");
        assert_ne!(command("accept, but first fix the docs"), "accept");
    }
}
