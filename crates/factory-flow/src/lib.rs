//! A flow: what a bead goes through in one project, from being chosen to being
//! closed. This crate reads a flow file, refuses one that is not safe to run,
//! and walks a unit through it one outcome at a time. It does no I/O beyond
//! parsing text.
//!
//! This is the thin first cut: steps run one at a time (no groups), and the
//! only factory action is `publish`.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FlowError {
    #[error("flow file does not parse: {0}")]
    Parse(String),
    /// The flow parses but is not safe to run. Each entry names one problem.
    #[error("flow `{flow}` is refused:\n  - {}", problems.join("\n  - "))]
    Invalid { flow: String, problems: Vec<String> },
    #[error("step `{step}` cannot end with `{outcome}`: the flow has no route for it")]
    Unrouted { step: String, outcome: String },
    #[error("the flow has no step `{0}`")]
    UnknownStep(String),
}

/// Where an outcome leads.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub enum Target {
    Step(String),
    /// Stop working the unit and keep everything for the owner.
    Park,
    /// Put the step's question to the owner, then continue the same step.
    Ask,
    /// The flow is finished.
    End,
}

impl From<Target> for String {
    fn from(target: Target) -> String {
        match target {
            Target::Step(id) => id,
            Target::Park => "park".to_string(),
            Target::Ask => "ask".to_string(),
            Target::End => "end".to_string(),
        }
    }
}

impl TryFrom<String> for Target {
    type Error = String;

    fn try_from(text: String) -> Result<Self, String> {
        Ok(match text.as_str() {
            "" => return Err("a route needs a target".to_string()),
            "park" => Target::Park,
            "ask" => Target::Ask,
            "end" => Target::End,
            _ => Target::Step(text),
        })
    }
}

/// A route out of a step. `max` bounds how often it may be taken; when it is
/// used up, `then` applies instead.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Route {
    pub to: Target,
    pub max: Option<u32>,
    pub then: Target,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawRoute {
    Plain(Target),
    Bounded {
        to: Target,
        max: Option<u32>,
        then: Option<Target>,
    },
}

impl<'de> Deserialize<'de> for Route {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match RawRoute::deserialize(deserializer)? {
            RawRoute::Plain(to) => Route {
                to,
                max: None,
                then: Target::Park,
            },
            RawRoute::Bounded { to, max, then } => Route {
                to,
                max,
                then: then.unwrap_or(Target::Park),
            },
        })
    }
}

/// Whether an agent step starts a new session or continues an earlier step's.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum SessionRule {
    Fresh,
    Continue(String),
}

