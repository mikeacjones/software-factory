//! Git operations the factory performs itself, each with a timeout.
//!
//! Agents are not sandboxed and can run git too. The factory relies on
//! neither their doing so nor their not doing so: every step starts from a
//! known commit, what a step changed is one diff, and the branch that is
//! published is the one the factory built.

use std::path::{Path, PathBuf};
use std::time::Duration;

use factory_supervise::{Captured, Spec, capture};

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error(transparent)]
    Run(#[from] factory_supervise::Error),
    #[error("`git {command}` in {dir} failed: {detail}")]
    Failed {
        command: String,
        dir: PathBuf,
        detail: String,
    },
    #[error("`git {command}` in {dir} timed out")]
    TimedOut { command: String, dir: PathBuf },
}

const LOCAL: Duration = Duration::from_secs(120);
const NETWORK: Duration = Duration::from_secs(600);

fn git_raw(dir: &Path, args: &[&str], timeout: Duration) -> Result<Captured, GitError> {
    let argv: Vec<&str> = std::iter::once("git").chain(args.iter().copied()).collect();
    let mut spec = Spec::new(argv, dir, timeout);
    // Never stop to ask for a password or open an editor.
    spec.env = vec![
        ("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()),
        ("GIT_EDITOR".to_string(), "true".to_string()),
    ];
    let out = capture(&spec)?;
    if out.timed_out {
        return Err(GitError::TimedOut {
            command: args.join(" "),
            dir: dir.to_path_buf(),
        });
    }
    Ok(out)
}

fn git(dir: &Path, args: &[&str], timeout: Duration) -> Result<String, GitError> {
    let out = git_raw(dir, args, timeout)?;
    if !out.success() {
        return Err(GitError::Failed {
            command: args.join(" "),
            dir: dir.to_path_buf(),
            detail: out.combined().trim().to_string(),
        });
    }
    Ok(out.stdout)
}

/// `factory/<bead>-<slug>`: the branch of a unit.
pub fn branch_name(bead: &str, title: &str) -> String {
    let mut slug = String::new();
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
        if slug.len() >= 40 {
            break;
        }
    }
    let slug = slug.trim_end_matches('-');
    let bead: String = bead
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect();
    if slug.is_empty() {
        format!("factory/{bead}")
    } else {
        format!("factory/{bead}-{slug}")
    }
}

pub fn fetch(repo: &Path, remote: &str) -> Result<(), GitError> {
    git(repo, &["fetch", "--prune", remote], NETWORK).map(|_| ())
}

pub fn rev_parse(dir: &Path, rev: &str) -> Result<String, GitError> {
    Ok(git(dir, &["rev-parse", "--verify", rev], LOCAL)?
        .trim()
        .to_string())
}

pub fn branch_exists(repo: &Path, branch: &str) -> Result<bool, GitError> {
    let reference = format!("refs/heads/{branch}");
    Ok(git_raw(
        repo,
        &["rev-parse", "--verify", "--quiet", &reference],
        LOCAL,
    )?
    .success())
}

/// A new worktree on a new branch starting at `start`.
pub fn worktree_add(repo: &Path, dir: &Path, branch: &str, start: &str) -> Result<(), GitError> {
    let dir = dir.to_string_lossy();
    git(repo, &["worktree", "add", "-b", branch, &dir, start], LOCAL).map(|_| ())
}

/// A new worktree with no branch, at `commit`. For steps whose changes are
/// thrown away.
pub fn worktree_add_detached(repo: &Path, dir: &Path, commit: &str) -> Result<(), GitError> {
    let dir = dir.to_string_lossy();
    git(repo, &["worktree", "add", "--detach", &dir, commit], LOCAL).map(|_| ())
}

pub fn worktree_remove(repo: &Path, dir: &Path) -> Result<(), GitError> {
    let dir = dir.to_string_lossy();
    git(repo, &["worktree", "remove", "--force", &dir], LOCAL).map(|_| ())
}

/// Commit everything in the worktree as a checkpoint. The project's hooks are
/// skipped: they are written for people, and a checkpoint is not a commit
/// anyone keeps.
/// Returns the commit the
/// worktree is at afterwards, and whether anything was committed.
pub fn checkpoint(dir: &Path, message: &str) -> Result<(String, bool), GitError> {
    git(dir, &["add", "--all"], LOCAL)?;
    let dirty = !git(dir, &["status", "--porcelain"], LOCAL)?
        .trim()
        .is_empty();
    if dirty {
        git(
            dir,
            &["commit", "--no-verify", "--quiet", "-m", message],
            LOCAL,
        )?;
    }
    Ok((rev_parse(dir, "HEAD")?, dirty))
}

/// The files that differ between two commits.
pub fn changed_files(dir: &Path, from: &str, to: &str) -> Result<Vec<String>, GitError> {
    let out = git(
        dir,
        &["diff", "--name-only", "--no-renames", from, to],
        LOCAL,
    )?;
    Ok(out.lines().map(str::to_string).collect())
}

/// `git diff --stat` between two commits.
pub fn diff_stat(dir: &Path, from: &str, to: &str) -> Result<String, GitError> {
    git(dir, &["diff", "--stat", from, to], LOCAL)
}

/// Everything a worktree holds beyond `commit`, new files included and
/// whether committed since or not, as a patch.
pub fn patch_since(dir: &Path, commit: &str) -> Result<String, GitError> {
    git(dir, &["add", "--all"], LOCAL)?;
    git(dir, &["diff", "--cached", "--binary", commit], LOCAL)
}

/// The tree a commit holds: equal trees mean equal content.
pub fn tree_of(dir: &Path, rev: &str) -> Result<String, GitError> {
    rev_parse(dir, &format!("{rev}^{{tree}}"))
}

