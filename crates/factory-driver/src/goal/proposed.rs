//! A goal definition the executive proposes to the owner, when none of the
//! project's suits the work. Code checks it the way `factory goal enable`
//! does before anything is posted, and sends every problem back to the
//! executive in the same turn. A proposal that passes is kept on the
//! thread's record and posted there with two buttons. Only the owner's
//! press acts: Approve puts the definition in force, files the bead that
//! commits it to the release, and opens the goal under it in the same
//! thread, with no executive turn. A definition with a `[schedule]` runs
//! by the clock and no goal is opened under it: Approve has the running
//! worker take it up as `factory goal enable` does, says in the thread
//! when it next runs, and leaves the thread the executive's. Reject
//! stores nothing and wakes the executive in the thread's session to hear
//! of it; the owner's next reply there is taken as why. While a proposal waits, the owner's replies reach
//! the executive with it named, and a new proposal supersedes it.

use super::*;

/// The button that puts a proposed definition in force and, for one with
/// no schedule, opens the goal.
pub const APPROVE: &str = "definition_approve";
/// The button that rejects one.
pub const REJECT: &str = "definition_reject";

/// How many lines of what did not change a diff shows around a change.
const CONTEXT: usize = 3;

/// A goal definition the executive proposed in a thread, and what became
/// of it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DefinitionProposal {
    /// The ID the definition gives itself.
    pub id: String,
    /// Its TOML, as the executive wrote it.
    pub text: String,
    /// The SHA-256 of the text: what it is stored under once approved.
    pub hash: String,
    /// Why no definition in force suits the work.
    pub reason: String,
    /// What the goal's director should know, as `open_goal`'s handoff.
    pub handoff: String,
    /// Whether a definition with that ID was in force when it was proposed.
    pub change: bool,
    /// When the definition runs by itself, in words, if it has a
    /// `[schedule]`: Approve puts it in force and opens no goal. Empty for
    /// one without.
    pub schedule: String,
    /// `waiting`, `approved`, `rejected` or `superseded`. Only one that
    /// waits can be approved, so a rejected or superseded one is never
    /// stored.
    pub state: String,
    /// Who pressed the button that settled it.
    pub by: String,
    pub settled_at: Option<u64>,
    /// What approving it did, in words for the thread.
    pub made: String,
    /// What was posted: the reason and the text, or its diff.
    pub message: String,
    /// The message the buttons are on.
    pub ts: String,
    /// The owner's first reply in the thread after rejecting it: why.
    pub why: String,
    /// The proposal (from 1) that superseded it, or 0.
    pub superseded_by: usize,
}

/// When a schedule runs, in words.
fn schedule_words(schedule: &Schedule) -> String {
    format!(
        "every {} day(s) at {:02}:00 UTC",
        schedule.every_days.max(1),
        schedule.hour_utc
    )
}

/// What a thread's record says the owner did on rejecting the `n`th
/// proposal (from 1).
fn rejection_said(proposal: &DefinitionProposal, n: usize) -> String {
    format!(
        "Rejected the proposed goal definition `{}` (proposal {n} in this thread).",
        proposal.id
    )
}

/// What a press on a proposal that no longer waits says.
fn settled_note(proposal: &DefinitionProposal) -> String {
    if proposal.state == "superseded" {
        return format!(
            "The proposed definition `{}` was superseded by proposal {} in this thread, so that press did nothing: it was not stored.",
            proposal.id, proposal.superseded_by
        );
    }
    let by = if proposal.by.is_empty() {
        String::new()
    } else {
        format!(" by <@{}>", proposal.by)
    };
    format!(
        "The proposed definition `{}` was already {}{by}, so that press did nothing.",
        proposal.id, proposal.state
    )
}

/// The proposals of a thread still waiting for the owner, named for the
/// executive, or nothing when none waits.
fn waiting_note(front: &Front) -> String {
    let waiting: Vec<String> = front
        .proposals
        .iter()
        .enumerate()
        .filter(|(_, proposal)| proposal.state == "waiting")
        .map(|(at, proposal)| format!("`{}` (proposal {})", proposal.id, at + 1))
        .collect();
    if waiting.is_empty() {
        return String::new();
    }
    format!(
        "\nYour proposed goal definition {} still waits for the owner's Approve or Reject. If what they say changes it, return the whole new TOML in `propose_definition`: it supersedes the one waiting, whose buttons then do nothing.\n",
        waiting.join(", ")
    )
}

