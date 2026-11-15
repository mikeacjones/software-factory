//! What the engine says about each thing it runs.
//!
//! Someone reading a workflow on the Temporal server sees event types and
//! activity names, which say what the engine is made of and not what it is
//! doing. So everything a workflow starts carries a summary: one line that
//! says what it is for, on which bead and at which step. A workflow is
//! started with a summary and details of its own, and keeps its current
//! details up to date with where it stands.
//!
//! Everything here is a pure function of what it is given. Workflow code
//! calls it, and has to get the same words on every replay.

use factory_flow::{ExitPolicy, SessionRule, Step, StepKind, Workspace};

use crate::accounts::{Picture, Waiting};
use crate::engine::Standing;

/// The most a summary holds. The server refuses a longer one.
pub const SUMMARY_MAX: usize = 200;

/// The most a workflow's details hold, in bytes. The server's own limit
/// is 20,000.
pub const DETAILS_MAX: usize = 16_000;

/// One line, short enough to be a summary: runs of white space become one
/// space, and what does not fit is cut at a character with a mark.
pub fn line(text: &str) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    cut(&joined, SUMMARY_MAX)
}

/// The text, cut to `max` bytes at a character, with a mark if it was cut.
pub fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max.saturating_sub('…'.len_utf8());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// What a pipeline is about, in a few words: its bead with its title, or
/// its unit when it was started for a unit that existed already.
pub fn subject(bead: &str, unit: &str, title: &str) -> String {
    let name = if bead.is_empty() { unit } else { bead };
    if title.is_empty() {
        format!("`{name}`")
    } else {
        format!("`{name}` ({})", cut(title, 70))
    }
}

/// A length of time as someone would say it.
pub fn span(secs: u64) -> String {
    if secs < 90 {
        return format!("{secs} s");
    }
    let mins = (secs + 30) / 60;
    match (mins / 60, mins % 60) {
        (0, mins) => format!("{mins} min"),
        (hours, 0) => format!("{hours} h"),
        (hours, mins) => format!("{hours} h {mins} min"),
    }
}

/// A moment as the time of day it falls on, in UTC.
pub fn clock(unix: u64) -> String {
    let day = unix % 86_400;
    format!("{:02}:{:02} UTC", day / 3_600, day % 3_600 / 60)
}

/// What a step is, whoever runs it.
pub fn step_kind(step: &Step) -> String {
    match &step.kind {
        StepKind::Agent {
            role,
            model,
            effort,
            session,
            workspace,
            ..
        } => {
            let session = match session {
                SessionRule::Fresh => String::new(),
                SessionRule::Continue(from) => format!(", in the session of `{from}`"),
            };
            let place = match workspace {
                Workspace::Unit => "",
                Workspace::Scratch => ", in a throwaway worktree",
            };
            format!("the {role} ({model}, {effort} effort{session}{place})")
        }
        StepKind::Command { command, policy } => {
            let rule = match policy {
                ExitPolicy::MustPass => "must pass",
                ExitPolicy::MustFail => "must fail",
                ExitPolicy::ReportOnly => "kept as a report",
            };
            format!("the project's `{command}` command ({rule})")
        }
        StepKind::Action { action } => format!("the factory's own `{action}`"),
    }
}

/// The summary of the activity that runs a step.
pub fn step(step: &Step, round: u32, account: Option<&str>, subject: &str) -> String {
    let round = if round > 1 {
        format!(", round {round}")
    } else {
        String::new()
    };
    let account = account
        .map(|account| format!(", under account `{account}`"))
        .unwrap_or_default();
    line(&format!(
        "Step `{}`{round}: {} on {subject}{account}",
        step.id,
        step_kind(step)
    ))
}

