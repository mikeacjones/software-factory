#!/bin/sh
# A scripted stand-in for the `gh` CLI, for the driver's tests. Pull requests
# and their checks live in files beside the script:
#   state/remote      path of the bare repository that plays GitHub
#   state/ci          one word per line, one line used per look at the checks:
#                     pending, red or green. The last line repeats.
#   state/pr          the head branch, once a pull request is open
#   state/merged      the merge commit, once it is merged
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
    if [ -f "$state/pr" ]; then echo '[{"number":7,"url":"https://github.com/me/toy/pull/7"}]'; else echo '[]'; fi ;;
  "pr create")
    cat > /dev/null
    arg_after --head "$@" > "$state/pr"
    echo "https://github.com/me/toy/pull/7" ;;
  "pr edit") cat > /dev/null ;;
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
    run() { printf '{"__typename":"CheckRun","name":"%s","status":"%s","conclusion":"%s","detailsUrl":"https://github.com/me/toy/actions/runs/55/job/66"}' "$1" "$2" "$3"; }
    case "$word" in
      pending) checks="$(run fmt COMPLETED SUCCESS),$(run unit IN_PROGRESS '')" ;;
      red) checks="$(run fmt COMPLETED SUCCESS),$(run unit COMPLETED FAILURE)" ;;
      early) checks="$(run fmt COMPLETED SUCCESS)" ;;
      *) checks="$(run fmt COMPLETED SUCCESS),$(run unit COMPLETED SUCCESS)" ;;
    esac
    mergeable=$(cat "$state/mergeable" 2>/dev/null || echo MERGEABLE)
    printf '{"state":"OPEN","headRefOid":"%s","mergeable":"%s","mergeCommit":null,"statusCheckRollup":[%s]}\n' "$head" "$mergeable" "$checks" ;;
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
    git merge --quiet --no-ff -m "${subject:-Merge pull request 7}" -m "${body:-No body.}" "origin/$branch" || exit 1
    git push --quiet origin HEAD:main || exit 1
    git rev-parse HEAD > "$state/merged"
    echo "$head" > "$state/merged-head" ;;
  *) echo "fake gh: unexpected call: $*" >&2; exit 1 ;;
esac
