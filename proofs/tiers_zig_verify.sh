#!/usr/bin/env bash
# Verify that a real sub-goal on temporal-sdk-zig was run by a foreman: its
# foreman started its beads and reported it ready, the director shipped it,
# and it landed. Reads the goal's own record and event log.
#
#   proofs/tiers_zig_verify.sh [GOAL_ID]    default: the newest goal with one
#
# Read-only.
set -euo pipefail

state="${FACTORY_STATE_DIR:-$HOME/.local/state/software-factory}"
fail() { echo "FAIL: $*" >&2; exit 1; }
# A landed sub-goal that had a foreman which ran at least once.
ran='[.subgoals[]? | select(.state == "landed" and (.foreman.wakes // 0) > 0)]'

goal="${1:-}"
if [ -z "$goal" ]; then
  for candidate in $(ls -t "$state/goals" 2>/dev/null); do
    file="$state/goals/$candidate/goal.json"
    [ -f "$file" ] || continue
    if jq -e ".project == \"temporal-sdk-zig\" and ($ran | length > 0)" "$file" >/dev/null; then
      goal="$candidate"
      break
    fi
  done
fi
[ -n "$goal" ] || fail "no goal for temporal-sdk-zig under $state/goals has a landed sub-goal that a foreman ran"
dir="$state/goals/$goal"
events="$dir/events.jsonl"
echo "goal: $goal"

sub="$(jq -r "$ran | .[0].id // empty" "$dir/goal.json")"
[ -n "$sub" ] || fail "no landed sub-goal of $goal was run by a foreman"
absorbed="$(jq --arg s "$sub" '[.subgoals[] | select(.id == $s) | .absorbed[]] | length' "$dir/goal.json")"
[ "$absorbed" -ge 1 ] || fail "sub-goal $sub absorbed no bead"
# Its foreman decided at least once that it was ready, and pipelines ran in it.
ready="$(jq -s --arg s "$sub" '[.[] | select(.kind == "foreman.decided" and .data.subgoal == $s and .data.report == "ready")] | length' "$events")"
[ "$ready" -ge 1 ] || fail "the foreman of $sub never reported it ready"
started="$(jq -s --arg s "$sub" '[.[] | select(.kind == "pipeline.started" and .data.subgoal == $s)] | length' "$events")"
[ "$started" -ge 1 ] || fail "no pipeline was started in $sub"
jq -s -e --arg s "$sub" '[.[] | select(.kind == "subgoal.shipping" and .data.id == $s)] | length > 0' "$events" >/dev/null || fail "$sub was never shipped"
wakes="$(jq --arg s "$sub" '.subgoals[] | select(.id == $s) | .foreman.wakes' "$dir/goal.json")"
echo "ok   sub-goal $sub: $started pipeline(s) started and $absorbed bead(s) absorbed over $wakes foreman wakes; reported ready, shipped and landed"
echo "PASS"
