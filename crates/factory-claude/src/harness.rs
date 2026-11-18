//! The agents a step can be run by. Each is a command-line tool that works
//! headless in a directory, and each has its own command line and its own
//! output stream. A [`Harness`] is what the factory needs from one of them:
//! the command for a run, a reading of its stream into a [`Transcript`], and
//! the one way the run ended.
//!
//! A new agent is one more implementation of the trait and one more line in
//! [`harness`]. Nothing else in the factory knows which tool ran a step.

use std::path::Path;

use factory_supervise::{Spec, StreamExit};

use crate::{Outcome, RunSpec, Transcript};

/// The agent a step runs under when it names none.
pub const DEFAULT_AGENT: &str = "claude";

pub trait Harness: Sync {
    /// The name a flow's step and the `[agent.<name>]` table use.
    fn name(&self) -> &'static str;

    /// The exact command for a run. `log` is the run's stream log: a
    /// harness that has to hand its tool a file writes it beside the log.
    fn command(&self, spec: &RunSpec, log: &Path) -> std::io::Result<Spec>;

    /// What is known of a run before its first line.
    fn begin(&self, _spec: &RunSpec, _transcript: &mut Transcript) {}

    /// Read one line of the run's stream.
    fn observe(&self, transcript: &mut Transcript, line: &str);

    /// Called once when the stream has ended, before the run is classified.
    fn settle(&self, _spec: &RunSpec, _log: &Path, _transcript: &mut Transcript) {}

    /// Decide how the run ended.
    fn classify(&self, transcript: &Transcript, exit: &StreamExit) -> Outcome;

    /// Whether runs are spread over the factory's Claude accounts, which
    /// also means their usage limits are waited out by the factory.
    fn uses_accounts(&self) -> bool {
        false
    }
}

/// The harness with this name, if the factory has one.
pub fn harness(name: &str) -> Option<&'static dyn Harness> {
    static ALL: [&dyn Harness; 2] = [&Claude, &crate::codex::Codex];
    ALL.iter().copied().find(|harness| harness.name() == name)
}

/// The names of every harness the factory has.
pub fn harness_names() -> Vec<&'static str> {
    vec![Claude.name(), crate::codex::Codex.name()]
}

/// Claude Code.
pub struct Claude;

impl Harness for Claude {
    fn name(&self) -> &'static str {
        DEFAULT_AGENT
    }

    fn command(&self, spec: &RunSpec, _log: &Path) -> std::io::Result<Spec> {
        Ok(crate::command(spec))
    }

    fn observe(&self, transcript: &mut Transcript, line: &str) {
        transcript.observe(line);
    }

    fn classify(&self, transcript: &Transcript, exit: &StreamExit) -> Outcome {
        crate::classify(transcript, exit)
    }

    fn uses_accounts(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_harness_is_found_by_the_name_a_step_gives_and_no_other() {
        assert_eq!(harness("claude").unwrap().name(), "claude");
        assert_eq!(harness("codex").unwrap().name(), "codex");
        assert!(harness("cursor").is_none());
        assert!(harness("claude").unwrap().uses_accounts());
        assert!(!harness("codex").unwrap().uses_accounts());
        assert_eq!(harness_names(), ["claude", "codex"]);
    }
}