/// The scheduled definitions the executive proposed in a thread that the
/// owner approved, named for the executive, or nothing when there are
/// none: they are in force, no goal was opened under them, and the thread
/// is still the executive's.
fn in_force_note(front: &Front) -> String {
    let approved: Vec<String> = front
        .proposals
        .iter()
        .enumerate()
        .filter(|(_, proposal)| proposal.state == "approved" && !proposal.schedule.is_empty())
        .map(|(at, proposal)| {
            format!(
                "`{}` (proposal {}), which runs {}",
                proposal.id,
                at + 1,
                proposal.schedule
            )
        })
        .collect();
    if approved.is_empty() {
        return String::new();
    }
    format!(
        "\nThe owner approved your proposed goal definition {}: it is in force, and no goal was opened under it. This thread is still yours.\n",
        approved.join(", ")
    )
}

/// What the executive is told in a thread whose session it resumes: the
/// owner's reply, or that the owner rejected the proposal at `rejected`,
/// and which of its proposals still wait.
pub fn resumed_prompt(front: &Front, text: &str, rejected: Option<usize>) -> String {
    let mut prompt = match rejected.and_then(|at| front.proposals.get(at).map(|p| (at, p))) {
        Some((at, proposal)) => format!(
            "The owner rejected your proposed goal definition `{}` (proposal {} in this thread). It is not stored or put in force, and no goal opens under it. If they say why, it comes as their next reply in this thread. Say in `reply` what you will do instead: propose another definition, open the goal under one in force, or ask the owner what they want.\n",
            proposal.id,
            at + 1
        ),
        None => {
            let mut said = format!(
                "The owner replied in the thread:\n\n{}\n",
                quote_plain(text)
            );
            if let Some(last) = front.proposals.last()
                && last.state == "rejected"
                && last.why == text
            {
                said.push_str(&format!(
                    "\nIt is their first word since they rejected your proposed goal definition `{}` (proposal {}): read it as why.\n",
                    last.id,
                    front.proposals.len()
                ));
            }
            said
        }
    };
    prompt.push_str(&in_force_note(front));
    prompt.push_str(&waiting_note(front));
    prompt
}

/// What the buttons of the `n`th proposal (from 1) of a thread carry.
pub fn button_value(root: &str, n: usize) -> String {
    format!("{root}/{n}")
}

/// The thread and the proposal's place in it (from 0) that a button names.
fn named(value: &str) -> Option<(&str, usize)> {
    let (root, n) = value.rsplit_once('/')?;
    let n = n.parse::<usize>().ok()?.checked_sub(1)?;
    Some((root, n))
}

/// What `propose_definition` in the executive's result is.
pub fn proposal_schema() -> Value {
    object_schema(json!({
        "toml": text_schema("When no goal definition of the project suits the work: the whole TOML of the one you propose, a new ID or a change to one in force. Code checks it against the project as `factory goal enable` would, and sends any problem back to you. Empty when proposing none."),
        "reason": text_schema("For the owner: why none of the definitions in force suits the work, and which of your choices you are least sure of. Empty when proposing none."),
        "handoff": text_schema("What the director of the goal opened under it should know that the owner's words do not say, as for `open_goal`. Empty for a definition with a `[schedule]`, which runs by the clock and has no goal opened under it, or when proposing none."),
    }))
}

/// The TOML an executive's result proposes, if it proposes one.
pub fn proposed_text(result: &Value) -> Option<&str> {
    result
        .get("propose_definition")
        .map(|proposal| strings(proposal, "toml"))
        .filter(|text| !text.trim().is_empty())
}

