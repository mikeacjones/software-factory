//! The agent contract: what each role must return, and the briefing each
//! agent step starts from.
//!
//! A briefing is rendered by code from the bead, the project record, the flow
//! step and what earlier steps produced. The same inputs give the same bytes.
//! A result is checked by code against its role's contract before the flow
//! routes it.

use std::collections::{BTreeMap, BTreeSet};

use factory_beads::Bead;
use factory_config::Project;
use factory_flow::{SessionRule, Step, StepKind, Workspace};
use serde_json::{Value, json};

/// The shape of a role's typed result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Contract {
    /// A change to the code, with evidence for each acceptance criterion.
    Change,
    /// Attempts to break a change by running it, with a reproduction for
    /// everything found.
    Probe,
    /// Findings from reading a change.
    Review,
    /// A commit message and pull-request text.
    Publication,
}

/// The roles a flow can name, with the contract each one works to.
pub const ROLES: [(&str, Contract); 4] = [
    ("builder", Contract::Change),
    ("tester", Contract::Probe),
    ("reviewer", Contract::Review),
    ("publisher", Contract::Publication),
];

pub fn contract_of(role: &str) -> Option<Contract> {
    ROLES
        .iter()
        .find(|(name, _)| *name == role)
        .map(|(_, contract)| *contract)
}

impl Contract {
    /// Every outcome a role with this contract may return, with what it means.
    pub fn outcomes(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Contract::Change => &[
                (
                    "done",
                    "The work is complete: every acceptance criterion is met and the project's commands pass.",
                ),
                (
                    "already_done",
                    "The default branch already does this. Nothing was changed, and the evidence says where it is done.",
                ),
                (
                    "needs_owner",
                    "You cannot proceed without a decision only the owner can make. Put the question in `question`.",
                ),
            ],
            Contract::Probe => &[
                (
                    "held",
                    "You tried hard to break the change and could not. Minor findings may still be listed.",
                ),
                (
                    "broken",
                    "You broke it: at least one finding, each with a reproduction you ran.",
                ),
                (
                    "needs_owner",
                    "You cannot proceed without a decision only the owner can make. Put the question in `question`.",
                ),
            ],
            Contract::Review => &[
                (
                    "approve",
                    "The change meets the acceptance criteria and nothing must be fixed before it merges.",
                ),
                (
                    "rework",
                    "At least one finding must be fixed before this merges.",
                ),
                (
                    "needs_owner",
                    "You cannot proceed without a decision only the owner can make. Put the question in `question`.",
                ),
            ],
            Contract::Publication => &[(
                "ready",
                "The commit message and pull-request text are written.",
            )],
        }
    }
}

/// Each role with the outcomes it may return, for the flow validator.
pub fn role_registry() -> BTreeMap<String, BTreeSet<String>> {
    ROLES
        .iter()
        .map(|(role, contract)| {
            let outcomes = contract
                .outcomes()
                .iter()
                .map(|(name, _)| name.to_string())
                .collect();
            (role.to_string(), outcomes)
        })
        .collect()
}

fn text() -> Value {
    json!({ "type": "string" })
}

fn object(properties: Value) -> Value {
    let required: Vec<String> = properties
        .as_object()
        .map(|map| map.keys().cloned().collect())
        .unwrap_or_default();
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": required,
        "properties": properties,
    })
}

fn list(items: Value) -> Value {
    json!({ "type": "array", "items": items })
}

