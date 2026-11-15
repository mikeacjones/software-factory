//! The accounts: one workflow that knows every account and hands them out.
//!
//! A step that is about to start a session does not choose its own account.
//! It asks here, and is told which account to run under, or to wait. This
//! workflow owns the waiting as well as the picture: when an account comes
//! back, because its limit ended or because the owner said to look again,
//! everything that was waiting is released at once. A timer in each
//! pipeline would sleep on to a reset time that is no longer true.
//!
//! What is handed out is an account's name. Its token is read by the worker
//! that runs the step, on the machine that holds it, and is never part of
//! any workflow's history.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use factory_claude::accounts::{AccountState, Choice, choose, family};
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use temporalio_macros::{workflow, workflow_methods};
use temporalio_sdk::{
    ActivityOptions, ContinueAsNewOptions, SignalWorkflowOptions, SyncWorkflowContext,
    TimerOptions, WorkflowContext, WorkflowContextView, WorkflowResult,
};

use crate::describe;
use crate::engine::Pipeline;
use crate::steps::Steps;

/// The ID of the accounts workflow. There is one in a namespace.
pub const ACCOUNTS_ID: &str = "accounts";

/// How many times the accounts are usually checked before the workflow
/// continues as a new run.
const CHECKS_PER_RUN: u32 = 40;

/// What is known about every account.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Picture {
    /// The accounts, in the order the factory's config lists them. The
    /// order breaks ties.
    pub order: Vec<String>,
    pub states: BTreeMap<String, AccountState>,
    /// How long after a limit ends an account is left alone, in seconds.
    #[serde(default)]
    pub grace_secs: u64,
}

impl Picture {
    /// The account for a run on `model`, or when the first one is back.
    fn pick(&self, model: &str, now: u64) -> Result<String, u64> {
        // An account is not used until its limit has been over for the
        // grace period: the clock is read as that much earlier.
        let then = now.saturating_sub(self.grace_secs);
        match choose(&self.order, &self.states, model, then) {
            Choice::Use(account) => Ok(account),
            Choice::Wait { until } => Err(until + self.grace_secs),
        }
    }
}

/// What the accounts workflow is started with, and what it hands to its
/// next run when it continues as new.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AccountsInput {
    /// How often to ask again what each account has left, in seconds.
    pub check_secs: u64,
    #[serde(default)]
    pub picture: Picture,
    #[serde(default)]
    pub waiting: Vec<Waiting>,
    /// When the accounts were last checked, in Unix seconds. Zero if never.
    #[serde(default)]
    pub checked_at: u64,
    /// How many checks a run makes before it continues as a new one, so
    /// that its history stays short. Zero for the usual number.
    #[serde(default)]
    pub checks_per_run: u32,
}

/// A pipeline that asks which account to run a step under.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ask {
    /// The workflow that asks. It is signalled when it may ask again.
    pub who: String,
    /// The model the step runs on: a limit can be on one model only.
    pub model: String,
}

/// The answer to an [`Ask`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Lease {
    /// Run under this account.
    Use { account: String },
    /// Every account is at a limit. The asker is signalled when one is
    /// back; `secs` is how long that is expected to take.
    Wait { secs: u64 },
}

/// A run that was stopped by a usage limit.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limit {
    pub account: String,
    pub model: String,
    /// How long until the limit ends, in seconds.
    pub secs: u64,
}

/// A pipeline waiting for an account.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Waiting {
    pub who: String,
    pub model: String,
}

/// How the accounts are to be checked.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckCall {
    /// An account heard from more recently than this is not asked about
    /// again. Zero asks about every account.
    pub max_age_secs: u64,
    /// Believe the answer over the limits runs have reported. For when the
    /// owner says the picture is wrong, as after a usage reset.
    pub forget_limits: bool,
}

/// A request for an account, as the activity that makes it is given it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountCall {
    /// The accounts workflow to ask.
    pub accounts: String,
    pub who: String,
    pub model: String,
}

/// What the accounts workflow shows anyone who asks.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Board {
    pub picture: Picture,
    pub waiting: Vec<Waiting>,
    pub checked_at: u64,
    /// Why the last check told it nothing, if it did not.
    pub trouble: Option<String>,
}

