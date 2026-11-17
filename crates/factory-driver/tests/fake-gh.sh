#!/bin/sh
# A scripted stand-in for the `gh` CLI, for the driver's tests. Pull requests
# and their checks live in files beside the script:
#   state/remote      path of the bare repository that plays GitHub
#   state/ci          one word per line, one line used per look at the checks:
#                     pending, red or green. The last line repeats.
#   state/pr          the head branch, once a pull request is open
#   state/merged      the merge commit, once it is merged
#   state/branch-ci   how the one workflow run on a branch's tip ended, when
#                     it did not pass: `failure`, `cancelled` and so on. A
#                     second word is the only commit that holds for.
#   state/reruns      how often a run's failed jobs were run again. Each
#                     time, the jobs the checks point at are new ones.
#   state/no-rerun    if it exists, running failed jobs again is refused
#   state/calls       one line per call
here=$(cd "$(dirname "$0")" && pwd)
state="$here/state-gh"
mkdir -p "$state"
echo "$*" >> "$state/calls"
remote=$(cat "$state/remote")

arg_after() { # the argument that follows $1 in the rest of the list
  want="$1"; shift
  while [ $# -gt 1 ]; do
    if [ "$1" = "$want" ]; then echo "$2"; return; fi
    shift
  done
}

case "$1 $2" in
  "auth status") exit 0 ;;
  "pr list")
    # Only the pull request of the branch asked for.
    want=$(arg_after --head "$@")
    if [ -f "$state/pr" ] && { [ -z "$want" ] || [ "$want" = "$(cat "$state/pr")" ]; }; then
      echo '[{"number":7,"url":"https://github.com/me/toy/pull/7"}]'
    else
      echo '[]'
    fi ;;
  "api graphql")
    # The repository's own issue types and its priority field. A read gets
    # what `state/graphql` holds, or an issue with neither set; a mutation
    # is recorded by the call itself.
    case "$*" in
      *mutation*) echo '{"data":{}}' ;;
      *)
        cat "$state/graphql" 2>/dev/null || echo '{"data":{"repository":{"issueTypes":{"nodes":[{"id":"IT_task","name":"Task","isEnabled":true},{"id":"IT_bug","name":"Bug","isEnabled":true}]},"issueFields":{"nodes":[{"id":"IFSS_priority","name":"Priority","options":[{"id":"O_critical","name":"Critical"},{"id":"O_high","name":"High"}]}]},"issue":{"id":"I_1","issueType":null,"issueFieldValues":{"nodes":[]}}}}}' ;;
    esac ;;
  "issue close")
    shift 2
    echo "$*" >> "$state/issues-closed" ;;
  "issue create")
    cat > /dev/null
    n=$(( $(cat "$state/issues" 2>/dev/null || echo 0) + 1 ))
    echo "$n" > "$state/issues"
    echo "https://github.com/me/toy/issues/$n" ;;
  "pr create")
    cat > "$state/pr-body"
    # A new pull request: whatever was merged before was another one.
    rm -f "$state/merged" "$state/merged-head"
    arg_after --head "$@" > "$state/pr"
    echo "https://github.com/me/toy/pull/7" ;;
  "pr edit") cat > "$state/pr-body" ;;
  "pr view")
    branch=$(cat "$state/pr")
    if [ -f "$state/merged" ]; then
      printf '{"state":"MERGED","headRefOid":"%s","mergeable":"UNKNOWN","mergeCommit":{"oid":"%s"},"statusCheckRollup":[]}\n' \
        "$(cat "$state/merged-head")" "$(cat "$state/merged")"
      exit 0
    fi
    head=$(git --git-dir="$remote" rev-parse "refs/heads/$branch")
    word=$(head -n 1 "$state/ci")
    if [ "$(wc -l < "$state/ci")" -gt 1 ]; then
      tail -n +2 "$state/ci" > "$state/ci.next" && mv "$state/ci.next" "$state/ci"
    fi
    job=$(( 66 + $(cat "$state/reruns" 2>/dev/null || echo 0) ))
    run() { printf '{"__typename":"CheckRun","name":"%s","status":"%s","conclusion":"%s","detailsUrl":"https://github.com/me/toy/actions/runs/55/job/%s"}' "$1" "$2" "$3" "$job"; }
    case "$word" in
      pending) checks="$(run fmt COMPLETED SUCCESS),$(run unit IN_PROGRESS '')" ;;
      red) checks="$(run fmt COMPLETED SUCCESS),$(run unit COMPLETED FAILURE)" ;;
      failing) checks="$(run fmt COMPLETED FAILURE),$(run unit IN_PROGRESS '')" ;;
      red-both) checks="$(run fmt COMPLETED FAILURE),$(run unit COMPLETED FAILURE)" ;;
      early) checks="$(run fmt COMPLETED SUCCESS)" ;;
      *) checks="$(run fmt COMPLETED SUCCESS),$(run unit COMPLETED SUCCESS)" ;;
    esac
    mergeable=$(cat "$state/mergeable" 2>/dev/null || echo MERGEABLE)
    printf '{"state":"OPEN","headRefOid":"%s","mergeable":"%s","mergeCommit":null,"statusCheckRollup":[%s]}\n' "$head" "$mergeable" "$checks" ;;
  "run list")
    # The default branch's own CI: one workflow run on the branch's tip,
    # which passed unless `state/branch-ci` says otherwise.
    branch=$(arg_after --branch "$@")
    tip=$(git --git-dir="$remote" rev-parse "refs/heads/$branch")
    conclusion=success
    if [ -f "$state/branch-ci" ]; then
      read -r word only < "$state/branch-ci"
      if [ -z "$only" ] || [ "$only" = "$tip" ]; then conclusion=$word; fi
    fi
    printf '[{"workflowName":"ci","headSha":"%s","status":"completed","conclusion":"%s","url":"https://github.com/me/toy/actions/runs/55"}]\n' "$tip" "$conclusion" ;;
  "run rerun")
    # Only the failed jobs are ever run again, never a whole run.
    case " $* " in
      *" --failed "*) ;;
      *) echo "fake gh: a run is only rerun with --failed: $*" >&2; exit 1 ;;
    esac
    if [ -f "$state/no-rerun" ]; then echo "run 55 cannot be rerun; its workflow file may be broken" >&2; exit 1; fi
    echo $(( $(cat "$state/reruns" 2>/dev/null || echo 0) + 1 )) > "$state/reruns" ;;
  "run view")
    echo "unit	test	error: expected 4, found 5"
    echo "unit	test	1 test failed on linux" ;;
  "pr merge")
    branch=$(cat "$state/pr")
    want=$(arg_after --match-head-commit "$@")
    subject=$(arg_after --subject "$@")
    body=$(arg_after --body "$@")
    head=$(git --git-dir="$remote" rev-parse "refs/heads/$branch")
    if [ "$want" != "$head" ]; then echo "head commit does not match" >&2; exit 1; fi
    work="$state/merge-work"
    rm -rf "$work"
    git clone --quiet "$remote" "$work"
    cd "$work" || exit 1
    git config user.name "GitHub" && git config user.email "noreply@localhost" && git config commit.gpgsign false
    case " $* " in
      *" --squash "*)
        git merge --quiet --squash "origin/$branch" || exit 1
        git commit --quiet -m "${subject:-Squashed 7}" -m "${body:-No body.}" || exit 1 ;;
      *)
        git merge --quiet --no-ff -m "${subject:-Merge pull request 7}" -m "${body:-No body.}" "origin/$branch" || exit 1 ;;
    esac
    git push --quiet origin HEAD:main || exit 1
    git rev-parse HEAD > "$state/merged"
    echo "$head" > "$state/merged-head" ;;
  *) echo "fake gh: unexpected call: $*" >&2; exit 1 ;;
esac
