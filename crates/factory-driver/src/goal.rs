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

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use factory_beads::{Change, NewBead, Source, Summary, readiness};
use factory_claude::{Outcome, RunSpec, Session, Usage};
use factory_config::{ConfigDir, Factory, GoalDefinition, Permission, Project, read_env_file};
use factory_core::{EventLog, ProjectId, UnitId, now_unix};
use factory_slack::socket::{Incoming, Listener};
use factory_slack::{Button, Slack, escape, quote, truncate};
use factory_supervise::Control;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{Driver, DriverError, Options, Status, bead_source};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// The owner and the director are still agreeing what the goal is.
    Intake,
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

/// One pipeline the goal started.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GoalUnit {
    pub bead: String,
    pub unit: Option<String>,
    /// `running`, `landed`, `published`, `parked`, `refused` or `failed`.
    pub state: String,
    pub detail: String,
}

/// Something the director has not been told yet.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Happening {
    Started,
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
}

impl Goal {
    fn running(&self) -> usize {
        self.units
            .iter()
            .filter(|unit| unit.state == "running")
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
        "edits": { "type": "array", "items": edit_schema(), "description": "Changes to existing beads the owner agreed to in this conversation, such as acceptance criteria. They are shown in the charter and applied when the owner starts the goal." },
    }))
}

