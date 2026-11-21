#!/bin/sh
# The factory's test-linux.sh for the Zig project, run without Docker: a
# stand-in `docker` on PATH plays each configuration's container, sleeping
# for a while and then passing, failing, being killed or not starting as
# the test says. It shows that runs started together never book more than
# Docker's budget, that bookings go back when a run ends however it ends,
# and that the exit status says whose fault a failure was.
#
#   sh test-linux.sh <path to config/tools/temporal-sdk-zig/test-linux.sh>
#
# TEST_LINUX_SHELL is the shell to run it with (sh unless set).
set -eu

script=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
shell=${TEST_LINUX_SHELL:-sh}
tmp=$(mktemp -d "${TMPDIR:-/tmp}/test-linux-test.XXXXXX")
tmp=$(cd "$tmp" && pwd -P)
started_pids=
cleanup() {
  for pid in $started_pids; do
    kill "$pid" 2>/dev/null || true
  done
  # Whatever stand-in containers are left.
  for file in "$tmp"/docker/pid-*; do
    [ -f "$file" ] && kill "$(cat "$file")" 2>/dev/null || true
  done
  rm -rf "$tmp"
}
trap cleanup EXIT

failures=0
fail() {
  echo "FAIL: $*" >&2
  failures=$((failures + 1))
}
pass() {
  echo "ok: $*"
}

export TEST_LINUX_LEDGER="$tmp/ledger"
export FAKE_DOCKER="$tmp/docker"
mkdir -p "$tmp/bin" "$tmp/docker" "$tmp/tmp" "$tmp/checkout/deps"
export TMPDIR="$tmp/tmp"
PATH="$tmp/bin:$PATH"
export PATH

cat >"$tmp/checkout/build.zig" <<'EOF'
EOF
cat >"$tmp/checkout/build.zig.zon" <<'EOF'
.{ .minimum_zig_version = "0.17.1" }
EOF
cat >"$tmp/checkout/deps/rust-toolchain.toml" <<'EOF'
[toolchain]
channel = "1.95"
EOF

# The stand-in for docker. FAKE_MEMTOTAL is the memory it says it has, in
# bytes; FAKE_SECS how long a container runs; FAKE_OUTCOMES what each
# configuration's container does ("Debug=fail musl-Debug=killed", pass
# unless named). Each container's start and end go to $FAKE_DOCKER/events
# with its --memory and what the ledger held as it started.
cat >"$tmp/bin/docker" <<'EOF'
#!/bin/sh
d=$FAKE_DOCKER
case "$1" in
  info)
    case "$*" in
      *MemTotal*) echo "$FAKE_MEMTOTAL" ;;
      *Architecture*) echo arm64 ;;
    esac
    exit 0 ;;
  image) exit 0 ;;
  kill)
    echo "$2" >>"$d/killed"
    if [ -f "$d/pid-$2" ]; then
      kill "$(cat "$d/pid-$2")" 2>/dev/null || true
    fi
    exit 0 ;;
  run) shift ;;
  *) echo "fake docker: $*" >&2; exit 99 ;;
esac
name=
memory=
while [ $# -gt 0 ]; do
  case "$1" in
    --name) name=$2; shift 2 ;;
    --memory) memory=$2; shift 2 ;;
    -v|-e) shift 2 ;;
    -*) shift ;;
    *) break ;;
  esac
done
cat >/dev/null
case "$name" in
  *-musl-ReleaseSafe) config=musl-ReleaseSafe ;;
  *-musl-Debug) config=musl-Debug ;;
  *-ReleaseSafe) config=ReleaseSafe ;;
  *-Debug) config=Debug ;;
esac
outcome=pass
for pair in ${FAKE_OUTCOMES:-}; do
  [ "${pair%%=*}" = "$config" ] && outcome=${pair#*=}
done
mib=${memory%m}
booked=$(awk -v name="$name" '$3 == name { print $4 }' "$TEST_LINUX_LEDGER/bookings")
total=$(awk '{ total += $4 } END { print total + 0 }' "$TEST_LINUX_LEDGER/bookings")
echo "start $name ${mib:-none} booked=${booked:-none} total=$total" >>"$d/events"
echo $$ >"$d/pid-$name"
ended() {
  echo "end $name ${mib:-none}" >>"$d/events"
  rm -f "$d/pid-$name"
}
trap 'kill $sleeper 2>/dev/null; ended; exit 137' TERM
sleep "${FAKE_SECS:-1}" &
sleeper=$!
wait $sleeper
ended
case "$outcome" in
  pass) echo "Build Summary: 3/3 steps succeeded"; exit 0 ;;
  fail) echo "error: 'root.test.something' failed"; echo "Build Summary: 2/3 steps succeeded (1 failed)"; exit 1 ;;
  killed) echo "error: the following command terminated with signal KILL"; exit 1 ;;
  unrun) echo "docker: Error response from daemon: no such image"; exit 125 ;;
