#!/usr/bin/env bash
# Walk one bead of a scratch project through the real flow with the real
# Claude CLI, every model replaced by Haiku. No GitHub and no Slack: the
# "remote" is a bare repository in a temp directory.
#
#   proofs/walk_scratch.sh [--keep]
#
# Passes if the run ends with one commit pushed to the unit's branch, that
# commit passes the project's test and does what the bead asked, and the event
# log names each step with its outcome, session and usage.
#
# Makes real Claude calls (live tier). --keep leaves the temp directory.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
keep=0
[ "${1:-}" = "--keep" ] && keep=1

work="$(mktemp -d "${TMPDIR:-/tmp}/walk_scratch.XXXXXX")"
work="$(cd "$work" && pwd -P)"
cleanup() { [ "$keep" = 1 ] && echo "kept: $work" || rm -rf "$work"; }
trap cleanup EXIT
fail() { echo "FAIL: $*" >&2; exit 1; }

cargo build --quiet --locked --package factory-cli --manifest-path "$root/Cargo.toml"
factory="$root/target/debug/factory"

# The project: a script, its test, and a bare remote.
mkdir -p "$work/toy" "$work/beads" "$work/config/projects"
cd "$work/toy"
git init --quiet -b main
git config user.name "Scratch"
git config user.email "scratch@localhost"
git config commit.gpgsign false
cat > greet.sh <<'SH'
#!/bin/sh
# Usage: greet.sh NAME
name="$1"
echo "Hello, $name!"
SH
cat > test.sh <<'SH'
#!/bin/sh
# The project's tests. Exits non-zero if any fails.
fail=0
check() {
  if [ "$2" != "$3" ]; then
    echo "FAIL $1: expected '$3', got '$2'"
    fail=1
  fi
}
check "greets by name" "$(sh greet.sh world)" "Hello, world!"
exit $fail
SH
cat > AGENTS.md <<'MD'
# toy

A shell script and its tests. Run `sh test.sh` before finishing. Every behaviour has a case in test.sh. POSIX sh only.
MD
git add --all
git commit --quiet -m "A greeting"
git init --quiet --bare "$work/remote.git"
git remote add origin "$work/remote.git"
git push --quiet origin main

cat > "$work/beads/toy-1.json" <<'JSON'
{
  "id": "toy-1",
  "title": "Add a --shout flag to greet.sh",
  "description": "greet.sh should accept an optional --shout flag before the name. With it, the greeting is printed with its ASCII letters in upper case.",
  "acceptance_criteria": "- `sh greet.sh --shout world` prints `HELLO, WORLD!`\n- Only the ASCII letters a to z are upper-cased. Every other byte of the name is printed unchanged, whatever the locale: this is the intended behaviour, not a gap\n- `sh greet.sh world` still prints `Hello, world!`\n- With no name, with or without --shout, greet.sh prints a usage line to stderr and exits 2\n- Arguments after the name are ignored, as they are today\n- test.sh covers each of the above and passes",
  "status": "open",
  "issue_type": "task"
}
JSON

cp -R "$root/config/flows" "$root/config/roles" "$work/config/"
cat > "$work/config/factory.toml" <<TOML
state_dir = "$work/state"
allowed_models = ["claude-opus-5-5", "claude-haiku-5-5"]

[model_override]
"*" = "claude-haiku-5-5"
TOML
cat > "$work/config/projects/toy.toml" <<TOML
id = "toy"
repo = "$work/toy"
default_branch = "main"
flow = "build-break-review"
forbidden_paths = [".github/**"]

[beads]
kind = "files"
dir = "$work/beads"

[scopes]
code = ["**"]

[commands.fmt]
argv = ["sh", "-n", "greet.sh"]
timeout_secs = 30

[commands.readme]
argv = ["test", "-f", "AGENTS.md"]
timeout_secs = 30

[commands.changelog]
argv = ["true"]
timeout_secs = 30

[commands.test]
argv = ["sh", "test.sh"]
timeout_secs = 60

[limits]
step_timeout_secs = 900
step_budget_usd = 2.0
TOML

cd "$work"
"$factory" flow check toy --config "$work/config"
set +e
"$factory" run toy toy-1 --config "$work/config" --no-slack
status=$?
set -e

unit_dir="$(find "$work/state/units" -mindepth 1 -maxdepth 1 -type d | head -n 1)"
events="$unit_dir/events.jsonl"
echo "--- steps"
jq -r 'select(.kind == "step.ended") | "\(.step) -> \(.data.outcome) (\(.data.secs)s)"' "$events"
[ "$status" = 0 ] || fail "the run did not end (exit $status); see $unit_dir (use --keep)"

branch="$(jq -r .branch "$unit_dir/unit.json")"
cd "$work/remote.git"
commits="$(git rev-list --count "main..refs/heads/$branch")"
[ "$commits" = 1 ] || fail "expected one commit on $branch, found $commits"

# What was published does what the bead asked, judged by this script and not
# by anything an agent wrote.
out="$work/published"
git --git-dir="$work/remote.git" worktree add --quiet --detach "$out" "refs/heads/$branch"
cd "$out"
sh test.sh || fail "the published commit fails the project's test"
[ "$(sh greet.sh --shout world)" = "HELLO, WORLD!" ] || fail "--shout does not shout"
[ "$(sh greet.sh world)" = "Hello, world!" ] || fail "the plain greeting changed"
set +e
sh greet.sh >/dev/null 2>"$work/usage.err"
code=$?
set -e
[ "$code" = 2 ] || fail "no name should exit 2, got $code"
[ -s "$work/usage.err" ] || fail "no name should print usage to stderr"

# The event log names each agent step with its outcome, session and usage.
for step in build break review describe; do
  jq -e --arg step "$step" 'select(.kind == "run.ended" and .step == $step and .data.outcome == "completed" and (.data.session | length > 0) and (.data.usage.turns > 0))' "$events" >/dev/null \
    || fail "no completed run with a session and usage for step $step"
done
jq -e 'select(.kind == "published.pushed")' "$events" >/dev/null || fail "no push event"
jq -e '.status == "in_progress" and .assignee == "factory"' "$work/beads/toy-1.json" >/dev/null || fail "the bead is not claimed by the factory"
models="$(jq -r 'select(.kind == "run.ended") | .data.usage.models | keys[]' "$events" | sort -u | tr '\n' ' ')"
echo "models used: $models"
case "$models" in *opus*|*fable*) fail "a model other than Haiku was used: $models" ;; esac

echo "PASS: $branch holds one commit that passes its checks"