/// Why the definition an executive proposes for a project may not be put
/// to the owner: every problem `check_candidate` finds with the project's
/// set as it would be, and what else makes it no use for the goal.
pub fn proposal_problems(config: &ConfigDir, project: &str, result: &Value) -> Vec<String> {
    let Some(text) = proposed_text(result) else {
        return Vec::new();
    };
    let proposal = &result["propose_definition"];
    let mut problems: Vec<String> = check_candidate(config, project, text)
        .into_iter()
        .map(|problem| format!("propose_definition: {problem}"))
        .collect();
    let opening = result
        .get("open_goal")
        .map(|open| strings(open, "definition").trim())
        .unwrap_or("");
    if !opening.is_empty() {
        problems.push(format!(
            "open_goal and propose_definition: do one or the other. Open the goal under `{opening}`, or propose a definition and open nothing: the goal opens under it when the owner approves it"
        ));
    }
    if strings(proposal, "reason").trim().is_empty() {
        problems.push(
            "propose_definition: say in `reason` why no definition in force suits the work"
                .to_string(),
        );
    }
    if !problems.is_empty() {
        return problems;
    }
    // It checks, so it reads.
    let Ok(candidate) = config.goal_definitions_with(project, text) else {
        return problems;
    };
    let id = candidate.stored.id;
    if in_force_text(config, project, &id).as_deref() == Some(text) {
        problems.push(format!(
            "propose_definition: that is the text of `{id}` in force already: open the goal under it with `open_goal`"
        ));
    }
    problems
}

/// The text of a project's goal definition `id` in force, if it has one.
fn in_force_text(config: &ConfigDir, project: &str, id: &str) -> Option<String> {
    let in_force = config.goal_definitions_in_force(project).ok()?;
    let found = in_force.iter().find(|one| one.definition.id == id)?;
    std::fs::read_to_string(&found.path).ok()
}

/// The message a proposal is posted as, without its buttons.
pub fn proposal_message(project: &str, proposal: &DefinitionProposal, old: Option<&str>) -> String {
    let what = if old.is_some() {
        format!(
            "a change to `{}`, the definition in force with that ID",
            proposal.id
        )
    } else {
        "a new definition".to_string()
    };
    let shown = match old {
        Some(old) => unified_diff(
            old,
            &proposal.text,
            &format!("{}.toml (in force)", proposal.id),
            &format!("{}.toml (proposed)", proposal.id),
        ),
        None => proposal.text.clone(),
    };
    let approve = if proposal.schedule.is_empty() {
        "Approve puts it in force and opens the goal under it in this thread.".to_string()
    } else {
        format!(
            "It has a schedule: approve puts the schedule in force, and the factory runs it {}. No goal is opened under it.",
            proposal.schedule
        )
    };
    format!(
        ":scroll: *Proposed goal definition* `{id}` for `{project}`: {what}\n{reason}\n```\n{shown}{newline}```\n{approve} It is checked again first.",
        id = proposal.id,
        reason = quote(proposal.reason.trim()),
        shown = escape(&shown),
        newline = if shown.ends_with('\n') { "" } else { "\n" },
    )
}

/// A unified diff of two texts, line by line, with [`CONTEXT`] lines of
/// what did not change around each change. Empty when they are the same.
pub fn unified_diff(old: &str, new: &str, from: &str, to: &str) -> String {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    // common[i][j]: the longest run of lines a[i..] and b[j..] share.
    let mut common = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            common[i][j] = if a[i] == b[j] {
                common[i + 1][j + 1] + 1
            } else {
                common[i + 1][j].max(common[i][j + 1])
            };
        }
    }
    // Each line of the script: its mark, the line, and where it stands in
    // each text (the next line's place, for one the text does not have).
    let mut script: Vec<(char, &str, usize, usize)> = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            script.push((' ', a[i], i, j));
            i += 1;
            j += 1;
        } else if i < a.len() && (j == b.len() || common[i + 1][j] >= common[i][j + 1]) {
            script.push(('-', a[i], i, j));
            i += 1;
        } else {
            script.push(('+', b[j], i, j));
            j += 1;
        }
    }
    let changed: Vec<usize> = (0..script.len())
        .filter(|&at| script[at].0 != ' ')
        .collect();
    let Some(&first) = changed.first() else {
        return String::new();
    };
    // Changes close enough to share their context are one hunk.
    let mut hunks = vec![(first, first)];
    for &at in &changed[1..] {
        let last = hunks.len() - 1;
        if at - hunks[last].1 <= 2 * CONTEXT + 1 {
            hunks[last].1 = at;
        } else {
            hunks.push((at, at));
        }
    }
    let mut out = format!("--- {from}\n+++ {to}\n");
    for (start, end) in hunks {
        let start = start.saturating_sub(CONTEXT);
        let end = (end + CONTEXT).min(script.len() - 1);
        let lines = &script[start..=end];
        let olds = lines.iter().filter(|line| line.0 != '+').count();
        let news = lines.iter().filter(|line| line.0 != '-').count();
        // An empty side is placed after the line before it, as diff does.
        let place = |at: usize, count: usize| if count == 0 { at } else { at + 1 };
        out.push_str(&format!(
            "@@ -{},{olds} +{},{news} @@\n",
            place(lines[0].2, olds),
            place(lines[0].3, news)
        ));
        for (mark, line, _, _) in lines {
            out.push_str(&format!("{mark}{line}\n"));
        }
    }
    out
}