/// The JSON Schema a step's result must match. `outcomes` are the ones the
/// step's flow routes, so a model cannot return one the flow does not handle.
pub fn schema(contract: Contract, outcomes: &[String]) -> Value {
    let severity = json!({ "enum": ["critical", "major", "minor"] });
    let mut properties = json!({
        "outcome": { "enum": outcomes },
        "summary": {
            "type": "string",
            "description": "What you did and found, in a few plain sentences. The owner reads this in Slack."
        },
        "question": {
            "type": "string",
            "description": "The question for the owner when the outcome is needs_owner. Otherwise empty."
        },
        "reply_to_owner": {
            "type": "string",
            "description": "Your answer to any message from the owner in the brief. Otherwise empty."
        },
    });
    let body = match contract {
        Contract::Change => json!({
            "files_touched": list(text()),
            "tests": {
                "type": "array",
                "items": text(),
                "description": "Tests you added or changed, as `path: test name`."
            },
            "evidence": list(object(json!({
                "criterion": {
                    "type": "string",
                    "description": "One acceptance criterion of the bead, quoted."
                },
                "evidence": {
                    "type": "string",
                    "description": "How you know it is met: the command you ran and what it showed, or the file and line."
                },
            }))),
        }),
        Contract::Probe => json!({
            "probes": {
                "type": "array",
                "description": "Everything you tried, whether or not it broke anything.",
                "items": object(json!({
                    "what": {
                        "type": "string",
                        "description": "What you were trying to break and how."
                    },
                    "command": {
                        "type": "string",
                        "description": "The exact command you ran."
                    },
                    "observed": text(),
                })),
            },
            "findings": list(object(json!({
                "title": text(),
                "severity": severity,
                "location": {
                    "type": "string",
                    "description": "`path:line` of the code at fault."
                },
                "test_names": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "The names of the regression tests you left in the worktree for this finding, exactly as they are written in the code. Required for a critical or major finding."
                },
                "reproduction": object(json!({
                    "command": {
                        "type": "string",
                        "description": "One shell command, run from the worktree root, that fails because of this finding and will pass once it is fixed: normally the project's test command narrowed to the regression test you left. The factory runs it before accepting the finding, and it must fail."
                    },
                    "expected": text(),
                    "observed": {
                        "type": "string",
                        "description": "What actually happened when you ran it."
                    },
                })),
            }))),
        }),
        Contract::Review => json!({
            "findings": list(object(json!({
                "title": text(),
                "severity": severity,
                "location": {
                    "type": "string",
                    "description": "`path:line` in the change or the repository."
                },
                "category": {
                    "type": "string",
                    "description": "A short stable key for the kind of problem, such as `missing-test`, `error-handling`, `api-shape`, `docs`."
                },
                "evidence": {
                    "type": "string",
                    "description": "The acceptance criterion it violates, the failing command, or the lines that show it."
                },
                "must_fix": { "type": "boolean" },
            }))),
        }),
        Contract::Publication => json!({
            "commit_message": {
                "type": "string",
                "description": "The full commit message: a subject line, a blank line, a body, and any trailers the project requires."
            },
            "pr_title": text(),
            "pr_body": text(),
            "merge_summary": {
                "type": "string",
                "description": "One paragraph of plain prose for the merge commit that lands this change: what the branch adds and why, naming the real types, functions and files. No heading, no list, no trailers."
            },
        }),
    };
    if let (Some(all), Some(extra)) = (properties.as_object_mut(), body.as_object()) {
        for (key, value) in extra {
            all.insert(key.clone(), value.clone());
        }
    }
    object(properties)
}

fn string_at<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

fn array_at<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