esac
EOF
chmod +x "$tmp/bin/docker"

allowance=$(sed -n 's/^vm_allowance=\([0-9]*\)$/\1/p' "$script")
[ -n "$allowance" ] || { echo "no vm_allowance= in $script" >&2; exit 1; }
# Docker's MemTotal, in bytes, for a budget of MiB.
memtotal_for() {
  echo $(( ($1 + allowance) * 1048576 ))
}

# Runs test-linux.sh from the stand-in checkout; its output is $tmp/out.<label>.
# A command a script starts in the background ignores SIGINT, and a shell
# cannot trap a signal it started with ignored: perl gives it back.
start_run() {
  label=$1
  shift
  (cd "$tmp/checkout" && exec perl -e '$SIG{INT} = "DEFAULT"; exec @ARGV or die' "$shell" "$script" "$@") >"$tmp/out.$label" 2>&1 &
  started_pids="$started_pids $!"
  last_pid=$!
}
run_status() {
  label=$1
  shift
  start_run "$label" "$@"
  status=0
  wait "$last_pid" || status=$?
  echo "$status"
}
bookings() {
  awk 'END { print NR }' "$TEST_LINUX_LEDGER/bookings" 2>/dev/null || echo 0
}
# Waits up to ten seconds for a container to have started.
await_start() {
  i=0
  while ! grep -q "^start .*$1" "$FAKE_DOCKER/events" 2>/dev/null; do
    i=$((i + 1))
    [ "$i" -lt 100 ] || return 1
    sleep 0.1
  done
}
reset() {
  rm -f "$FAKE_DOCKER"/events "$FAKE_DOCKER"/killed
}