/// Where an agent step works.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Workspace {
    /// The unit's own worktree. What the step changes is kept as a checkpoint.
    Unit,
    /// A throwaway worktree at the unit's current checkpoint. Nothing the step
    /// does there reaches the unit's branch.
    Scratch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExitPolicy {
    /// A check: exit 0 is `pass`, anything else is `fail`.
    MustPass,
    /// Exit 0 is `fail`: for example, new tests must be red before the code exists.
    MustFail,
    /// Always `pass`; the output is kept as a report.
    ReportOnly,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum StepKind {
    Agent {
        role: String,
        model: String,
        effort: String,
        session: SessionRule,
        workspace: Workspace,
        /// The path scopes this step's changes must stay within. Empty means
        /// the step may change nothing.
        writes: Vec<String>,
    },
    Command {
        /// A named command from the project record.
        command: String,
        policy: ExitPolicy,
    },
    Action {
        action: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Step {
    pub id: String,
    pub kind: StepKind,
    /// The artifact a command step writes, such as a directory of reports.
    pub produces: Option<String>,
    /// The artifacts an agent step is given. Each must have been produced,
    /// on the code as it now stands, on every path that reaches the step.
    pub needs: Vec<String>,
    /// For a command step: path scopes. The command runs only when the
    /// unit's change touches one of them, and passes unrun otherwise.
    pub when_changed: Vec<String>,
    /// For an agent step: the effort of each round, the last one repeating.
    /// Empty means the step's own effort every round.
    pub effort_by_round: Vec<String>,
    pub timeout_secs: Option<u64>,
    /// Outcome to route.
    pub on: BTreeMap<String, Route>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Flow {
    pub id: String,
    pub start: String,
    pub steps: Vec<Step>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFlow {
    id: String,
    start: String,
    #[serde(default, rename = "step")]
    steps: Vec<RawStep>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStep {
    id: String,
    kind: String,
    role: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    session: Option<String>,
    workspace: Option<Workspace>,
    writes: Option<Vec<String>>,
    command: Option<String>,
    policy: Option<ExitPolicy>,
    action: Option<String>,
    produces: Option<String>,
    needs: Option<Vec<String>>,
    when_changed: Option<Vec<String>>,
    effort_by_round: Option<Vec<String>>,
    timeout_secs: Option<u64>,
    #[serde(default)]
    on: BTreeMap<String, Route>,
}

/// A factory action: fixed behaviour, placed by the flow.
pub struct ActionSpec {
    pub name: &'static str,
    /// Every outcome the action can end with.
    pub outcomes: &'static [&'static str],
    /// The outcomes a flow must route.
    pub required: &'static [&'static str],
}

pub const ACTIONS: [ActionSpec; 4] = [
    // One final commit, a push, and the pull request.
    ActionSpec {
        name: "publish",
        outcomes: &["done", "failed"],
        required: &["done"],
    },
    // Wait for the pull request's checks: `red` as soon as one fails,
    // `green` once every one has passed.
    ActionSpec {
        name: "await-ci",
        outcomes: &["green", "red"],
        required: &["green", "red"],
    },
    // Add the unit's change, as one commit, to the branch of the sub-goal it
    // belongs to. Nothing is pushed and the bead stays open.
    ActionSpec {
        name: "absorb",
        outcomes: &["done", "failed"],
        required: &["done"],
    },
    // Merge the pull request, confirm it, close the bead.
    ActionSpec {
        name: "land",
        outcomes: &["done", "failed"],
        required: &["done"],
    },
];

/// What a flow is checked against: the things it may name.
#[derive(Clone, Debug, Default)]
pub struct Registry {
    /// Each role with the outcomes it may return.
    pub roles: BTreeMap<String, BTreeSet<String>>,
    pub models: BTreeSet<String>,
    pub commands: BTreeSet<String>,
    pub scopes: BTreeSet<String>,
}

impl Flow {
    /// Parse a flow file. The result has the right shape but has not been
    /// checked against a registry: call [`Flow::check`] before running it.
    pub fn parse(text: &str) -> Result<Flow, FlowError> {
        let raw: RawFlow = toml::from_str(text).map_err(|err| FlowError::Parse(err.to_string()))?;
        let mut problems = Vec::new();
        let mut steps = Vec::new();
        for step in raw.steps {
            match convert(step) {
                Ok(step) => steps.push(step),
                Err(problem) => problems.push(problem),
            }
        }
        if !problems.is_empty() {
            return Err(FlowError::Invalid {
                flow: raw.id,
                problems,
            });
        }
        Ok(Flow {
            id: raw.id,
            start: raw.start,
            steps,
        })
    }

    pub fn step(&self, id: &str) -> Option<&Step> {
        self.steps.iter().find(|step| step.id == id)
    }

    fn index(&self, id: &str) -> Option<usize> {
        self.steps.iter().position(|step| step.id == id)
    }

    /// Refuse a flow that is not safe to run, naming every problem found.
    pub fn check(&self, registry: &Registry) -> Result<(), FlowError> {
        let mut problems = Vec::new();
        let mut seen = BTreeSet::new();
        for step in &self.steps {
            if !seen.insert(step.id.as_str()) {
                problems.push(format!("step `{}` is defined twice", step.id));
            }
            if matches!(step.id.as_str(), "park" | "ask" | "end") {
                problems.push(format!("`{}` is reserved and cannot name a step", step.id));
            }
        }
        if self.step(&self.start).is_none() {
            problems.push(format!("start step `{}` does not exist", self.start));
        }

        for (index, step) in self.steps.iter().enumerate() {
            let id = &step.id;
            if step.on.is_empty() {
                problems.push(format!("step `{id}` has no routes, so it can never end"));
            }
            match &step.kind {
                StepKind::Agent {
                    role,
                    model,
                    session,
                    workspace,
                    writes,
                    ..
                } => {
                    match registry.roles.get(role) {
                        None => problems.push(format!("step `{id}`: unknown role `{role}`")),
                        Some(allowed) => {
                            for outcome in step.on.keys() {
                                if !allowed.contains(outcome) {
                                    problems.push(format!(
                                        "step `{id}`: role `{role}` cannot return `{outcome}`"
                                    ));
                                }
                            }
                        }
                    }
                    if !registry.models.contains(model) {
                        problems.push(format!("step `{id}`: model `{model}` is not allowed"));
                    }
                    for scope in writes {
                        if !registry.scopes.contains(scope) {
                            problems.push(format!("step `{id}`: unknown path scope `{scope}`"));
                        }
                    }
                    if *workspace == Workspace::Scratch && !writes.is_empty() {
                        problems.push(format!(
                            "step `{id}`: a scratch workspace is thrown away, so it cannot have write scopes"
                        ));
                    }
                    if let SessionRule::Continue(earlier) = session {
                        match self.step(earlier).map(|other| &other.kind) {
                            Some(StepKind::Agent {
                                workspace: other, ..
                            }) => {
                                // A session lives where it was started.
                                if other != workspace {
                                    problems.push(format!(
                                        "step `{id}`: continues the session of `{earlier}`, which runs in a different workspace"
                                    ));
                                }
                            }
                            Some(_) => problems.push(format!(
                                "step `{id}`: continues the session of `{earlier}`, which is not an agent step"
                            )),
                            None => problems.push(format!(
                                "step `{id}`: continues the session of `{earlier}`, which does not exist"
                            )),
                        }
                    }
                }
                StepKind::Command { command, .. } => {
                    if !registry.commands.contains(command) {
                        problems.push(format!("step `{id}`: unknown command `{command}`"));
                    }
                    for scope in &step.when_changed {
                        if !registry.scopes.contains(scope) {
                            problems.push(format!("step `{id}`: unknown path scope `{scope}`"));
                        }
                    }
                    for outcome in ["pass", "fail"] {
                        if !step.on.contains_key(outcome) {
                            problems.push(format!(
                                "step `{id}`: a command step must route `{outcome}`"
                            ));
                        }
                    }
                    for outcome in step.on.keys() {
                        if !matches!(outcome.as_str(), "pass" | "fail") {
                            problems.push(format!(
                                "step `{id}`: a command step cannot end with `{outcome}`"
                            ));
                        }
                    }
                }
                StepKind::Action { action } => {
                    match ACTIONS.iter().find(|spec| spec.name == action) {
                        None => problems.push(format!("step `{id}`: unknown action `{action}`")),
                        Some(spec) => {
                            for outcome in step.on.keys() {
                                if !spec.outcomes.contains(&outcome.as_str()) {
                                    problems.push(format!(
                                        "step `{id}`: the action `{action}` cannot end with `{outcome}`"
                                    ));
                                }
                            }
                            for outcome in spec.required {
                                if !step.on.contains_key(*outcome) {
                                    problems.push(format!(
                                        "step `{id}`: the action `{action}` must route `{outcome}`"
                                    ));
                                }
                            }
                        }
                    }
                }
            }

            for (outcome, route) in &step.on {
                for target in [&route.to, &route.then] {
                    if let Target::Step(to) = target
                        && self.step(to).is_none()
                    {
                        problems.push(format!(
                            "step `{id}`: `{outcome}` leads to `{to}`, which does not exist"
                        ));
                    }
                }
                if route.to == Target::Ask && !matches!(step.kind, StepKind::Agent { .. }) {
                    problems.push(format!("step `{id}`: only an agent step can ask the owner"));
                }
                // A route that does not move forward could repeat for ever.
                let backward = match &route.to {
                    Target::Step(to) => self.index(to).is_some_and(|target| target <= index),
                    Target::Ask => true,
                    Target::Park | Target::End => false,
                };
                if backward && route.max.is_none() {
                    problems.push(format!(
                        "step `{id}`: `{outcome}` leads backwards with no maximum"
                    ));
                }
                if backward && matches!(&route.then, Target::Ask) {
                    problems.push(format!(
                        "step `{id}`: `{outcome}` cannot fall back to asking the owner"
                    ));
                }
                if let (true, Target::Step(then)) = (backward, &route.then)
                    && self.index(then).is_some_and(|target| target <= index)
                {
                    problems.push(format!(
                        "step `{id}`: when `{outcome}` is used up it must lead forwards, not to `{then}`"
                    ));
                }
            }
        }

        if problems.is_empty() {
            problems.extend(self.reachability());
        }
        if problems.is_empty() {
            problems.extend(self.artifact_problems());
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(FlowError::Invalid {
                flow: self.id.clone(),
                problems,
            })
        }
    }

    fn successors<'a>(&'a self, step: &'a Step) -> impl Iterator<Item = &'a Target> {
        step.on
            .values()
            .flat_map(|route| [&route.to, &route.then].into_iter())
    }

    /// Whether a step can change the unit's code, which makes every artifact
    /// written about the code out of date.
    fn changes_code(step: &Step) -> bool {
        matches!(
            &step.kind,
            StepKind::Agent { workspace: Workspace::Unit, writes, .. } if !writes.is_empty()
        )
    }

    /// The artifacts sure to exist, and to describe the current code, when
    /// each step starts: on every path from the start, some step produced
    /// them after the last step that could change the code.
    pub fn artifacts_on_entry(&self) -> BTreeMap<String, BTreeSet<String>> {
        let all: BTreeSet<String> = self
            .steps
            .iter()
            .filter_map(|step| step.produces.clone())
            .collect();
        // Start from "everything" and remove what some path does not supply.
        let mut entry: BTreeMap<String, BTreeSet<String>> = self
            .steps
            .iter()
            .map(|step| {
                let known = if step.id == self.start {
                    BTreeSet::new()
                } else {
                    all.clone()
                };
                (step.id.clone(), known)
            })
            .collect();
        loop {
            let mut changed = false;
            for step in &self.steps {
                let mut leaving = if Self::changes_code(step) {
                    BTreeSet::new()
                } else {
                    entry.get(&step.id).cloned().unwrap_or_default()
                };
                leaving.extend(step.produces.clone());
                for target in self.successors(step) {
                    let Target::Step(next) = target else { continue };
                    if let Some(known) = entry.get_mut(next) {
                        let before = known.len();
                        known.retain(|artifact| leaving.contains(artifact));
                        changed |= known.len() != before;
                    }
                }
            }
            if !changed {
                return entry;
            }
        }
    }

    fn artifact_problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let entry = self.artifacts_on_entry();
        for step in &self.steps {
            for artifact in &step.needs {
                if !self
                    .steps
                    .iter()
                    .any(|other| other.produces.as_ref() == Some(artifact))
                {
                    problems.push(format!(
                        "step `{}`: needs `{artifact}`, which no step produces",
                        step.id
                    ));
                } else if !entry
                    .get(&step.id)
                    .is_some_and(|known| known.contains(artifact))
                {
                    problems.push(format!(
                        "step `{}`: needs `{artifact}`, but on some path that reaches it `{artifact}` has not been produced since the code last changed",
                        step.id
                    ));
                }
            }
        }
        problems
    }

    /// Every step can be reached from the start, and from every step some
    /// path reaches the end.
    fn reachability(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let mut reached = BTreeSet::new();
        let mut queue = vec![self.start.as_str()];
        while let Some(id) = queue.pop() {
            if !reached.insert(id) {
                continue;
            }
            if let Some(step) = self.step(id) {
                for target in self.successors(step) {
                    if let Target::Step(next) = target {
                        queue.push(next);
                    }
                }
            }
        }
        for step in &self.steps {
            if !reached.contains(step.id.as_str()) {
                problems.push(format!(
                    "step `{}` cannot be reached from the start",
                    step.id
                ));
            }
        }

        // Work backwards from the end: a step can finish if any route leads to
        // the end or to a step that can finish.
        let mut can_end: BTreeSet<&str> = BTreeSet::new();
        loop {
            let before = can_end.len();
            for step in &self.steps {
                let finishes = self.successors(step).any(|target| match target {
                    Target::End => true,
                    Target::Step(next) => can_end.contains(next.as_str()),
                    Target::Park | Target::Ask => false,
                });
                if finishes {
                    can_end.insert(&step.id);
                }
            }
            if can_end.len() == before {
                break;
            }
        }
        for step in &self.steps {
            if !can_end.contains(step.id.as_str()) {
                problems.push(format!("no path leads from step `{}` to the end", step.id));
            }
        }
        problems
    }

    /// The outcomes a step may end with: exactly the ones the flow routes.
    pub fn outcomes(&self, step: &str) -> Result<Vec<String>, FlowError> {
        let step = self
            .step(step)
            .ok_or_else(|| FlowError::UnknownStep(step.to_string()))?;
        Ok(step.on.keys().cloned().collect())
    }

    /// The most steps a unit can run before the flow must end or park. Every
    /// route that does not move forward has a maximum, so this is finite.
    pub fn longest_run(&self) -> u64 {
        // Each bounded route can restart the forward pass at most `max` times.
        let restarts: u64 = self
            .steps
            .iter()
            .flat_map(|step| step.on.values())
            .filter_map(|route| route.max)
            .map(u64::from)
            .sum();
        (restarts + 1) * self.steps.len() as u64
    }
}

fn convert(raw: RawStep) -> Result<Step, String> {
    let id = raw.id.clone();
    let unused = |fields: &[(&str, bool)]| -> Result<(), String> {
        for (name, present) in fields {
            if *present {
                return Err(format!(
                    "step `{id}`: `{name}` does not apply to a {} step",
                    raw.kind
                ));
            }
        }
        Ok(())
    };
    let need = |name: &str, value: &Option<String>| -> Result<String, String> {
        value
            .clone()
            .ok_or_else(|| format!("step `{id}`: a {} step needs `{name}`", raw.kind))
    };
    let kind = match raw.kind.as_str() {
        "agent" => {
            unused(&[
                ("command", raw.command.is_some()),
                ("policy", raw.policy.is_some()),
                ("action", raw.action.is_some()),
            ])?;
            let session = match raw.session.as_deref() {
                None | Some("fresh") => SessionRule::Fresh,
                Some(other) => match other.strip_prefix("continue:") {
                    Some(step) if !step.is_empty() => SessionRule::Continue(step.to_string()),
                    _ => {
                        return Err(format!(
                            "step `{id}`: session must be `fresh` or `continue:<step>`"
                        ));
                    }
                },
            };
            StepKind::Agent {
                role: need("role", &raw.role)?,
                model: need("model", &raw.model)?,
                effort: need("effort", &raw.effort)?,
                session,
                workspace: raw.workspace.unwrap_or(Workspace::Unit),
                writes: raw.writes.clone().unwrap_or_default(),
            }
        }
        "command" => {
            unused(&[
                ("role", raw.role.is_some()),
                ("model", raw.model.is_some()),
                ("effort", raw.effort.is_some()),
                ("session", raw.session.is_some()),
                ("workspace", raw.workspace.is_some()),
                ("writes", raw.writes.is_some()),
                ("action", raw.action.is_some()),
            ])?;
            StepKind::Command {
                command: need("command", &raw.command)?,
                policy: raw.policy.unwrap_or(ExitPolicy::MustPass),
            }
        }
        "action" => {
            unused(&[
                ("role", raw.role.is_some()),
                ("model", raw.model.is_some()),
                ("effort", raw.effort.is_some()),
                ("session", raw.session.is_some()),
                ("workspace", raw.workspace.is_some()),
                ("writes", raw.writes.is_some()),
                ("command", raw.command.is_some()),
                ("policy", raw.policy.is_some()),
            ])?;
            StepKind::Action {
                action: need("action", &raw.action)?,
            }
        }
        other => return Err(format!("step `{id}`: unknown kind `{other}`")),
    };
    if raw.produces.is_some() && !matches!(kind, StepKind::Command { .. }) {
        return Err(format!(
            "step `{}`: only a command step can produce an artifact",
            raw.id
        ));
    }
    if raw.when_changed.is_some() && !matches!(kind, StepKind::Command { .. }) {
        return Err(format!(
            "step `{}`: only a command step can depend on what changed",
            raw.id
        ));
    }
    if raw.effort_by_round.is_some() && !matches!(kind, StepKind::Agent { .. }) {
        return Err(format!(
            "step `{}`: only an agent step has an effort",
            raw.id
        ));
    }
    if raw.needs.is_some() && !matches!(kind, StepKind::Agent { .. }) {
        return Err(format!(
            "step `{}`: only an agent step can need an artifact",
            raw.id
        ));
    }
    Ok(Step {
        id: raw.id,
        kind,
        produces: raw.produces,
        needs: raw.needs.unwrap_or_default(),
        when_changed: raw.when_changed.unwrap_or_default(),
        effort_by_round: raw.effort_by_round.unwrap_or_default(),
        timeout_secs: raw.timeout_secs,
        on: raw.on,
    })
}

/// Where a unit is in its flow.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub step: String,
    /// How often each bounded route has been taken, keyed `step:outcome`.
    #[serde(default)]
    pub used: BTreeMap<String, u32>,
    /// How often each step has been entered.
    #[serde(default)]
    pub rounds: BTreeMap<String, u32>,
}

/// What happens after a step ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Next {
    /// Run this step.
    Step(String),
    /// Ask the owner, then run the same step again.
    Ask,
    /// Stop, for this reason.
    Park(String),
    End,
}

impl Position {
    pub fn start(flow: &Flow) -> Position {
        let mut position = Position {
            step: flow.start.clone(),
            used: BTreeMap::new(),
            rounds: BTreeMap::new(),
        };
        position.rounds.insert(flow.start.clone(), 1);
        position
    }

    /// Which round of its step the unit is in, counting from 1.
    pub fn round(&self) -> u32 {
        self.rounds.get(&self.step).copied().unwrap_or(1)
    }

    /// Route the outcome the current step ended with, and move.
    pub fn advance(&mut self, flow: &Flow, outcome: &str) -> Result<Next, FlowError> {
        let step = flow
            .step(&self.step)
            .ok_or_else(|| FlowError::UnknownStep(self.step.clone()))?;
        let route = step.on.get(outcome).ok_or_else(|| FlowError::Unrouted {
            step: self.step.clone(),
            outcome: outcome.to_string(),
        })?;
        let key = format!("{}:{outcome}", self.step);
        let mut exhausted = false;
        let target = match route.max {
            Some(max) => {
                let used = self.used.entry(key).or_insert(0);
                if *used >= max {
                    exhausted = true;
                    &route.then
                } else {
                    *used += 1;
                    &route.to
                }
            }
            None => &route.to,
        };
        Ok(match target {
            Target::Step(next) => {
                self.step = next.clone();
                *self.rounds.entry(next.clone()).or_insert(0) += 1;
                Next::Step(next.clone())
            }
            Target::Ask => Next::Ask,
            Target::End => Next::End,
            Target::Park if exhausted => Next::Park(format!(
                "step `{}` ended with `{outcome}` more often than the flow allows",
                step.id
            )),
            Target::Park => Next::Park(format!("step `{}` ended with `{outcome}`", step.id)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape of the first real flow, on made-up models.
    const FLOW: &str = r#"
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
on.already_done = "park"
on.needs_owner = { to = "ask", max = 2 }

[[step]]
id = "test"
kind = "command"
command = "test"
on.pass = "break"
on.fail = { to = "fix", max = 2 }

[[step]]
id = "break"
kind = "agent"
role = "tester"
model = "m"
effort = "high"
workspace = "scratch"
on.held = "review"
on.broken = { to = "fix", max = 2 }

[[step]]
id = "review"
kind = "agent"
role = "reviewer"
model = "m"
effort = "medium"
workspace = "scratch"
on.approve = "describe"
on.rework = { to = "fix", max = 1 }

[[step]]
id = "fix"
kind = "agent"
role = "builder"
model = "m"
effort = "medium"
session = "continue:build"
writes = ["code"]
on.done = { to = "test", max = 5 }

[[step]]
id = "describe"
kind = "agent"
role = "publisher"
model = "m"
effort = "low"
session = "continue:build"
on.ready = "publish"

[[step]]
id = "publish"
kind = "action"
action = "publish"
on.done = "end"
on.failed = "park"
"#;

    fn registry() -> Registry {
        let set = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>();
        let mut roles = BTreeMap::new();
        roles.insert(
            "builder".to_string(),
            set(&["done", "already_done", "needs_owner"]),
        );
        roles.insert("tester".to_string(), set(&["held", "broken"]));
        roles.insert("reviewer".to_string(), set(&["approve", "rework"]));
        roles.insert("publisher".to_string(), set(&["ready"]));
        Registry {
            roles,
            models: set(&["m"]),
            commands: set(&["test"]),
            scopes: set(&["code"]),
        }
    }

    fn problems(text: &str) -> Vec<String> {
        match Flow::parse(text).and_then(|flow| flow.check(&registry())) {
            Err(FlowError::Invalid { problems, .. }) => problems,
            other => panic!("expected the flow to be refused, got {other:?}"),
        }
    }

    #[test]
    fn the_reference_shape_is_accepted() {
        let flow = Flow::parse(FLOW).unwrap();
        flow.check(&registry()).unwrap();
        assert_eq!(flow.outcomes("break").unwrap(), ["broken", "held"]);
        assert!(flow.longest_run() < 200);
    }

    #[test]
    fn an_unknown_target_is_refused() {
        let text = FLOW.replace(r#"on.held = "review""#, r#"on.held = "nowhere""#);
        assert!(problems(&text).contains(
            &"step `break`: `held` leads to `nowhere`, which does not exist".to_string()
        ));
    }

    #[test]
    fn a_backward_route_without_a_maximum_is_refused() {
        let text = FLOW.replace(
            r#"on.fail = { to = "fix", max = 2 }"#,
            r#"on.fail = "build""#,
        );
        assert_eq!(
            problems(&text),
            ["step `test`: `fail` leads backwards with no maximum"]
        );
    }

    #[test]
    fn an_outcome_the_role_cannot_return_is_refused() {
        let text = FLOW.replace(
            r#"on.held = "review""#,
            "on.held = \"review\"\non.bored = \"park\"",
        );
        assert_eq!(
            problems(&text),
            ["step `break`: role `tester` cannot return `bored`"]
        );
    }

    #[test]
    fn a_command_step_must_route_both_results() {
        let text = FLOW.replace("on.pass = \"break\"\n", "");
        let found = problems(&text);
        assert!(found.contains(&"step `test`: a command step must route `pass`".to_string()));
    }

    #[test]
    fn unknown_names_are_refused() {
        let text = FLOW
            .replace(r#"role = "tester""#, r#"role = "wizard""#)
            .replace(r#"command = "test""#, r#"command = "lint""#)
            .replace(r#"effort = "low""#, "effort = \"low\"\nwrites = [\"docs\"]");
        let found = problems(&text);
        assert!(found.contains(&"step `break`: unknown role `wizard`".to_string()));
        assert!(found.contains(&"step `test`: unknown command `lint`".to_string()));
        assert!(found.contains(&"step `describe`: unknown path scope `docs`".to_string()));
    }

    #[test]
    fn a_session_cannot_be_continued_from_another_workspace() {
        let text = FLOW.replace(
            "effort = \"low\"\nsession = \"continue:build\"",
            "effort = \"low\"\nsession = \"continue:review\"",
        );
        assert_eq!(
            problems(&text),
            [
                "step `describe`: continues the session of `review`, which runs in a different workspace"
            ]
        );
    }

    #[test]
    fn an_unreachable_step_and_a_dead_end_are_refused() {
        let text = format!(
            "{FLOW}\n[[step]]\nid = \"island\"\nkind = \"command\"\ncommand = \"test\"\non.pass = \"park\"\non.fail = \"park\"\n"
        );
        let found = problems(&text);
        assert!(found.contains(&"step `island` cannot be reached from the start".to_string()));
        assert!(found.contains(&"no path leads from step `island` to the end".to_string()));
    }

    #[test]
    fn a_field_that_does_not_apply_is_refused() {
        let text = FLOW.replace(r#"command = "test""#, "command = \"test\"\nmodel = \"m\"");
        assert_eq!(
            problems(&text),
            ["step `test`: `model` does not apply to a command step"]
        );
    }

    #[test]
    fn an_action_must_route_what_it_can_end_with() {
        let tail = r#"
[[step]]
id = "ci"
kind = "action"
action = "await-ci"
on.green = "land"

[[step]]
id = "land"
kind = "action"
action = "land"
on.done = "end"
on.merged = "end"
"#;
        let text = format!(
            "{}{tail}",
            FLOW.replace("on.done = \"end\"", "on.done = \"ci\"")
        );
        let found = problems(&text);
        assert!(found.contains(&"step `ci`: the action `await-ci` must route `red`".to_string()));
        assert!(
            found.contains(&"step `land`: the action `land` cannot end with `merged`".to_string())
        );

        let good = text
            .replace(
                "on.green = \"land\"",
                "on.green = \"land\"\non.red = { to = \"fix\", max = 2 }",
            )
            .replace("on.merged = \"end\"\n", "");
        Flow::parse(&good).unwrap().check(&registry()).unwrap();
    }

    /// The reference flow with a reports step between the test and the tester.
    fn with_reports(needs_on: &str) -> String {
        let text = FLOW
            .replace(
                "on.pass = \"break\"\n",
                "on.pass = \"reports\"\n",
            )
            .replace(
                "[[step]]\nid = \"break\"",
                "[[step]]\nid = \"reports\"\nkind = \"command\"\ncommand = \"test\"\npolicy = \"report-only\"\nproduces = \"reports\"\non.pass = \"break\"\non.fail = \"park\"\n\n[[step]]\nid = \"break\"",
            );
        text.replace(
            &format!("id = \"{needs_on}\"\nkind = \"agent\""),
            &format!("id = \"{needs_on}\"\nkind = \"agent\"\nneeds = [\"reports\"]"),
        )
    }

    #[test]
    fn a_step_may_need_what_every_path_produces_on_the_current_code() {
        for step in ["break", "review"] {
            Flow::parse(&with_reports(step))
                .unwrap()
                .check(&registry())
                .unwrap();
        }
        let flow = Flow::parse(&with_reports("break")).unwrap();
        let entry = flow.artifacts_on_entry();
        assert!(entry["review"].contains("reports"));
        assert!(entry["build"].is_empty());
        // The fix step changes the code, so what it leaves is not described
        // by any report until the reports step has run again.
        assert!(entry["test"].is_empty());
    }

    #[test]
    fn a_needed_artifact_that_is_missing_or_stale_on_some_path_is_refused() {
        // The fix step is reached from the test step before any report exists.
        assert_eq!(
            problems(&with_reports("fix")),
            [
                "step `fix`: needs `reports`, but on some path that reaches it `reports` has not been produced since the code last changed"
            ]
        );
        // A fix that skips the reports step leaves the reviewer with reports
        // about code that has since changed.
        let stale = with_reports("review").replace(
            "on.done = { to = \"test\", max = 5 }",
            "on.done = { to = \"review\", max = 5 }",
        );
        assert_eq!(
            problems(&stale),
            [
                "step `review`: needs `reports`, but on some path that reaches it `reports` has not been produced since the code last changed"
            ]
        );
        let unknown = FLOW.replace(
            "id = \"review\"\nkind = \"agent\"",
            "id = \"review\"\nkind = \"agent\"\nneeds = [\"audit\"]",
        );
        assert_eq!(
            problems(&unknown),
            ["step `review`: needs `audit`, which no step produces"]
        );
    }

    #[test]
    fn a_command_can_depend_on_what_changed_and_an_agent_on_its_round() {
        let text = FLOW
            .replace(
                "command = \"test\"\non.pass",
                "command = \"test\"\nwhen_changed = [\"code\"]\non.pass",
            )
            .replace(
                "session = \"continue:build\"\nwrites",
                "session = \"continue:build\"\neffort_by_round = [\"medium\", \"high\"]\nwrites",
            );
        let flow = Flow::parse(&text).unwrap();
        flow.check(&registry()).unwrap();
        assert_eq!(flow.step("test").unwrap().when_changed, ["code"]);
        assert_eq!(
            flow.step("fix").unwrap().effort_by_round,
            ["medium", "high"]
        );
        let unknown = text.replace("when_changed = [\"code\"]", "when_changed = [\"docs\"]");
        assert_eq!(
            problems(&unknown),
            ["step `test`: unknown path scope `docs`"]
        );
        let misplaced = FLOW.replace(
            "command = \"test\"\non.pass",
            "command = \"test\"\neffort_by_round = [\"high\"]\non.pass",
        );
        assert_eq!(
            problems(&misplaced),
            ["step `test`: only an agent step has an effort"]
        );
    }

    #[test]
    fn only_commands_produce_and_only_agents_need() {
        let text = FLOW.replace(
            "id = \"review\"\nkind = \"agent\"",
            "id = \"review\"\nkind = \"agent\"\nproduces = \"x\"",
        );
        assert_eq!(
            problems(&text),
            ["step `review`: only a command step can produce an artifact"]
        );
    }

    #[test]
    fn an_unknown_key_does_not_parse() {
        let text = FLOW.replace(r#"start = "build""#, "start = \"build\"\nspeed = \"fast\"");
        assert!(matches!(Flow::parse(&text), Err(FlowError::Parse(_))));
    }

    /// A scripted unit: build, a failing test, a fix, a tester that breaks it,
    /// another fix, a clean pass, review, publication.
    #[test]
    fn a_scripted_unit_walks_the_whole_flow() {
        let flow = Flow::parse(FLOW).unwrap();
        let mut position = Position::start(&flow);
        let script = [
            ("build", "done"),
            ("test", "fail"),
            ("fix", "done"),
            ("test", "pass"),
            ("break", "broken"),
            ("fix", "done"),
            ("test", "pass"),
            ("break", "held"),
            ("review", "approve"),
            ("describe", "ready"),
        ];
        for (step, outcome) in script {
            assert_eq!(position.step, step);
            assert!(matches!(
                position.advance(&flow, outcome).unwrap(),
                Next::Step(_)
            ));
        }
        assert_eq!(position.step, "publish");
        assert_eq!(position.advance(&flow, "done").unwrap(), Next::End);
        assert_eq!(position.rounds["fix"], 2);
        assert_eq!(position.rounds["test"], 3);
    }

    #[test]
    fn a_route_used_up_parks_the_unit() {
        let flow = Flow::parse(FLOW).unwrap();
        let mut position = Position::start(&flow);
        position.advance(&flow, "done").unwrap();
        for _ in 0..2 {
            assert_eq!(
                position.advance(&flow, "fail").unwrap(),
                Next::Step("fix".to_string())
            );
            position.advance(&flow, "done").unwrap();
        }
        assert_eq!(
            position.advance(&flow, "fail").unwrap(),
            Next::Park("step `test` ended with `fail` more often than the flow allows".to_string())
        );
    }

    #[test]
    fn asking_the_owner_keeps_the_step_and_is_bounded() {
        let flow = Flow::parse(FLOW).unwrap();
        let mut position = Position::start(&flow);
        assert_eq!(position.advance(&flow, "needs_owner").unwrap(), Next::Ask);
        assert_eq!(position.step, "build");
        assert_eq!(position.advance(&flow, "needs_owner").unwrap(), Next::Ask);
        assert!(matches!(
            position.advance(&flow, "needs_owner").unwrap(),
            Next::Park(_)
        ));
    }

    #[test]
    fn an_outcome_with_no_route_is_an_error() {
        let flow = Flow::parse(FLOW).unwrap();
        let mut position = Position::start(&flow);
        assert_eq!(
            position.advance(&flow, "shrug"),
            Err(FlowError::Unrouted {
                step: "build".to_string(),
                outcome: "shrug".to_string()
            })
        );
    }

    #[test]
    fn a_position_survives_being_saved() {
        let flow = Flow::parse(FLOW).unwrap();
        let mut position = Position::start(&flow);
        position.advance(&flow, "done").unwrap();
        position.advance(&flow, "fail").unwrap();
        let saved = toml::to_string(&position).unwrap();
        assert_eq!(toml::from_str::<Position>(&saved).unwrap(), position);
    }
}
