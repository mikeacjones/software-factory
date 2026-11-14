#!/bin/sh
# A scripted stand-in for the `claude` CLI, for the driver's tests. It reads
# the same arguments the factory passes, plays the role named by the system
# prompt, and prints a stream the factory's parser accepts.
#
# State lives beside the script, so each test uses its own copy:
#   state/calls      one line per call: role, session, working directory
#   state/scenario   optional words that change behaviour:
#                      reject-once   the builder's first result has no evidence
#                      ask-once      the builder first asks the owner a question
here=$(cd "$(dirname "$0")" && pwd)
state="$here/state"
mkdir -p "$state"

if [ "$1" = "--version" ]; then
  echo "2.1.325 (Claude Code)"
  exit 0
fi

system=""
resume=""
model="unknown"
while [ $# -gt 0 ]; do
  case "$1" in
    --append-system-prompt) system="$2"; shift ;;
    --resume) resume="$2"; shift ;;
    --model) model="$2"; shift ;;
  esac
  shift
done
brief=$(cat)

count=$(( $(cat "$state/count" 2>/dev/null || echo 0) + 1 ))
echo "$count" > "$state/count"
scenario=$(cat "$state/scenario" 2>/dev/null)

case "$system" in
  *"You are the builder"*) role=builder ;;
  *"You are the adversarial tester"*) role=tester ;;
  *"You are the reviewer"*) role=reviewer ;;
  *) role=publisher ;;
esac
session=${resume:-fake-$role-$count}
echo "$role $session $(pwd)" >> "$state/calls"
printf '%s\n' "$brief" > "$state/brief-$count.md"

common='"question":"","reply_to_owner":""'
case "$role" in
  builder)
    case "$brief" in
      *"Your result was not accepted"*) ;;
      *) echo "line $count" >> feature.txt ;;
    esac
    evidence='[{"criterion":"the feature exists","evidence":"cat feature.txt shows it"}]'
    case "$scenario" in
      *reject-once*)
        if [ ! -f "$state/rejected" ]; then
          touch "$state/rejected"
          evidence='[]'
        fi ;;
    esac
    output='{"outcome":"done","summary":"Added a line to feature.txt.",'$common',"files_touched":["feature.txt"],"tests":[],"evidence":'$evidence'}'
    case "$scenario" in
      *ask-once*)
        if [ ! -f "$state/asked" ]; then
          touch "$state/asked"
          output='{"outcome":"needs_owner","summary":"I need a decision.","question":"Should the feature shout?","reply_to_owner":"I will use the short name.","files_touched":[],"tests":[],"evidence":[]}'
        fi ;;
    esac
    ;;
  tester)
    echo "probe" > probe.txt
    probes='[{"what":"empty input","command":"sh probe.sh","observed":"exit 1"}]'
    if [ ! -f "$state/broke" ]; then
      touch "$state/broke"
      output='{"outcome":"broken","summary":"It fails on empty input.",'$common',"probes":'$probes',"findings":[{"title":"fails on empty input","severity":"major","location":"feature.txt:1","reproduction":{"command":"sh probe.sh","expected":"exit 0","observed":"exit 1"}}]}'
    else
      output='{"outcome":"held","summary":"Could not break it.",'$common',"probes":'$probes',"findings":[]}'
    fi
    ;;
  reviewer)
    output='{"outcome":"approve","summary":"Meets the criteria.",'$common',"findings":[]}'
    ;;
  publisher)
    output='{"outcome":"ready","summary":"Wrote the text.",'$common',"commit_message":"Add the feature\n\nIt adds lines to feature.txt.\n","pr_title":"Add the feature","pr_body":"Adds the feature for toy-1."}'
    ;;
esac

printf '%s\n' '{"type":"system","subtype":"init","session_id":"'"$session"'","model":"'"$model"'","claude_code_version":"2.1.325"}'
printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]}}'
printf '%s\n' '{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1794680400,"rateLimitType":"five_hour","unifiedWindows":{"five_hour":{"utilization":0.2,"resetsAt":1794680400,"status":"allowed"}}}}'
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"num_turns":3,"duration_ms":10,"total_cost_usd":0.5,"modelUsage":{"'"$model"'":{"inputTokens":10,"outputTokens":100,"cacheReadInputTokens":1000,"cacheCreationInputTokens":50,"costUSD":0.5}},"session_id":"'"$session"'","structured_output":'"$output"'}'
