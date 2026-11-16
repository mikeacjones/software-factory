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
#                      minor-finding the reviewer approves with one finding left open
#                      bogus-finding the tester's first reproduction passes when run
#                      lazy-fix      the builder's first fix leaves the regression test failing
#                      forget-closes the publisher first leaves the issues out of the pull request
#                      commit-as-you-go
#                                    the builder commits its work and goes on working for a while
#                      own-file      the builder writes under a directory named after its bead,
#                                    not to the one file every bead shares
#                      lazy-merge    the builder's first try at a merge conflict leaves the markers in
#                      trespass      the builder first also writes a file under .github, which
#                                    no step may change, and takes it away when it is
#                                    told to put it back
#                      limit-default-once
#                                    the first run without an account token is refused
#                                    at the five-hour limit
#   state/director-N.json, state/intake-N.json, state/foreman-N.json,
#   state/executive-N.json
#                    what a goal's director, a sub-goal's foreman or a
#                    project's executive returns on its Nth call
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

scenario=$(cat "$state/scenario" 2>/dev/null)

case "$system" in
  *"You direct a goal"*) role=director ;;
  *"You are the foreman"*) role=foreman ;;
  *"You are the executive"*) role=executive ;;
  *"You are opening a goal"*) role=intake ;;
  *"You are the builder"*) role=builder ;;
  *"You are the adversarial tester"*) role=tester ;;
  *"You are the reviewer"*) role=reviewer ;;
  *) role=publisher ;;
esac
# Pipelines may run side by side: number the calls one at a time, so that
# line N of `calls` is the call whose brief is brief-N.
while ! mkdir "$state/lock" 2>/dev/null; do sleep 0.01; done
count=$(( $(cat "$state/count" 2>/dev/null || echo 0) + 1 ))
echo "$count" > "$state/count"
session=${resume:-fake-$role-$count}
echo "$role $session $(pwd) ${CLAUDE_CODE_OAUTH_TOKEN:-default}" >> "$state/calls"
rmdir "$state/lock"
# The bead a pipeline's agent is working on.
bead=$(printf '%s' "$brief" | grep -o -E '(toy|filed)-[0-9]+' | head -1)

# The CLI's own login is at its five-hour limit, once: the run is refused.
case "$scenario" in
  *limit-default-once*)
    if [ -z "${CLAUDE_CODE_OAUTH_TOKEN:-}" ] && [ ! -f "$state/limited" ]; then
      touch "$state/limited"
      reset=$(( $(date +%s) + 3600 ))
      printf '%s\n' '{"type":"system","subtype":"init","session_id":"'"$session"'","model":"'"$model"'","claude_code_version":"2.1.325"}'
      printf '%s\n' '{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":'"$reset"',"rateLimitType":"five_hour","unifiedWindows":{"five_hour":{"utilization":1.0,"resetsAt":'"$reset"',"status":"rejected"}}}}'
      printf '%s\n' '{"type":"result","subtype":"success","is_error":true,"api_error_status":429,"num_turns":1,"duration_ms":5,"result":"limit reached","session_id":"'"$session"'","modelUsage":{}}'
      exit 1
    fi ;;
esac
printf '%s\n' "$brief" > "$state/brief-$count.md"

# A goal's director, and a sub-goal's foreman, say whatever the test wrote
# for their nth call.
case "$role" in
  director|intake|foreman|executive)
    n=$(( $(cat "$state/n-$role" 2>/dev/null || echo 0) + 1 ))
    echo "$n" > "$state/n-$role"
    output=$(cat "$state/$role-$n.json" 2>/dev/null || echo '{}')
    ;;
esac