/// What is wrong with a typed result, beyond its schema. Empty means the
/// result may be routed.
pub fn result_problems(contract: Contract, outcomes: &[String], result: &Value) -> Vec<String> {
    let mut problems = Vec::new();
    let outcome = string_at(result, "outcome");
    if !outcomes.iter().any(|allowed| allowed == outcome) {
        problems.push(format!(
            "`outcome` is `{outcome}`; it must be one of: {}",
            outcomes.join(", ")
        ));
    }
    if string_at(result, "summary").trim().is_empty() {
        problems.push("`summary` is empty".to_string());
    }
    if outcome == "needs_owner" {
        if string_at(result, "question").trim().is_empty() {
            problems.push("the outcome is needs_owner but `question` is empty".to_string());
        }
        return problems;
    }
    match contract {
        Contract::Change => {
            if array_at(result, "evidence").is_empty() {
                problems.push(
                    "`evidence` is empty: give one entry for each acceptance criterion".to_string(),
                );
            }
            for entry in array_at(result, "evidence") {
                if string_at(entry, "evidence").trim().is_empty() {
                    problems.push(format!(
                        "the criterion `{}` has no evidence",
                        string_at(entry, "criterion")
                    ));
                }
            }
            if outcome == "done" && array_at(result, "files_touched").is_empty() {
                problems.push(
                    "the outcome is done but `files_touched` is empty; if nothing needed changing the outcome is already_done"
                        .to_string(),
                );
            }
        }
        Contract::Probe => {
            let findings = array_at(result, "findings");
            if array_at(result, "probes").is_empty() {
                problems.push(
                    "`probes` is empty: a tester that ran nothing has not tested anything"
                        .to_string(),
                );
            }
            for finding in findings {
                let title = string_at(finding, "title");
                let reproduction = finding.get("reproduction").unwrap_or(&Value::Null);
                if string_at(reproduction, "command").trim().is_empty()
                    || string_at(reproduction, "observed").trim().is_empty()
                {
                    problems.push(format!(
                        "the finding `{title}` has no reproduction: give the command you ran and what it showed, or drop the finding"
                    ));
                }
            }
            let serious = findings
                .iter()
                .any(|finding| matches!(string_at(finding, "severity"), "critical" | "major"));
            if outcome == "broken" && findings.is_empty() {
                problems.push("the outcome is broken but there are no findings".to_string());
            }
            if outcome == "held" && serious {
                problems.push(
                    "the outcome is held but a finding is critical or major: that is broken"
                        .to_string(),
                );
            }
        }
        Contract::Review => {
            let findings = array_at(result, "findings");
            let must_fix = findings
                .iter()
                .filter(|finding| finding.get("must_fix").and_then(Value::as_bool) == Some(true))
                .count();
            for finding in findings {
                if string_at(finding, "location").trim().is_empty()
                    || string_at(finding, "evidence").trim().is_empty()
                {
                    problems.push(format!(
                        "the finding `{}` needs a location and evidence",
                        string_at(finding, "title")
                    ));
                }
            }
            if outcome == "rework" && must_fix == 0 {
                problems
                    .push("the outcome is rework but no finding is marked must_fix".to_string());
            }
            if outcome == "approve" && must_fix > 0 {
                problems.push(
                    "the outcome is approve but a finding is marked must_fix: that is rework"
                        .to_string(),
                );
            }
        }
        Contract::Publication => {
            for key in ["commit_message", "pr_title", "pr_body", "merge_summary"] {
                if string_at(result, key).trim().is_empty() {
                    problems.push(format!("`{key}` is empty"));
                }
            }
            if string_at(result, "pr_title").lines().count() > 1 {
                problems.push("`pr_title` must be one line".to_string());
            }
        }
    }
    problems
}

/// A tester's or reviewer's findings as text for the step that must act on them.
pub fn findings_text(result: &Value) -> String {
    let mut out = String::new();
    for (index, finding) in array_at(result, "findings").iter().enumerate() {
        out.push_str(&format!(
            "{}. [{}] {}\n   Location: {}\n",
            index + 1,
            string_at(finding, "severity"),
            string_at(finding, "title"),
            string_at(finding, "location"),
        ));
        if let Some(reproduction) = finding.get("reproduction") {
            out.push_str(&format!(
                "   Reproduce: {}\n   Expected: {}\n   Observed: {}\n",
                string_at(reproduction, "command"),
                string_at(reproduction, "expected"),
                string_at(reproduction, "observed"),
            ));
        }
        if !string_at(finding, "evidence").is_empty() {
            out.push_str(&format!(
                "   Evidence: {}\n",
                string_at(finding, "evidence")
            ));
        }
        if let Some(must_fix) = finding.get("must_fix").and_then(Value::as_bool) {
            out.push_str(&format!(
                "   Must fix: {}\n",
                if must_fix { "yes" } else { "no" }
            ));
        }
    }
    out
}