# What each configuration books, taken from one run with room for all.
export FAKE_MEMTOTAL=$(memtotal_for 1000000)
export FAKE_SECS=0
reset
status=$(run_status alone all)
[ "$status" -eq 0 ] || fail "a run where all pass exits $status, not 0: $(cat "$tmp/out.alone")"
one_run=0
for config in Debug ReleaseSafe musl-Debug musl-ReleaseSafe; do
  line=$(grep "^start test-linux-[A-Za-z0-9]*-$config " "$FAKE_DOCKER/events" || true)
  set -- $line
  if [ $# -ne 5 ] || [ "$3" = none ] || [ "$4" != "booked=$3" ]; then
    fail "$config's container did not run with --memory equal to its booking: $line"
    continue
  fi
  one_run=$((one_run + $3))
done
[ "$(bookings)" -eq 0 ] || fail "a run that ended left bookings: $(cat "$TEST_LINUX_LEDGER/bookings")"
pass "all passed: exit 0, each container held to its booking ($one_run MiB for the four)"

# Two runs at once, with room for one run's four and no more: never more
# booked than the budget, by the ledger or by the containers running.
budget=$one_run
export FAKE_MEMTOTAL=$(memtotal_for "$budget")
export FAKE_SECS=1
reset
(
  while :; do
    awk '{ total += $4 } END { print total + 0 }' "$TEST_LINUX_LEDGER/bookings" 2>/dev/null
    sleep 0.05
  done
) >"$tmp/samples" &
sampler=$!
started_pids="$started_pids $sampler"
start_run first all
first=$last_pid
start_run second all
second=$last_pid
first_status=0
wait "$first" || first_status=$?
second_status=0
wait "$second" || second_status=$?
kill "$sampler" 2>/dev/null || true
wait "$sampler" 2>/dev/null || true
[ "$first_status" -eq 0 ] || fail "the first of two runs exits $first_status: $(cat "$tmp/out.first")"
[ "$second_status" -eq 0 ] || fail "the second of two runs exits $second_status: $(cat "$tmp/out.second")"
starts=$(grep -c '^start ' "$FAKE_DOCKER/events" || true)
[ "$starts" -eq 8 ] || fail "two runs of four started $starts containers, not 8"
most_running=$(awk '$1 == "start" { now += $3; if (now > most) most = now } $1 == "end" { now -= $3 } END { print most + 0 }' "$FAKE_DOCKER/events")
most_seen=$(awk '$1 == "start" { split($5, t, "="); if (t[2] > most) most = t[2] } END { print most + 0 }' "$FAKE_DOCKER/events")
most_sampled=$(awk '$1 > most { most = $1 } END { print most + 0 }' "$tmp/samples")
[ "$most_running" -le "$budget" ] || fail "containers using $most_running MiB ran at once, over the budget of $budget MiB"
[ "$most_seen" -le "$budget" ] || fail "$most_seen MiB was booked as a container started, over the budget of $budget MiB"
[ "$most_sampled" -le "$budget" ] || fail "$most_sampled MiB was booked at one moment, over the budget of $budget MiB"
[ "$most_sampled" -gt 0 ] || fail "the ledger was never seen to hold a booking"
! grep '^start [^ ]* none ' "$FAKE_DOCKER/events" || fail "a container ran without --memory"
[ "$(bookings)" -eq 0 ] || fail "two runs that ended left bookings: $(cat "$TEST_LINUX_LEDGER/bookings")"
pass "two runs at once: at most $most_sampled MiB booked and $most_running MiB running, within $budget MiB"

# The exit status: 1 if any configuration really failed, whatever the
# others did; 2 only if every one that did not pass could not run.
export FAKE_MEMTOTAL=$(memtotal_for 1000000)
export FAKE_SECS=0
check_exit() {
  want=$1
  outcomes=$2
  shift 2
  reset
  status=$(FAKE_OUTCOMES=$outcomes run_status exit "$@")
  if [ "$status" -eq "$want" ]; then
    pass "exit $want with $outcomes"
  else
    fail "with $outcomes it exits $status, not $want: $(tail -n 20 "$tmp/out.exit")"
  fi
  [ "$(bookings)" -eq 0 ] || fail "with $outcomes it left bookings"
}
check_exit 1 "Debug=fail" all
check_exit 1 "Debug=fail ReleaseSafe=killed" all
check_exit 1 "Debug=killed musl-Debug=unrun musl-ReleaseSafe=fail" all
check_exit 1 "Debug=unrun ReleaseSafe=fail" Debug ReleaseSafe
check_exit 2 "Debug=killed" all
check_exit 2 "Debug=killed ReleaseSafe=pass musl-Debug=unrun" all
check_exit 0 "" Debug ReleaseSafe

# Stopped by a signal, it stops its containers and gives back its bookings.
export FAKE_SECS=60
for signal in TERM INT HUP; do
  reset
  start_run signal Debug ReleaseSafe
  pid=$last_pid
  if ! await_start -ReleaseSafe; then
    fail "SIG$signal: the run's containers did not start: $(cat "$tmp/out.signal")"
    kill "$pid" 2>/dev/null || true
    continue
  fi
  kill -s "$signal" "$pid"
  status=0
  wait "$pid" || status=$?
  [ "$status" -eq 2 ] || fail "stopped by SIG$signal it exits $status, not 2: $(cat "$tmp/out.signal")"
  grep -q -- '-ReleaseSafe$' "$FAKE_DOCKER/killed" 2>/dev/null || fail "SIG$signal did not stop the containers"
  [ "$(bookings)" -eq 0 ] || fail "stopped by SIG$signal it left bookings: $(cat "$TEST_LINUX_LEDGER/bookings")"
  pass "SIG$signal: containers stopped, bookings released, exit $status"
done

# A run killed outright cannot give back what it booked: the next run takes
# it back, and stops the containers nobody is waiting for. The budget has
# room for one run's Debug and no more.
export FAKE_SECS=60
reset
budget=$one_run
export FAKE_MEMTOTAL=$(memtotal_for "$budget")
start_run killed all
pid=$last_pid
if await_start -musl-ReleaseSafe; then
  kill -s KILL "$pid"
  wait "$pid" 2>/dev/null || true
  held=$(bookings)
  [ "$held" -gt 0 ] || fail "a run killed outright had nothing booked to take back"
  export FAKE_SECS=0
  status=$(run_status after all)
  [ "$status" -eq 0 ] || fail "the run after a killed one exits $status: $(cat "$tmp/out.after")"
  grep -q "taking it back" "$tmp/out.after" || fail "the run after a killed one did not say it took bookings back: $(cat "$tmp/out.after")"
  grep -q -- '-musl-ReleaseSafe$' "$FAKE_DOCKER/killed" 2>/dev/null || fail "the killed run's containers were not stopped"
  [ "$(bookings)" -eq 0 ] || fail "bookings are left after a killed run's were taken back: $(cat "$TEST_LINUX_LEDGER/bookings")"
  pass "a killed run's $held bookings taken back by the next"
else
  fail "the run to be killed did not start its containers: $(cat "$tmp/out.killed")"
fi

# A booking whose pid is now another process's is taken back too.
export FAKE_SECS=0
reset
mkdir -p "$TEST_LINUX_LEDGER"
echo "$$ Mon_Jan_1_00:00:00_2001 test-linux-reused-Debug $budget" >"$TEST_LINUX_LEDGER/bookings"
status=$(run_status reused Debug)
[ "$status" -eq 0 ] || fail "with a booking of a reused pid it exits $status: $(cat "$tmp/out.reused")"
grep -q 'test-linux-reused-Debug' "$FAKE_DOCKER/killed" 2>/dev/null || fail "the reused pid's booking was not taken back"
[ "$(bookings)" -eq 0 ] || fail "a reused pid's booking is left: $(cat "$TEST_LINUX_LEDGER/bookings")"
pass "a reused pid's booking taken back"

if [ "$failures" -gt 0 ]; then
  echo "$failures failed" >&2
  exit 1
fi
echo "all passed"