/// Replace every commit since `base` with one commit carrying `message`.
pub fn squash(dir: &Path, base: &str, message: &str) -> Result<String, GitError> {
    git(dir, &["reset", "--soft", base], LOCAL)?;
    git(
        dir,
        &["commit", "--no-verify", "--quiet", "-m", message],
        LOCAL,
    )?;
    rev_parse(dir, "HEAD")
}

pub fn commit_count(dir: &Path, from: &str, to: &str) -> Result<u64, GitError> {
    let range = format!("{from}..{to}");
    Ok(git(dir, &["rev-list", "--count", &range], LOCAL)?
        .trim()
        .parse()
        .unwrap_or(0))
}

pub fn remote_url(repo: &Path, remote: &str) -> Result<String, GitError> {
    Ok(git(repo, &["remote", "get-url", "--push", remote], LOCAL)?
        .trim()
        .to_string())
}

/// Push the branch as the factory built it, over whatever is on the remote
/// under that name.
pub fn push(dir: &Path, remote: &str, branch: &str) -> Result<(), GitError> {
    let refspec = format!("+HEAD:refs/heads/{branch}");
    git(dir, &["push", "--no-verify", remote, &refspec], NETWORK).map(|_| ())
}

/// `owner/name` from a GitHub remote URL, in either of its two forms.
pub fn github_slug(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("git@github.com:")
        .or_else(|| url.strip_prefix("https://github.com/"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))?;
    let slug = rest.trim_end_matches('/').trim_end_matches(".git");
    (slug.split('/').count() == 2 && !slug.starts_with('/')).then(|| slug.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("factory-git-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn repo(name: &str) -> PathBuf {
        let dir = temp(name);
        for args in [
            &["init", "--quiet", "-b", "main"][..],
            &["config", "user.name", "Test"],
            &["config", "user.email", "test@localhost"],
            &["config", "commit.gpgsign", "false"],
        ] {
            git(&dir, args, LOCAL).unwrap();
        }
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        checkpoint(&dir, "first").unwrap();
        dir
    }

    #[test]
    fn branch_names_are_tame() {
        assert_eq!(
            branch_name(
                "zig-d51.16.2",
                "samples/README.md: Go sample → Zig map & status"
            ),
            "factory/zig-d51.16.2-samples-readme-md-go-sample-zig-map-stat"
        );
        assert_eq!(branch_name("x 1", "!!!"), "factory/x-1");
    }

    #[test]
    fn github_slugs_are_read_from_both_url_forms() {
        assert_eq!(
            github_slug("git@github.com:me/toy.git").as_deref(),
            Some("me/toy")
        );
        assert_eq!(
            github_slug("https://github.com/me/toy").as_deref(),
            Some("me/toy")
        );
        assert_eq!(github_slug("/tmp/bare.git"), None);
        assert_eq!(github_slug("git@example.com:me/toy.git"), None);
    }

    #[test]
    fn a_unit_is_built_from_checkpoints_and_squashed_for_publication() {
        let repo = repo("unit");
        let base = rev_parse(&repo, "HEAD").unwrap();
        let work = temp("unit-wt");
        std::fs::remove_dir_all(&work).unwrap();
        worktree_add(&repo, &work, "factory/toy-1", &base).unwrap();

        // Nothing changed: no commit.
        assert_eq!(checkpoint(&work, "empty").unwrap(), (base.clone(), false));

        std::fs::write(work.join("b.txt"), "two\n").unwrap();
        let (first, committed) = checkpoint(&work, "checkpoint: build").unwrap();
        assert!(committed);
        std::fs::write(work.join("a.txt"), "one\nmore\n").unwrap();
        let (second, _) = checkpoint(&work, "checkpoint: fix").unwrap();

        assert_eq!(changed_files(&work, &base, &first).unwrap(), ["b.txt"]);
        assert_eq!(changed_files(&work, &first, &second).unwrap(), ["a.txt"]);
        assert_eq!(commit_count(&work, &base, "HEAD").unwrap(), 2);

        let tree = tree_of(&work, "HEAD").unwrap();
        let published = squash(&work, &base, "Add b\n\nBody.").unwrap();
        assert_eq!(tree_of(&work, &published).unwrap(), tree);
        assert_eq!(commit_count(&work, &base, &published).unwrap(), 1);
        assert_eq!(
            changed_files(&work, &base, &published).unwrap(),
            ["a.txt", "b.txt"]
        );

        // A scratch worktree's changes come back as a patch and leave no trace.
        let scratch = temp("unit-scratch");
        std::fs::remove_dir_all(&scratch).unwrap();
        worktree_add_detached(&repo, &scratch, &published).unwrap();
        std::fs::write(scratch.join("probe.txt"), "probe\n").unwrap();
        let patch = patch_since(&scratch, &published).unwrap();
        assert!(patch.contains("probe.txt"));
        worktree_remove(&repo, &scratch).unwrap();
        assert!(!scratch.exists());
        assert_eq!(rev_parse(&work, "HEAD").unwrap(), published);

        // Publishing goes to a remote under the unit's branch name.
        let bare = temp("unit-bare");
        git(&bare, &["init", "--quiet", "--bare"], LOCAL).unwrap();
        git(
            &repo,
            &["remote", "add", "origin", &bare.to_string_lossy()],
            LOCAL,
        )
        .unwrap();
        push(&work, "origin", "factory/toy-1").unwrap();
        assert_eq!(
            rev_parse(&bare, "refs/heads/factory/toy-1").unwrap(),
            published
        );

        worktree_remove(&repo, &work).unwrap();
        for dir in [repo, bare] {
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
}
