//! The review of the factory as a whole: a scheduled run whose agent is
//! shown how the factory ran, from the records, and proposes goals to the
//! owner. It files nothing and starts nothing. Each proposal is posted with
//! two buttons, and only the owner's press does anything: approving opens
//! the goal in its project as an owner's message there would, and
//! declining records the proposal with the owner's reason, if one is given.

use super::*;

/// The button that approves a proposal.
pub const APPROVE: &str = "proposal_approve";
/// The button that declines one.
pub const DECLINE: &str = "proposal_decline";

/// How many earlier proposals a run is shown.
const SHOWN: usize = 20;

/// A goal a review proposed to the owner, and what became of it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Proposal {
    /// What the buttons carry: the definition's seat, the run and the
    /// proposal's place in it.
    pub id: String,
    pub run: u32,
    pub at: u64,
    /// The project the goal is for.
    pub project: String,
    pub title: String,
    pub objective: String,
    pub stop_when: String,
    /// The figures of the report that call for it.
    pub figures: String,
    /// `waiting`, `approved` or `declined`.
    pub state: String,
    /// Why the owner declined it, if they said.
    pub reason: String,
    /// Who pressed the button.
    pub by: String,
    pub settled_at: Option<u64>,
    /// What approving it does: `goal` where the project has a definition
    /// the owner can open a goal under, `bead` where it has none.
    pub opens: String,
    /// What approving it made: the goal, the bead, or where it was handed.
    pub made: String,
    /// The message the buttons are on.
    pub channel: String,
    pub ts: String,
}

/// Whether a button is a proposal's, and if so whether it approves.
pub fn pressed(action_id: &str) -> Option<bool> {
    match action_id {
        APPROVE => Some(true),
        DECLINE => Some(false),
        _ => None,
    }
}

/// What a review returns.
pub fn review_schema() -> Value {
    object_schema(json!({
        "summary": text_schema("What you looked at and concluded, in a few plain sentences."),
        "proposals": {
            "type": "array",
            "description": "The goals you propose to the owner, the most valuable first. Empty when nothing is worth one.",
            "items": object_schema(json!({
                "project": text_schema("The project the goal is for, by its ID as the brief lists it."),
                "title": text_schema("The goal in one line, as the owner will read it above the buttons."),
                "objective": text_schema("What the goal is to achieve, as you would write it to the project's director: what is wrong today, what should be true afterwards, and what is out of scope."),
                "stop_when": text_schema("The condition that says the goal is reached, as something that can be checked."),
                "figures": text_schema("The figures from the report that call for this goal, quoted with their numbers, and the figure the goal should move."),
            })),
        },
        "digest": text_schema("For the owner, who reads it in the channel: how the factory ran in the period, in a few lines, with the figures that matter. If you propose nothing, say so in one line and say what you looked at."),
        "journal": text_schema("What the next run should know: what you looked at, what you chose not to propose and why, what to watch. You start fresh each time; this is all that is kept."),
    }))
}

