//! The factory's records, read into one picture of what it is doing.
//!
//! Everything here is read from the files the factory keeps under its
//! state directory: the executives' and goals' records, the units' records
//! and event logs, the accounts' windows. Nothing is written, and no
//! factory crate is used: a field that is missing or has changed its
//! shape is left out of the picture, not an error.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

/// A five-hour window, in seconds.
pub const FIVE_HOURS: u64 = 5 * 3600;
/// A seven-day window, in seconds.
pub const SEVEN_DAYS: u64 = 7 * 24 * 3600;
/// Records older than this say nothing about a window still open.
const HISTORY_SECS: u64 = 8 * 24 * 3600;
/// A tier's run whose log has been quiet this long is taken to be over,
/// though nothing in the log says so: its worker died.
const TIER_QUIET_SECS: u64 = 20 * 60;
/// A goal in one of these phases is not shown as work in flight.
const FINISHED: [&str; 4] = ["complete", "closed", "abandoned", "dropped"];

/// Where the engine's own pages are, to link to.
#[derive(Clone, Debug, Default)]
pub struct Links {
    /// The address of the Temporal server's web pages, if there are any.
    pub temporal_ui: Option<String>,
    pub namespace: String,
}

/// One window of an account as a run saw it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Window {
    pub utilization: f64,
    pub resets_at: u64,
}

/// What a run that ended said of its account, and what it cost.
#[derive(Clone, Debug, PartialEq)]
pub struct Reading {
    pub at: u64,
    pub account: String,
    pub five: Option<Window>,
    pub seven: Option<Window>,
    pub cost: f64,
}

/// An agent's run that began and has not ended.
#[derive(Clone, Debug, PartialEq)]
pub struct OpenRun {
    pub run: u64,
    pub step: String,
    pub role: String,
    pub model: String,
    pub effort: String,
    pub account: String,
    /// The name of its log, in the unit's `runs` directory.
    pub file: String,
    pub since: u64,
}

/// A step that began and has not ended.
#[derive(Clone, Debug, PartialEq)]
pub struct OpenStep {
    pub step: String,
    pub round: u64,
    pub since: u64,
}

/// What an event log says that the picture needs.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Digest {
    pub readings: Vec<Reading>,
    /// When each account was last given a run.
    pub handed: BTreeMap<String, u64>,
    pub run: Option<OpenRun>,
    pub step: Option<OpenStep>,
}

fn window_of(value: &Value) -> Option<Window> {
    Some(Window {
        utilization: value["utilization"].as_f64()?,
        resets_at: value["resets_at"].as_u64()?,
    })
}

fn text(value: &Value) -> String {
    value.as_str().unwrap_or("").to_string()
}

/// Read an event log's lines for the runs it tells of.
pub fn digest(log: &str) -> Digest {
    let mut digest = Digest::default();
    // The account each run was given: a unit's log names it when the run
    // starts and not when it ends.
    let mut given: HashMap<u64, String> = HashMap::new();
    for line in log.lines() {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let at = event["time_unix"].as_u64().unwrap_or(0);
        let data = &event["data"];
        match event["kind"].as_str().unwrap_or("") {
            "run.started" => {
                let run = OpenRun {
                    run: data["run"].as_u64().unwrap_or(0),
                    step: text(&event["step"]),
                    role: text(&data["role"]),
                    model: text(&data["model"]),
                    effort: text(&data["effort"]),
                    account: text(&data["account"]),
                    file: Path::new(data["log"].as_str().unwrap_or(""))
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    since: at,
                };
                given.insert(run.run, run.account.clone());
                digest.handed.insert(run.account.clone(), at);
                digest.run = Some(run);
            }
            "run.ended" => {
                let ended = data["run"].as_u64();
                if digest
                    .run
                    .as_ref()
                    .is_some_and(|run| Some(run.run) == ended)
                {
                    digest.run = None;
                }
            }
            "step.started" => {
                digest.step = Some(OpenStep {
                    step: text(&event["step"]),
                    round: data["round"].as_u64().unwrap_or(1),
                    since: at,
                });
            }
            "step.ended" => digest.step = None,
            _ => {}
        }
        // Whatever ended a run says what the run used.
        let usage = &data["usage"];
        if !usage.is_object() {
            continue;
        }
        let account = data["account"].as_str().map(str::to_string).or_else(|| {
            data["run"]
                .as_u64()
                .and_then(|run| given.get(&run).cloned())
        });
        let Some(account) = account.filter(|account| !account.is_empty()) else {
            continue;
        };
        let windows = &usage["rate_limit"]["windows"];
        if data["account"].is_string() {
            let last = digest.handed.entry(account.clone()).or_insert(at);
            *last = (*last).max(at);
        }
        digest.readings.push(Reading {
            at,
            account,
            five: window_of(&windows["five_hour"]),
            seven: window_of(&windows["seven_day"]),
            cost: usage["cost_usd"].as_f64().unwrap_or(0.0),
        });
    }
    digest
}