/// The accounts workflow.
#[workflow]
#[derive(Default)]
pub struct Accounts {
    picture: Picture,
    waiting: Vec<Waiting>,
    checked_at: u64,
    trouble: Option<String>,
    /// Someone asked for the accounts to be looked at again now.
    demanded: bool,
    /// Someone new is waiting: the next reset to wake for may be sooner.
    stirred: bool,
}

fn unix(time: Option<SystemTime>) -> u64 {
    time.and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

impl Accounts {
    /// Take in a fresh check. Limits that runs reported and that have not
    /// ended still stand, unless the check was told to forget them.
    fn take_in(&mut self, mut fresh: Picture, forget_limits: bool, now: u64) {
        if !forget_limits {
            for (id, known) in &self.picture.states {
                let state = fresh.states.entry(id.clone()).or_default();
                for (scope, until) in &known.blocked {
                    if *until > now {
                        state.blocked.entry(scope.clone()).or_insert(*until);
                    }
                }
            }
        }
        self.picture = fresh;
        self.checked_at = now;
        self.trouble = None;
    }

    /// What it shows someone reading it on the server.
    fn details(&self, now: u64) -> String {
        describe::accounts_details(
            &self.picture,
            &self.waiting,
            self.checked_at,
            self.trouble.as_deref(),
            now,
        )
    }

    /// Those waiting for whom there is an account now. They stop waiting.
    fn release(&mut self, now: u64) -> Vec<Waiting> {
        let picture = &self.picture;
        let (ready, still): (Vec<Waiting>, Vec<Waiting>) = std::mem::take(&mut self.waiting)
            .into_iter()
            .partition(|waiting| picture.pick(&waiting.model, now).is_ok());
        self.waiting = still;
        ready
    }

    /// When the first of those waiting could be given an account.
    fn next_release(&self, now: u64) -> Option<u64> {
        self.waiting
            .iter()
            .filter_map(|waiting| self.picture.pick(&waiting.model, now).err())
            .min()
    }
}

#[workflow_methods]
impl Accounts {
    #[run]
    pub async fn run(ctx: &mut WorkflowContext<Self>, input: AccountsInput) -> WorkflowResult<()> {
        let check_secs = input.check_secs.max(1);
        let checks_per_run = match input.checks_per_run {
            0 => CHECKS_PER_RUN,
            some => some,
        };
        // What the run before this one knew. Anything this run was told
        // before it got here is kept.
        ctx.state_mut(|state| {
            if state.picture.order.is_empty() {
                let mut picture = input.picture.clone();
                for (id, known) in std::mem::take(&mut state.picture.states) {
                    picture
                        .states
                        .entry(id)
                        .or_default()
                        .blocked
                        .extend(known.blocked);
                }
                state.picture = picture;
            }
            for waiting in &input.waiting {
                if !state.waiting.contains(waiting) {
                    state.waiting.push(waiting.clone());
                }
            }
            state.checked_at = input.checked_at;
        });
        let mut checks = 0;
        loop {
            let now = unix(ctx.workflow_time());
            let demanded = ctx.state_mut(|state| std::mem::take(&mut state.demanded));
            let checked_at = ctx.state(|state| state.checked_at);
            if demanded || checked_at == 0 || now >= checked_at + check_secs {
                let call = CheckCall {
                    max_age_secs: if demanded { 0 } else { check_secs / 2 },
                    forget_limits: demanded,
                };
                let summary = if demanded {
                    "Read what every account has left, now, because someone asked: what is found is believed over the limits runs reported"
                } else if checked_at == 0 {
                    "Read what every account has left, for the first time: none is handed out before"
                } else {
                    "Read what every account has left: the regular check"
                };
                let options =
                    ActivityOptions::with_start_to_close_timeout(Duration::from_secs(300))
                        .summary(describe::line(summary))
                        .build();
                let checked = ctx.execute_activity(Steps::accounts, call, options).await;
                let now = unix(ctx.workflow_time());
                ctx.state_mut(|state| match checked {
                    Ok(fresh) => state.take_in(fresh, demanded, now),
                    // What was known still stands. It is looked at again
                    // when the next check is due.
                    Err(err) => {
                        state.trouble = Some(err.to_string());
                        state.checked_at = now;
                    }
                });
                checks += 1;
            }
            let now = unix(ctx.workflow_time());
            let ready = ctx.state_mut(|state| {
                state.stirred = false;
                state.release(now)
            });
            let details = ctx.state(|state| state.details(now));
            ctx.set_current_details(details);
            for waiting in ready {
                let options = SignalWorkflowOptions::builder()
                    .summary(describe::line(&format!(
                        "An account is free for {} again: `{}` may ask for it",
                        waiting.model, waiting.who
                    )))
                    .build();
                // One that has ended since it asked is not there to tell.
                let _ = ctx
                    .external_workflow(waiting.who, None)
                    .signal(Pipeline::account_ready, (), options)
                    .await;
            }
            if checks >= checks_per_run && !ctx.state(|state| state.demanded || state.stirred) {
                let next = ctx.state(|state| AccountsInput {
                    check_secs,
                    picture: state.picture.clone(),
                    waiting: state.waiting.clone(),
                    checked_at: state.checked_at,
                    checks_per_run: input.checks_per_run,
                });
                ctx.continue_as_new(next, ContinueAsNewOptions::default())?;
            }
            // Asleep, with nothing running, until the next check is due,
            // a limit someone is waiting on ends, or someone speaks.
            let now = unix(ctx.workflow_time());
            let (wake, why) = ctx.state(|state| {
                let check = state.checked_at + check_secs;
                match state.next_release(now) {
                    Some(release) if release < check => (
                        release,
                        format!(
                            "a limit that {} pipeline(s) wait on ends",
                            state.waiting.len()
                        ),
                    ),
                    _ => (
                        check,
                        "the next regular check of the accounts is due".to_string(),
                    ),
                }
            });
            let sleep = wake.saturating_sub(now).max(1);
            let options = TimerOptions::builder(Duration::from_secs(sleep))
                .summary(describe::line(&format!(
                    "Nothing to do for {}: at {} {why}",
                    describe::span(sleep),
                    describe::clock(now + sleep)
                )))
                .build();
            let mut timer = ctx.timer(options).fuse();
            let mut spoken = ctx
                .wait_condition(|state| state.demanded || state.stirred)
                .fuse();
            futures::select_biased! {
                heard = spoken => heard?,
                _ = timer => {}
            }
        }
    }

    /// Which account to run under. Answers at once: with an account, or
    /// with a wait, in which case the asker is signalled when to ask again.
    #[update]
    pub fn acquire(&mut self, ctx: &mut SyncWorkflowContext<Self>, ask: Ask) -> Lease {
        let now = unix(ctx.workflow_time());
        let waiting = Waiting {
            who: ask.who,
            model: ask.model,
        };
        self.waiting.retain(|other| other.who != waiting.who);
        // Before the first check nothing is known, and nothing is handed out.
        let picked = if self.checked_at == 0 {
            Err(now)
        } else {
            self.picture.pick(&waiting.model, now)
        };
        let lease = match picked {
            Ok(account) => Lease::Use { account },
            Err(until) => {
                self.waiting.push(waiting);
                self.stirred = true;
                Lease::Wait {
                    secs: until.saturating_sub(now).max(1),
                }
            }
        };
        ctx.set_current_details(self.details(now));
        lease
    }

    /// A run was stopped by a usage limit. The account is not handed out
    /// for that model again until the limit ends.
    #[signal]
    pub fn limited(&mut self, ctx: &mut SyncWorkflowContext<Self>, limit: Limit) {
        let now = unix(ctx.workflow_time());
        self.picture
            .states
            .entry(limit.account)
            .or_default()
            .blocked
            .insert(family(&limit.model), now + limit.secs.max(1));
        ctx.set_current_details(self.details(now));
    }

    /// Look at every account again now, and believe what is found: for
    /// when the owner has changed something, such as resetting usage.
    #[signal]
    pub fn recheck(&mut self, _ctx: &mut SyncWorkflowContext<Self>, _nothing: ()) {
        self.demanded = true;
    }

    /// What is known, and who is waiting.
    #[query]
    pub fn board(&self, _ctx: &WorkflowContextView) -> Board {
        Board {
            picture: self.picture.clone(),
            waiting: self.waiting.clone(),
            checked_at: self.checked_at,
            trouble: self.trouble.clone(),
        }
    }
}
