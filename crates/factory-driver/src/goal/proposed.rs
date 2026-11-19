//! A goal definition the executive proposes to the owner, when none of the
//! project's suits the work. Code checks it the way `factory goal enable`
//! does before anything is posted, and sends every problem back to the
//! executive in the same turn. A proposal that passes is kept on the
//! thread's record and posted there with two buttons. Only the owner's
//! press acts: Approve puts the definition in force, files the bead that
//! commits it to the release, and opens the goal under it in the same
//! thread, with no executive turn.

use super::*;

/// The button that puts a proposed definition in force and opens the goal.
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
    /// `waiting`, `approved` or `rejected`.
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
        "handoff": text_schema("What the director of the goal opened under it should know that the owner's words do not say, as for `open_goal`. Empty when proposing none."),
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
    if candidate
        .definitions
        .iter()
        .any(|definition| definition.id == id && definition.schedule.is_some())
    {
        problems.push(format!(
            "propose_definition: `{id}` has a [schedule]: the factory runs such a definition at its times, and no goal is opened under it"
        ));
    }
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
    format!(
        ":scroll: *Proposed goal definition* `{id}` for `{project}`: {what}\n{reason}\n```\n{shown}{newline}```\nApprove puts it in force and opens the goal under it in this thread. It is checked again first.",
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
        let id = match self.config.goal_definitions_with(project, text) {
            Ok(candidate) => candidate.stored.id,
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
            state: "waiting".to_string(),
            ..DefinitionProposal::default()
        };
        let message = proposal_message(project, &proposal, old.as_deref());
        proposal.message = message.clone();
        let value = button_value(root, front.proposals.len() + 1);
        let seat = self.executive_seat(project);
        self.log_at(
            &seat,
            "definition.proposed",
            json!({ "thread": root, "proposal": value, "definition": proposal.id,
                    "hash": proposal.hash, "change": proposal.change, "text": message }),
        );
        if let Some(slack) = &self.slack {
            let buttons = [
                Button {
                    action_id: APPROVE,
                    label: "Approve: put it in force and open the goal",
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
            let by = if proposal.by.is_empty() {
                String::new()
            } else {
                format!(" by <@{}>", proposal.by)
            };
            self.post(
                &channel,
                root,
                &format!(
                    "The proposed definition `{}` was already {}{by}, so that press did nothing.",
                    proposal.id, proposal.state
                ),
            );
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
