#!/usr/bin/env bash
# Verify the first real run: a factory unit for temporal-sdk-zig that ended
# with a pull request into main. Reads the unit's own record, then checks each
# claim against GitHub, Slack's log of the run and the bead database, not
# against what the unit says about itself.
#
#   proofs/walk_zig_verify.sh [UNIT_ID]     default: the newest ended unit
#
# Read-only everywhere.
set -euo pipefail

state="${FACTORY_STATE_DIR:-$HOME/.local/state/software-factory}"
repo="mikeacjones/temporal-sdk-zig"
fail() { echo "FAIL: $*" >&2; exit 1; }

unit="${1:-}"
if [ -z "$unit" ]; then
  for candidate in $(ls -t "$state/units" 2>/dev/null); do
    file="$state/units/$candidate/unit.json"
    [ -f "$file" ] || continue
    if jq -e '.project == "temporal-sdk-zig" and .status == "ended"' "$file" >/dev/null; then
      unit="$candidate"
      break
    fi
  done
fi
[ -n "$unit" ] || fail "no ended unit for temporal-sdk-zig under $state/units"
dir="$state/units/$unit"
events="$dir/events.jsonl"
echo "unit: $unit"

url="$(jq -r '.pull_request // empty' "$dir/unit.json")"
branch="$(jq -r .branch "$dir/unit.json")"
bead="$(jq -r .bead "$dir/unit.json")"
[ -n "$url" ] || fail "the unit has no pull request"

# GitHub's own record of the pull request.
pr="$(gh pr view "$url" --repo "$repo" --json state,baseRefName,headRefName,commits,url)"
echo "$pr" | jq -e '.state == "OPEN" or .state == "MERGED"' >/dev/null || fail "the pull request is $(echo "$pr" | jq -r .state)"
echo "$pr" | jq -e '.baseRefName == "main"' >/dev/null || fail "the pull request is not into main"
echo "$pr" | jq -e --arg b "$branch" '.headRefName == $b and (.headRefName | startswith("factory/"))' >/dev/null || fail "the pull request is not from the unit's factory branch"
echo "$pr" | jq -e '.commits | length == 1' >/dev/null || fail "the pull request does not hold exactly one commit"
echo "ok   pull request $url: $(echo "$pr" | jq -r .state), one commit, $branch -> main"

# Every step of the flow ended, and each agent step has a session and usage.
steps="$(jq -r 'select(.kind == "step.ended") | .step' "$events" | sort -u | tr '\n' ' ')"
for step in baseline build fmt index readme changelog test samples break review describe publish; do
  case " $steps " in *" $step "*) ;; *) fail "step $step never ended" ;; esac
done
for step in build break review describe; do
  jq -e --arg s "$step" 'select(.kind == "run.ended" and .step == $s and .data.outcome == "completed" and (.data.session | length > 0) and (.data.usage.turns > 0))' "$events" >/dev/null \
    || fail "no completed run with a session and usage for step $step"
done
echo "ok   every step ended; agent steps have sessions and usage"

# The Slack thread was opened and nothing sent to it failed.
jq -e 'select(.kind == "slack.thread_opened")' "$events" >/dev/null || fail "no Slack thread was opened"
failed="$(jq -r 'select(.kind == "slack.failed") | .data.error' "$events" | wc -l | tr -d ' ')"
[ "$failed" = 0 ] || fail "$failed Slack calls failed during the run"
said="$(jq -r 'select(.kind == "slack.say") | .kind' "$events" | wc -l | tr -d ' ')"
echo "ok   Slack thread opened, $said updates posted, none failed"

# The bead database's own record of the claim.
row="$(dolt --host 127.0.0.1 --port 3306 --user root --password "" --no-tls --use-db temporal-sdk-zig sql -r csv -q "select status, coalesce(assignee, '') from issues where id = '$bead'" | tail -n 1 | tr -d '\r')"
case "$row" in
  "in_progress,factory") echo "ok   bead $bead is in progress, claimed by factory" ;;
  closed,*) echo "ok   bead $bead is closed (the pull request merged)" ;;
  *) fail "bead $bead is '$row', expected in_progress,factory" ;;
esac
echo "PASS"