/// A pipeline's details as it starts, before anything is known but what
/// it was started with.
pub fn pipeline_static(project: &str, bead: &str, unit: &str, flow: Option<&str>) -> String {
    let mut text = String::new();
    if !bead.is_empty() {
        text.push_str(&format!("Bead `{bead}` of project `{project}`.\n\n"));
    }
    if !unit.is_empty() {
        text.push_str(&format!("Unit `{unit}`.\n\n"));
    }
    text.push_str(&match flow {
        Some(flow) => format!("It runs the flow `{flow}`."),
        None => "It runs the flow its project names for a bead.".to_string(),
    });
    text.push_str(
        "\n\nA pipeline walks its flow a step at a time. Each step is one activity: an agent \
         run, a command of the project's, or an action of the factory's own. Between steps it \
         may wait for an account, or be parked until someone continues it \
         (`factory-engine go-on`) or gives it up (`factory-engine give-up`). \
         Where it stands now is under \"Current details\".",
    );
    text
}

/// What a pipeline is doing now, for its current details.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Showing {
    pub project: String,
    pub bead: String,
    pub title: String,
    pub flow: String,
    /// What is happening at the step it is in, as a sentence.
    pub doing: String,
    /// It was given up while it was parked, and has ended.
    pub given_up: bool,
    /// It was told to end when it parked, and has.
    pub left_parked: bool,
}

/// A pipeline's current details: what it is about, what it is doing, and
/// every step that has ended. Markdown.
pub fn pipeline_details(showing: &Showing, standing: &Standing) -> String {
    let mut text = String::new();
    let name = if showing.bead.is_empty() {
        &standing.unit
    } else {
        &showing.bead
    };
    if !name.is_empty() {
        text.push_str(&format!("**`{name}`**"));
        if !showing.title.is_empty() {
            text.push_str(&format!(" {}", showing.title));
        }
        text.push_str("\n\n");
    }
    let mut facts = Vec::new();
    if !showing.project.is_empty() {
        facts.push(format!("project `{}`", showing.project));
    }
    if !standing.unit.is_empty() {
        facts.push(format!("unit `{}`", standing.unit));
    }
    if !showing.flow.is_empty() {
        facts.push(format!("flow `{}`", showing.flow));
    }
    if !facts.is_empty() {
        text.push_str(&format!("{}\n\n", facts.join(" · ")));
    }
    if let Some(reason) = &standing.refused {
        text.push_str(&format!("**Ended before its first step.** {reason}\n\n"));
    } else if let Some(reason) = &standing.parked {
        let state = if showing.left_parked {
            format!(
                "**Parked at step `{}`, and ended there.** Whoever runs its sub-goal decides what happens to it; going on is a new pipeline that takes the unit up.",
                standing.step
            )
        } else if showing.given_up {
            format!(
                "**Given up** while it was parked at step `{}`.",
                standing.step
            )
        } else {
            format!(
                "**Parked at step `{}`.** It waits until someone continues it or gives it up.",
                standing.step
            )
        };
        text.push_str(&format!("{state}\n\n{}\n\n", cut(reason, 4_000)));
    } else if !showing.doing.is_empty() {
        text.push_str(&format!("**Now:** {}\n\n", showing.doing));
    }
    if !standing.done.is_empty() {
        text.push_str("| | step | outcome |\n|---|---|---|\n");
        // The newest are what a reader wants, and a long pipeline has
        // more than fit.
        let skip = standing.done.len().saturating_sub(150);
        for (at, (step, outcome)) in standing.done.iter().enumerate().skip(skip) {
            text.push_str(&format!("| {} | `{step}` | {outcome} |\n", at + 1));
        }
    }
    cut(text.trim_end(), DETAILS_MAX)
}

/// The accounts workflow's details as it starts.
pub fn accounts_static() -> String {
    "There is one of these in a namespace. Every agent step of every pipeline asks it which \
     Claude account to run under, and is told an account or to wait.\n\n\
     It reads what each account has left on a schedule, hears from pipelines when a run was \
     stopped at a usage limit, and signals every pipeline that waits as soon as an account is \
     back. It holds account names only: tokens are read by the worker that runs a step and \
     are never in any history.\n\n\
     `factory-engine accounts` shows what it knows. `factory-engine accounts recheck` makes \
     it look again now and believe what it finds, as after a usage reset.\n\n\
     It continues as a new run from time to time to keep its history short."
        .to_string()
}

