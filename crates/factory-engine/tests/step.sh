#!/bin/sh
# A scripted stand-in for the work of a step, for the engine's tests. It
# speaks the protocol of `ExecRunner`: `session <id>`, then `outcome <word>`
# or `limited <seconds>`.
#
# State lives beside the script, so each test uses its own copy:
#   state/calls      one line per call
#   state/scenario   words that change behaviour:
#     break-once     the tester breaks the change the first time
#     test-fails     the test command fails until a fix was given a note
#     hang-build     the first build reports its session and never ends
#     limited        the first tester run stops at a usage limit for 3 seconds
#     hold-<unit>    that unit's build waits until state/release-<unit> exists
#     limits-<account>  an agent run under that account stops at a usage limit
#
# Asked for the accounts (FACTORY_KIND=accounts) it answers with
# state/picture.json and notes the check in state/checks. A step handed an
# account reads that account's token from state/token-<account>, as a real
# run reads it from the keychain, and notes it in state/tokens-used.
here=$(cd "$(dirname "$0")" && pwd)
state="$here/state"
mkdir -p "$state"
if [ "$FACTORY_KIND" = accounts ]; then
  echo "$(date +%s) forget=${FACTORY_FORGET:--}" >> "$state/checks"
  echo "accounts $(tr -d '\n' < "$state/picture.json")"
  exit 0
fi
# Asked to sweep a project's issues (FACTORY_KIND=issues) it notes the sweep
# in state/sweeps and says what it did; `slow-sweep` in the scenario makes it
# take 5 seconds and `sweep-fails` makes one bead fail.
if [ "$FACTORY_KIND" = issues ]; then
  scenario=$(cat "$state/scenario" 2>/dev/null)
  echo "$(date +%s) $FACTORY_PROJECT start" >> "$state/sweeps"
  case "$scenario" in *slow-sweep*) sleep 5 ;; esac
  failed='[]'
  case "$scenario" in *sweep-fails*) failed='["toy-9: gh said no"]' ;; esac
  echo "issues {\"kept\":true,\"open\":3,\"created\":[\"toy-1 https://github.com/me/toy/issues/1\"],\"changed\":[],\"left\":[],\"failed\":$failed}"
  exit 0
fi
key="$FACTORY_UNIT-$FACTORY_STEP"
n=$(( $(cat "$state/n-$key" 2>/dev/null || echo 0) + 1 ))
echo "$n" > "$state/n-$key"
scenario=$(cat "$state/scenario" 2>/dev/null)
echo "$(date +%s) $FACTORY_UNIT $FACTORY_KIND $FACTORY_STEP round=$FACTORY_ROUND session=${FACTORY_SESSION:--} note=${FACTORY_NOTE:--} build=${FACTORY_BUILD:--}${FACTORY_ACCOUNT:+ account=$FACTORY_ACCOUNT}" >> "$state/calls"
if [ -n "$FACTORY_ACCOUNT" ] && [ -f "$state/token-$FACTORY_ACCOUNT" ]; then
  cat "$state/token-$FACTORY_ACCOUNT" >> "$state/tokens-used"
fi

if [ "$FACTORY_KIND" = agent ]; then
  echo "session ${FACTORY_SESSION:-sess-$key-$n}"
fi

case "$scenario" in
  *"limits-${FACTORY_ACCOUNT:-none}"*)
    if [ "$FACTORY_KIND" = agent ]; then echo "limited 600"; exit 0; fi ;;
esac
case "$scenario" in
  *hang-build*)
    if [ "$FACTORY_STEP" = build ] && [ -z "$FACTORY_SESSION" ]; then
      echo $$ > "$state/hung.pid"
      exec sleep 300
    fi ;;
esac
case "$scenario" in
  *"hold-$FACTORY_UNIT"*)
    if [ "$FACTORY_STEP" = build ]; then
      touch "$state/holding-$FACTORY_UNIT"
      while [ ! -f "$state/release-$FACTORY_UNIT" ]; do sleep 0.2; done
    fi ;;
esac

outcome=""
case "$FACTORY_STEP" in
  build|fix) outcome=done ;;
  test|regress) outcome=pass ;;
  break) outcome=held ;;
  review) outcome=approve ;;
  describe) outcome=ready ;;
  publish) outcome=done ;;
  reproduce) outcome=reproduced ;;
esac
case "$scenario:$FACTORY_STEP:$n" in
  *break-once*:break:1) outcome=broken ;;
  *limited*:break:1) echo "limited 3"; exit 0 ;;
esac
case "$scenario:$FACTORY_STEP" in
  *test-fails*:fix) [ -n "$FACTORY_NOTE" ] && touch "$state/fixed" ;;
  *test-fails*:test) [ -f "$state/fixed" ] || outcome=fail ;;
esac
echo "outcome $outcome"