/// How fast a window is being used, and whether it runs out.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Burn {
    /// Share of the window used per hour, lately. `None` when too little
    /// is known to say.
    pub per_hour: Option<f64>,
    /// When the window is full at that rate, if that is before it resets.
    pub full_at: Option<u64>,
}

/// The rate a window was used at over the last `over` seconds, from its
/// readings, each a time and the share used then, oldest first. What was
/// used since the last reading before that span is spread over the time
/// from that reading until now, so an account nothing runs on shows a
/// rate that falls, never one left over from when it was busy.
pub fn burn(trail: &[(u64, f64)], now: u64, resets_at: u64, over: u64) -> Burn {
    let unknown = Burn {
        per_hour: None,
        full_at: None,
    };
    let Some(&(_, last)) = trail.last() else {
        return unknown;
    };
    let since = now.saturating_sub(over);
    let Some(&(from, before)) = trail
        .iter()
        .rev()
        .find(|(at, _)| *at <= since)
        .or(trail.first())
    else {
        return unknown;
    };
    let span = now.saturating_sub(from);
    // Readings a moment apart say nothing of a rate.
    if span < over / 12 {
        return unknown;
    }
    let per_hour = ((last - before) / (span as f64 / 3600.0)).max(0.0);
    let full_at = (per_hour > 0.0)
        .then(|| now + (((1.0 - last).max(0.0) / per_hour) * 3600.0) as u64)
        .filter(|at| *at < resets_at);
    Burn {
        per_hour: Some(per_hour),
        full_at,
    }
}

/// The readings since the share used last fell. Use only grows inside a
/// window, so a fall means the readings before it are of something else:
/// the limit was lifted, or the name was given to another account. They
/// say nothing of how fast it is going now.
pub fn since_last_fall(trail: &[(u64, f64)]) -> &[(u64, f64)] {
    let fell = trail
        .windows(2)
        .rposition(|pair| pair[1].1 + 0.02 < pair[0].1)
        .map_or(0, |at| at + 1);
    &trail[fell..]
}

type Stamp = (Option<SystemTime>, u64);

fn stamp(path: &Path) -> Option<Stamp> {
    let meta = fs::metadata(path).ok()?;
    Some((meta.modified().ok(), meta.len()))
}

fn secs(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

fn changed_at(path: &Path) -> u64 {
    fs::metadata(path)
        .ok()
        .and_then(|meta| meta.modified().ok())
        .map(secs)
        .unwrap_or(0)
}

/// The directories in a directory, by name.
fn dirs(path: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.'))
        .collect();
    names.sort();
    names
}

/// The logs of runs in a directory, oldest first.
fn logs(path: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".jsonl") && !name.starts_with('.'))
        .collect();
    names.sort();
    names
}

