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

    /// Whether `gh` is logged in at all.
    pub fn authenticated(&self, cwd: &Path) -> bool {
        self.gh(cwd, &["auth", "status"], None).is_ok()
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

    #[test]
    fn a_repository_outside_the_write_list_is_refused_before_anything_runs() {
        // A `gh` that does not exist: reaching it would be a different error.
        let mut github = GitHub::new(vec!["me/toy".to_string()]);
        github.gh_bin = "no-such-gh-anywhere".to_string();
        let result = github.publish_pull_request(
            &std::env::temp_dir(),
            "someone/else",
            "factory/x",
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
                .publish_pull_request(&dir, "me/toy", "factory/x", "main", "Title", "Body")
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