/// What one thing handed to a project's tiers is, as the summary of
/// the turn that deals with it.
pub fn event(event: &factory_driver::goal::Event) -> String {
    use factory_driver::goal::Event;
    use factory_slack::socket::Incoming;
    let quoted = |text: &str| cut(&line(text), 90);
    match event {
        Event::Slack(Incoming::Message {
            text, thread_ts, ..
        }) => format!(
            "the owner wrote {}: “{}”",
            if thread_ts.is_some() {
                "in a thread"
            } else {
                "in the channel"
            },
            quoted(text)
        ),
        Event::Slack(Incoming::Action {
            action_id, value, ..
        }) => format!("the owner pressed `{action_id}` on `{value}`"),
        Event::Began { goal, bead, unit } => {
            format!("the pipeline for `{bead}` of goal `{goal}` has its unit, `{unit}`")
        }
        Event::Ended {
            goal,
            bead,
            ship,
            word,
            ..
        } => format!(
            "{} `{bead}` of goal `{goal}` is over ({}): whoever started it decides what is next",
            if *ship {
                "the shipping of sub-goal"
            } else {
                "the pipeline for"
            },
            match word.as_str() {
                "end" => "it ran to its end or parked",
                "refused" => "the bead was refused",
                _ => "it failed",
            }
        ),
        Event::Tick => {
            "see to what has come due: an agent to try again, a note to deal with, a bead waiting to be closed".to_string()
        }
        Event::Resume => {
            "pick up what was running before: ask again for the pipelines the records say are running, and wake whoever is owed an event".to_string()
        }
        Event::Open { text, .. } => format!("open a goal for the owner: “{}”", quoted(text)),
        Event::Tell { text, .. } => {
            format!("the owner told the executive: “{}”", quoted(text))
        }
    }
}

/// A project's workflow's details as it starts.
pub fn project_static(project: &str) -> String {
    format!(
        "Everything that happens to project `{project}` comes here as a signal and waits its \
         turn: what the owner writes or presses in Slack, a pipeline that has its unit or has \
         ended, a time that has come due.\n\n\
         Each is one activity, a turn: the project's executive, its goals' directors and their \
         foremen read what happened and decide, and what they decided is on the goals' records \
         when the turn is over. One turn runs at a time. The pipelines a turn asks for are \
         started from here as workflows of their own (`pipeline-<goal>-<bead>-…`), and go on \
         whatever becomes of this one.\n\n\
         A message that cannot be answered because every account is at its usage limit is kept \
         here and handed in again when one is back. Nothing waits in a process.\n\n\
         `factory-engine project {project}` shows where it stands. It continues as a new run \
         from time to time to keep its history short."
    )
}

/// The intake workflow's details as it starts.
pub fn intake_static() -> String {
    "There is one of these in a namespace. It keeps two activities running, and starts them \
     again whenever one ends.\n\n\
     The listener holds the socket to Slack and signals each thing the owner does to the \
     workflow of the project whose channel it was in (`project-<id>`), as soon as it \
     arrives.\n\n\
     The watcher looks at the pipelines the goals' records say are running, and signals each \
     one's project when it has a unit and when it has ended, however it ended: also when it \
     failed, was ended by hand, or was never started.\n\n\
     Neither decides anything. It continues as a new run once a day."
        .to_string()
}

fn percent(part: f64) -> String {
    format!("{:.0}%", part * 100.0)
}