/// Something an earlier step produced that this step is given.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Note {
    pub title: String,
    pub body: String,
}

/// Everything a briefing is rendered from.
pub struct Brief<'a> {
    pub project: &'a Project,
    pub bead: &'a Bead,
    pub step: &'a Step,
    /// Which time the unit is in this step, counting from 1.
    pub round: u32,
    /// The commit the unit's branch started from.
    pub base: &'a str,
    /// `git diff --stat` of the change so far. Empty before the first change.
    pub diff_stat: &'a str,
    /// True if the step's session has been briefed before, so only what is new
    /// is sent.
    pub continuing: bool,
    /// What sent the unit to this step, when it came backwards.
    pub feedback: Option<&'a str>,
    /// What earlier steps produced that this step should see.
    pub notes: &'a [Note],
    /// Messages the owner wrote in the run's thread since the last agent step.
    pub owner_messages: &'a [String],
    /// The owner's answer to the question this step asked.
    pub answer: Option<&'a str>,
    /// Why this step's last result was rejected by code.
    pub rejected: &'a [String],
    /// What whoever runs the bead's sub-goal has decided about the bead,
    /// for every step that follows: what is in scope, how a question of
    /// design is settled.
    pub rulings: &'a [String],
}

fn section(out: &mut String, title: &str, body: &str) {
    if body.trim().is_empty() {
        return;
    }
    out.push_str(&format!("## {title}\n\n{}\n\n", body.trim()));
}

fn bullets(items: impl IntoIterator<Item = String>) -> String {
    items
        .into_iter()
        .map(|item| format!("- {item}"))
        .collect::<Vec<_>>()
        .join("\n")
}

const PUBLICATION_TASK: &str = "The change has passed its checks and reviews. Write the commit message and the pull-request title and body for it. The whole change becomes one commit with your message when it lands. Follow this project's rules for commit messages and changelog trailers. Do not mention people with @, and do not refer to issues or pull requests in other repositories.";

