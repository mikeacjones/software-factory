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
    run_command_with_grace(argv, cwd, timeout, factory_supervise::DEFAULT_GRACE)
}

/// [`run_command`], giving a command that reaches its timeout `grace` after
/// SIGTERM to stop what it started and print what it has.
pub fn run_command_with_grace(
    argv: &[String],
    cwd: &Path,
    timeout: Duration,
    grace: Duration,
) -> CommandResult {
    let mut spec = Spec::new(argv.iter().cloned(), cwd, timeout);
    spec.grace = grace;
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

/// Why a run that failed or timed out was the host's fault, by the project's
/// own word: the first output line that matches one of `patterns`, or the
/// exit code when it is one of `exits`. None when nothing says so.
pub fn host_fault(
    output: &str,
    code: Option<i32>,
    patterns: &[String],
    exits: &[i32],
) -> Result<Option<String>, CheckError> {
    if let Some(line) = first_match(output, patterns)? {
        return Ok(Some(format!("its output says so: `{}`", line.trim())));
    }
    Ok(code
        .filter(|code| exits.contains(code))
        .map(|code| format!("it exited {code}, which the project names as the host's fault")))
}

/// A failure, made comparable with another: the lines `patterns` pick out
/// (the names of what failed), or the last lines of the output where none
/// match, with what changes from run to run (numbers, hex, absolute paths,
/// spacing) taken out, without duplicates and in order.
pub fn failure_signature(output: &str, patterns: &[String]) -> Result<String, CheckError> {
    let compiled = regexes(patterns)?;
    let picked: Vec<&str> = output
        .lines()
        .filter(|line| compiled.iter().any(|pattern| pattern.is_match(line)))
        .collect();
    let lines: Vec<&str> = if picked.is_empty() {
        let meaningful: Vec<&str> = output.lines().filter(|l| !l.trim().is_empty()).collect();
        meaningful[meaningful.len().saturating_sub(20)..].to_vec()
    } else {
        picked
    };
    let mut normal: Vec<String> = lines.iter().map(|line| normalize(line)).collect();
    normal.sort();
    normal.dedup();
    Ok(normal.join("\n"))
}

fn normalize(line: &str) -> String {
    line.split_whitespace()
        .map(|word| {
            let bare = word.trim_start_matches("0x");
            if word.starts_with('/') || word.contains("/var/") || word.contains("/tmp/") {
                "<path>".to_string()
            } else if bare.len() >= 7
                && bare.chars().all(|c| c.is_ascii_hexdigit())
                && bare.chars().any(|c| c.is_ascii_digit())
            {
                // An ID, a hash or an address.
                "<hex>".to_string()
            } else {
                let mut out = String::new();
                for ch in word.chars() {
                    if !ch.is_ascii_digit() {
                        out.push(ch);
                    } else if !out.ends_with('#') {
                        out.push('#');
                    }
                }
                out
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Debug, thiserror::Error)]
pub enum BaseCopyError {
    #[error("the change touches the submodule `{0}`, which a copy cannot be given back")]
    Submodule(String),
    #[error("could not copy {from} to {to}: {detail}")]
    Copy {
        from: String,
        to: String,
        detail: String,
    },
    #[error(transparent)]
    Git(#[from] factory_git::GitError),
    #[error("{path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
}

/// What a copy at the base was given back.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BaseCopy {
    /// Files written as they are at the base.
    pub restored: Vec<String>,
    /// Files the change added, taken out.
    pub removed: Vec<String>,
}

/// Make `dest` a copy of `worktree` (its build caches included, by a clone
/// where the file system has them, and without its link to git) with every
/// file the change touched since `base` put back as it is at `base`: the
/// code as it was, in the same place with the same tools. The worktree is
/// not touched. A change to a submodule cannot be put back, and is an error.
pub fn copy_at_base(worktree: &Path, base: &str, dest: &Path) -> Result<BaseCopy, BaseCopyError> {
    let changed = factory_git::changed_since(worktree, base)?;
    if let Some(sub) = changed.iter().find(|c| c.submodule) {
        return Err(BaseCopyError::Submodule(sub.path.clone()));
    }
    clone_tree(worktree, dest)?;
    let link = dest.join(".git");
    let removed_link = if link.is_dir() {
        std::fs::remove_dir_all(&link)
    } else {
        std::fs::remove_file(&link)
    };
    if let Err(source) = removed_link
        && source.kind() != std::io::ErrorKind::NotFound
    {
        return Err(BaseCopyError::Io {
            path: link.display().to_string(),
            source,
        });
    }
    let mut copy = BaseCopy::default();
    for change in &changed {
        if change.added {
            let path = dest.join(&change.path);
            match std::fs::remove_file(&path) {
                Ok(()) => copy.removed.push(change.path.clone()),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(BaseCopyError::Io {
                        path: path.display().to_string(),
                        source,
                    });
                }
            }
        } else {
            copy.restored.push(change.path.clone());
        }
    }
    let index = dest.with_extension("index");
    let written = factory_git::write_paths_at(worktree, dest, base, &copy.restored, &index);
    let _ = std::fs::remove_file(&index);
    written?;
    Ok(copy)
}

/// `cp -c -R` on macOS (APFS clones: a copy of a large build cache costs
/// nothing), `cp -R --reflink=auto` elsewhere, and a plain copy where
/// neither works.
fn clone_tree(from: &Path, to: &Path) -> Result<(), BaseCopyError> {
    let attempt = |args: &[&str]| -> Result<bool, BaseCopyError> {
        let _ = std::fs::remove_dir_all(to);
        let mut argv: Vec<String> = vec!["cp".to_string()];
        argv.extend(args.iter().map(|a| a.to_string()));
        argv.push(from.display().to_string());
        argv.push(to.display().to_string());
        let spec = Spec::new(argv, from, Duration::from_secs(1800));
        let out = capture(&spec).map_err(|err| BaseCopyError::Copy {
            from: from.display().to_string(),
            to: to.display().to_string(),
            detail: err.to_string(),
        })?;
        Ok(out.success())
    };
    let clone: &[&str] = if cfg!(target_os = "macos") {
        &["-c", "-R"]
    } else {
        &["-R", "--reflink=auto"]
    };
    if attempt(clone)? || attempt(&["-R"])? {
        return Ok(());
    }
    Err(BaseCopyError::Copy {
        from: from.display().to_string(),
        to: to.display().to_string(),
        detail: "cp failed".to_string(),
    })
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
    fn the_host_is_named_at_fault_by_a_line_or_an_exit_code_the_project_gives() {
        let patterns = strings(&["invalid ephemeral server download", "^docker: .*daemon"]);
        let output = "test 5/6...\n[temporal] (err): starting the dev server failed: Connection failed: invalid ephemeral server download: could not extract tar archive entry\nFAIL\n";
        let found = host_fault(output, Some(1), &patterns, &[])
            .unwrap()
            .unwrap();
        assert!(
            found.contains("invalid ephemeral server download"),
            "{found}"
        );
        assert!(
            host_fault("Debug: COULD NOT RUN", Some(2), &[], &[2])
                .unwrap()
                .unwrap()
                .contains("exited 2")
        );
        // A test that fails is the code's until something says otherwise.
        assert_eq!(
            host_fault("error: 'a test' failed", Some(1), &patterns, &[2]).unwrap(),
            None
        );
        assert!(host_fault("x", None, &strings(&["("]), &[]).is_err());
    }

    #[test]
    fn a_failure_is_the_same_failure_across_runs_and_another_when_something_else_fails() {
        let lines = strings(&[r"^error: '.*' (failed|timed out)"]);
        let first = "seed 0x7af66445\nerror: 'testing.test.update-with-start' timed out after 3m101.262ms\n/var/folders/qs/T/test-linux.xPU1L2/Debug.log\nBuild Summary: 12/14 steps succeeded\n";
        let again = "seed 0x3da27918\nerror: 'testing.test.update-with-start' timed out after 3m57.821ms\n/var/folders/qs/T/test-linux.IMhQju/Debug.log\nBuild Summary: 12/14 steps succeeded\n";
        let other = "seed 0x3da27918\nerror: 'client.test.reset' failed:\nBuild Summary: 12/14 steps succeeded\n";
        let sig = |text| failure_signature(text, &lines).unwrap();
        assert_eq!(sig(first), sig(again));
        assert_ne!(sig(first), sig(other));
        // Without failure lines the last lines stand for it, with the parts
        // that differ between runs taken out.
        let tail = |text| failure_signature(text, &[]).unwrap();
        assert_eq!(tail(first), tail(again));
        assert_ne!(tail(first), tail(other));
    }

    #[test]
    fn a_copy_at_the_base_holds_the_code_as_it_was_and_leaves_the_worktree_alone() {
        let root = std::env::temp_dir().join(format!("factory-checks-base-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let work = root.join("work");
        std::fs::create_dir_all(work.join("cache")).unwrap();
        let git = |args: &[&str]| {
            let out = capture(&Spec::new(
                std::iter::once("git").chain(args.iter().copied()),
                &work,
                Duration::from_secs(30),
            ))
            .unwrap();
            assert!(out.success(), "git {args:?}: {}", out.combined());
            out.stdout
        };
        git(&["init", "--quiet", "-b", "main"]);
        git(&["config", "user.name", "Test"]);
        git(&["config", "user.email", "test@localhost"]);
        git(&["config", "commit.gpgsign", "false"]);
        std::fs::write(work.join(".gitignore"), "cache/\n").unwrap();
        std::fs::write(work.join("lib.txt"), "base\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "base"]);
        let base = git(&["rev-parse", "HEAD"]).trim().to_string();
        std::fs::write(work.join("lib.txt"), "change\n").unwrap();
        std::fs::write(work.join("test.txt"), "a test the change adds\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "change"]);
        // A build cache git does not track, which the copy keeps.
        std::fs::write(work.join("cache").join("built"), "cached\n").unwrap();

        let dest = root.join("at-base");
        let copy = copy_at_base(&work, &base, &dest).unwrap();
        assert_eq!(copy.restored, ["lib.txt"]);
        assert_eq!(copy.removed, ["test.txt"]);
        assert_eq!(
            std::fs::read_to_string(dest.join("lib.txt")).unwrap(),
            "base\n"
        );
        assert!(!dest.join("test.txt").exists());
        assert_eq!(
            std::fs::read_to_string(dest.join("cache/built")).unwrap(),
            "cached\n"
        );
        assert!(!dest.join(".git").exists());
        assert_eq!(
            std::fs::read_to_string(work.join("lib.txt")).unwrap(),
            "change\n"
        );
        assert_eq!(git(&["status", "--porcelain"]), "");
        std::fs::remove_dir_all(&root).unwrap();
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