/// The accounts workflow's current details: each account, who is waiting,
/// and when it last looked. Markdown.
pub fn accounts_details(
    picture: &Picture,
    waiting: &[Waiting],
    checked_at: u64,
    trouble: Option<&str>,
    now: u64,
) -> String {
    let mut text = String::new();
    if checked_at == 0 {
        text.push_str("The accounts have not been checked yet, and none is handed out.\n\n");
    } else {
        text.push_str(&format!(
            "Last checked at {}, {} ago.\n\n",
            clock(checked_at),
            span(now.saturating_sub(checked_at))
        ));
    }
    if let Some(trouble) = trouble {
        text.push_str(&format!(
            "**The last check failed**, and what was known before still stands: {}\n\n",
            cut(trouble, 1_000)
        ));
    }
    if !picture.order.is_empty() {
        text.push_str("| account | five hours | week | at a limit |\n|---|---|---|---|\n");
        for id in &picture.order {
            let state = picture.states.get(id);
            let window = |window: Option<&factory_claude::Window>| match window {
                Some(window) if window.resets_at > now => format!(
                    "{} (resets {})",
                    percent(window.utilization),
                    clock(window.resets_at)
                ),
                Some(_) => "reset since it was read".to_string(),
                None => "not known".to_string(),
            };
            let blocked = state
                .map(|state| {
                    state
                        .blocked
                        .iter()
                        .filter(|(_, until)| **until > now)
                        .map(|(scope, until)| {
                            let scope = if scope == "*" { "everything" } else { scope };
                            format!("{scope} until {}", clock(*until))
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            text.push_str(&format!(
                "| `{id}` | {} | {} | {} |\n",
                window(state.and_then(|state| state.five_hour.as_ref())),
                window(state.and_then(|state| state.seven_day.as_ref())),
                if blocked.is_empty() { "no" } else { &blocked },
            ));
        }
        text.push('\n');
    }
    if waiting.is_empty() {
        text.push_str("No pipeline is waiting for an account.");
    } else {
        text.push_str(&format!(
            "**Waiting for an account ({}):**\n",
            waiting.len()
        ));
        for waiting in waiting.iter().take(50) {
            text.push_str(&format!("- `{}` for {}\n", waiting.who, waiting.model));
        }
    }
    cut(text.trim_end(), DETAILS_MAX)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use factory_flow::Flow;

    #[test]
    fn a_summary_is_one_line_and_never_longer_than_the_server_takes() {
        assert_eq!(line("a\n  b\tc"), "a b c");
        let long = line(&"é".repeat(400));
        assert!(long.len() <= SUMMARY_MAX);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn a_length_of_time_reads_as_someone_would_say_it() {
        assert_eq!(span(45), "45 s");
        assert_eq!(span(600), "10 min");
        assert_eq!(span(3 * 3600), "3 h");
        assert_eq!(span(4_838), "1 h 21 min");
        assert_eq!(clock(1_794_774_660), "20:31 UTC");
    }

    #[test]
    fn a_step_says_what_runs_it_on_what_and_under_which_account() {
        let flow = Flow::parse(
            r#"
id = "toy"
start = "build"

[[step]]
id = "build"
kind = "agent"
role = "builder"
model = "claude-opus-5-5"
effort = "high"
writes = ["src"]
on.done = "test"

[[step]]
id = "test"
kind = "command"
command = "test"
on.pass = "end"
on.fail = "park"
"#,
        )
        .unwrap();
        let about = subject("toy-1", "", "Add the thing");
        assert_eq!(
            step(flow.step("build").unwrap(), 2, Some("claude"), &about),
            "Step `build`, round 2: the builder (claude-opus-5-5, high effort) on `toy-1` \
             (Add the thing), under account `claude`"
        );
        assert_eq!(
            step(flow.step("test").unwrap(), 1, None, &about),
            "Step `test`: the project's `test` command (must pass) on `toy-1` (Add the thing)"
        );
    }

    #[test]
    fn a_pipelines_details_say_where_it_stands_and_what_has_ended() {
        let showing = Showing {
            project: "toy".to_string(),
            bead: "toy-1".to_string(),
            title: "Add the thing".to_string(),
            flow: "toy".to_string(),
            doing: "step `test`".to_string(),
            given_up: false,
            left_parked: false,
        };
        let mut standing = Standing {
            unit: "toy-1-17".to_string(),
            step: "test".to_string(),
            done: vec![("build".to_string(), "done".to_string())],
            ..Standing::default()
        };
        let running = pipeline_details(&showing, &standing);
        assert!(running.contains("**`toy-1`** Add the thing"));
        assert!(running.contains("**Now:** step `test`"));
        assert!(running.contains("| 1 | `build` | done |"));
        standing.parked = Some("the tests fail".to_string());
        let parked = pipeline_details(&showing, &standing);
        assert!(parked.contains("**Parked at step `test`.**"));
        assert!(parked.contains("the tests fail"));
        assert!(!parked.contains("**Now:**"));
    }
}