common='"question":"","reply_to_owner":""'
case "$role" in
  builder)
    case "$brief" in
      *"Your result was not accepted"*) ;;
      *"conflict markers"*)
        # The sub-goal's branch was merged in and conflicted: keep both sides.
        resolve=1
        case "$scenario" in
          *lazy-merge*) [ -f "$state/lazy-merge" ] || { touch "$state/lazy-merge"; resolve=0; } ;;
        esac
        if [ "$resolve" = 1 ]; then
          grep -v -E '^(<<<<<<<|=======|>>>>>>>)' feature.txt > feature.tmp
          mv feature.tmp feature.txt
        fi ;;
      *)
        case "$scenario" in
          *own-file*) mkdir -p "$bead"; echo "line $count" >> "$bead/feature.txt" ;;
          *) echo "line $count" >> feature.txt ;;
        esac ;;
    esac
    case "$scenario" in
      *trespass*)
        case "$brief" in
          *"put it back"*) rm -rf .github ;;
          *) [ -f "$state/trespassed" ] || { touch "$state/trespassed"; mkdir -p .github; echo "on: push" > .github/ci.yml; } ;;
        esac ;;
    esac
    # It commits what it has, as a builder is told to, and is not done yet.
    case "$scenario" in
      *commit-as-you-go*)
        git add -A && git commit --no-verify --quiet -m "wip: a line" && sleep 5 ;;
    esac
    # A fix: make the tester's regression test, or the report's gate, pass.
    case "$brief" in
      *"regression test"*|*"must not pass"*)
        case "$scenario" in
          *lazy-fix*)
            if [ -f "$state/lazy" ]; then touch fixed.txt; else touch "$state/lazy"; fi ;;
          *) touch fixed.txt ;;
        esac ;;
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
    retry=0
    case "$brief" in *"Your result was not accepted"*) retry=1 ;; esac
    probes='[{"what":"empty input","command":"sh regress.sh","observed":"exit 1"}]'
    if [ ! -f "$state/broke" ] || [ "$retry" = 1 ]; then
      touch "$state/broke"
      # The regression test: it fails until the builder's fix exists.
      printf '%s\n' '# regression: empty-input' 'test -f fixed.txt' > regress.sh
      proof='sh regress.sh'
      case "$scenario" in
        *bogus-finding*) [ "$retry" = 0 ] && proof='true' ;;
      esac
      output='{"outcome":"broken","summary":"It fails on empty input.",'$common',"probes":'$probes',"findings":[{"title":"fails on empty input","severity":"major","location":"feature.txt:1","category":"empty-input","test_names":["empty-input"],"reproduction":{"command":"'"$proof"'","expected":"exit 0","observed":"exit 1"}}]}'
    else
      echo "probe" > probe.txt
      output='{"outcome":"held","summary":"Could not break it.",'$common',"probes":'$probes',"findings":[]}'
    fi
    ;;
  reviewer)
    findings='[]'
    case "$scenario" in
      *minor-finding*) findings='[{"title":"The doc omits the timeout","severity":"minor","location":"feature.txt:1","category":"docs","evidence":"line 1 says nothing about it","must_fix":false}]' ;;
    esac
    output='{"outcome":"approve","summary":"Meets the criteria.",'$common',"findings":'$findings'}'
    ;;
  publisher)
    # Close the issues the brief names, as a real publisher is told to.
    closes=$(printf '%s' "$brief" | grep -o -E 'Closes #[0-9]+' | sort -u | tr '\n' ' ')
    case "$scenario" in
      *forget-closes*)
        # The first time it has issues to close, it leaves them out.
        if [ -n "$closes" ] && [ ! -f "$state/forgot" ]; then touch "$state/forgot"; closes=""; fi ;;
    esac
    trailer=""
    case "$brief" in
      *"publish check"*) trailer='\nChangelog: none (test)\n' ;;
    esac
    output='{"outcome":"ready","summary":"Wrote the text.",'$common',"commit_message":"Add the feature\n\nIt adds lines to feature.txt.\n'$trailer'","pr_title":"Add the feature","pr_body":"Adds the feature for toy-1. '"$closes"'","merge_summary":"The branch adds feature.txt, which the bead asked for."}'
    ;;
esac

printf '%s\n' '{"type":"system","subtype":"init","session_id":"'"$session"'","model":"'"$model"'","claude_code_version":"2.1.325"}'
printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]}}'
printf '%s\n' '{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1794680400,"rateLimitType":"five_hour","unifiedWindows":{"five_hour":{"utilization":0.2,"resetsAt":1794680400,"status":"allowed"}}}}'
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"num_turns":3,"duration_ms":10,"total_cost_usd":0.5,"modelUsage":{"'"$model"'":{"inputTokens":10,"outputTokens":100,"cacheReadInputTokens":1000,"cacheCreationInputTokens":50,"costUSD":0.5}},"session_id":"'"$session"'","structured_output":'"$output"'}'