impl Supervisor {
    /// The executive proposed the `n`th definition (from 1) of a thread:
    /// every one still waiting there is superseded by it, and its message
    /// says so in place of its buttons.
    fn supersede(&mut self, seat: &Seat, front: &mut Front, root: &str, n: usize) {
        for (at, proposal) in front.proposals.iter_mut().enumerate() {
            if proposal.state != "waiting" {
                continue;
            }
            proposal.state = "superseded".to_string();
            proposal.superseded_by = n;
            proposal.settled_at = Some(now_unix());
            self.log_at(
                seat,
                "definition.superseded",
                json!({ "proposal": button_value(root, at + 1),
                        "by": button_value(root, n), "definition": proposal.id }),
            );
            if let (Some(slack), false) = (&self.slack, proposal.ts.is_empty()) {
                let _ = slack.update(
                    &front.channel,
                    &proposal.ts,
                    &format!(
                        "{}\n_Superseded by proposal {n} below: its buttons do nothing._",
                        proposal.message
                    ),
                );
            }
        }
    }

    /// The owner pressed Reject on a proposed definition: it is marked
    /// rejected, which nothing stores or puts in force, and the executive
    /// is woken once in the thread's session to hear of it. A press on one
    /// that no longer waits does nothing but say so.
    pub(super) fn definition_rejected(
        &mut self,
        value: &str,
        user: &str,
    ) -> Result<(), DriverError> {
        let Some((root, at)) = named(value) else {
            return Ok(());
        };
        let Some(mut executive) = self
            .executives
            .values()
            .find(|executive| {
                executive
                    .threads
                    .get(root)
                    .is_some_and(|front| at < front.proposals.len())
            })
            .cloned()
        else {
            return Ok(());
        };
        let project = executive.project.clone();
        let mut front = executive.threads[root].clone();
        let channel = front.channel.clone();
        let mut proposal = front.proposals[at].clone();
        let said = rejection_said(&proposal, at + 1);
        // The press handed in again because every account was at a limit
        // when it came: the rejection is on record, and the executive has
        // still to hear of it.
        let unheard = front.waiting
            && front
                .said
                .last()
                .is_some_and(|(who, what)| who == "owner" && *what == said);
        if proposal.state == "rejected" && front.goal.is_none() && unheard {
            return self
                .front_turn(&project, &channel, root, user, &said, Some(at))
                .map(|_| ());
        }
        if proposal.state != "waiting" {
            self.post(&channel, root, &settled_note(&proposal));
            return Ok(());
        }
        proposal.state = "rejected".to_string();
        proposal.by = user.to_string();
        proposal.settled_at = Some(now_unix());
        front.proposals[at] = proposal.clone();
        executive.threads.insert(root.to_string(), front.clone());
        self.save_executive(executive)?;
        let seat = self.executive_seat(&project);
        self.log_at(
            &seat,
            "definition.rejected",
            json!({ "proposal": value, "by": user, "definition": proposal.id }),
        );
        if let (Some(slack), false) = (&self.slack, proposal.ts.is_empty()) {
            let _ = slack.update(
                &channel,
                &proposal.ts,
                &format!("{}\n_Rejected by <@{user}>._", proposal.message),
            );
        }
        // A thread that became a goal is its director's: the executive has
        // no turn there.
        if front.goal.is_some() {
            return Ok(());
        }
        self.front_turn(&project, &channel, root, user, &said, Some(at))
            .map(|_| ())
    }