/// The end of a file, as text.
fn tail(path: &Path, most: u64) -> String {
    let Ok(mut file) = fs::File::open(path) else {
        return String::new();
    };
    let size = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    if file
        .seek(SeekFrom::Start(size.saturating_sub(most)))
        .is_err()
    {
        return String::new();
    }
    let mut bytes = Vec::new();
    let _ = file.take(most).read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The start of a file, as text.
fn head(path: &Path, most: u64) -> String {
    let Ok(file) = fs::File::open(path) else {
        return String::new();
    };
    let mut bytes = Vec::new();
    let _ = file.take(most).read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
}

/// What a run still going last said of its account's windows.
fn windows_in_log(path: &Path) -> Option<(Option<Window>, Option<Window>)> {
    let end = tail(path, 96 * 1024);
    let line = end
        .lines()
        .rev()
        .find(|line| line.contains("\"rate_limit_event\""))?;
    let event: Value = serde_json::from_str(line).ok()?;
    let windows = &event["rate_limit_info"]["unifiedWindows"];
    let read = |value: &Value| {
        Some(Window {
            utilization: value["utilization"].as_f64()?,
            resets_at: value["resetsAt"].as_u64()?,
        })
    };
    Some((read(&windows["five_hour"]), read(&windows["seven_day"])))
}

fn cut(value: &Value, most: usize) -> Value {
    match value.as_str() {
        Some(text) if text.chars().count() > most => {
            let mut short: String = text.chars().take(most).collect();
            short.push('…');
            Value::String(short)
        }
        Some(text) => Value::String(text.to_string()),
        None => Value::Null,
    }
}

/// Reads the factory's records. It remembers what it read of a file
/// until the file changes, so a look costs little while little happens.
pub struct Reader {
    state: PathBuf,
    links: Links,
    records: HashMap<PathBuf, (Stamp, Arc<Value>)>,
    digests: HashMap<PathBuf, (Stamp, Arc<Digest>)>,
}

/// What is gathered on the way through the records.
#[derive(Default)]
struct Gathered {
    readings: Vec<Reading>,
    handed: BTreeMap<String, u64>,
    runs: Vec<Value>,
}

impl Gathered {
    fn take(&mut self, digest: &Digest) {
        self.readings.extend(digest.readings.iter().cloned());
        for (account, at) in &digest.handed {
            let last = self.handed.entry(account.clone()).or_insert(*at);
            *last = (*last).max(*at);
        }
    }
}

impl Reader {
    pub fn new(state: PathBuf, links: Links) -> Reader {
        Reader {
            state,
            links,
            records: HashMap::new(),
            digests: HashMap::new(),
        }
    }

    /// A record, as JSON. A flow definition is TOML and read as such.
    fn record(&mut self, path: &Path) -> Option<Arc<Value>> {
        let now = stamp(path)?;
        if let Some((then, value)) = self.records.get(path)
            && *then == now
        {
            return Some(value.clone());
        }
        let text = fs::read_to_string(path).ok()?;
        let value: Value = if path.extension().is_some_and(|ext| ext == "toml") {
            toml::from_str(&text).ok()?
        } else {
            serde_json::from_str(&text).ok()?
        };
        let value = Arc::new(value);
        self.records
            .insert(path.to_path_buf(), (now, value.clone()));
        Some(value)
    }

    fn digest(&mut self, path: &Path) -> Arc<Digest> {
        let Some(now) = stamp(path) else {
            return Arc::default();
        };
        if let Some((then, digest)) = self.digests.get(path)
            && *then == now
        {
            return digest.clone();
        }
        let read = Arc::new(digest(&fs::read_to_string(path).unwrap_or_default()));
        self.digests.insert(path.to_path_buf(), (now, read.clone()));
        read
    }

    fn workflow(&self, id: &str) -> Value {
        match &self.links.temporal_ui {
            Some(ui) if !id.is_empty() => Value::String(format!(
                "{}/namespaces/{}/workflows/{id}",
                ui.trim_end_matches('/'),
                self.links.namespace
            )),
            _ => Value::Null,
        }
    }

    /// The engine's list of the workflows whose names begin so.
    fn workflows_named(&self, start: &str) -> Value {
        let plain = start
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
        match &self.links.temporal_ui {
            Some(ui) if plain => Value::String(format!(
                "{}/namespaces/{}/workflows?query=WorkflowId%20STARTS_WITH%20%22{start}%22",
                ui.trim_end_matches('/'),
                self.links.namespace
            )),
            _ => Value::Null,
        }
    }

    /// The whole picture, as of `now`. Every time in it is a time, never
    /// how long ago: the picture changes only when a record does.
    pub fn picture(&mut self, now: u64) -> Value {
        let state = self.state.clone();
        let mut gathered = Gathered::default();
        let recent = |path: &Path| changed_at(path) + HISTORY_SECS >= now;

        // The goals, by project.
        let mut goals: BTreeMap<String, Vec<(String, Arc<Value>)>> = BTreeMap::new();
        for id in dirs(&state.join("goals")) {
            let dir = state.join("goals").join(&id);
            let Some(goal) = self.record(&dir.join("goal.json")) else {
                continue;
            };
            if recent(&dir.join("events.jsonl")) {
                let digest = self.digest(&dir.join("events.jsonl"));
                gathered.take(&digest);
            }
            goals
                .entry(text(&goal["project"]))
                .or_default()
                .push((id, goal));
        }
        let mut projects: BTreeSet<String> = dirs(&state.join("projects")).into_iter().collect();
        projects.extend(goals.keys().cloned());

        // Every unit lately touched tells of its runs. One that is
        // running and belongs to no goal is shown by itself.
        let mut in_goals: BTreeSet<String> = BTreeSet::new();
        let mut alone: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for id in dirs(&state.join("units")) {
            let dir = state.join("units").join(&id);
            if !recent(&dir.join("unit.json")) {
                continue;
            }
            let digest = self.digest(&dir.join("events.jsonl"));
            gathered.take(&digest);
            if let Some(unit) = self.record(&dir.join("unit.json"))
                && unit["status"] == "running"
            {
                alone.entry(text(&unit["project"])).or_default().push(id);
            }
        }

        let mut shown = Vec::new();
        for project in &projects {
            let mut active = Vec::new();
            let mut finished = Vec::new();
            for (id, goal) in goals.get(project).map(Vec::as_slice).unwrap_or_default() {
                let phase = goal["phase"].as_str().unwrap_or("");
                if FINISHED.contains(&phase) {
                    finished.push(json!({
                        "id": id,
                        "phase": phase,
                        "objective": cut(&goal["charter"]["objective"], 240),
                        "cost": goal["usage"]["cost_usd"].as_f64().unwrap_or(0.0),
                        "started_at": goal["started_at"],
                    }));
                } else {
                    active.push(self.goal(id, goal, now, &mut gathered, &mut in_goals));
                }
            }
            let loose: Vec<Value> = alone
                .get(project)
                .into_iter()
                .flatten()
                .filter(|unit| !in_goals.contains(unit.as_str()))
                .filter_map(|unit| self.unit(unit, Value::Null, &mut gathered))
                .collect();
            let executive = self.executive(project, now, &mut gathered);
            shown.push(json!({
                "id": project,
                "workflow": self.workflow(&format!("project-{project}")),
                "executive": executive,
                "goals": active,
                "finished": finished,
                "loose": loose,
            }));
        }

        let accounts = self.accounts(now, &mut gathered);
        json!({
            "release": fs::read_link(state.join("current"))
                .ok()
                .and_then(|to| to.file_name().map(|name| name.to_string_lossy().into_owned())),
            "links": {
                "accounts": self.workflow("accounts"),
                "intake": self.workflow("intake"),
            },
            "accounts": accounts,
            "runs": gathered.runs,
            "projects": shown,
        })
    }

    /// A tier's runs in a directory, and the one still going, if any. A
    /// tier's run is in no event log until it has ended, so it is known
    /// by its log: the newest one, when nothing in it says it ended.
    fn tier(
        &mut self,
        place: &str,
        id: &str,
        dir: &Path,
        now: u64,
        about: Value,
        gathered: &mut Gathered,
    ) -> (Vec<String>, Value) {
        let files = logs(dir);
        let Some(file) = files.last() else {
            return (files, Value::Null);
        };
        let path = dir.join(file);
        let active_at = changed_at(&path);
        if active_at + TIER_QUIET_SECS < now || tail(&path, 4096).contains("\"type\":\"result\"") {
            return (files, Value::Null);
        }
        let role = file
            .trim_end_matches(".jsonl")
            .split_once('-')
            .map_or("", |(_, role)| role);
        let start = head(&path, 8192);
        let model = start
            .split_once("\"model\":\"")
            .and_then(|(_, rest)| rest.split_once('"'))
            .map_or("", |(model, _)| model);
        let since = fs::metadata(&path)
            .ok()
            .and_then(|meta| meta.created().ok())
            .map(secs)
            .unwrap_or(active_at);
        let mut run = json!({
            "where": place,
            "id": id,
            "file": file,
            "role": role,
            "model": model,
            "account": Value::Null,
            "since": since,
            "active_at": active_at,
        });
        if let (Some(run), Some(about)) = (run.as_object_mut(), about.as_object()) {
            run.extend(about.clone());
        }
        gathered.runs.push(run.clone());
        (files, run)
    }

    fn executive(&mut self, project: &str, now: u64, gathered: &mut Gathered) -> Value {
        let dir = self.state.join("projects").join(project);
        let Some(record) = self.record(&dir.join("executive.json")) else {
            return Value::Null;
        };
        let events = dir.join("executive").join("events.jsonl");
        if changed_at(&events) + HISTORY_SECS >= now {
            let digest = self.digest(&events);
            gathered.take(&digest);
        }
        let (files, run) = self.tier(
            "project",
            project,
            &dir.join("executive").join("runs"),
            now,
            json!({"project": project, "what": "the executive"}),
            gathered,
        );
        let threads = record["threads"].as_object();
        json!({
            "wakes": record["wakes"],
            "cost": record["usage"]["cost_usd"].as_f64().unwrap_or(0.0),
            "threads": threads.map_or(0, Map::len),
            "waiting": threads.map_or(0, |threads| {
                threads.values().filter(|thread| thread["waiting"] == true).count()
            }),
            "memos": record["memos"].as_array().map_or(0, Vec::len),
            "pending": record["pending"].as_array().map(|pending| {
                pending
                    .iter()
                    .map(|one| json!({
                        "bead": one["bead"],
                        "as": one["as"],
                        "once_closed": one["once_closed"],
                        "reason": cut(&one["reason"], 300),
                    }))
                    .collect::<Vec<_>>()
            }),
            "retry_at": record["desk_retry_at"].as_u64().filter(|at| *at > 0),
            "run": run,
            "runs": files,
        })
    }

    fn goal(
        &mut self,
        id: &str,
        goal: &Value,
        now: u64,
        gathered: &mut Gathered,
        in_goals: &mut BTreeSet<String>,
    ) -> Value {
        let dir = self.state.join("goals").join(id);
        // The engine's name for each unit's pipeline, from the goal's
        // record of what it asked for.
        let mut pipelines: HashMap<String, String> = HashMap::new();
        if let Ok(entries) = fs::read_dir(dir.join("pipelines")) {
            let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
            paths.sort();
            for path in paths {
                let Some(asked) = self.record(&path) else {
                    continue;
                };
                let name = text(&asked["id"]);
                for key in [&asked["unit"], &asked["bead"]] {
                    if let Some(key) = key.as_str() {
                        pipelines.insert(key.to_string(), name.clone());
                    }
                }
            }
        }
        let units: Vec<&Value> = goal["units"].as_array().into_iter().flatten().collect();
        let mut row = |reader: &mut Reader, bead: &str, ship: bool, subgoal: &str| -> Value {
            let entry = units.iter().rev().find(|unit| {
                unit["bead"] == bead
                    && unit["ship"].as_bool().unwrap_or(false) == ship
                    && (unit["subgoal"] == subgoal || unit["subgoal"].is_null())
            });
            let mut shown = json!({
                "bead": bead,
                "state": entry.map_or(json!("planned"), |entry| entry["state"].clone()),
                "detail": entry.map_or(Value::Null, |entry| cut(&entry["detail"], 400)),
            });
            let unit = entry.and_then(|entry| entry["unit"].as_str());
            if let Some(unit) = unit {
                in_goals.insert(unit.to_string());
            }
            // The pipeline's workflow: by the name on record, or else
            // every run of this bead's pipeline for this goal.
            let workflow = match unit
                .and_then(|unit| pipelines.get(unit))
                .or_else(|| pipelines.get(bead))
            {
                Some(name) => reader.workflow(name),
                None => reader.workflows_named(&format!("pipeline-{id}-{bead}-")),
            };
            let more = match unit {
                Some(unit) => reader.unit(unit, workflow, gathered),
                None => None,
            };
            if let (Some(shown), Some(Value::Object(more))) = (shown.as_object_mut(), more) {
                for (key, value) in more {
                    shown.entry(key).or_insert(value);
                }
            }
            shown
        };
        let mut subgoals = Vec::new();
        for subgoal in goal["subgoals"].as_array().into_iter().flatten() {
            let name = subgoal["id"].as_str().unwrap_or("");
            let mut beads: Vec<String> = Vec::new();
            let planned = subgoal["planned"].as_array().into_iter().flatten();
            let added = subgoal["added"].as_array().into_iter().flatten();
            let ran = units
                .iter()
                .filter(|unit| unit["subgoal"] == name && unit["ship"] != true)
                .map(|unit| &unit["bead"]);
            for bead in planned.chain(added).chain(ran).filter_map(Value::as_str) {
                if !beads.iter().any(|known| known == bead) {
                    beads.push(bead.to_string());
                }
            }
            let rows: Vec<Value> = beads
                .iter()
                .map(|bead| row(self, bead, false, name))
                .collect();
            let shipping = units
                .iter()
                .any(|unit| unit["subgoal"] == name && unit["ship"] == true)
                .then(|| row(self, name, true, name));
            let foreman = &subgoal["foreman"];
            subgoals.push(json!({
                "id": name,
                "title": subgoal["title"],
                "objective": cut(&subgoal["objective"], 700),
                "state": subgoal["state"],
                "branch": subgoal["branch"],
                "pull_request": subgoal["pull_request"],
                "foreman": {
                    "wakes": foreman["wakes"],
                    "inbox": foreman["inbox"].as_array().map_or(0, Vec::len),
                    "failures": foreman["failures"],
                    "retry_at": foreman["retry_at"],
                    "waiting": foreman["waiting"],
                    "message": cut(&foreman["message"], 300),
                },
                "beads": rows,
                "ship": shipping,
            }));
        }
        let (files, run) = self.tier(
            "goal",
            id,
            &dir.join("runs"),
            now,
            json!({"project": goal["project"], "goal": id, "what": "the goal"}),
            gathered,
        );
        json!({
            "id": id,
            "phase": goal["phase"],
            "objective": cut(&goal["charter"]["objective"], 600),
            "stop_when": cut(&goal["charter"]["stop_when"], 900),
            "wakes": goal["wakes"],
            "failures": goal["failures"],
            "retry_at": goal["retry_at"],
            "inbox": goal["inbox"].as_array().map_or(0, Vec::len),
            "asked": cut(&goal["request"], 600),
            "cost": goal["usage"]["cost_usd"].as_f64().unwrap_or(0.0),
            "started_at": goal["started_at"],
            "subgoals": subgoals,
            "run": run,
            "runs": files,
        })
    }

    /// A unit: the bead it carries, and where it is in its pipeline.
    fn unit(&mut self, id: &str, workflow: Value, gathered: &mut Gathered) -> Option<Value> {
        let dir = self.state.join("units").join(id);
        let unit = self.record(&dir.join("unit.json"))?;
        let status = unit["status"].as_str().unwrap_or("");
        let position = &unit["position"];
        let steps: Vec<Value> = self
            .record(&dir.join("flow.toml"))
            .and_then(|flow| {
                flow["step"].as_array().map(|steps| {
                    steps
                        .iter()
                        .map(|step| {
                            let name = step["id"].as_str().unwrap_or("");
                            json!({
                                "id": name,
                                "kind": step["kind"],
                                "role": step["role"],
                                "rounds": position["rounds"][name].as_u64().unwrap_or(0),
                            })
                        })
                        .collect()
                })
            })
            .unwrap_or_default();
        let digest = self.digest(&dir.join("events.jsonl"));
        let going = status == "running";
        let mut run = Value::Null;
        if going && let Some(open) = &digest.run {
            let log = dir.join("runs").join(&open.file);
            let active_at = changed_at(&log);
            if !open.account.is_empty()
                && let Some((five, seven)) = windows_in_log(&log)
            {
                gathered.readings.push(Reading {
                    at: active_at,
                    account: open.account.clone(),
                    five,
                    seven,
                    cost: 0.0,
                });
            }
            run = json!({
                "where": "unit",
                "id": id,
                "file": open.file,
                "role": open.role,
                "model": open.model,
                "effort": open.effort,
                "account": open.account,
                "step": open.step,
                "since": open.since,
                "active_at": active_at,
                "project": unit["project"],
                "bead": unit["bead"],
                "title": unit["title"],
            });
            gathered.runs.push(run.clone());
        }
        // A step with no agent in it is a command the factory runs.
        let command = match (&digest.step, going && run.is_null()) {
            (Some(step), true) => {
                json!({"step": step.step, "round": step.round, "since": step.since})
            }
            _ => Value::Null,
        };
        let cost: f64 = unit["usage"]
            .as_object()
            .into_iter()
            .flat_map(Map::values)
            .map(|step| step["usage"]["cost_usd"].as_f64().unwrap_or(0.0))
            .sum();
        Some(json!({
            "unit": id,
            "bead": unit["bead"],
            "title": unit["title"],
            "status": status,
            "flow": unit["flow"],
            "step": position["step"],
            "steps": steps,
            "run": run,
            "command": command,
            "cost": cost,
            "pull_request": unit["pull_request"],
            "merged": unit["merged"],
            "branch": unit["branch"],
            "workflow": workflow,
            "runs": logs(&dir.join("runs")),
            "changed_at": changed_at(&dir.join("unit.json")),
        }))
    }

    /// Each account: its windows, how fast they are being used, what it
    /// cost lately and what is running on it.
    fn accounts(&mut self, now: u64, gathered: &mut Gathered) -> Vec<Value> {
        let Some(file) = self.record(&self.state.join("accounts.json")) else {
            return Vec::new();
        };
        gathered.readings.sort_by_key(|reading| reading.at);
        let mut shown = Vec::new();
        for (name, record) in file.as_object().into_iter().flatten() {
            let mine: Vec<&Reading> = gathered
                .readings
                .iter()
                .filter(|reading| &reading.account == name)
                .collect();
            let read_at = record["updated_at"].as_u64().unwrap_or(0);
            let one =
                |key: &str, length: u64, over: u64, pick: &dyn Fn(&Reading) -> Option<Window>| {
                    // The newest word on the window: the factory's own record
                    // of it, or a run that has said something since.
                    let mut latest = window_of(&record[key]).map(|known| (read_at, known));
                    for reading in &mine {
                        if let Some(window) = pick(reading)
                            && latest.is_none_or(|(at, known)| {
                                reading.at >= at && window.resets_at >= known.resets_at
                            })
                        {
                            latest = Some((reading.at, window));
                        }
                    }
                    let Some((at, window)) = latest.filter(|(_, window)| window.resets_at > now)
                    else {
                        // It has reset, and nothing has run on it since.
                        return json!({
                            "utilization": 0.0,
                            "resets_at": Value::Null,
                            "length": length,
                            "status": "allowed",
                            "trail": [],
                        });
                    };
                    let mut trail: Vec<(u64, f64)> = mine
                        .iter()
                        .filter_map(|reading| {
                            pick(reading)
                                .filter(|seen| seen.resets_at == window.resets_at)
                                .map(|seen| (reading.at, seen.utilization))
                        })
                        .collect();
                    if let Some(known) =
                        window_of(&record[key]).filter(|known| known.resets_at == window.resets_at)
                    {
                        trail.push((read_at, known.utilization));
                    }
                    trail.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
                    trail.dedup();
                    // A window opens with nothing used. That counts
                    // toward the rate, but is not drawn: what happened
                    // between the opening and the first reading is not
                    // known.
                    let mut known = vec![(window.resets_at.saturating_sub(length), 0.0)];
                    known.extend(trail.iter().copied());
                    let known = since_last_fall(&known);
                    let rate = burn(known, now, window.resets_at, over);
                    // A long trail is thinned for the drawing of it.
                    let every = trail.len().div_ceil(120).max(1);
                    let last = trail.len().saturating_sub(1);
                    let drawn: Vec<Value> = trail
                        .iter()
                        .enumerate()
                        .filter(|(index, _)| index % every == 0 || *index == last)
                        .map(|(_, (at, used))| json!([at, used]))
                        .collect();
                    json!({
                        "utilization": window.utilization,
                        "resets_at": window.resets_at,
                        "length": length,
                        "status": record[key]["status"],
                        "read_at": at,
                        "per_hour": rate.per_hour,
                        "full_at": rate.full_at,
                        "trail": drawn,
                    })
                };
            let five = one("five_hour", FIVE_HOURS, 3600, &|reading| reading.five);
            let seven = one("seven_day", SEVEN_DAYS, 24 * 3600, &|reading| reading.seven);
            // One model's own week, such as Fable's. Only the usage endpoint
            // reports it, so it is as of the last time that was asked, and
            // no run's readings make a trail or a rate for it.
            let checked_at = record["usage_checked_at"].as_u64().unwrap_or(0);
            let models: serde_json::Map<String, Value> = record["models"]
                .as_object()
                .into_iter()
                .flatten()
                .filter_map(|(model, known)| {
                    let window = window_of(known)?;
                    let shown = if window.resets_at > now {
                        json!({
                            "utilization": window.utilization,
                            "resets_at": window.resets_at,
                            "length": SEVEN_DAYS,
                            "status": known["status"],
                            "read_at": checked_at,
                            "trail": [],
                        })
                    } else {
                        // It has reset since it was read: nothing is known
                        // of the new one but that it started empty.
                        json!({
                            "utilization": 0.0,
                            "resets_at": Value::Null,
                            "length": SEVEN_DAYS,
                            "status": "allowed",
                            "trail": [],
                        })
                    };
                    Some((model.clone(), shown))
                })
                .collect();
            let spent = |since: u64| -> f64 {
                mine.iter()
                    .filter(|reading| reading.at + since >= now)
                    .map(|reading| reading.cost)
                    .sum()
            };
            let runs: Vec<&Value> = gathered
                .runs
                .iter()
                .filter(|run| run["account"] == name.as_str())
                .collect();
            shown.push(json!({
                "name": name,
                "five_hour": five,
                "seven_day": seven,
                "models": models,
                "blocked": record["blocked"],
                "concern": record["concern"],
                "spent_hour": spent(3600),
                "spent_day": spent(24 * 3600),
                "handed_at": gathered.handed.get(name),
                "runs": runs,
            }));
        }
        shown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_event_log_tells_of_the_run_still_going_and_of_what_ended_runs_used() {
        let log = [
            r#"{"time_unix":100,"kind":"step.started","step":"build","data":{"round":1}}"#,
            r#"{"time_unix":101,"kind":"run.started","step":"build","data":{"account":"one","role":"builder","model":"m","effort":"high","run":1,"log":"/state/units/u/runs/001-build.jsonl"}}"#,
            r#"{"time_unix":200,"kind":"run.ended","step":"build","data":{"run":1,"usage":{"cost_usd":2.5,"rate_limit":{"windows":{"five_hour":{"utilization":0.2,"resets_at":9000},"seven_day":{"utilization":0.1,"resets_at":90000}}}}}}"#,
            r#"{"time_unix":201,"kind":"step.ended","step":"build","data":{}}"#,
            r#"{"time_unix":210,"kind":"step.started","step":"test","data":{"round":2}}"#,
            "not a line of the log",
        ]
        .join("\n");
        let read = digest(&log);
        // The run that ended left a reading under the account it began on.
        assert_eq!(
            read.readings,
            vec![Reading {
                at: 200,
                account: "one".to_string(),
                five: Some(Window {
                    utilization: 0.2,
                    resets_at: 9000
                }),
                seven: Some(Window {
                    utilization: 0.1,
                    resets_at: 90000
                }),
                cost: 2.5,
            }]
        );
        assert_eq!(read.run, None);
        assert_eq!(read.handed.get("one"), Some(&101));
        assert_eq!(
            read.step,
            Some(OpenStep {
                step: "test".to_string(),
                round: 2,
                since: 210
            })
        );

        // A run that began and has not ended is the one going.
        let going = digest(&format!(
            "{log}\n{}",
            r#"{"time_unix":300,"kind":"run.started","step":"fix","data":{"account":"two","role":"builder","model":"m","effort":"high","run":2,"log":"/state/units/u/runs/002-fix.jsonl"}}"#
        ));
        let run = going.run.unwrap();
        assert_eq!((run.step.as_str(), run.account.as_str()), ("fix", "two"));
        assert_eq!(run.file, "002-fix.jsonl");

        // A tier's run is on record only once it has ended, account and all.
        let tier = digest(
            r#"{"time_unix":500,"kind":"director.ran","data":{"account":"two","run":7,"usage":{"cost_usd":1.0,"rate_limit":{"windows":{}}}}}"#,
        );
        assert_eq!(tier.readings[0].account, "two");
        assert_eq!(tier.readings[0].five, None);
        assert_eq!(tier.handed.get("two"), Some(&500));
    }

    #[test]
    fn the_rate_is_what_was_used_lately_and_says_whether_the_window_runs_out() {
        let hour = 3600;
        // 30% in the last hour with half the window left to use, and the
        // window resets in three hours: it is full in 100 minutes.
        let trail = [(0, 0.1), (hour, 0.2), (2 * hour, 0.5)];
        let fast = burn(&trail, 2 * hour, 5 * hour, hour);
        assert!((fast.per_hour.unwrap() - 0.3).abs() < 1e-9, "{fast:?}");
        let full = fast.full_at.unwrap();
        assert!(
            (2 * hour + 5999..=2 * hour + 6001).contains(&full),
            "{full}"
        );
        // The same rate with the reset 90 minutes off: it lasts.
        let lasts = burn(&trail, 2 * hour, 2 * hour + 5400, hour);
        assert_eq!(lasts.full_at, None);
        assert!(lasts.per_hour.is_some());
        // The rate falls as time passes with nothing read, and after an
        // hour of that nothing is being used.
        let slower = burn(&trail, 2 * hour + 1800, 5 * hour, hour);
        assert!((slower.per_hour.unwrap() - 0.2).abs() < 1e-9, "{slower:?}");
        let idle = burn(&trail, 4 * hour, 5 * hour, hour);
        assert_eq!(
            idle,
            Burn {
                per_hour: Some(0.0),
                full_at: None
            }
        );
        // One reading, or readings a moment apart, say nothing of a rate.
        assert_eq!(burn(&[(0, 0.4)], 10, 5 * hour, hour).per_hour, None);
        let close = [(0, 0.4), (60, 0.41)];
        assert_eq!(burn(&close, 70, 5 * hour, hour).per_hour, None);
        assert_eq!(burn(&[], 10, 5 * hour, hour).per_hour, None);
        // What was read before the share fell is of another window.
        let fell = [(0, 0.0), (100, 0.9), (200, 0.95), (300, 0.05), (400, 0.06)];
        assert_eq!(since_last_fall(&fell), &fell[3..]);
        assert_eq!(since_last_fall(&fell[..3]), &fell[..3]);
        assert_eq!(since_last_fall(&[]), &[]);
        // A reading that went down is no negative rate.
        let down = [(0, 0.4), (1800, 0.3)];
        assert_eq!(burn(&down, 1900, 5 * hour, hour).per_hour, Some(0.0));
    }
}