/// What is wrong with what a review returned. It proposes goals, up to its
/// limit and for projects the factory knows, and does nothing else.
pub fn review_problems(
    most: u32,
    projects: &[(String, bool)],
    waiting: &[&Proposal],
    result: &Value,
) -> Vec<String> {
    let mut problems = Vec::new();
    if let Some(map) = result.as_object() {
        for (key, value) in map {
            let known = matches!(key.as_str(), "summary" | "proposals" | "digest" | "journal");
            let empty = match value {
                Value::Null => true,
                Value::String(text) => text.trim().is_empty(),
                Value::Array(list) => list.is_empty(),
                _ => false,
            };
            if !known && !empty {
                problems.push(format!(
                    "`{key}`: a review proposes goals to the owner; it files, starts and opens nothing"
                ));
            }
        }
    }
    let proposals = items(result, "proposals");
    if proposals.len() > most as usize {
        problems.push(format!(
            "proposals: {} proposed, and a run may propose {most} at most. Keep the most valuable, and put the rest in `journal`",
            proposals.len()
        ));
    }
    for (n, proposal) in proposals.iter().enumerate() {
        let n = n + 1;
        let project = strings(proposal, "project").trim();
        if !projects.iter().any(|(id, _)| id == project) {
            problems.push(format!(
                "proposals[{n}]: `{project}` is not a project of this factory. The projects: {}",
                projects
                    .iter()
                    .map(|(id, _)| format!("`{id}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        for key in ["title", "objective", "stop_when", "figures"] {
            if strings(proposal, key).trim().is_empty() {
                problems.push(format!("proposals[{n}]: `{key}` is empty"));
            }
        }
        let title = strings(proposal, "title").trim();
        if let Some(earlier) = waiting
            .iter()
            .find(|earlier| earlier.project == project && earlier.title.trim() == title)
        {
            problems.push(format!(
                "proposals[{n}]: the owner has not answered `{}`, which proposes the same. Do not propose it again",
                earlier.id
            ));
        }
    }
    problems
}

/// One earlier proposal as a later run is shown it.
fn proposal_line(proposal: &Proposal) -> String {
    let became = match proposal.state.as_str() {
        "approved" if proposal.made.is_empty() => "approved by the owner".to_string(),
        "approved" => format!("approved by the owner: {}", proposal.made),
        "declined" if proposal.reason.trim().is_empty() => {
            "declined by the owner, who gave no reason".to_string()
        }
        "declined" => format!(
            "declined by the owner: “{}”",
            truncate(proposal.reason.trim(), 400)
        ),
        _ => "not answered by the owner yet".to_string(),
    };
    format!(
        "- `{}` ({}, for `{}`): {}\n  {became}\n",
        proposal.id,
        date_utc(proposal.at),
        proposal.project,
        proposal.title.trim()
    )
}

/// What a review is told. `report` is how the factory ran in the period,
/// as code made it from the records under `state_dir`; `projects` is each
/// project with whether a goal can be opened in it.
pub fn review_brief(
    schedule: &Schedule,
    record: &Scheduled,
    most: u32,
    from: u64,
    report: &str,
    state_dir: &Path,
    projects: &[(String, bool)],
) -> String {
    let mut brief = format!(
        "# Review of the factory, run {}\n\n## What these runs are for\n\n{}\n\n## What you may do\n\nPropose goals to the owner: at most {most} in this run, and none if nothing is worth one. You file no bead, start nothing and open nothing. Each proposal is posted to the owner with an approve and a decline button, and only the owner's press does anything.\n\n",
        record.runs + 1,
        schedule.charter.trim(),
    );
    brief.push_str("## The projects\n\n");
    for (id, opens) in projects {
        brief.push_str(&if *opens {
            format!("- `{id}`: an approved proposal opens a goal in it, as a message from the owner in its channel would.\n")
        } else {
            format!("- `{id}`: it has no goal definition the owner can open a goal under, so an approved proposal is filed as a bead in its tracker, and nothing builds it by itself.\n")
        });
    }
    brief.push_str(&format!(
        "\n## How the factory ran since {}\n\nMade by code from the factory's own records. These are the facts: do not work them out again, and quote the figures you rest a proposal on.\n\n```\n{}\n```\n\nThe records themselves are under `{}`, to read and never to change: a unit's steps in `units/<unit>/events.jsonl` and its agents' runs in `units/<unit>/runs`, a goal's record and its director's journal in `goals/<goal>`, and each scheduled run in `projects/<project>/schedules`.\n\n",
        date_utc(from),
        report.trim_end(),
        state_dir.display()
    ));
    brief.push_str("## What became of earlier proposals\n\n");
    if record.proposals.is_empty() {
        brief.push_str("None has been made yet.\n");
    } else {
        let skip = record.proposals.len().saturating_sub(SHOWN);
        for proposal in &record.proposals[skip..] {
            brief.push_str(&proposal_line(proposal));
        }
        brief.push_str("\nA proposal the owner declined is not proposed again unless the figures have changed, and then say what changed. One the owner has not answered is not repeated.\n");
    }
    brief.push_str("\n## What the last run left for you\n\n");
    brief.push_str(if record.journal.trim().is_empty() {
        "Nothing.\n"
    } else {
        record.journal.trim()
    });
    brief.push('\n');
    brief
}

/// A proposal as the owner reads it above its buttons.
fn proposal_text(proposal: &Proposal) -> String {
    let does = if proposal.opens == "goal" {
        format!(
            "Approving opens this goal in `{}`, as your own message there would.",
            proposal.project
        )
    } else {
        format!(
            "`{}` has no goal definition yet, so approving files this as a bead in its tracker. Nothing builds it by itself.",
            proposal.project
        )
    };
    format!(
        ":bulb: *Proposed goal for* `{}`: *{}*\n{}\n*Stop when:* {}\n*Why:* {}\n_{does} Declining records it; a reply in this thread is kept as your reason._",
        proposal.project,
        escape(proposal.title.trim()),
        quote(&truncate(proposal.objective.trim(), 1500)),
        escape(&truncate(proposal.stop_when.trim(), 600)),
        escape(&truncate(proposal.figures.trim(), 600)),
    )
}

/// The goal as it is put to its project: what the owner would have written.
fn goal_text(proposal: &Proposal) -> String {
    format!(
        "{}\n\n{}\n\nStop when: {}\n\nThe factory's own review proposed this (`{}`) and the owner approved it. The figures that called for it: {}",
        proposal.title.trim(),
        proposal.objective.trim(),
        proposal.stop_when.trim(),
        proposal.id,
        proposal.figures.trim()
    )
}

impl Supervisor {
    /// Every project of the factory, with whether a goal can be opened in
    /// it by the owner.
    fn review_projects(&self) -> Vec<(String, bool)> {
        let mut out = Vec::new();
        for id in self.config.projects().unwrap_or_default() {
            let opens = self
                .config
                .goal_definitions(&id)
                .unwrap_or_default()
                .iter()
                .any(|definition| definition.schedule.is_none());
            out.push((id, opens));
        }
        out
    }

    /// Run a review that is due: show its agent how the factory ran since
    /// its last run and what became of what it proposed, and put what it
    /// proposes to the owner.
    pub(super) fn run_review(
        &mut self,
        project: &Project,
        definition: &GoalDefinition,
        schedule: &Schedule,
        seat: &Seat,
        mut record: Scheduled,
        most: u32,
    ) -> Result<bool, DriverError> {
        let now = now_unix();
        // The first run looks back a day; later ones, to the run before.
        let from = if record.runs == 0 {
            record.since.saturating_sub(86_400)
        } else {
            record.since
        };
        let period = crate::report::Period {
            since: from,
            until: now,
        };
        let report = crate::report::build(&self.factory.state_dir, period, None).text();
        let projects = self.review_projects();
        let brief = review_brief(
            schedule,
            &record,
            most,
            from,
            &report,
            &self.factory.state_dir,
            &projects,
        );
        let earlier = record.proposals.clone();
        let waiting: Vec<&Proposal> = earlier
            .iter()
            .filter(|proposal| proposal.state == "waiting")
            .collect();
        let check = |result: &Value| review_problems(most, &projects, &waiting, result);
        std::fs::create_dir_all(seat.dir.join("runs")).map_err(crate::io(&seat.dir))?;
        let turn = Turn {
            who: "inspector",
            role: "inspector",
            tier: definition.director.clone(),
            place: None,
            schema: review_schema(),
            session: Session::Fresh,
            prompt: brief,
        };
        let result = match self.run_agent(seat, &mut record.usage, turn, &check) {
            Ok((result, _)) => result,
            Err(reason) => {
                self.schedule_failed(project, definition, seat, &mut record, &reason)?;
                return Ok(true);
            }
        };
        record.runs += 1;
        record.failures = 0;
        record.retry_at = None;
        record.since = now_unix();
        record.journal = strings(&result, "journal").trim().to_string();
        let mut made: Vec<Proposal> = items(&result, "proposals")
            .iter()
            .take(most as usize)
            .enumerate()
            .map(|(n, proposal)| {
                let target = strings(proposal, "project").trim().to_string();
                let opens = projects.iter().any(|(id, opens)| *opens && id == &target);
                Proposal {
                    id: format!("{}-{}-{}", seat.id, record.runs, n + 1),
                    run: record.runs,
                    at: record.since,
                    project: target,
                    title: strings(proposal, "title").trim().to_string(),
                    objective: strings(proposal, "objective").trim().to_string(),
                    stop_when: strings(proposal, "stop_when").trim().to_string(),
                    figures: strings(proposal, "figures").trim().to_string(),
                    state: "waiting".to_string(),
                    opens: if opens { "goal" } else { "bead" }.to_string(),
                    ..Proposal::default()
                }
            })
            .collect();
        self.log_at(
            seat,
            "schedule.ran",
            json!({ "definition": definition.id, "run": record.runs,
                    "proposed": made.iter().map(|proposal| &proposal.id).collect::<Vec<_>>(),
                    "summary": strings(&result, "summary") }),
        );
        let digest = strings(&result, "digest").trim();
        let said = if made.is_empty() {
            format!(
                ":mag: *{}* · run {}: nothing worth proposing. {}",
                escape(&definition.id),
                record.runs,
                escape(&truncate(digest.lines().next().unwrap_or(""), 400))
            )
        } else {
            format!(
                ":mag: *{}* · run {} · {} proposal(s) below, each with its buttons\n{}",
                escape(&definition.id),
                record.runs,
                made.len(),
                quote(&truncate(digest, 2500))
            )
        };
        // A run that proposes something needs the owner's answer to each
        // proposal; one that proposes nothing needs nobody.
        let channel = if made.is_empty() {
            self.tell_channel(project, seat, &said)
        } else {
            self.call_channel(project, seat, &said)
        }
        .map(|(channel, _)| channel);
        for proposal in &mut made {
            let text = proposal_text(proposal);
            self.log_at(
                seat,
                "proposal.posted",
                json!({ "proposal": proposal.id, "project": proposal.project,
                        "title": proposal.title, "opens": proposal.opens, "text": text }),
            );
            let (Some(slack), Some(channel)) = (&self.slack, &channel) else {
                continue;
            };
            let approve = if proposal.opens == "goal" {
                "Approve: open the goal"
            } else {
                "Approve: file the bead"
            };
            let buttons = [
                Button {
                    action_id: APPROVE,
                    label: approve,
                    value: &proposal.id,
                    style: Some("primary"),
                },
                Button {
                    action_id: DECLINE,
                    label: "Decline",
                    value: &proposal.id,
                    style: None,
                },
            ];
            match slack.post_with_buttons(channel, None, &text, &buttons) {
                Ok(ts) => {
                    proposal.channel = channel.clone();
                    proposal.ts = ts;
                }
                Err(err) => self.log_at(seat, "slack.failed", json!({ "error": err.to_string() })),
            }
        }
        record.proposals.extend(made);
        self.save_schedule(seat, &record)?;
        Ok(true)
    }

    /// The scheduled definition that made a proposal, with its record and
    /// the proposal's place in it.
    fn proposal_home(
        &self,
        find: &dyn Fn(&Proposal) -> bool,
    ) -> Option<(Project, Seat, Scheduled, usize)> {
        for (project, definition) in self.scheduled() {
            let Some(record) = self.schedule_record(&project, &definition.id) else {
                continue;
            };
            if let Some(at) = record.proposals.iter().position(find) {
                let seat = self.schedule_seat(&project, &definition.id);
                return Some((project, seat, record, at));
            }
        }
        None
    }

    /// The owner pressed a proposal's button. One already answered stays
    /// as it was answered.
    pub(super) fn proposal_answered(
        &mut self,
        id: &str,
        approved: bool,
        user: &str,
    ) -> Result<(), DriverError> {
        let Some((_, seat, mut record, at)) = self.proposal_home(&|proposal| proposal.id == id)
        else {
            return Ok(());
        };
        if record.proposals[at].state != "waiting" {
            return Ok(());
        }
        let mut proposal = record.proposals[at].clone();
        proposal.by = user.to_string();
        proposal.settled_at = Some(now_unix());
        let outcome = if approved {
            proposal.state = "approved".to_string();
            // Saved as approved before anything is opened: a press that is
            // handed in twice must not open the goal twice.
            record.proposals[at] = proposal.clone();
            self.save_schedule(&seat, &record)?;
            let outcome = match self.open_proposed(&seat, &proposal, user) {
                Ok(made) => made,
                Err(err) => format!("it could not be opened: {err}"),
            };
            proposal.made = outcome.clone();
            self.log_at(
                &seat,
                "proposal.approved",
                json!({ "proposal": id, "by": user, "made": outcome }),
            );
            format!("Approved by <@{user}>: {outcome}")
        } else {
            proposal.state = "declined".to_string();
            self.log_at(
                &seat,
                "proposal.declined",
                json!({ "proposal": id, "by": user }),
            );
            format!("Declined by <@{user}>. A reply in this thread is kept as the reason.")
        };
        if let (Some(slack), false) = (&self.slack, proposal.ts.is_empty()) {
            // The message without its buttons, and how it was answered.
            let _ = slack.update(
                &proposal.channel,
                &proposal.ts,
                &format!("{}\n_{}_", proposal_text(&proposal), escape(&outcome)),
            );
        }
        record.proposals[at] = proposal;
        self.save_schedule(&seat, &record)
    }

    /// Do what approving a proposal does, and say what was made.
    fn open_proposed(
        &mut self,
        seat: &Seat,
        proposal: &Proposal,
        user: &str,
    ) -> Result<String, DriverError> {
        let target = self.config.project(&proposal.project)?;
        if proposal.opens != "goal" {
            let source = bead_source(&target);
            let file = json!({
                "title": proposal.title,
                "type": "task",
                "priority": 2,
                "description": format!(
                    "{}\n\nThe factory's own review proposed this (`{}`) and the owner approved it. The figures that called for it: {}",
                    proposal.objective, proposal.id, proposal.figures
                ),
                "acceptance_criteria": proposal.stop_when,
                "parent": "",
                "discovered_from": "",
                "deferred": false,
            });
            let (_, line) = self.file_bead_at(seat, &source, &target, &file);
            return Ok(line);
        }
        let text = goal_text(proposal);
        let ours = self
            .scope
            .as_ref()
            .is_none_or(|scope| scope == &proposal.project);
        // The goal's thread. In the review's own channel it is the
        // proposal's message; in another project's it is a message put
        // there for it.
        let home = self.channel_of.get(&proposal.project).cloned();
        let thread = match (&self.slack, home) {
            (Some(_), Some(channel)) if channel == proposal.channel && !proposal.ts.is_empty() => {
                Some((channel, proposal.ts.clone()))
            }
            (Some(slack), Some(channel)) => slack
                .post(
                    &channel,
                    None,
                    &format!(
                        ":bulb: <@{user}> approved a goal the factory's review proposed for this project:\n{}",
                        quote(&truncate(&text, 2500))
                    ),
                )
                .ok()
                .map(|ts| (channel, ts)),
            _ => None,
        };
        if ours {
            let (channel, ts) = thread.unwrap_or_else(|| (String::new(), now_unix().to_string()));
            return Ok(
                match self.owner_opens(&proposal.project, &channel, &ts, user, &text)? {
                    Some(goal) => format!("goal `{goal}` is opened in `{}`", proposal.project),
                    None => format!("it is with the executive of `{}`", proposal.project),
                },
            );
        }
        // Another project's records are that project's turn to write: it
        // is handed the owner's message, or, with no thread to open it on,
        // asked to open the goal.
        let event = match thread {
            Some((channel, ts)) => Event::Slack(Incoming::Message {
                channel,
                user: user.to_string(),
                text,
                ts,
                thread_ts: None,
            }),
            None => Event::Open {
                project: proposal.project.clone(),
                text,
            },
        };
        self.handed.push((proposal.project.clone(), event));
        Ok(format!(
            "handed to `{}`, which opens it in its turn",
            proposal.project
        ))
    }

    /// The owner wrote under a message. If it is a proposal they declined
    /// and gave no reason for, this is the reason.
    pub(super) fn proposal_reason(
        &mut self,
        channel: &str,
        root: &str,
        text: &str,
    ) -> Result<(), DriverError> {
        let Some((_, seat, mut record, at)) = self.proposal_home(&|proposal| {
            !proposal.ts.is_empty() && proposal.ts == root && proposal.channel == channel
        }) else {
            return Ok(());
        };
        let proposal = &mut record.proposals[at];
        if proposal.state != "declined" || !proposal.reason.trim().is_empty() {
            return Ok(());
        }
        proposal.reason = text.trim().to_string();
        self.log_at(
            &seat,
            "proposal.reason",
            json!({ "proposal": proposal.id, "reason": proposal.reason }),
        );
        self.save_schedule(&seat, &record)
    }
}