    /// Keep the definition an executive's checked result proposes on its
    /// thread's record, and post it there with its buttons.
    pub(super) fn post_proposal(
        &mut self,
        project: &str,
        front: &mut Front,
        root: &str,
        result: &Value,
    ) {
        let Some(text) = proposed_text(result) else {
            return;
        };
        let given = &result["propose_definition"];
        let (id, schedule) = match self.config.goal_definitions_with(project, text) {
            Ok(candidate) => {
                let id = candidate.stored.id;
                let schedule = candidate
                    .definitions
                    .iter()
                    .find(|definition| definition.id == id)
                    .and_then(|definition| definition.schedule.as_ref())
                    .map(schedule_words)
                    .unwrap_or_default();
                (id, schedule)
            }
            Err(_) => return,
        };
        let old = in_force_text(&self.config, project, &id);
        let mut proposal = DefinitionProposal {
            id,
            text: text.to_string(),
            hash: factory_config::goal_definition_hash(text),
            reason: strings(given, "reason").trim().to_string(),
            handoff: strings(given, "handoff").trim().to_string(),
            change: old.is_some(),
            schedule,
            state: "waiting".to_string(),
            ..DefinitionProposal::default()
        };
        let message = proposal_message(project, &proposal, old.as_deref());
        proposal.message = message.clone();
        let n = front.proposals.len() + 1;
        let value = button_value(root, n);
        let seat = self.executive_seat(project);
        self.supersede(&seat, front, root, n);
        self.log_at(
            &seat,
            "definition.proposed",
            json!({ "thread": root, "proposal": value, "definition": proposal.id,
                    "hash": proposal.hash, "change": proposal.change, "text": message }),
        );
        if let Some(slack) = &self.slack {
            let approve = if proposal.schedule.is_empty() {
                "Approve: put it in force and open the goal"
            } else {
                "Approve: put its schedule in force"
            };
            let buttons = [
                Button {
                    action_id: APPROVE,
                    label: approve,
                    value: &value,
                    style: Some("primary"),
                },
                Button {
                    action_id: REJECT,
                    label: "Reject",
                    value: &value,
                    style: None,
                },
            ];
            match slack.post_with_buttons(&front.channel, Some(root), &message, &buttons) {
                Ok(ts) => proposal.ts = ts,
                Err(err) => self.log_at(&seat, "slack.failed", json!({ "error": err.to_string() })),
            }
        }
        front.proposals.push(proposal);
    }

