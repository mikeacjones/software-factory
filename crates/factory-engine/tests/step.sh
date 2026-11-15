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
here=$(cd "$(dirname "$0")" && pwd)
state="$here/state"
mkdir -p "$state"
key="$FACTORY_UNIT-$FACTORY_STEP"
n=$(( $(cat "$state/n-$key" 2>/dev/null || echo 0) + 1 ))
echo "$n" > "$state/n-$key"
scenario=$(cat "$state/scenario" 2>/dev/null)
echo "$(date +%s) $FACTORY_UNIT $FACTORY_KIND $FACTORY_STEP round=$FACTORY_ROUND session=${FACTORY_SESSION:--} note=${FACTORY_NOTE:--} build=${FACTORY_BUILD:--}" >> "$state/calls"

if [ "$FACTORY_KIND" = agent ]; then
  echo "session ${FACTORY_SESSION:-sess-$key-$n}"
fi

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
