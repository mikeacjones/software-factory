//! Deterministic checks on a worker's output: the project's own commands, and
//! each step's changes against the paths its flow lets it write.

use std::path::Path;
use std::time::Duration;

use factory_supervise::{Spec, capture};
use globset::{Glob, GlobSetBuilder};

#[derive(Debug, thiserror::Error)]
pub enum CheckError {
    #[error("`{0}` is not a path pattern: {1}")]
    BadGlob(String, String),
}

/// How a project command ended. "It failed" and "it could not run" are
/// different facts: the first is about the code, the second about the host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Passed,
    Failed { code: Option<i32> },
    TimedOut,
    CouldNotRun(String),
}

#[derive(Clone, Debug)]
pub struct CommandResult {
    pub verdict: Verdict,
    /// stdout then stderr, in full.
    pub output: String,
    pub duration: Duration,
}

/// Run a named project command in a worktree.
pub fn run_command(argv: &[String], cwd: &Path, timeout: Duration) -> CommandResult {
    let spec = Spec::new(argv.iter().cloned(), cwd, timeout);
    match capture(&spec) {
        Err(err) => CommandResult {
            verdict: Verdict::CouldNotRun(err.to_string()),
            output: String::new(),
            duration: Duration::ZERO,
        },
        Ok(out) => {
            let verdict = if out.timed_out {
                Verdict::TimedOut
            } else if out.code == Some(0) {
                Verdict::Passed
            } else if matches!(out.code, Some(126 | 127)) {
                // The shell's codes for "not executable" and "not found".
                Verdict::CouldNotRun(format!(
                    "`{}` exited {}: a tool it needs is missing",
                    spec.display(),
                    out.code.unwrap_or(0)
                ))
            } else {
                Verdict::Failed { code: out.code }
            };
            CommandResult {
                verdict,
                output: out.combined(),
                duration: out.duration,
            }
        }
    }
}

/// A changed file a step was not allowed to change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    pub path: String,
    pub reason: String,
}

fn globs(patterns: &[String]) -> Result<globset::GlobSet, CheckError> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern)
            .map_err(|err| CheckError::BadGlob(pattern.clone(), err.to_string()))?;
        builder.add(glob);
    }
    builder
        .build()
        .map_err(|err| CheckError::BadGlob(patterns.join(", "), err.to_string()))
}

/// Compare the files a step changed with what it may write. `allowed` is the
/// union of the step's path scopes; a step with none may change nothing.
/// `forbidden` applies to every step.
pub fn scope_violations(
    changed: &[String],
    allowed: &[String],
    forbidden: &[String],
) -> Result<Vec<Violation>, CheckError> {
    let allowed_set = globs(allowed)?;
    let forbidden_set = globs(forbidden)?;
    let mut violations = Vec::new();
    for path in changed {
        if forbidden_set.is_match(path) {
            violations.push(Violation {
                path: path.clone(),
                reason: "no step may change this path".to_string(),
            });
        } else if allowed.is_empty() {
            violations.push(Violation {
                path: path.clone(),
                reason: "this step may not change any file".to_string(),
            });
        } else if !allowed_set.is_match(path) {
            violations.push(Violation {
                path: path.clone(),
                reason: "outside the paths this step may write".to_string(),
            });
        }
    }
    Ok(violations)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn sh(script: &str, timeout_ms: u64) -> CommandResult {
        run_command(
            &strings(&["sh", "-c", script]),
            &std::env::temp_dir(),
            Duration::from_millis(timeout_ms),
        )
    }

    #[test]
    fn a_command_passes_fails_times_out_or_cannot_run() {
        assert_eq!(sh("echo fine", 5000).verdict, Verdict::Passed);
        let failed = sh("echo broken >&2; exit 1", 5000);
        assert_eq!(failed.verdict, Verdict::Failed { code: Some(1) });
        assert_eq!(failed.output, "broken\n");
        assert_eq!(sh("sleep 30", 200).verdict, Verdict::TimedOut);
        assert!(matches!(
            sh("no-such-tool-anywhere", 5000).verdict,
            Verdict::CouldNotRun(_)
        ));
        let missing = run_command(
            &strings(&["no-such-tool-anywhere"]),
            &std::env::temp_dir(),
            Duration::from_secs(5),
        );
        assert!(matches!(missing.verdict, Verdict::CouldNotRun(_)));
    }

    #[test]
    fn changes_inside_the_scopes_are_fine() {
        let changed = strings(&["src/a.zig", "src/deep/b.zig", "README.md"]);
        let found = scope_violations(
            &changed,
            &strings(&["src/**", "README.md"]),
            &strings(&[".github/**"]),
        )
        .unwrap();
        assert_eq!(found, []);
    }

    #[test]
    fn changes_outside_the_scopes_are_reported() {
        let changed = strings(&["src/a.zig", "build.zig", ".github/workflows/ci.yml"]);
        let found =
            scope_violations(&changed, &strings(&["src/**"]), &strings(&[".github/**"])).unwrap();
        assert_eq!(
            found,
            [
                Violation {
                    path: "build.zig".to_string(),
                    reason: "outside the paths this step may write".to_string()
                },
                Violation {
                    path: ".github/workflows/ci.yml".to_string(),
                    reason: "no step may change this path".to_string()
                },
            ]
        );
    }

    #[test]
    fn a_step_with_no_scopes_may_change_nothing() {
        let found = scope_violations(&strings(&["src/a.zig"]), &[], &[]).unwrap();
        assert_eq!(found[0].reason, "this step may not change any file");
    }

    #[test]
    fn forbidden_wins_over_allowed() {
        let found = scope_violations(
            &strings(&[".beads/metadata.json"]),
            &strings(&["**"]),
            &strings(&[".beads/**"]),
        )
        .unwrap();
        assert_eq!(found.len(), 1);
    }
}