    /// The owner pressed Approve on a proposed definition. If it can still
    /// be put in force, it is, the bead that commits it to the release is
    /// filed, and the goal opens under it in the proposal's thread as
    /// `open_goal` opens one. No executive turn runs.
    pub(super) fn definition_approved(
        &mut self,
        value: &str,
        user: &str,
    ) -> Result<(), DriverError> {
        let Some((root, at)) = named(value) else {
            return Ok(());
        };
        let Some(mut executive) = self
            .executives
            .values()
            .find(|executive| {
                executive
                    .threads
                    .get(root)
                    .is_some_and(|front| at < front.proposals.len())
            })
            .cloned()
        else {
            return Ok(());
        };
        let project = executive.project.clone();
        let seat = self.executive_seat(&project);
        let mut front = executive.threads[root].clone();
        let channel = front.channel.clone();
        let mut proposal = front.proposals[at].clone();
        if proposal.state != "waiting" {
            self.post(&channel, root, &settled_note(&proposal));
            return Ok(());
        }
        if let Some(goal) = &front.goal {
            self.post(
                &channel,
                root,
                &format!(
                    "This thread is goal `{goal}` already, so nothing was put in force and no goal was opened."
                ),
            );
            return Ok(());
        }
        // What is in force may have changed since it was proposed: it is
        // checked again, and with a problem nothing is stored.
        let stored = match enable_definition(&self.config, &project, &proposal.text) {
            Ok(stored) => stored,
            Err(DriverError::DefinitionRefused { problems, .. }) => {
                self.log_at(
                    &seat,
                    "definition.refused",
                    json!({ "proposal": value, "by": user, "problems": problems }),
                );
                let lines: Vec<String> = problems
                    .iter()
                    .map(|problem| format!("• {}", escape(problem)))
                    .collect();
                self.post(
                    &channel,
                    root,
                    &format!(
                        ":warning: The proposed definition `{}` cannot be put in force as things stand now, so nothing was stored and no goal was opened:\n{}\nReply here to have the executive change it.",
                        proposal.id,
                        lines.join("\n")
                    ),
                );
                return Ok(());
            }
            Err(err) => {
                self.log_at(
                    &seat,
                    "definition.failed",
                    json!({ "proposal": value, "by": user, "error": err.to_string() }),
                );
                self.post(
                    &channel,
                    root,
                    &format!(
                        ":warning: The proposed definition `{}` could not be stored ({}). Press Approve again to retry.",
                        proposal.id,
                        escape(&err.to_string())
                    ),
                );
                return Ok(());
            }
        };
        // Settled before anything else is done: a press handed in twice
        // must not open the goal twice.
        proposal.state = "approved".to_string();
        proposal.by = user.to_string();
        proposal.settled_at = Some(now_unix());
        front.proposals[at] = proposal.clone();
        executive.threads.insert(root.to_string(), front.clone());
        self.save_executive(executive.clone())?;
        let mut made = vec![format!(
            "`{}` is in force (stored as {})",
            stored.id,
            stored.path.display()
        )];
        match file_catch_up_bead(&self.config, &project, &stored, &proposal.text) {
            Ok(Some(catch_up)) => made.push(format!(
                "bead `{}` in `{}` commits it to the release{}",
                catch_up.bead.id,
                catch_up.project,
                if catch_up.owners {
                    ", by the owner's hand"
                } else {
                    ""
                }
            )),
            Ok(None) => {}
            Err(err) => made.push(format!(
                "no bead was filed to commit it to the release: {err}"
            )),
        }
        let schedule = self
            .config
            .goal_definitions(&project)
            .ok()
            .and_then(|definitions| {
                definitions
                    .into_iter()
                    .find(|definition| definition.id == stored.id)
            })
            .and_then(|definition| definition.schedule);
        if let Some(schedule) = schedule {
            return self.schedule_approved(ScheduleApproved {
                value,
                user,
                project: &project,
                root,
                at,
                proposal,
                stored: &stored,
                schedule: &schedule,
                made,
            });
        }
        self.log_at(
            &seat,
            "definition.approved",
            json!({ "proposal": value, "by": user, "definition": stored.id,
                    "hash": stored.hash, "made": made }),
        );
        let opened = self.open_from_front(
            &project,
            &mut executive,
            root,
            front,
            &stored.id,
            &proposal.handoff,
        );
        match &opened {
            Ok((goal, _)) => made.push(format!("goal `{goal}` is opened under it")),
            Err(err) => made.push(format!("the goal could not be opened: {err}")),
        }
        proposal.made = made.join("; ");
        if let Some(mut record) = self.executives.get(&project).cloned()
            && let Some(front) = record.threads.get_mut(root)
        {
            front.proposals[at] = proposal.clone();
            self.save_executive(record)?;
        }
        if let (Some(slack), false) = (&self.slack, proposal.ts.is_empty()) {
            // The message without its buttons, and how it was answered.
            let _ = slack.update(
                &channel,
                &proposal.ts,
                &format!(
                    "{}\n_Approved by <@{user}>: {}_",
                    proposal.message,
                    escape(&proposal.made)
                ),
            );
        }
        let (goal, asked) = opened?;
        self.intake_turn(&goal, &asked)
    }