/// What the director returns each time an event wakes it.
pub fn wake_schema() -> Value {
    object_schema(json!({
        "summary": text_schema("What you concluded and decided, for the owner, in a few plain sentences."),
        "start": { "type": "array", "description": "Pipelines to start now.", "items": object_schema(json!({
            "bead": text_schema("The ID of a launchable bead."),
            "reason": text_schema("Why this one next."),
        }))},
        "file": { "type": "array", "description": "New beads to file.", "items": object_schema(json!({
            "title": text_schema("What the bead is, as a short title."),
            "type": { "enum": BEAD_TYPES },
            "priority": { "type": "integer", "minimum": 0, "maximum": 4 },
            "description": text_schema("Why it exists and what needs doing, for someone who has not seen this goal."),
            "acceptance_criteria": text_schema("How anyone will know it is done."),
            "parent": text_schema("The epic it belongs under, or empty."),
            "discovered_from": text_schema("The bead whose work turned this up, or empty."),
            "deferred": { "type": "boolean", "description": "True to file it into the icebox: kept, but not ready work." },
        }))},
        "edit": { "type": "array", "description": "Changes to beads that exist.", "items": edit_schema() },
        "dropped_findings": { "type": "array", "description": "Findings from an ended pipeline that you decided not to file.", "items": object_schema(json!({
            "finding": text_schema("Which finding, by its label."),
            "reason": text_schema("Why it is not worth a bead."),
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
    if strings(result, "summary").trim().is_empty() {
        problems.push("`summary` is empty".to_string());
    }

    let mut starting: Vec<&str> = Vec::new();
    for start in items(result, "start") {
        let bead = strings(start, "bead");
        if starting.contains(&bead) {
            problems.push(format!("start: `{bead}` is listed twice"));
            continue;
        }
        starting.push(bead);
        if !known(bead) {
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
    let in_flight = goal.running() + starting.len();
    if in_flight > definition.max_pipelines as usize {
        problems.push(format!(
            "start: that would make {in_flight} pipelines at once, and this goal may run {}",
            definition.max_pipelines
        ));
    }

    for file in items(result, "file") {
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
        if !starting.is_empty() || goal.running() > 0 {
            problems.push(
                "complete: a goal cannot be complete while a pipeline is running or being started"
                    .to_string(),
            );
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
            "nothing would be running and nobody would be asked: start a pipeline, ask the owner, or propose that the goal is complete"
                .to_string(),
        );
    }
    problems
}

fn table(tracker: &[Summary]) -> String {
    let mut out = String::from("id | P | type | state | title\n");
    for bead in tracker.iter().take(300) {
        let state = if bead.launchable() {
            "launchable".to_string()
        } else if bead.status != "open" {
            bead.status.clone()
        } else if bead.blocked {
            "blocked".to_string()
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
    format!(
        "- Starting a pipeline: carried out at once, on a launchable bead, up to {} at a time.\n- Filing a new bead {}.\n- Changing a bead that exists (criteria, priority, deferral) {}.\n- Ending the goal: you propose, the owner decides.",
        definition.max_pipelines,
        say(definition.permissions.file_beads),
        say(definition.permissions.edit_beads),
    )
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
    let mut label = 0;
    for happening in &goal.inbox {
        match happening {
            Happening::Started => out.push_str("- The owner started the goal.\n"),
            Happening::OwnerSaid { text } => {
                out.push_str(&format!(
                    "- The owner wrote in the goal's thread:\n\n{}\n\n",
                    quote_plain(text)
                ));
            }
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
                ..
            } => {
                out.push_str(&format!(
                    "- The pipeline for `{bead}` ended: **{outcome}**. {detail}"
                ));
                if let Some(url) = pull_request {
                    out.push_str(&format!(" Pull request: {url}"));
                }
                out.push('\n');
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
        table(tracker)
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

/// One run of the director: which prompt it works to, what it must return,
/// and what it is told.
struct Turn {
    role: &'static str,
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
            if !factory.allowed_models.contains(&definition.director.model) {
                return Err(refuse(format!(
                    "the director's model `{}` is not allowed",
                    definition.director.model
                )));
            }
            for role in ["director", "director-intake"] {
                config.role_prompt(role)?;
            }
            lines.push(format!(
                "{id}/{}: director {} ({}), pipeline `{}`, {} at a time{}",
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
        let (sender, receiver) = mpsc::channel();
        Ok(Supervisor {
            config,
            factory,
            slack,
            owners,
            channels,
            goals,
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

    fn log(&self, goal: &Goal, kind: &str, data: Value) {
        let log = EventLog::new(
            self.dir(&goal.id).join("events.jsonl"),
            ProjectId::new(goal.project.clone()),
            UnitId::new(goal.id.clone()),
        );
        let _ = log.append(kind, None, data);
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
                        if let Some(project) = self.channels.get(&channel).cloned() {
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
                        match goal {
                            Some(id) => self.owner_said(&id, &text),
                            // A pipeline's own thread: its driver reads it.
                            None => Ok(()),
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

    fn open_goal(
        &mut self,
        project: &str,
        channel: &str,
        ts: &str,
        user: &str,
        text: &str,
    ) -> Result<(), DriverError> {
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
            return Ok(());
        };
        let goal = Goal {
            id: format!("{project}-g{}", now_unix()),
            project: project.to_string(),
            definition: definition.id.clone(),
            phase: Phase::Intake,
            channel: channel.to_string(),
            thread_ts: ts.to_string(),
            opened_by: user.to_string(),
            intake_session: None,
            charter: None,
            charter_edits: Vec::new(),
            units: Vec::new(),
            inbox: Vec::new(),
            request: None,
            prompt: None,
            evidence: None,
            wakes: 0,
            failures: 0,
            retry_at: None,
            usage: Usage::default(),
            started_at: now_unix(),
        };
        let id = goal.id.clone();
        self.save(goal.clone())?;
        self.log(
            &goal,
            "goal.opened",
            json!({ "text": text, "definition": goal.definition }),
        );
        self.intake_turn(&id, text)
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
                self.save(goal)
            }
            ("request_approve" | "request_reject", Phase::Running) if goal.request.is_some() => {
                let approved = action == "request_approve";
                let request = goal.request.take().unwrap_or_default();
                let mut what = Vec::new();
                if approved {
                    let project = self.config.project(&goal.project)?;
                    let source = bead_source(&project);
                    for file in &request.files {
                        what.push(self.file_bead(&goal, &source, &project, file));
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
                self.save(goal)
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

    /// A checkout of the default branch for the director to read, kept apart
    /// from the owner's own checkout and from every pipeline's worktree.
    fn checkout(&self, goal: &Goal, project: &Project) -> Result<PathBuf, DriverError> {
        let dir = self.dir(&goal.id).join("checkout");
        let head = format!("{}/{}", project.remote, project.default_branch);
        factory_git::fetch(&project.repo, &project.remote)?;
        if dir.exists() {
            factory_git::checkout_detached(&dir, &head)?;
        } else {
            factory_git::worktree_add_detached(&project.repo, &dir, &head)?;
        }
        Ok(dir)
    }

    /// Run the director once, sending a result that code refuses back to the
    /// same session up to twice. Returns the result and the session.
    fn run_director(
        &mut self,
        goal: &mut Goal,
        definition: &GoalDefinition,
        turn: Turn,
        check: &dyn Fn(&Value) -> Vec<String>,
    ) -> Result<(Value, String), String> {
        let Turn {
            role,
            schema,
            mut session,
            mut prompt,
        } = turn;
        let project = self
            .config
            .project(&goal.project)
            .map_err(|err| err.to_string())?;
        let cwd = self
            .checkout(goal, &project)
            .map_err(|err| err.to_string())?;
        let system_prompt = self
            .config
            .role_prompt(role)
            .map_err(|err| err.to_string())?;
        let dir = self.dir(&goal.id).join("runs");
        for attempt in 1..=3 {
            let run = std::fs::read_dir(&dir)
                .map(|entries| entries.count())
                .unwrap_or(0)
                / 2
                + 1;
            let _ = std::fs::write(dir.join(format!("{run:03}-{role}-brief.md")), &prompt);
            let model = self.model(&definition.director.model);
            let (account, oauth_token) = match crate::pick_account(&self.factory, &model) {
                Ok(crate::AccountPick::Use { id, token }) => (id, token),
                Ok(crate::AccountPick::Wait { until }) => return Err(format!("{LIMITED}{until}")),
                Err(err) => return Err(err.to_string()),
            };
            let spec = RunSpec {
                claude_bin: self.factory.claude_bin.clone(),
                model,
                effort: definition.director.effort.clone(),
                session: session.clone(),
                system_prompt: system_prompt.clone(),
                schema: schema.clone(),
                cwd: cwd.clone(),
                prompt: prompt.clone(),
                budget_usd: project.limits.step_budget_usd,
                timeout: Duration::from_secs(definition.director.timeout_secs),
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
            goal.usage.add(&usage);
            self.log(
                goal,
                "director.ran",
                json!({ "role": role, "run": run, "account": account, "outcome": finished.outcome.name(), "session": finished.transcript.session_id, "usage": usage }),
            );
            let id = finished.transcript.session_id.clone().unwrap_or_default();
            match finished.outcome {
                Outcome::Completed(result) => {
                    let problems = check(&result);
                    if problems.is_empty() {
                        return Ok((result, id));
                    }
                    self.log(goal, "director.refused", json!({ "problems": problems }));
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
                other => return Err(format!("the director's run ended as `{}`", other.name())),
            }
        }
        Err("the director did not return a result".to_string())
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
        self.save(goal)
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
                        "# A new goal on project `{}`\n\nThe owner opened it with this message:\n\n{}\n{standing}\n## The tracker: open work, most urgent first\n\n{}\n## Project memory\n\n{}\n\n## What the director of this goal may decide once it runs\n\n{}\n",
                        goal.project,
                        quote_plain(owner_text),
                        table(&tracker),
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
            }
            problems
        };
        let turn = Turn {
            role: "director-intake",
            schema: intake_schema(),
            session,
            prompt,
        };
        let result = self.run_director(&mut goal, &definition, turn, &check);
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
            });
            goal.charter_edits = edits_of(&result, "edits");
            let mut text = format!(
                "{}\n\n:dart: *Objective*\n{}\n\n:checkered_flag: *Stop when*\n{}\n\n:compass: *Plan*\n{}",
                escape(strings(&result, "reply")),
                quote(strings(&result, "objective")),
                quote(strings(&result, "stop_when")),
                quote(&truncate(strings(&result, "plan"), 2500)),
            );
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
                "\n\n_Director: `{}` · pipeline: `{}` · {} at a time. Press *Start*, or reply to change anything._",
                self.model(&definition.director.model),
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

    fn file_bead(&self, goal: &Goal, source: &Source, project: &Project, file: &Value) -> String {
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
                self.log(
                    goal,
                    "bead.filed",
                    json!({ "bead": bead.id, "request": file }),
                );
                format!(
                    "filed `{}` ({}, P{}{}): {}",
                    bead.id,
                    new.issue_type,
                    new.priority,
                    if new.deferred { ", deferred" } else { "" },
                    new.title
                )
            }
            Err(err) => {
                self.log(
                    goal,
                    "bead.failed",
                    json!({ "request": file, "error": err.to_string() }),
                );
                format!("could not file `{}`: {err}", new.title)
            }
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

        let launchable = |bead: &str| match source.read(bead) {
            Ok(bead) => readiness(&bead, &project.bead_actor),
            Err(_) => vec!["unknown".to_string()],
        };
        let snapshot = goal.clone();
        let check = |result: &Value| {
            decision_problems(&definition, &snapshot, &tracker, &launchable, result)
        };
        let turn = Turn {
            role: "director",
            schema: wake_schema(),
            session: Session::Fresh,
            prompt: brief,
        };
        let result = self.run_director(&mut goal, &definition, turn, &check);
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
                Permission::Allow => did.push(self.file_bead(&goal, &source, &project, file)),
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
        for start in items(&result, "start") {
            let bead = strings(start, "bead").to_string();
            goal.units.push(GoalUnit {
                bead: bead.clone(),
                unit: None,
                state: "running".to_string(),
                detail: String::new(),
            });
            self.log(
                &goal,
                "pipeline.started",
                json!({ "bead": bead, "reason": strings(start, "reason") }),
            );
            self.launch(&goal, &definition, bead, None);
        }
        self.save(goal)
    }

    /// Run one pipeline on a thread of its own, and report back when it ends.
    fn launch(
        &self,
        goal: &Goal,
        definition: &GoalDefinition,
        bead: String,
        resume: Option<String>,
    ) {
        let (config, sender) = (self.config.clone(), self.sender.clone());
        let (goal_id, project) = (goal.id.clone(), goal.project.clone());
        let options = Options {
            slack: self.unit_slack,
            flow: definition.flow.clone(),
        };
        std::thread::spawn(move || {
            let ended =
                |outcome: &str, detail: String, driver: Option<&Driver>| Happening::PipelineEnded {
                    bead: bead.clone(),
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
                };
            let started = match &resume {
                Some(unit) => Driver::resume(config, unit, &options, None),
                None => Driver::start(config, &project, &bead, &options),
            };
            let happening = match started {
                Err(err) => ended("refused", err.to_string(), None),
                Ok(mut driver) => {
                    let _ = sender.send(Internal::Began {
                        goal: goal_id.clone(),
                        bead: bead.clone(),
                        unit: driver.unit().id.to_string(),
                    });
                    match driver.run() {
                        Ok(Status::Ended) if driver.unit().merged.is_some() => ended(
                            "landed",
                            "Its change is merged and its bead is closed.".to_string(),
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
                    if let Some(entry) = goal
                        .units
                        .iter_mut()
                        .rev()
                        .find(|u| u.bead == bead && u.state == "running")
                    {
                        entry.unit = Some(unit.clone());
                    }
                    self.say(&goal, &format!(":arrow_forward: Pipeline started for `{}` (run `{unit}`). It reports in its own thread in this channel.", escape(&bead)));
                    self.save(goal)?;
                }
                Internal::Ended { goal, happening } => {
                    let Some(mut goal) = self.goals.get(&goal).cloned() else {
                        continue;
                    };
                    let id = goal.id.clone();
                    if let Happening::PipelineEnded {
                        bead,
                        outcome,
                        detail,
                        unit,
                        pull_request,
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
                        }
                        self.say(
                            &goal,
                            &format!(
                                "{} Pipeline for `{}` ended: *{outcome}*. {}{}",
                                if outcome == "landed" {
                                    ":white_check_mark:"
                                } else {
                                    ":double_vertical_bar:"
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
                    goal.inbox.push(happening);
                    self.save(goal)?;
                    self.wake(&id)?;
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
            let goal = self.goals[&id].clone();
            if !matches!(goal.phase, Phase::Running | Phase::Stopped) {
                continue;
            }
            let definition = self.definition(&goal)?;
            let mut lost = Vec::new();
            for unit in goal.units.iter().filter(|unit| unit.state == "running") {
                match &unit.unit {
                    Some(unit_id) => {
                        self.launch(&goal, &definition, unit.bead.clone(), Some(unit_id.clone()))
                    }
                    // It never got as far as having a unit: start it again.
                    None => lost.push(unit.bead.clone()),
                }
            }
            for bead in lost {
                self.launch(&goal, &definition, bead, None);
            }
            self.wake(&id)?;
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
