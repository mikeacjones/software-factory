//! The factory's own GitHub writes: opening and updating a pull request with
//! `gh`. Every write names its repository, and a write to a repository that
//! is not in the project's write list cannot be made through this crate.
//!
//! That list binds the factory's code. What an agent can reach by hand is set
//! by the credential stored on the machine.

use std::path::Path;
use std::time::Duration;

use factory_supervise::{Spec, capture};
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum GitHubError {
    #[error("refused: `{0}` is not in the project's write list")]
    NotAllowed(String),
    #[error(transparent)]
    Run(#[from] factory_supervise::Error),
    #[error("`{command}` failed: {detail}")]
    Failed { command: String, detail: String },
    #[error("could not read the answer of `{command}`: {detail}")]
    Answer { command: String, detail: String },
}

const TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequest {
    pub number: u64,
    pub url: String,
    /// False if an open pull request for the branch already existed.
    pub created: bool,
}

#[derive(Clone, Debug)]
pub struct GitHub {
    /// The `gh` binary. Tests point this at a stand-in.
    pub gh_bin: String,
    write: Vec<String>,
}

impl GitHub {
    pub fn new(write: Vec<String>) -> Self {
        Self {
            gh_bin: "gh".to_string(),
            write,
        }
    }

    /// Every write goes through here first.
    pub fn check_allowed(&self, repo: &str) -> Result<(), GitHubError> {
        if self.write.iter().any(|allowed| allowed == repo) {
            Ok(())
        } else {
            Err(GitHubError::NotAllowed(repo.to_string()))
        }
    }

    fn gh(&self, cwd: &Path, args: &[&str], stdin: Option<&str>) -> Result<String, GitHubError> {
        let argv: Vec<&str> = std::iter::once(self.gh_bin.as_str())
            .chain(args.iter().copied())
            .collect();
        let mut spec = Spec::new(argv, cwd, TIMEOUT);
        spec.stdin = stdin.map(str::to_string);
        spec.env = vec![
            ("GH_PROMPT_DISABLED".to_string(), "1".to_string()),
            ("NO_COLOR".to_string(), "1".to_string()),
        ];
        let out = capture(&spec)?;
        if !out.success() {
            return Err(GitHubError::Failed {
                command: format!("gh {}", args.join(" ")),
                detail: out.combined().trim().to_string(),
            });
        }
        Ok(out.stdout)
    }

    /// The open pull request for a branch, if there is one.
    pub fn open_pull_request(
        &self,
        cwd: &Path,
        repo: &str,
        head: &str,
    ) -> Result<Option<(u64, String)>, GitHubError> {
        let args = [
            "pr",
            "list",
            "--repo",
            repo,
            "--head",
            head,
            "--state",
            "open",
            "--json",
            "number,url",
        ];
        let out = self.gh(cwd, &args, None)?;
        let list: Value = serde_json::from_str(out.trim()).map_err(|err| GitHubError::Answer {
            command: format!("gh {}", args.join(" ")),
            detail: err.to_string(),
        })?;
        Ok(list
            .as_array()
            .and_then(|list| list.first())
            .and_then(|pr| {
                Some((
                    pr.get("number")?.as_u64()?,
                    pr.get("url")?.as_str()?.to_string(),
                ))
            }))
    }

    /// Open a pull request for a branch, or bring the one already open up to
    /// date. Doing this twice yields one pull request.
    pub fn publish_pull_request(
        &self,
        cwd: &Path,
        repo: &str,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
    ) -> Result<PullRequest, GitHubError> {
        self.check_allowed(repo)?;
        if let Some((number, url)) = self.open_pull_request(cwd, repo, head)? {
            let number_text = number.to_string();
            self.gh(
                cwd,
                &[
                    "pr",
                    "edit",
                    &number_text,
                    "--repo",
                    repo,
                    "--title",
                    title,
                    "--body-file",
                    "-",
                ],
                Some(body),
            )?;
            return Ok(PullRequest {
                number,
                url,
                created: false,
            });
        }
        let args = [
            "pr",
            "create",
            "--repo",
            repo,
            "--head",
            head,
            "--base",
            base,
            "--title",
            title,
            "--body-file",
            "-",
        ];
        let out = self.gh(cwd, &args, Some(body))?;
        let url = out
            .lines()
            .rev()
            .find(|line| line.contains("/pull/"))
            .map(|line| line.trim().to_string())
            .ok_or_else(|| GitHubError::Answer {
                command: format!("gh {}", args.join(" ")),
                detail: format!("no pull request URL in `{}`", out.trim()),
            })?;
        let number = url
            .rsplit('/')
            .next()
            .and_then(|last| last.parse().ok())
            .unwrap_or(0);
        Ok(PullRequest {
            number,
            url,
            created: true,
        })
    }

    /// GitHub's own record of a pull request: its state, its head commit and
    /// the checks on that commit.
    pub fn pull_request_status(
        &self,
        cwd: &Path,
        repo: &str,
        number: u64,
    ) -> Result<PullRequestStatus, GitHubError> {
        let number = number.to_string();
        let args = [
            "pr",
            "view",
            &number,
            "--repo",
            repo,
            "--json",
            "state,headRefOid,mergeable,mergeCommit,statusCheckRollup",
        ];
        let out = self.gh(cwd, &args, None)?;
        let answer: Value =
            serde_json::from_str(out.trim()).map_err(|err| GitHubError::Answer {
                command: format!("gh {}", args.join(" ")),
                detail: err.to_string(),
            })?;
        let text = |key: &str| {
            answer
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        Ok(PullRequestStatus {
            state: text("state"),
            head: text("headRefOid"),
            mergeable: text("mergeable"),
            merge_commit: answer
                .pointer("/mergeCommit/oid")
                .and_then(Value::as_str)
                .map(str::to_string),
            checks: answer
                .get("statusCheckRollup")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .map(check_of)
                .collect(),
        })
    }

    /// The failing part of a check's log, if GitHub will give it.
    pub fn failed_log(&self, cwd: &Path, repo: &str, check: &Check) -> Option<String> {
        // .../actions/runs/<run>/job/<job>
        let url = check.url.as_deref()?;
        let run = url.split("/actions/runs/").nth(1)?.split('/').next()?;
        if run.is_empty() || !run.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        self.gh(
            cwd,
            &["run", "view", run, "--repo", repo, "--log-failed"],
            None,
        )
        .ok()
        .filter(|log| !log.trim().is_empty())
    }

    /// Merge a pull request with a merge commit, but only if its head is
    /// still `head`: nothing that the factory did not publish is merged.
    pub fn merge(
        &self,
        cwd: &Path,
        repo: &str,
        number: u64,
        head: &str,
        subject: Option<&str>,
    ) -> Result<(), GitHubError> {
        self.check_allowed(repo)?;
        let number = number.to_string();
        let mut args = vec![
            "pr",
            "merge",
            &number,
            "--repo",
            repo,
            "--merge",
            "--match-head-commit",
            head,
        ];
        if let Some(subject) = subject {
            args.extend(["--subject", subject]);
        }
        self.gh(cwd, &args, None).map(|_| ())
    }

    /// Whether `gh` is logged in at all.
    pub fn authenticated(&self, cwd: &Path) -> bool {
        self.gh(cwd, &["auth", "status"], None).is_ok()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckState {
    Pending,
    Passed,
    Failed,
}

/// One check on a pull request's head commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub state: CheckState,
    pub url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequestStatus {
    /// `OPEN`, `MERGED` or `CLOSED`.
    pub state: String,
    /// The commit the pull request's branch is at.
    pub head: String,
    /// `MERGEABLE`, `CONFLICTING` or `UNKNOWN`.
    pub mergeable: String,
    pub merge_commit: Option<String>,
    pub checks: Vec<Check>,
}

/// Read one entry of GitHub's check rollup: a check run or a commit status.
fn check_of(entry: &Value) -> Check {
    let text = |key: &str| entry.get(key).and_then(Value::as_str).unwrap_or("");
    let is_status = text("__typename") == "StatusContext" || entry.get("context").is_some();
    let (name, state, url) = if is_status {
        let state = match text("state") {
            "SUCCESS" => CheckState::Passed,
            "FAILURE" | "ERROR" => CheckState::Failed,
            _ => CheckState::Pending,
        };
        (text("context"), state, text("targetUrl"))
    } else {
        let state = if text("status") != "COMPLETED" {
            CheckState::Pending
        } else {
            match text("conclusion") {
                "SUCCESS" | "NEUTRAL" | "SKIPPED" => CheckState::Passed,
                // Cancelled, timed out, action required, or anything new:
                // not a pass, so not something to merge on.
                _ => CheckState::Failed,
            }
        };
        (text("name"), state, text("detailsUrl"))
    };
    Check {
        name: name.to_string(),
        state,
        url: (!url.is_empty()).then(|| url.to_string()),
    }
}

/// What a pull request's checks say, taken together.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CiVerdict {
    /// Nothing has failed, and something has not finished or not appeared yet.
    Pending {
        passed: usize,
        total: usize,
        waiting: Vec<String>,
    },
    /// Every check has passed, and every required check is among them.
    Green { passed: usize },
    /// At least one check has failed. The others no longer matter.
    Red { failed: Vec<Check> },
}

/// Judge the checks. One failure is red at once. Green needs every check that
/// exists to have passed and every name in `required` to be among them, so a
/// check that has not been created yet cannot be mistaken for a pass.
pub fn ci_verdict(checks: &[Check], required: &[String]) -> CiVerdict {
    let failed: Vec<Check> = checks
        .iter()
        .filter(|check| check.state == CheckState::Failed)
        .cloned()
        .collect();
    if !failed.is_empty() {
        return CiVerdict::Red { failed };
    }
    let passed = checks
        .iter()
        .filter(|check| check.state == CheckState::Passed)
        .count();
    let mut waiting: Vec<String> = checks
        .iter()
        .filter(|check| check.state == CheckState::Pending)
        .map(|check| check.name.clone())
        .collect();
    for name in required {
        if !checks.iter().any(|check| &check.name == name) {
            waiting.push(format!("{name} (not started)"));
        }
    }
    if checks.is_empty() {
        waiting.push("no checks have appeared yet".to_string());
    }
    if waiting.is_empty() {
        CiVerdict::Green { passed }
    } else {
        CiVerdict::Pending {
            passed,
            total: checks.len().max(required.len()),
            waiting,
        }
    }
}

/// Text the factory would publish under the owner's name: a commit message, a
/// pull-request title or body.
///
/// Returns why it may not be published. A mention notifies a person, and a
/// reference to another repository adds a back-link there: both are actions
/// outside the project that the owner has not authorized.
pub fn publication_problems(text: &str, own_repo: Option<&str>, max_chars: usize) -> Vec<String> {
    let mut problems = Vec::new();
    let length = text.chars().count();
    if text.trim().is_empty() {
        problems.push("it is empty".to_string());
    }
    if length > max_chars {
        problems.push(format!(
            "it has {length} characters; the limit is {max_chars}"
        ));
    }
    for marker in [
        "ghp_",
        "github_pat_",
        "gho_",
        "xoxb-",
        "xoxp-",
        "xapp-",
        "sk-ant-",
        "-----BEGIN",
    ] {
        if text.contains(marker) {
            problems.push(format!(
                "it contains what looks like a secret (`{marker}…`)"
            ));
        }
    }

    let prose = without_code(text);
    let chars: Vec<char> = prose.chars().collect();
    for (index, c) in chars.iter().enumerate() {
        let before = index.checked_sub(1).map(|i| chars[i]);
        let after = chars.get(index + 1);
        if *c == '@'
            && after.is_some_and(|next| next.is_ascii_alphanumeric())
            && !before.is_some_and(|prev| prev.is_ascii_alphanumeric() || prev == '.')
        {
            let name: String = chars[index + 1..]
                .iter()
                .take_while(|c| c.is_ascii_alphanumeric() || **c == '-')
                .collect();
            problems.push(format!(
                "it mentions `@{name}`, which would notify that account"
            ));
        }
    }

    for token in prose.split(|c: char| c.is_whitespace() || "()[]<>,;\"'".contains(c)) {
        // owner/name#123
        if let Some((slug, number)) = token.split_once('#')
            && slug.split('/').count() == 2
            && !slug.starts_with('/')
            && number.chars().next().is_some_and(|c| c.is_ascii_digit())
            && Some(slug) != own_repo
        {
            problems.push(format!("it refers to `{slug}#…`, in another repository"));
        }
        // https://github.com/owner/name/issues/123
        if let Some(rest) = token.split("github.com/").nth(1) {
            let parts: Vec<&str> = rest.split('/').collect();
            if parts.len() >= 4 && matches!(parts[2], "issues" | "pull" | "discussions") {
                let slug = format!("{}/{}", parts[0], parts[1]);
                if Some(slug.as_str()) != own_repo {
                    problems.push(format!(
                        "it links to an issue or pull request in `{slug}`, another repository"
                    ));
                }
            }
        }
    }
    problems.dedup();
    problems
}

/// The text with fenced code blocks and inline code spans blanked out: GitHub
/// does not act on mentions or references inside code.
fn without_code(text: &str) -> String {
    let mut out = String::new();
    let mut in_fence = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            out.push('\n');
            continue;
        }
        if in_fence {
            out.push('\n');
            continue;
        }
        let mut in_span = false;
        for c in line.chars() {
            if c == '`' {
                in_span = !in_span;
                out.push(' ');
            } else if in_span {
                out.push(' ');
            } else {
                out.push(c);
            }
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWN: Option<&str> = Some("me/toy");

    #[test]
    fn ordinary_text_may_be_published() {
        let text = "Add retries to the client\n\nFixes #12 and me/toy#13. Run `zig build test`.\nSee https://github.com/me/toy/pull/14 and https://example.com/x.\nReach me at dev@example.com.";
        assert_eq!(publication_problems(text, OWN, 2000), Vec::<String>::new());
    }

    #[test]
    fn a_mention_is_refused_unless_it_is_code() {
        assert_eq!(
            publication_problems("thanks @octocat for the idea", OWN, 2000),
            ["it mentions `@octocat`, which would notify that account"]
        );
        let code = "use `@import(\"std\")` here\n```\n@panic(\"x\")\n```\n";
        assert_eq!(publication_problems(code, OWN, 2000), Vec::<String>::new());
    }

    #[test]
    fn a_reference_to_another_repository_is_refused() {
        assert_eq!(
            publication_problems("same as other/repo#7", OWN, 2000),
            ["it refers to `other/repo#…`, in another repository"]
        );
        assert_eq!(
            publication_problems("see https://github.com/other/repo/issues/7", OWN, 2000),
            ["it links to an issue or pull request in `other/repo`, another repository"]
        );
    }

    #[test]
    fn secrets_size_and_emptiness_are_refused() {
        assert_eq!(publication_problems("  ", OWN, 10), ["it is empty"]);
        assert_eq!(
            publication_problems("abcdefghijk", OWN, 10),
            ["it has 11 characters; the limit is 10"]
        );
        assert!(
            publication_problems("token ghp_abc123", OWN, 100)[0].contains("looks like a secret")
        );
    }

    fn check(name: &str, state: CheckState) -> Check {
        Check {
            name: name.to_string(),
            state,
            url: None,
        }
    }

    #[test]
    fn one_failed_check_is_red_at_once_whatever_else_is_pending() {
        let checks = [
            check("fmt", CheckState::Passed),
            check("unit", CheckState::Failed),
            check("samples", CheckState::Pending),
        ];
        assert_eq!(
            ci_verdict(&checks, &[]),
            CiVerdict::Red {
                failed: vec![check("unit", CheckState::Failed)]
            }
        );
    }

    #[test]
    fn green_needs_every_check_passed_and_every_required_check_present() {
        let two = [
            check("fmt", CheckState::Passed),
            check("unit", CheckState::Passed),
        ];
        assert_eq!(ci_verdict(&two, &[]), CiVerdict::Green { passed: 2 });

        // A workflow that has not started yet has created no check: without
        // the required list this would look green.
        let required = ["fmt".to_string(), "unit".to_string(), "samples".to_string()];
        assert_eq!(
            ci_verdict(&two, &required),
            CiVerdict::Pending {
                passed: 2,
                total: 3,
                waiting: vec!["samples (not started)".to_string()]
            }
        );
        let pending = [
            check("fmt", CheckState::Passed),
            check("unit", CheckState::Pending),
        ];
        assert!(matches!(
            ci_verdict(&pending, &[]),
            CiVerdict::Pending { passed: 1, .. }
        ));
        // No checks at all is never green.
        assert!(matches!(ci_verdict(&[], &[]), CiVerdict::Pending { .. }));
    }

    #[test]
    fn check_runs_and_commit_statuses_are_both_read() {
        let run = |status: &str, conclusion: &str| {
            check_of(&serde_json::json!({
                "__typename": "CheckRun", "name": "unit", "status": status,
                "conclusion": conclusion, "detailsUrl": "https://github.com/me/toy/actions/runs/55/job/66"
            }))
            .state
        };
        assert_eq!(run("IN_PROGRESS", ""), CheckState::Pending);
        assert_eq!(run("QUEUED", ""), CheckState::Pending);
        assert_eq!(run("COMPLETED", "SUCCESS"), CheckState::Passed);
        assert_eq!(run("COMPLETED", "SKIPPED"), CheckState::Passed);
        for conclusion in [
            "FAILURE",
            "CANCELLED",
            "TIMED_OUT",
            "ACTION_REQUIRED",
            "SOMETHING_NEW",
        ] {
            assert_eq!(
                run("COMPLETED", conclusion),
                CheckState::Failed,
                "{conclusion}"
            );
        }
        let status = |state: &str| {
            check_of(
                &serde_json::json!({"__typename": "StatusContext", "context": "ci/x", "state": state}),
            )
        };
        assert_eq!(status("SUCCESS").state, CheckState::Passed);
        assert_eq!(status("PENDING").state, CheckState::Pending);
        assert_eq!(status("ERROR").state, CheckState::Failed);
        assert_eq!(status("SUCCESS").name, "ci/x");
    }

    #[test]
    fn a_merge_outside_the_write_list_is_refused() {
        let mut github = GitHub::new(vec!["me/toy".to_string()]);
        github.gh_bin = "no-such-gh-anywhere".to_string();
        let result = github.merge(&std::env::temp_dir(), "someone/else", 7, "abc", None);
        assert!(matches!(result, Err(GitHubError::NotAllowed(_))));
    }

    #[test]
    fn a_repository_outside_the_write_list_is_refused_before_anything_runs() {
        // A `gh` that does not exist: reaching it would be a different error.
        let mut github = GitHub::new(vec!["me/toy".to_string()]);
        github.gh_bin = "no-such-gh-anywhere".to_string();
        let result = github.publish_pull_request(
            &std::env::temp_dir(),
            "someone/else",
            "feat/x",
            "main",
            "t",
            "b",
        );
        assert!(matches!(result, Err(GitHubError::NotAllowed(repo)) if repo == "someone/else"));
    }

    /// A stand-in `gh` that keeps pull requests in a file.
    #[test]
    fn publishing_twice_yields_one_pull_request() {
        let dir = std::env::temp_dir().join(format!("factory-github-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("gh");
        std::fs::write(
            &script,
            format!(
                r#"#!/bin/sh
state="{}/prs"
case "$1 $2" in
  "pr list") if [ -f "$state" ]; then echo '[{{"number":7,"url":"https://github.com/me/toy/pull/7"}}]'; else echo '[]'; fi ;;
  "pr create") cat > /dev/null; echo created >> "$state"; echo "https://github.com/me/toy/pull/7" ;;
  "pr edit") cat > /dev/null; echo edited >> "$state" ;;
  *) exit 1 ;;
esac
"#,
                dir.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut github = GitHub::new(vec!["me/toy".to_string()]);
        github.gh_bin = script.to_string_lossy().into_owned();
        let publish = || {
            github
                .publish_pull_request(&dir, "me/toy", "feat/x", "main", "Title", "Body")
                .unwrap()
        };
        let first = publish();
        let second = publish();
        let log = std::fs::read_to_string(dir.join("prs")).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            first,
            PullRequest {
                number: 7,
                url: "https://github.com/me/toy/pull/7".to_string(),
                created: true
            }
        );
        assert!(!second.created);
        assert_eq!(second.number, 7);
        assert_eq!(log, "created\nedited\n");
    }
}