/// Render the briefing for an agent step.
pub fn render(brief: &Brief<'_>) -> String {
    let StepKind::Agent {
        role,
        session,
        workspace,
        writes,
        ..
    } = &brief.step.kind
    else {
        return String::new();
    };
    let contract = contract_of(role);
    let mut out = String::new();

    if !brief.rejected.is_empty() {
        out.push_str("# Your result was not accepted\n\n");
        section(
            &mut out,
            "What is wrong",
            &bullets(brief.rejected.iter().cloned()),
        );
        out.push_str("Fix these and return the result again. Do any work the fix needs first.\n\n");
        section(&mut out, "Result", &outcome_text(brief, contract));
        return out;
    }
    if let Some(answer) = brief.answer {
        out.push_str("# The owner answered your question\n\n");
        section(&mut out, "Answer", answer);
        section(
            &mut out,
            "Other messages from the owner",
            &owner_text(brief),
        );
        out.push_str("Carry on with the step and return its result.\n\n");
        section(&mut out, "Result", &outcome_text(brief, contract));
        return out;
    }

    let bead = brief.bead;
    out.push_str(&format!(
        "# Step `{}` (round {}) for bead {}\n\n",
        brief.step.id, brief.round, bead.id
    ));

    if brief.continuing {
        // The session already knows the bead and the rules. Send what is new.
        let task = match (contract, session) {
            (Some(Contract::Publication), _) => PUBLICATION_TASK,
            (_, SessionRule::Continue(_)) => {
                "You are continuing your earlier work on this bead. A later step sent it back. Fix what it found, keep every acceptance criterion met, and run the project's commands before you return."
            }
            _ => "Continue the step.",
        };
        section(&mut out, "Task", task);
        if contract == Some(Contract::Publication) {
            section(
                &mut out,
                "This project's rules for the text",
                brief.project.publication_note.as_deref().unwrap_or(""),
            );
        }
        if contract == Some(Contract::Publication) && !brief.project.publish_checks.is_empty() {
            let checks = bullets(brief.project.publish_checks.iter().filter_map(|name| {
                let command = brief.project.commands.get(name)?;
                Some(format!("`{name}`: `{}`", command.argv.join(" ")))
            }));
            section(
                &mut out,
                "Checks on the final commit",
                &format!(
                    "The factory runs these on the one commit it makes from your message, before it pushes. A failure comes back to you.\n\n{checks}"
                ),
            );
        }
        section(
            &mut out,
            "Why you are here again",
            brief.feedback.unwrap_or(""),
        );
    } else {
        section(
            &mut out,
            "Bead",
            &format!("**{}** ({})", bead.title, bead.id),
        );
        section(&mut out, "Description", &bead.description);
        section(&mut out, "Design notes", &bead.design);
        section(&mut out, "Acceptance criteria", &bead.acceptance_criteria);
        section(&mut out, "Notes on the bead", &bead.notes);

        let place = match workspace {
            Workspace::Unit => format!(
                "Your working directory is the unit's git worktree, on its own branch, started from commit `{}` of `{}`. What you leave in the worktree is what the factory commits.",
                brief.base, brief.project.default_branch
            ),
            Workspace::Scratch => format!(
                "Your working directory is a scratch git worktree holding the change under test. The change is `git diff {}..HEAD`. Write and run anything you like here: the worktree is thrown away when you finish, and nothing in it reaches the unit's branch.",
                brief.base
            ),
        };
        section(&mut out, "Where you are", &place);
        if contract == Some(Contract::Publication) {
            // A publisher that did not build the change still needs its task.
            section(&mut out, "Task", PUBLICATION_TASK);
            section(
                &mut out,
                "This project's rules for the text",
                brief.project.publication_note.as_deref().unwrap_or(""),
            );
        }
        section(
            &mut out,
            "Why you are here again",
            brief.feedback.unwrap_or(""),
        );
    }

    if !brief.diff_stat.trim().is_empty() {
        section(
            &mut out,
            "The change so far",
            &format!(
                "```\n{}\n```\nSee it in full with `git diff {}..HEAD`.",
                brief.diff_stat.trim_end(),
                brief.base
            ),
        );
    }
    for note in brief.notes {
        section(&mut out, &note.title, &note.body);
    }

    if !brief.continuing {
        let commands = bullets(
            brief
                .project
                .commands
                .iter()
                // A command the factory fills in before running is not one
                // an agent can run as written.
                .filter(|(_, command)| !command.wants_out())
                .map(|(name, command)| format!("`{name}`: `{}`", command.argv.join(" "))),
        );
        section(
            &mut out,
            "Project commands",
            &format!(
                "{commands}\n\nThe factory runs these itself after you. Run the ones that apply before you return."
            ),
        );

        let mut paths = String::new();
        match workspace {
            Workspace::Scratch => {}
            Workspace::Unit if writes.is_empty() => {
                paths.push_str("You may not change any file in this step.\n");
            }
            Workspace::Unit => {
                let allowed: Vec<String> = writes
                    .iter()
                    .flat_map(|scope| brief.project.scopes.get(scope).cloned().unwrap_or_default())
                    .map(|glob| format!("`{glob}`"))
                    .collect();
                paths.push_str(&format!(
                    "You may change files matching: {}.\n",
                    allowed.join(", ")
                ));
            }
        }
        if *workspace == Workspace::Unit && !brief.project.forbidden_paths.is_empty() {
            let forbidden: Vec<String> = brief
                .project
                .forbidden_paths
                .iter()
                .map(|glob| format!("`{glob}`"))
                .collect();
            paths.push_str(&format!("No step may change: {}.\n", forbidden.join(", ")));
        }
        if !paths.is_empty() {
            paths.push_str("Your changes are compared with these paths after you finish.");
        }
        section(&mut out, "Paths", &paths);

        section(
            &mut out,
            "Rules",
            &bullets(
                [
                    "Do not commit, push, open pull requests or comment on GitHub. The factory commits what you leave in the worktree and does the rest.",
                    "Do not change beads. The bead above is all you need from the tracker.",
                    "Do not change anything outside your working directory.",
                    "Leave nothing running when you finish: no servers, no background jobs.",
                    "Read the repository's own guidance first (AGENTS.md, CLAUDE.md, CONTRIBUTING.md where they exist): it is not loaded for you. Follow it, except where it tells you to track tasks, commit or push: the factory does those.",
                ]
                .map(str::to_string),
            ),
        );
    }

    section(&mut out, "Rulings on this bead", &rulings_text(brief));
    section(&mut out, "Messages from the owner", &owner_text(brief));
    section(&mut out, "Result", &outcome_text(brief, contract));
    out
}

