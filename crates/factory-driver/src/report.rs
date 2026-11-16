//! How the factory ran over a period, worked out by code from its own
//! records: the units' and goals' event logs and the units' records. It reads
//! the state directory and changes nothing. The nightly review is given this
//! and reads facts, not a model's reckoning of them; a model reviewing the
//! factory is partly grading its own work.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::Serialize;
use serde_json::Value;

/// The period a report covers, in Unix seconds: `since` included, `until`
/// not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Period {
    pub since: u64,
    pub until: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Report {
    pub period: Option<Period>,
    /// Only this project's records, when one was named.
    pub project: Option<String>,
    pub units: Units,
    /// Spent by units' agent runs, by directors and foremen, and per bead
    /// closed in the period. Estimated by the CLI, in US dollars.
    pub cost: Cost,
    /// How many fix rounds each unit that had any ran, most first.
    pub fix_rounds: Vec<Count>,
    /// Each step's runs that ended in the period, slowest in total first.
    pub steps: Vec<StepTime>,
    /// Why units parked, the same reason counted together, most first.
    pub parks: Vec<Reason>,
    /// How often each triage outcome fired; base runs by their verdict.
    pub triage: BTreeMap<String, u64>,
    /// Checks that ran until their time limit.
    pub timeouts: Vec<Timeout>,
    pub owner: Owner,
    /// Classes of defect that testers proved in the period, most beads first.
    pub classes: Vec<ClassFigure>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Units {
    pub started: u64,
    /// Merged pull requests.
    pub landed: u64,
    /// Beads absorbed into a sub-goal's branch.
    pub absorbed: u64,
    pub parked: u64,
    /// Units whose pipeline ended, landed or not.
    pub ended: u64,
    pub beads_closed: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Cost {
    pub units_usd: f64,
    pub tiers_usd: f64,
    /// Everything spent, over the beads closed. None when none closed.
    pub per_bead_closed_usd: Option<f64>,
    /// The units that cost most, most first.
    pub top_units: Vec<UnitCost>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct UnitCost {
    pub unit: String,
    pub usd: f64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Count {
    pub name: String,
    pub count: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StepTime {
    pub step: String,
    pub runs: u64,
    pub total_secs: u64,
    pub max_secs: u64,
    /// The unit of the longest run.
    pub slowest_unit: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Reason {
    /// The reason's first line, with numbers taken out.
    pub reason: String,
    pub count: u64,
    pub units: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Timeout {
    pub unit: String,
    pub command: String,
    pub secs: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Owner {
    /// Questions and buttons put to the owner by a goal.
    pub prompts: u64,
    /// How long each answered prompt waited, longest first.
    pub waits_secs: Vec<u64>,
    /// Prompts with no answer by the end of the period.
    pub unanswered: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ClassFigure {
    pub class: String,
    pub findings: u64,
    pub beads: u64,
}

struct Event {
    time: u64,
    kind: String,
    step: String,
    data: Value,
}

fn events(path: &Path) -> Vec<Event> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .map(|e| Event {
            time: e["time_unix"].as_u64().unwrap_or(0),
            kind: e["kind"].as_str().unwrap_or("").to_string(),
            step: e["step"].as_str().unwrap_or("").to_string(),
            data: e["data"].clone(),
        })
        .collect()
}

fn dirs(path: &Path) -> Vec<std::path::PathBuf> {
    let mut found: Vec<_> = std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    found.sort();
    found
}

fn project_of(record: &Path) -> Option<String> {
    let text = std::fs::read_to_string(record).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value["project"].as_str().map(str::to_string)
}

/// A reason's first line with what differs between instances (numbers)
/// taken out, so that the same reason is counted together.
fn reason_key(reason: &str) -> String {
    let first = reason.lines().next().unwrap_or("").trim();
    let mut out = String::new();
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        let hex = word.len() >= 7
            && word.chars().all(|c| c.is_ascii_hexdigit())
            && word.chars().any(|c| c.is_ascii_digit());
        if hex {
            out.push_str("<hex>");
        } else {
            for ch in word.chars() {
                if !ch.is_ascii_digit() {
                    out.push(ch);
                } else if !out.ends_with('#') {
                    out.push('#');
                }
            }
        }
        word.clear();
    };
    for ch in first.chars() {
        if ch.is_ascii_alphanumeric() {
            word.push(ch);
        } else {
            flush(&mut word, &mut out);
            out.push(ch);
        }
    }
    flush(&mut word, &mut out);
    out.chars().take(200).collect()
}

/// Build the report of `period` from the records under `state_dir`, for one
/// project or all.
pub fn build(state_dir: &Path, period: Period, project: Option<&str>) -> Report {
    let within = |time: u64| time >= period.since && time < period.until;
    let mut report = Report {
        period: Some(period),
        project: project.map(str::to_string),
        ..Report::default()
    };
    let mut unit_cost: BTreeMap<String, f64> = BTreeMap::new();
    let mut fixes: BTreeMap<String, u64> = BTreeMap::new();
    let mut steps: BTreeMap<String, StepTime> = BTreeMap::new();
    let mut parks: BTreeMap<String, Reason> = BTreeMap::new();
    let mut classes: BTreeMap<String, (u64, BTreeSet<String>)> = BTreeMap::new();

    for dir in dirs(&state_dir.join("units")) {
        let record = dir.join("unit.json");
        if let Some(wanted) = project
            && project_of(&record).as_deref() != Some(wanted)
        {
            continue;
        }
        let unit = dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut tester_rounds = BTreeSet::new();
        for event in events(&dir.join("events.jsonl")) {
            if !within(event.time) {
                continue;
            }
            match event.kind.as_str() {
                "unit.created" => report.units.started += 1,
                "landed" => report.units.landed += 1,
                "absorbed" => report.units.absorbed += 1,
                "unit.ended" => report.units.ended += 1,
                "bead.closed" => report.units.beads_closed += 1,
                "unit.parked" => {
                    report.units.parked += 1;
                    let reason = event.data["reason"].as_str().unwrap_or("");
                    let entry = parks.entry(reason_key(reason)).or_insert_with(|| Reason {
                        reason: reason_key(reason),
                        count: 0,
                        units: Vec::new(),
                    });
                    entry.count += 1;
                    if !entry.units.contains(&unit) {
                        entry.units.push(unit.clone());
                    }
                }
                "run.ended" => {
                    let usd = event.data["usage"]["cost_usd"].as_f64().unwrap_or(0.0);
                    *unit_cost.entry(unit.clone()).or_default() += usd;
                }
                "step.started" if event.step == "fix" => {
                    *fixes.entry(unit.clone()).or_default() += 1;
                }
                "step.ended" => {
                    let secs = event.data["secs"].as_u64().unwrap_or(0);
                    let entry = steps.entry(event.step.clone()).or_insert_with(|| StepTime {
                        step: event.step.clone(),
                        runs: 0,
                        total_secs: 0,
                        max_secs: 0,
                        slowest_unit: String::new(),
                    });
                    entry.runs += 1;
                    entry.total_secs += secs;
                    if secs >= entry.max_secs {
                        entry.max_secs = secs;
                        entry.slowest_unit = unit.clone();
                    }
                }
                "command.ran" if event.data["verdict"].as_str() == Some("TimedOut") => {
                    report.timeouts.push(Timeout {
                        unit: unit.clone(),
                        command: event.data["command"].as_str().unwrap_or("").to_string(),
                        secs: event.data["secs"].as_u64().unwrap_or(0),
                    });
                }
                "command.base" => {
                    let verdict = event.data["verdict"]
                        .as_str()
                        .map(|v| v.split([' ', '(']).next().unwrap_or(v).to_string())
                        .unwrap_or_else(|| "not run".to_string());
                    *report
                        .triage
                        .entry(format!("command.base: {verdict}"))
                        .or_default() += 1;
                }
                "command.flaky" | "command.host_fault" | "command.repeated" | "command.rerun" => {
                    *report.triage.entry(event.kind.clone()).or_default() += 1;
                }
                "regressions.added" => {
                    if let Some(round) = event.data["round"].as_u64() {
                        tester_rounds.insert(round);
                    }
                }
                _ => {}
            }
        }
        if !tester_rounds.is_empty() {
            let record: Value = std::fs::read_to_string(&record)
                .ok()
                .and_then(|text| serde_json::from_str(&text).ok())
                .unwrap_or(Value::Null);
            let bead = record["bead"].as_str().unwrap_or("").to_string();
            for regression in record["regressions"].as_array().into_iter().flatten() {
                let class = regression["category"].as_str().unwrap_or("");
                let round = regression["round"].as_u64().unwrap_or(0);
                if factory_brief::is_class_key(class) && tester_rounds.contains(&round) {
                    let (findings, beads) = classes.entry(class.to_string()).or_default();
                    *findings += 1;
                    beads.insert(bead.clone());
                }
            }
        }
    }

    for dir in dirs(&state_dir.join("goals")) {
        if let Some(wanted) = project
            && project_of(&dir.join("goal.json")).as_deref() != Some(wanted)
        {
            continue;
        }
        let mut asked: Option<u64> = None;
        for event in events(&dir.join("events.jsonl")) {
            match event.kind.as_str() {
                "director.ran" | "foreman.ran" | "executive.ran" if within(event.time) => {
                    report.cost.tiers_usd +=
                        event.data["usage"]["cost_usd"].as_f64().unwrap_or(0.0);
                }
                "slack.prompt" if within(event.time) => {
                    report.owner.prompts += 1;
                    if asked.is_some() {
                        // Asked again before an answer: the first still waits.
                        report.owner.unanswered += 1;
                    }
                    asked = Some(event.time);
                }
                "owner.message" | "owner.action" => {
                    if let Some(at) = asked.take()
                        && within(at)
                    {
                        report.owner.waits_secs.push(event.time.saturating_sub(at));
                    }
                }
                _ => {}
            }
        }
        if asked.is_some_and(within) {
            report.owner.unanswered += 1;
        }
    }

    report.cost.units_usd = unit_cost.values().sum();
    let spent = report.cost.units_usd + report.cost.tiers_usd;
    report.cost.per_bead_closed_usd =
        (report.units.beads_closed > 0).then(|| spent / report.units.beads_closed as f64);
    let mut top: Vec<UnitCost> = unit_cost
        .into_iter()
        .map(|(unit, usd)| UnitCost { unit, usd })
        .collect();
    top.sort_by(|a, b| b.usd.total_cmp(&a.usd).then(a.unit.cmp(&b.unit)));
    top.truncate(5);
    report.cost.top_units = top;

    let mut fix_rounds: Vec<Count> = fixes
        .into_iter()
        .map(|(name, count)| Count { name, count })
        .collect();
    fix_rounds.sort_by(|a, b| b.count.cmp(&a.count).then(a.name.cmp(&b.name)));
    report.fix_rounds = fix_rounds;

    let mut steps: Vec<StepTime> = steps.into_values().collect();
    steps.sort_by(|a, b| b.total_secs.cmp(&a.total_secs).then(a.step.cmp(&b.step)));
    report.steps = steps;

    let mut parks: Vec<Reason> = parks.into_values().collect();
    parks.sort_by(|a, b| b.count.cmp(&a.count).then(a.reason.cmp(&b.reason)));
    report.parks = parks;

    report
        .timeouts
        .sort_by(|a, b| b.secs.cmp(&a.secs).then(a.unit.cmp(&b.unit)));
    report.owner.waits_secs.sort_unstable_by(|a, b| b.cmp(a));

    let mut classes: Vec<ClassFigure> = classes
        .into_iter()
        .map(|(class, (findings, beads))| ClassFigure {
            class,
            findings,
            beads: beads.len() as u64,
        })
        .collect();
    classes.sort_by(|a, b| b.beads.cmp(&a.beads).then(a.class.cmp(&b.class)));
    report.classes = classes;
    report
}

fn duration(secs: u64) -> String {
    match secs {
        s if s >= 3600 => format!("{}h {:02}m", s / 3600, (s % 3600) / 60),
        s if s >= 60 => format!("{}m {:02}s", s / 60, s % 60),
        s => format!("{s}s"),
    }
}

impl Report {
    /// The report as JSON, for a program or an agent's brief.
    pub fn json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// The report as people read it.
    pub fn text(&self) -> String {
        let mut out = String::new();
        let mut line = |text: String| {
            out.push_str(&text);
            out.push('\n');
        };
        if let Some(period) = self.period {
            line(format!(
                "Report of {} to {} (Unix seconds){}",
                period.since,
                period.until,
                self.project
                    .as_ref()
                    .map(|p| format!(", project {p}"))
                    .unwrap_or_default()
            ));
        }
        let u = &self.units;
        line(format!(
            "\nUnits: {} started, {} landed, {} absorbed, {} parked, {} ended; {} bead(s) closed.",
            u.started, u.landed, u.absorbed, u.parked, u.ended, u.beads_closed
        ));
        line(format!(
            "Cost: ${:.2} in units, ${:.2} in directors and foremen{}.",
            self.cost.units_usd,
            self.cost.tiers_usd,
            self.cost
                .per_bead_closed_usd
                .map(|usd| format!(", ${usd:.2} per bead closed"))
                .unwrap_or_default()
        ));
        for top in &self.cost.top_units {
            line(format!("  ${:>7.2}  {}", top.usd, top.unit));
        }
        if !self.fix_rounds.is_empty() {
            line("\nFix rounds:".to_string());
            for count in &self.fix_rounds {
                line(format!("  {:>3}  {}", count.count, count.name));
            }
        }
        if !self.steps.is_empty() {
            line("\nSteps (runs, total, longest):".to_string());
            for step in &self.steps {
                line(format!(
                    "  {:<14} {:>4}  {:>9}  {:>9}  {}",
                    step.step,
                    step.runs,
                    duration(step.total_secs),
                    duration(step.max_secs),
                    step.slowest_unit
                ));
            }
        }
        if !self.parks.is_empty() {
            line("\nParked because:".to_string());
            for park in &self.parks {
                line(format!("  {:>3}  {}", park.count, park.reason));
            }
        }
        if !self.triage.is_empty() {
            line("\nTriage:".to_string());
            for (kind, count) in &self.triage {
                line(format!("  {count:>3}  {kind}"));
            }
        }
        if !self.timeouts.is_empty() {
            line("\nChecks that ran to their time limit:".to_string());
            for timeout in &self.timeouts {
                line(format!(
                    "  {:>9}  {}  {}",
                    duration(timeout.secs),
                    timeout.command,
                    timeout.unit
                ));
            }
        }
        let owner = &self.owner;
        line(format!(
            "\nOwner: {} prompt(s), {} answered{}, {} unanswered.",
            owner.prompts,
            owner.waits_secs.len(),
            owner
                .waits_secs
                .first()
                .map(|longest| format!(" (longest wait {})", duration(*longest)))
                .unwrap_or_default(),
            owner.unanswered
        ));
        if !self.classes.is_empty() {
            line("\nClasses of defect testers proved:".to_string());
            for class in &self.classes {
                line(format!(
                    "  {:<36} {} finding(s) on {} bead(s)",
                    class.class, class.findings, class.beads
                ));
            }
        }
        out
    }
}

/// When `since` means, from `now`: `24h`, `7d`, `90m`, or Unix seconds.
pub fn parse_since(since: &str, now: u64) -> Option<u64> {
    let since = since.trim();
    let (number, unit) =
        since.split_at(since.find(|c: char| !c.is_ascii_digit())?.min(since.len()));
    let number: u64 = number.parse().ok()?;
    let secs = match unit {
        "m" => number * 60,
        "h" => number * 3600,
        "d" => number * 86_400,
        _ => return None,
    };
    Some(now.saturating_sub(secs))
}

/// [`parse_since`], or Unix seconds when it is all digits.
pub fn since_time(since: &str, now: u64) -> Option<u64> {
    if since.chars().all(|c| c.is_ascii_digit()) && !since.is_empty() {
        return since.parse().ok();
    }
    parse_since(since, now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write_events(dir: &Path, events: &[Value]) {
        std::fs::create_dir_all(dir).unwrap();
        let text: String = events.iter().map(|e| format!("{e}\n")).collect();
        std::fs::write(dir.join("events.jsonl"), text).unwrap();
    }

    fn ev(time: u64, kind: &str, step: Option<&str>, data: Value) -> Value {
        json!({ "schema_version": 1, "time_unix": time, "kind": kind, "step": step, "data": data })
    }

    #[test]
    fn a_report_counts_what_the_records_show_within_its_period() {
        let root = std::env::temp_dir().join(format!("factory-report-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // One unit that landed after a fix, one that parked for the host
        // and hit its time limit, one of another project, and one outside
        // the period.
        let a = root.join("units/toy-1-100");
        write_events(
            &a,
            &[
                ev(100, "unit.created", None, json!({})),
                ev(
                    110,
                    "run.ended",
                    Some("build"),
                    json!({"usage": {"cost_usd": 2.0}}),
                ),
                ev(
                    120,
                    "step.ended",
                    Some("build"),
                    json!({"secs": 60, "outcome": "done"}),
                ),
                ev(
                    130,
                    "step.ended",
                    Some("test"),
                    json!({"secs": 300, "outcome": "fail"}),
                ),
                ev(
                    131,
                    "command.base",
                    Some("test"),
                    json!({"verdict": "Passed"}),
                ),
                ev(140, "step.started", Some("fix"), json!({})),
                ev(
                    150,
                    "run.ended",
                    Some("fix"),
                    json!({"usage": {"cost_usd": 1.0}}),
                ),
                ev(
                    160,
                    "step.ended",
                    Some("test"),
                    json!({"secs": 200, "outcome": "pass"}),
                ),
                ev(165, "regressions.added", Some("break"), json!({"round": 1})),
                ev(170, "landed", Some("land"), json!({})),
                ev(171, "bead.closed", None, json!({})),
                ev(172, "unit.ended", None, json!({})),
            ],
        );
        std::fs::write(
            a.join("unit.json"),
            json!({"project": "toy", "bead": "toy-1", "regressions": [
                {"round": 1, "category": "error-path-cleanup"},
                {"round": 1, "category": "empty-input"},
                {"round": 2, "category": "double-call"}
            ]})
            .to_string(),
        )
        .unwrap();
        let b = root.join("units/toy-2-200");
        write_events(
            &b,
            &[
                ev(200, "unit.created", None, json!({})),
                ev(
                    210,
                    "command.ran",
                    Some("test-linux"),
                    json!({"command": "test-linux", "verdict": "TimedOut", "secs": 3600}),
                ),
                ev(211, "command.host_fault", Some("test-linux"), json!({})),
                ev(
                    212,
                    "unit.parked",
                    Some("test-linux"),
                    json!({"reason": "the command `test-linux` failed (exit code 2) and could not run properly\nmore"}),
                ),
                ev(
                    213,
                    "run.ended",
                    Some("build"),
                    json!({"usage": {"cost_usd": 4.0}}),
                ),
            ],
        );
        std::fs::write(
            b.join("unit.json"),
            json!({"project": "toy", "bead": "toy-2"}).to_string(),
        )
        .unwrap();
        let c = root.join("units/other-1-300");
        write_events(&c, &[ev(300, "unit.created", None, json!({}))]);
        std::fs::write(c.join("unit.json"), json!({"project": "other"}).to_string()).unwrap();
        let old = root.join("units/toy-0-1");
        write_events(
            &old,
            &[
                ev(1, "unit.created", None, json!({})),
                ev(2, "landed", None, json!({})),
            ],
        );
        std::fs::write(old.join("unit.json"), json!({"project": "toy"}).to_string()).unwrap();
        let goal = root.join("goals/toy-g1");
        write_events(
            &goal,
            &[
                ev(
                    105,
                    "director.ran",
                    None,
                    json!({"usage": {"cost_usd": 3.0}}),
                ),
                ev(106, "slack.prompt", None, json!({})),
                ev(406, "owner.action", None, json!({})),
                ev(500, "slack.prompt", None, json!({})),
            ],
        );
        std::fs::write(
            goal.join("goal.json"),
            json!({"project": "toy"}).to_string(),
        )
        .unwrap();

        let report = build(
            &root,
            Period {
                since: 50,
                until: 1000,
            },
            Some("toy"),
        );
        assert_eq!(
            report.units,
            Units {
                started: 2,
                landed: 1,
                absorbed: 0,
                parked: 1,
                ended: 1,
                beads_closed: 1
            }
        );
        assert_eq!(report.cost.units_usd, 7.0);
        assert_eq!(report.cost.tiers_usd, 3.0);
        assert_eq!(report.cost.per_bead_closed_usd, Some(10.0));
        assert_eq!(
            report.cost.top_units[0],
            UnitCost {
                unit: "toy-2-200".to_string(),
                usd: 4.0
            }
        );
        assert_eq!(
            report.fix_rounds,
            [Count {
                name: "toy-1-100".to_string(),
                count: 1
            }]
        );
        let test = report.steps.iter().find(|s| s.step == "test").unwrap();
        assert_eq!((test.runs, test.total_secs, test.max_secs), (2, 500, 300));
        assert_eq!(report.parks.len(), 1);
        assert_eq!(
            report.parks[0].reason,
            "the command `test-linux` failed (exit code #) and could not run properly"
        );
        assert_eq!(report.triage.get("command.base: Passed"), Some(&1));
        assert_eq!(report.triage.get("command.host_fault"), Some(&1));
        assert_eq!(
            report.timeouts,
            [Timeout {
                unit: "toy-2-200".to_string(),
                command: "test-linux".to_string(),
                secs: 3600
            }]
        );
        assert_eq!(
            report.owner,
            Owner {
                prompts: 2,
                waits_secs: vec![300],
                unanswered: 1
            }
        );
        let classes: Vec<&str> = report.classes.iter().map(|c| c.class.as_str()).collect();
        assert_eq!(
            classes,
            ["empty-input", "error-path-cleanup"],
            "round 2 is not in the period's events"
        );
        let text = report.text();
        for expected in [
            "2 started, 1 landed",
            "$10.00 per bead closed",
            "test-linux",
            "1 unanswered",
        ] {
            assert!(text.contains(expected), "{expected}: {text}");
        }
        // Every project: the other one's unit counts too.
        assert_eq!(
            build(
                &root,
                Period {
                    since: 50,
                    until: 1000
                },
                None
            )
            .units
            .started,
            3
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn park_reasons_that_differ_only_in_commits_and_numbers_count_together() {
        let one = reason_key(
            "step `break` needs `reports`: it describes commit `9f1628eac63`, but the code is now at `641b0a6e9f`\nmore",
        );
        let two = reason_key(
            "step `break` needs `reports`: it describes commit `1f90f52aa01`, but the code is now at `db923951c2`",
        );
        assert_eq!(one, two);
        assert_eq!(
            one,
            "step `break` needs `reports`: it describes commit `<hex>`, but the code is now at `<hex>`"
        );
        assert_eq!(reason_key("round 5 of 3"), "round # of #");
        assert_eq!(reason_key("the feature works"), "the feature works");
    }

    #[test]
    fn a_period_is_named_as_a_span_back_from_now_or_as_unix_seconds() {
        assert_eq!(since_time("24h", 100_000), Some(100_000 - 86_400));
        assert_eq!(since_time("7d", 1_000_000), Some(1_000_000 - 7 * 86_400));
        assert_eq!(since_time("90m", 10_000), Some(10_000 - 5400));
        assert_eq!(since_time("1794800000", 0), Some(1_794_800_000));
        assert_eq!(since_time("soon", 0), None);
        assert_eq!(since_time("", 0), None);
    }
}