    /// The owner approved a proposed definition with a `[schedule]`, and
    /// it is in force: the running worker is told as `factory goal enable`
    /// tells it, so the project looks at its schedule now, and the thread
    /// is told when it next runs. No goal is opened under it, and the
    /// thread stays the executive's.
    fn schedule_approved(&mut self, approved: ScheduleApproved<'_>) -> Result<(), DriverError> {
        let ScheduleApproved {
            value,
            user,
            project,
            root,
            at,
            mut proposal,
            stored,
            schedule,
            mut made,
        } = approved;
        let seat = self.executive_seat(project);
        let engine = match tell_engine(&self.config, project, &stored.id) {
            Ok(said) => {
                let said: Vec<&str> = said.lines().filter(|line| !line.is_empty()).collect();
                made.push(said.join("; "));
                json!({ "said": said })
            }
            Err(err) => {
                made.push(format!("the running worker was not told: {err}"));
                json!({ "error": err.to_string() })
            }
        };
        let since = self
            .config
            .project(project)
            .ok()
            .and_then(|record| self.schedule_record(&record, &stored.id));
        let next = match since {
            Some(record) => record
                .retry_at
                .unwrap_or_else(|| schedule_due(schedule, record.since)),
            None => schedule_due(schedule, now_unix()),
        };
        let words = schedule_words(schedule);
        let when = if next <= now_unix() {
            "and is due now".to_string()
        } else {
            format!("next at {}", date_utc(next))
        };
        made.push(format!(
            "it runs {words}, {when}; no goal is opened under it"
        ));
        self.log_at(
            &seat,
            "definition.approved",
            json!({ "proposal": value, "by": user, "definition": stored.id,
                    "hash": stored.hash, "made": made, "schedule": words,
                    "next_at": next, "engine": engine }),
        );
        proposal.made = made.join("; ");
        let channel = match self.executives.get(project).cloned() {
            Some(mut record) => {
                let channel = record
                    .threads
                    .get(root)
                    .map(|front| front.channel.clone())
                    .unwrap_or_default();
                if let Some(front) = record.threads.get_mut(root) {
                    front.proposals[at] = proposal.clone();
                    self.save_executive(record)?;
                }
                channel
            }
            None => String::new(),
        };
        if let (Some(slack), false) = (&self.slack, proposal.ts.is_empty()) {
            // The message without its buttons, and how it was answered.
            let _ = slack.update(
                &channel,
                &proposal.ts,
                &format!(
                    "{}\n_Approved by <@{user}>: {}_",
                    proposal.message,
                    escape(&proposal.made)
                ),
            );
        }
        self.post(
            &channel,
            root,
            &format!(
                ":calendar: `{id}` is in force for `{project}`: the factory runs it {words}, {when}. No goal is opened under it. Reply here to go on with the executive.",
                id = stored.id,
            ),
        );
        Ok(())
    }
}

/// What [`Supervisor::schedule_approved`] is given: the press, and what
/// approving the proposal has done so far.
struct ScheduleApproved<'a> {
    value: &'a str,
    user: &'a str,
    project: &'a str,
    root: &'a str,
    /// The proposal's place in its thread, from 0.
    at: usize,
    proposal: DefinitionProposal,
    stored: &'a factory_config::StoredGoalDefinition,
    schedule: &'a Schedule,
    made: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_diff_shows_each_change_with_the_lines_around_it() {
        let old = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\n";
        let new = "a\nB\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\n";
        assert_eq!(
            unified_diff(old, new, "old", "new"),
            "--- old\n+++ new\n@@ -1,5 +1,5 @@\n a\n-b\n+B\n c\n d\n e\n@@ -11,3 +11,4 @@\n k\n l\n m\n+n\n"
        );
    }

    #[test]
    fn a_diff_joins_changes_that_share_context_and_places_an_empty_side() {
        assert_eq!(
            unified_diff("x\n", "", "old", "new"),
            "--- old\n+++ new\n@@ -1,1 +0,0 @@\n-x\n"
        );
        assert_eq!(
            unified_diff("", "y\n", "old", "new"),
            "--- old\n+++ new\n@@ -0,0 +1,1 @@\n+y\n"
        );
        assert_eq!(unified_diff("same\n", "same\n", "old", "new"), "");
        let joined = unified_diff(
            "1\n2\n3\n4\n5\n6\n7\n8\n",
            "1\nX\n3\n4\n5\n6\n7\nY\n",
            "o",
            "n",
        );
        assert_eq!(joined.matches("@@ -").count(), 1, "{joined}");
    }

    #[test]
    fn a_button_names_the_thread_and_the_proposal() {
        let value = button_value("1795.0001", 2);
        assert_eq!(named(&value), Some(("1795.0001", 1)));
        assert_eq!(named("1795.0001/0"), None);
        assert_eq!(named("nothing"), None);
    }
}
