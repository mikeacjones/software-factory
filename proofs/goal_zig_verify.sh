#!/usr/bin/env bash
# Verify that a real goal ran to its end on temporal-sdk-zig: opened from a
# Slack message, started by the owner, at least one bead landed, and accepted
# as complete by the owner. Reads the goal's own record and event log.
#
#   proofs/goal_zig_verify.sh [GOAL_ID]     default: the newest complete goal
#
# Read-only.
set -euo pipefail

state="${FACTORY_STATE_DIR:-$HOME/.local/state/software-factory}"
fail() { echo "FAIL: $*" >&2; exit 1; }

goal="${1:-}"
if [ -z "$goal" ]; then
  for candidate in $(ls -t "$state/goals" 2>/dev/null); do
    file="$state/goals/$candidate/goal.json"
    [ -f "$file" ] || continue
    if jq -e '.project == "temporal-sdk-zig" and .phase == "complete"' "$file" >/dev/null; then
      goal="$candidate"
      break
    fi
  done
fi
[ -n "$goal" ] || fail "no complete goal for temporal-sdk-zig under $state/goals"
dir="$state/goals/$goal"
events="$dir/events.jsonl"
echo "goal: $goal"

jq -e '.phase == "complete" and (.charter.objective | length > 0)' "$dir/goal.json" >/dev/null || fail "the goal is not complete with a charter"
for kind in goal.opened goal.complete; do
  jq -e --arg k "$kind" 'select(.kind == $k)' "$events" >/dev/null || fail "no $kind event"
done
for action in goal_start goal_accept; do
  jq -e --arg a "$action" 'select(.kind == "owner.action" and .data.action == $a)' "$events" >/dev/null || fail "the owner never did $action"
done
landed="$(jq '[.units[] | select(.state == "landed")] | length' "$dir/goal.json")"
[ "$landed" -ge 1 ] || fail "no pipeline of the goal landed"
failed="$(jq -r 'select(.kind == "slack.failed") | .kind' "$events" | wc -l | tr -d ' ')"
[ "$failed" = 0 ] || fail "$failed Slack calls failed"
echo "ok   opened, started and accepted by the owner; $landed bead(s) landed; $(jq .wakes "$dir/goal.json") director wakes"
echo "PASS"