fn rulings_text(brief: &Brief<'_>) -> String {
    if brief.rulings.is_empty() {
        return String::new();
    }
    format!(
        "The agent that runs this bead's sub-goal stopped the pipeline, looked at it, and decided these. They hold for every step from here on, yours included. Take them as given: do not build, break or review against them. If you think one is wrong, say so in your summary and do the step as ruled.\n\n{}",
        bullets(
            brief
                .rulings
                .iter()
                .map(|ruling| ruling.trim().replace('\n', "\n  "))
        )
    )
}

fn owner_text(brief: &Brief<'_>) -> String {
    if brief.owner_messages.is_empty() {
        return String::new();
    }
    let quoted: Vec<String> = brief
        .owner_messages
        .iter()
        .map(|message| {
            message
                .lines()
                .map(|line| format!("> {line}"))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect();
    format!(
        "The owner wrote these in the run's Slack thread. Treat them as instructions for this step, and answer in `reply_to_owner`.\n\n{}",
        quoted.join("\n\n")
    )
}

fn outcome_text(brief: &Brief<'_>, contract: Option<Contract>) -> String {
    let meanings: BTreeMap<&str, &str> = contract
        .map(|contract| contract.outcomes().iter().copied().collect())
        .unwrap_or_default();
    let outcomes = bullets(brief.step.on.keys().map(|outcome| {
        format!(
            "`{outcome}`: {}",
            meanings.get(outcome.as_str()).copied().unwrap_or("")
        )
    }));
    format!(
        "End by returning the structured result. `outcome` must be one of:\n\n{outcomes}\n\nThe result is checked by code: claims in it are compared with the worktree."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use factory_flow::Flow;

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
on.done = "break"
on.needs_owner = { to = "ask", max = 1 }

[[step]]
id = "break"
kind = "agent"
role = "tester"
model = "m"
effort = "high"
workspace = "scratch"
on.held = "end"
on.broken = { to = "fix", max = 1 }

[[step]]
id = "fix"
kind = "agent"
role = "builder"
model = "m"
effort = "medium"
session = "continue:build"
writes = ["code"]
on.done = { to = "break", max = 2 }
"#;

    const PROJECT: &str = r#"
id = "toy"
repo = "/tmp/toy"
default_branch = "main"
flow = "toy"
forbidden_paths = [".github/**"]

[beads]
kind = "files"
dir = "/tmp/toy-beads"

[scopes]
code = ["src/**", "README.md"]

[commands.test]
argv = ["make", "test"]
timeout_secs = 60
"#;

    fn bead() -> Bead {
        Bead {
            id: "toy-1".to_string(),
            title: "Add a greeting".to_string(),
            description: "Greet the user.".to_string(),
            acceptance_criteria: "- `greet()` returns hello".to_string(),
            status: "open".to_string(),
            ..Bead::default()
        }
    }

    fn brief_for<'a>(project: &'a Project, bead: &'a Bead, step: &'a Step) -> Brief<'a> {
        Brief {
            project,
            bead,
            step,
            round: 1,
            base: "abc123",
            diff_stat: "",
            continuing: false,
            feedback: None,
            notes: &[],
            owner_messages: &[],
            answer: None,
            rejected: &[],
            rulings: &[],
        }
    }

    #[test]
    fn the_schema_allows_exactly_the_outcomes_the_flow_routes() {
        let flow = Flow::parse(FLOW).unwrap();
        for step in &flow.steps {
            let StepKind::Agent { role, .. } = &step.kind else {
                continue;
            };
            let outcomes = flow.outcomes(&step.id).unwrap();
            let schema = schema(contract_of(role).unwrap(), &outcomes);
            let allowed: Vec<String> = schema["properties"]["outcome"]["enum"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect();
            assert_eq!(allowed, outcomes, "step {}", step.id);
            // Every property is required and nothing else is allowed.
            assert_eq!(schema["additionalProperties"], false);
            assert_eq!(
                schema["required"].as_array().unwrap().len(),
                schema["properties"].as_object().unwrap().len()
            );
        }
    }

    #[test]
    fn every_role_can_be_checked_against_a_flow() {
        let registry = role_registry();
        assert_eq!(registry.len(), ROLES.len());
        assert!(registry["tester"].contains("broken"));
        assert!(!registry["publisher"].contains("needs_owner"));
    }

    #[test]
    fn a_briefing_is_the_same_bytes_for_the_same_inputs() {
        let flow = Flow::parse(FLOW).unwrap();
        let project: Project = toml::from_str(PROJECT).unwrap();
        let bead = bead();
        let step = flow.step("build").unwrap();
        let first = render(&brief_for(&project, &bead, step));
        let second = render(&brief_for(&project, &bead, step));
        assert_eq!(first, second);
        assert!(first.contains("# Step `build` (round 1) for bead toy-1"));
        assert!(first.contains("- `greet()` returns hello"));
        assert!(first.contains("`test`: `make test`"));
        assert!(first.contains("You may change files matching: `src/**`, `README.md`."));
        assert!(first.contains("No step may change: `.github/**`."));
        assert!(first.contains("`done`:"));
        assert!(first.contains("`needs_owner`:"));
        // Not routed by this step, so not offered.
        assert!(!first.contains("`already_done`"));
    }

    #[test]
    fn a_tester_is_told_its_worktree_is_scratch() {
        let flow = Flow::parse(FLOW).unwrap();
        let project: Project = toml::from_str(PROJECT).unwrap();
        let bead = bead();
        let mut brief = brief_for(&project, &bead, flow.step("break").unwrap());
        brief.diff_stat = " src/greet.zig | 4 ++++\n";
        let text = render(&brief);
        assert!(text.contains("scratch git worktree"));
        assert!(text.contains("git diff abc123..HEAD"));
        assert!(text.contains("src/greet.zig | 4 ++++"));
        assert!(!text.contains("You may change files matching"));
    }

    #[test]
    fn a_continued_session_is_sent_only_what_is_new() {
        let flow = Flow::parse(FLOW).unwrap();
        let project: Project = toml::from_str(PROJECT).unwrap();
        let bead = bead();
        let mut brief = brief_for(&project, &bead, flow.step("fix").unwrap());
        brief.continuing = true;
        brief.feedback =
            Some("The step `break` ended with `broken`.\n1. [major] greet() panics on empty input");
        let messages = ["please keep the old name too".to_string()];
        brief.owner_messages = &messages;
        let text = render(&brief);
        assert!(text.contains("greet() panics on empty input"));
        assert!(text.contains("> please keep the old name too"));
        assert!(!text.contains("Greet the user."));
        assert!(!text.contains("## Rules"));
    }

    #[test]
    fn a_rejected_result_is_sent_back_with_the_reasons() {
        let flow = Flow::parse(FLOW).unwrap();
        let project: Project = toml::from_str(PROJECT).unwrap();
        let bead = bead();
        let mut brief = brief_for(&project, &bead, flow.step("build").unwrap());
        let rejected = ["`evidence` is empty".to_string()];
        brief.rejected = &rejected;
        let text = render(&brief);
        assert!(text.starts_with("# Your result was not accepted"));
        assert!(text.contains("- `evidence` is empty"));
    }

    fn outcomes(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_tester_finding_without_a_reproduction_is_rejected() {
        let allowed = outcomes(&["held", "broken"]);
        let mut result = json!({
            "outcome": "broken",
            "summary": "It breaks on empty input.",
            "question": "",
            "reply_to_owner": "",
            "probes": [{"what": "empty input", "command": "zig test probe.zig", "observed": "panic"}],
            "findings": [{
                "title": "panics on empty input",
                "severity": "major",
                "location": "src/greet.zig:3",
                "reproduction": {"command": "", "expected": "an error", "observed": ""}
            }],
        });
        assert_eq!(
            result_problems(Contract::Probe, &allowed, &result),
            [
                "the finding `panics on empty input` has no reproduction: give the command you ran and what it showed, or drop the finding"
            ]
        );
        result["findings"][0]["reproduction"] = json!({"command": "zig test probe.zig", "expected": "an error", "observed": "panic: index out of bounds"});
        assert_eq!(
            result_problems(Contract::Probe, &allowed, &result),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_tester_must_have_run_something_and_cannot_hold_a_major_finding() {
        let allowed = outcomes(&["held", "broken"]);
        let idle =
            json!({"outcome": "held", "summary": "Looks fine.", "probes": [], "findings": []});
        assert_eq!(
            result_problems(Contract::Probe, &allowed, &idle),
            ["`probes` is empty: a tester that ran nothing has not tested anything"]
        );
        let lenient = json!({
            "outcome": "held", "summary": "One problem.",
            "probes": [{"what": "x", "command": "c", "observed": "o"}],
            "findings": [{"title": "t", "severity": "major", "location": "a:1",
                          "reproduction": {"command": "c", "expected": "e", "observed": "o"}}],
        });
        assert_eq!(
            result_problems(Contract::Probe, &allowed, &lenient),
            ["the outcome is held but a finding is critical or major: that is broken"]
        );
    }

    #[test]
    fn a_change_needs_evidence_and_a_review_must_be_consistent() {
        let change = json!({"outcome": "done", "summary": "Did it.", "files_touched": ["a"], "tests": [], "evidence": []});
        assert_eq!(
            result_problems(Contract::Change, &outcomes(&["done"]), &change),
            ["`evidence` is empty: give one entry for each acceptance criterion"]
        );
        let review = json!({
            "outcome": "approve", "summary": "Fine.",
            "findings": [{"title": "t", "severity": "major", "location": "a:1", "category": "c", "evidence": "e", "must_fix": true}],
        });
        assert_eq!(
            result_problems(Contract::Review, &outcomes(&["approve", "rework"]), &review),
            ["the outcome is approve but a finding is marked must_fix: that is rework"]
        );
    }

    #[test]
    fn an_outcome_the_flow_does_not_route_is_rejected() {
        let result = json!({"outcome": "already_done", "summary": "s", "evidence": [{"criterion": "c", "evidence": "e"}]});
        let problems = result_problems(Contract::Change, &outcomes(&["done"]), &result);
        assert_eq!(
            problems,
            ["`outcome` is `already_done`; it must be one of: done"]
        );
    }

    #[test]
    fn asking_the_owner_needs_a_question() {
        let result = json!({"outcome": "needs_owner", "summary": "Stuck.", "question": " "});
        assert_eq!(
            result_problems(
                Contract::Change,
                &outcomes(&["done", "needs_owner"]),
                &result
            ),
            ["the outcome is needs_owner but `question` is empty"]
        );
    }

    #[test]
    fn findings_are_rendered_for_the_step_that_must_fix_them() {
        let result = json!({"findings": [{
            "title": "panics on empty input", "severity": "major", "location": "src/greet.zig:3",
            "reproduction": {"command": "zig test probe.zig", "expected": "an error", "observed": "panic"}
        }]});
        let text = findings_text(&result);
        assert!(
            text.starts_with("1. [major] panics on empty input\n   Location: src/greet.zig:3\n")
        );
        assert!(text.contains("Reproduce: zig test probe.zig"));
    }
}
