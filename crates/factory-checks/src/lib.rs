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

fn regexes(patterns: &[String]) -> Result<Vec<regex::Regex>, CheckError> {
    patterns
        .iter()
        .map(|pattern| {
            regex::Regex::new(pattern)
                .map_err(|err| CheckError::BadGlob(pattern.clone(), err.to_string()))
        })
        .collect()
}

/// The first line of `text` that matches one of the patterns, if any does.
pub fn first_match(text: &str, patterns: &[String]) -> Result<Option<String>, CheckError> {
    let patterns = regexes(patterns)?;
    Ok(text
        .lines()
        .find(|line| patterns.iter().any(|pattern| pattern.is_match(line)))
        .map(str::to_string))
}

/// Lines a change removed that look like tests or assertions: the sign of a
/// test made easier to pass. `diff` is a unified diff with no context;
/// `patterns` say what a test line looks like in this project. Each entry is
/// `path: the removed line`. A line that was only moved or reworded shows up
/// too, which is why this is a pointer for a reviewer and not a verdict.
pub fn weakened_tests(diff: &str, patterns: &[String]) -> Result<Vec<String>, CheckError> {
    let patterns = regexes(patterns)?;
    let mut file = String::new();
    let mut removed = Vec::new();
    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("--- a/") {
            file = path.to_string();
        } else if line.starts_with("--- ") || line.starts_with("+++ ") {
            continue;
        } else if let Some(text) = line.strip_prefix('-')
            && patterns.iter().any(|pattern| pattern.is_match(text))
        {
            removed.push(format!("{file}: {}", text.trim()));
        }
    }
    Ok(removed)
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
    fn removed_test_lines_are_pointed_out() {
        let diff = "diff --git a/src/a.zig b/src/a.zig\n--- a/src/a.zig\n+++ b/src/a.zig\n@@ -10,2 +10,1 @@\n-test \"signals are not lost\" {\n-    try std.testing.expectEqual(2, seen);\n+    const seen = 2;\n@@ -40 +39 @@\n-    return x + 1;\n+    return x + 2;\n";
        let patterns = strings(&[r#"^\s*test\s+""#, r"std\.testing\.expect"]);
        assert_eq!(
            weakened_tests(diff, &patterns).unwrap(),
            [
                "src/a.zig: test \"signals are not lost\" {",
                "src/a.zig: try std.testing.expectEqual(2, seen);"
            ]
        );
        // Added test lines and changed code are not removals of tests.
        let added =
            "--- a/src/a.zig\n+++ b/src/a.zig\n@@ -1 +1,2 @@\n+test \"new\" {}\n-    return 1;\n";
        assert!(weakened_tests(added, &patterns).unwrap().is_empty());
        assert!(weakened_tests(diff, &[]).unwrap().is_empty());
        assert!(weakened_tests(diff, &strings(&["("])).is_err());
    }

    #[test]
    fn a_summary_line_that_must_never_pass_is_found() {
        let index = "fmt                exit 0\nzlint              2 error(s), 3 warning(s)\n\nFAILED: changelog-check gitleaks\n";
        let gate = strings(&[
            r"^FAILED:.*\b(ast-check|gitleaks)\b",
            r"^zlint\s+[1-9]\d* error",
        ]);
        assert_eq!(
            first_match(index, &gate).unwrap().as_deref(),
            Some("zlint              2 error(s), 3 warning(s)")
        );
        let clean = "zlint              0 error(s), 3 warning(s)\n\nFAILED: changelog-check\n";
        assert_eq!(first_match(clean, &gate).unwrap(), None);
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
