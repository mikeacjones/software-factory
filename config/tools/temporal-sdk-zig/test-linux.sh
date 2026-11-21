#!/bin/sh
# Run the project's unit tests on Linux, in containers, from the checkout in
# the current directory. For a machine that is not Linux, such as the Mac the
# factory runs on: GitHub's CI tests Linux, and a failure there cannot
# otherwise be seen to fail, or to pass, before a change is pushed.
#
#   test-linux.sh [configuration...] [-- more `zig build test` arguments...]
#
# A configuration is one row of CI's Linux matrix:
#
#   Debug  ReleaseSafe              the native (glibc) target
#   musl-Debug  musl-ReleaseSafe    the musl target, statically linked
#   all                             those four
#
# With none named it runs Debug. Several run at the same time, each in a
# container of its own, as far as the memory Docker has allows: each
# configuration books what it was measured to take at its worst, plus 15%,
# from one ledger that every test-linux.sh on this host shares, and those
# that do not fit run in turn, so that two runs at once (a builder's and a
# ship's) queue rather than overcommit. Each container is held to its
# booking with --memory. TEST_LINUX_JOBS, if set, is the most of this run's
# to run at once; TEST_LINUX_LEDGER, if set, is the ledger's directory in
# place of the host's (for a test of this script). Each test may run for
# TEST_LINUX_TEST_TIMEOUT (10m unless set, or a --test-timeout after `--`)
# before it fails as hung. Asked to stop with SIGTERM, it stops its
# containers and prints what each configuration got to, keeping the logs.
#
# It needs Docker. The containers are the architecture Docker runs natively
# (arm64 on Apple silicon; CI's x86_64 rows are not covered) and are set up
# as the project's CI sets up its Linux runners: Ubuntu 24.04, the Zig
# release build.zig.zon names, and the Rust toolchain deps/rust-toolchain.toml
# pins. The checkout is copied in as it stands, uncommitted changes included,
# and nothing is written back to it. Builds are kept between runs in Docker
# volumes, so the first run is slow (it compiles the Rust core) and later
# ones are not.
#
# What it prints ends with one line for each configuration and, for each
# that failed, the errors from its log. The whole logs are kept when
# anything fails, and it says where.
#
# Exit status: 0 if every configuration passed; 1 if any failed its tests,
# whatever happened to the others; 2 only if every configuration that did
# not pass could not be run (the host's fault, not the change's).
set -eu

configs=
while [ $# -gt 0 ]; do
  case "$1" in
    --) shift; break ;;
    all) configs="$configs Debug ReleaseSafe musl-Debug musl-ReleaseSafe" ;;
    Debug|ReleaseSafe|musl-Debug|musl-ReleaseSafe) configs="$configs $1" ;;
    *)
      echo "test-linux: \`$1\` is not a configuration (Debug, ReleaseSafe, musl-Debug, musl-ReleaseSafe, all); other arguments go after \`--\`" >&2
      exit 2 ;;
  esac
  shift
done
[ -n "$configs" ] || configs=Debug
# Each one once, in the order named.
configs=$(printf '%s\n' $configs | awk '!seen[$0]++' | tr '\n' ' ')

if [ ! -f build.zig ] || [ ! -f build.zig.zon ]; then
  echo "test-linux: run it from the root of a checkout (no build.zig here)" >&2
  exit 2
fi
if ! docker info >/dev/null 2>&1; then
  echo "test-linux: Docker is not running on this machine" >&2
  exit 2
fi

zig_version=$(sed -n 's/.*minimum_zig_version *= *"\([^"]*\)".*/\1/p' build.zig.zon | head -n 1)
rust_channel=$(sed -n 's/^channel *= *"\([^"]*\)".*/\1/p' deps/rust-toolchain.toml | head -n 1)
arch=$(docker info --format '{{.Architecture}}')
case "$arch" in
  arm64) arch=aarch64 ;;
  amd64) arch=x86_64 ;;
esac
if [ -z "$zig_version" ] || [ -z "$rust_channel" ]; then
  echo "test-linux: could not read the Zig version or the Rust channel from the checkout" >&2
  exit 2
fi

# One image for each pair of toolchains, built the first time it is needed.
image="zig-linux-test:zig$zig_version-rust$rust_channel-$arch-2"
if ! docker image inspect "$image" >/dev/null 2>&1; then
  echo "test-linux: building the image $image (once)" >&2
  docker build --quiet --tag "$image" \
    --build-arg "ZIG_VERSION=$zig_version" \
    --build-arg "RUST_CHANNEL=$rust_channel" \
    --build-arg "ARCH=$arch" - >&2 <<'DOCKERFILE'
FROM ubuntu:24.04
ARG ZIG_VERSION
ARG RUST_CHANNEL
ARG ARCH
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update -qq \
 && apt-get install -y -qq --no-install-recommends \
      ca-certificates curl xz-utils git file build-essential cmake pkg-config clang perl python3 \
      musl-tools \
 && rm -rf /var/lib/apt/lists/*
RUN curl -fsSL "https://ziglang.org/download/${ZIG_VERSION}/zig-${ARCH}-linux-${ZIG_VERSION}.tar.xz" \
      | tar -xJ -C /opt \
 && ln -s "/opt/zig-${ARCH}-linux-${ZIG_VERSION}/zig" /usr/local/bin/zig
ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:$PATH
RUN curl -fsSL https://sh.rustup.rs \
      | sh -s -- -y --profile minimal --default-toolchain "${RUST_CHANNEL}" \
 && rustup target add "${ARCH}-unknown-linux-musl"
WORKDIR /work
DOCKERFILE
fi

# Memory, in MiB. Each configuration's peak is the cgroup v2 memory.peak of
# its own container, read from /sys/fs/cgroup/memory.peak inside it as
# `zig build test` ended, run as this script runs it (same image, volumes
# and command, no --memory) on a Mac with Docker Desktop 4.100 (16 CPUs,
# MemTotal 31.27 GiB, kernel 7.0.14-linuxkit) against temporal-sdk-zig at
# d8cd551, all four at once, on 2026-11-21. The caches were empty, so each
# also compiled the Rust core, except ReleaseSafe, whose first run lost a
# race in cargo's registry and was measured again with the core built (its
# cargo build is Debug's, whose peak covers it). memory.peak counts the
# page cache the build writes as well as what the processes hold, so it is
# the larger of the two; what the processes held at most (anon, kernel and
# shmem in memory.stat, sampled every half second or less) is beside it:
#
#   configuration       memory.peak     held by the processes
#   Debug               6486 MiB        3701 MiB (anon only)
#   ReleaseSafe         6690 MiB        6512 MiB
#   musl-Debug          5314 MiB        3684 MiB (anon only)
#   musl-ReleaseSafe    7097 MiB        6685 MiB (anon only)
#
# Docker's VM with nothing running used 676 MiB, and 287 MiB of cache, as
# `free -b` in an ubuntu:24.04 container counts them (its /proc/meminfo is
# the VM's). The allowance is that, rounded up, with room for Docker's own
# daemons to grow.
peak() {
  case "$1" in
    Debug) echo 6486 ;;
    ReleaseSafe) echo 6690 ;;
    musl-Debug) echo 5314 ;;
    musl-ReleaseSafe) echo 7097 ;;
  esac
}
vm_allowance=1536

# What a configuration books, and the most its container may use: its
# measured peak and 15% more, and never more than all there is.
need() {
  echo $(( ($(peak "$1") * 115 + 99) / 100 ))
}
budget=$(( $(docker info --format '{{.MemTotal}}') / 1048576 - vm_allowance ))
if [ "$budget" -le 0 ]; then
  echo "test-linux: Docker has no more memory than its VM's own allowance ($vm_allowance MiB)" >&2
  exit 2
fi
booking() {
  mib=$(need "$1")
  [ "$mib" -le "$budget" ] || mib=$budget
  echo "$mib"
}

# One ledger for every test-linux.sh on this host, since they share one
# Docker: a run that assumed it had all of Docker's memory would overcommit
# beside another, and the kernel would kill what did not fit. It is a fixed
# path, not under TMPDIR, which differs between the factory's processes.
# Each line is one booking: the pid of the run that made it, when that
# process started (a pid can be reused), the container's name, and MiB. It
# is read and written only under an flock(2) of its lock file, which the
# kernel releases when the process holding it dies, however it dies.
ledger=${TEST_LINUX_LEDGER:-/tmp/test-linux-ledger-$(id -u)}
started() {
  ps -o lstart= -p "$1" 2>/dev/null | sed 's/^ *//; s/ *$//; s/  */ /g' | tr ' ' '_'
}
me_started=$(started $$)
[ -n "$me_started" ] || me_started=-
# Runs its arguments with the ledger locked, in a subshell, so that the
# lock goes when they end. It exits 3 if the ledger cannot be locked.
locked() {
  (
    exec 9>>"$ledger/lock" || exit 3
    if command -v flock >/dev/null 2>&1; then
      flock 9 || exit 3
    elif command -v lockf >/dev/null 2>&1; then
      lockf -s 9 || exit 3
    else
      perl -MFcntl=:flock -e 'open(my $f, ">&=", 9) && flock($f, LOCK_EX) or exit 1' || exit 3
    fi
    [ -f "$ledger/bookings" ] || : >"$ledger/bookings"
    "$@"
  )
}
# Drops the bookings of runs that have ended without releasing them (killed
# with SIGKILL, say), and stops their containers, which nobody is waiting
# for any more.
tidy() {
  : >"$ledger/bookings.new"
  checked=
  alive=
  while read -r pid start name mib; do
    [ -n "$mib" ] || continue
    if [ "$pid" != "$checked" ]; then
      checked=$pid
      alive=
      if [ "$pid" = $$ ] || { kill -0 "$pid" 2>/dev/null && { [ "$start" = - ] || [ "$(started "$pid")" = "$start" ]; }; }; then
        alive=1
      fi
    fi
    if [ -n "$alive" ]; then
      echo "$pid $start $name $mib" >>"$ledger/bookings.new"
    else
      echo "test-linux: $name ($mib MiB) was booked by a run that has ended (pid $pid); taking it back" >&2
      docker kill "$name" >/dev/null 2>&1 || true
    fi
  done <"$ledger/bookings"
  mv "$ledger/bookings.new" "$ledger/bookings"
}
# Books MiB for a container if the ledger has room for it, or returns 1.
book() {
  tidy
  used=$(awk '{ used += $4 } END { print used + 0 }' "$ledger/bookings")
  [ $((used + $2)) -le "$budget" ] || return 1
  echo "$$ $me_started $1 $2" >>"$ledger/bookings"
}
# Releases this run's bookings of the containers named, or all of them.
release() {
  awk -v pid=$$ -v names=" $* " \
    '!($1 == pid && (names == "  " || index(names, " " $3 " ")))' \
    "$ledger/bookings" >"$ledger/bookings.new"
  mv "$ledger/bookings.new" "$ledger/bookings"
}
if ! mkdir -p "$ledger" || ! locked true; then
  echo "test-linux: cannot use the memory ledger in $ledger" >&2
  exit 2
fi

work=$(mktemp -d "${TMPDIR:-/tmp}/test-linux.XXXXXX")
run=$(basename "$work" | tr -c 'A-Za-z0-9\n' '-')
# However it ends, what it booked goes back.
trap 'locked release || true' EXIT

# A process the kernel killed, or a container that never ran, says nothing
# about the change under test.
could_not_run() {
  [ "$1" -ge 125 ] || grep -Eq 'signal KILL|Out of memory|No space left on device' "$2"
}

# Containers outlive the script that started them unless they are stopped.
stop() {
  for config in $configs; do
    docker kill "$run-$config" >/dev/null 2>&1 || true
  done
}
# Asked to stop (the factory does at a command's time limit): stop the
# containers, then say what each configuration got to, so that whoever reads
# the end of the output knows which test it was on. The logs are kept. It
# exits 1 if one had already failed its tests, and 2 if none had.
stopped() {
  stop
  wait 2>/dev/null || true
  echo "===== test-linux: stopped before it ended ====="
  failed=0
  for config in $configs; do
    [ -f "$work/$config.log" ] || continue
    if [ -f "$work/$config.status" ]; then
      read -r status secs <"$work/$config.status" || status=125
      if [ "$status" -ne 0 ] && ! could_not_run "$status" "$work/$config.log"; then
        echo "$config: had ended, and FAILED (exit $status)."
        failed=1
      else
        echo "$config: had ended (exit $status)."
      fi
    else
      echo "$config: still running. What failed so far, then its last lines:"
      grep -E '^error: ' "$work/$config.log" | cut -c1-400 | head -n 20
      tail -n 15 "$work/$config.log" | cut -c1-400
    fi
  done
  rm -f "$work/src.tar"
  echo "The whole logs are in $work"
  [ "$failed" -eq 0 ] || exit 1
  exit 2
}
trap stopped TERM
trap 'stop; rm -rf "$work"; exit 2' INT HUP

# The checkout goes in as an archive: a build over a shared folder is slow,
# and this way a container cannot change the checkout. The caches are the
# containers' own: a Linux build must not share a cargo target directory
# with the host's.
export COPYFILE_DISABLE=1
tar -cf "$work/src.tar" \
    --exclude ./.git --exclude ./.zig-cache --exclude ./zig-out --exclude ./worktrees \
    .

# One configuration, in its own container. Zig's caches and cargo's are
# shared by all of them and lock what they write, so the core is compiled
# once and the others wait for it. The container may use no more memory
# than its booking, which it gives back when it ends.
one() {
  config=$1
  mib=$2
  shift 2
  case "$config" in
    musl-*) target=$arch-linux-musl; optimize=${config#musl-} ;;
    *) target=; optimize=$config ;;
  esac
  began=$(date +%s)
  status=0
  docker run --rm -i --init --name "$run-$config" --memory "${mib}m" \
      -v zig-linux-test-local:/work/.zig-cache \
      -v zig-linux-test-global:/root/.cache/zig \
      -v zig-linux-test-cargo-registry:/opt/cargo/registry \
      -v zig-linux-test-cargo-git:/opt/cargo/git \
      -e "OPTIMIZE=$optimize" -e "TARGET=$target" -e "ARCH=$arch" \
      -e "TEST_TIMEOUT=${TEST_LINUX_TEST_TIMEOUT:-10m}" \
      "$image" sh -euc '
        tar -xf - -C /work 2>/dev/null
        # Copying stamps every file with the current time, and cargo would
        # rebuild the core on every run. Its sources are pinned: give them
        # a fixed old time, as CI does.
        find deps -exec touch -h -t 200001010000 {} +
        if [ -n "$TARGET" ]; then
          # cc-rs compiles the C that the core depends on with this.
          export "CC_${ARCH}_unknown_linux_musl=musl-gcc"
          set -- "-Dtarget=$TARGET" "$@"
        fi
        # A test that hangs fails by name instead of holding the run.
        case " $* " in
          *" --test-timeout "*) ;;
          *) set -- --test-timeout "$TEST_TIMEOUT" "$@" ;;
        esac
        echo "test-linux: $(uname -sm), zig $(zig version), $(rustc --version), -Doptimize=$OPTIMIZE ${TARGET:+-Dtarget=$TARGET}" >&2
        exec zig build test "-Doptimize=$OPTIMIZE" --summary all "$@"
      ' sh "$@" <"$work/src.tar" >"$work/$config.log" 2>&1 || status=$?
  locked release "$run-$config" || true
  echo "$status $(( $(date +%s) - began ))" >"$work/$config.status"
}

# Start each configuration as soon as the ledger has room for it beside
# what this run and the others on the host have booked. One that is killed
# for want of memory looks like a failure of the change, so they are not all
# simply started together.
jobs=${TEST_LINUX_JOBS:-0}
wanted=0
for config in $configs; do
  wanted=$((wanted + $(booking "$config")))
done
if [ "$wanted" -gt "$budget" ]; then
  echo "test-linux: Docker has $budget MiB for tests and these would take $wanted MiB at once; those that do not fit together run in turn" >&2
fi
pending=$configs
running=
waiting=
while [ -n "$pending$running" ]; do
  still=
  count=0
  for config in $running; do
    if [ ! -f "$work/$config.status" ]; then
      still="$still $config"
      count=$((count + 1))
    fi
  done
  running=$still
  rest=
  for config in $pending; do
    mib=$(booking "$config")
    if [ "$jobs" -gt 0 ] && [ "$count" -ge "$jobs" ]; then
      rest="$rest $config"
    elif locked book "$run-$config" "$mib"; then
      one "$config" "$mib" "$@" &
      running="$running $config"
      count=$((count + 1))
    elif [ $? -eq 1 ]; then
      rest="$rest $config"
    else
      echo "test-linux: cannot lock the memory ledger in $ledger" >&2
      stop
      exit 2
    fi
  done
  pending=$rest
  if [ -n "$pending" ] && [ -z "$running" ] && [ -z "$waiting" ]; then
    echo "test-linux: waiting for memory that other test-linux runs on this host have booked" >&2
    waiting=1
  fi
  [ -z "$pending$running" ] || sleep 1
done
wait
rm -f "$work/src.tar"

failed=0
unrun=0
for config in $configs; do
  read -r status secs <"$work/$config.status" || { status=125; secs=0; }
  if [ "$status" -ne 0 ]; then
    if could_not_run "$status" "$work/$config.log"; then
      unrun=$((unrun + 1))
    else
      failed=$((failed + 1))
    fi
    echo "===== $config: the whole log ====="
    cat "$work/$config.log"
  fi
done

# The summary comes last, where a reader of only the end finds it.
echo "===== test-linux: Linux $arch ====="
bad=$((failed + unrun))
for config in $configs; do
  read -r status secs <"$work/$config.status" || { status=125; secs=0; }
  summary=$(grep -E '^Build Summary:' "$work/$config.log" | tail -n 1 || true)
  if [ "$status" -eq 0 ]; then
    echo "$config: passed in ${secs}s. $summary"
  elif could_not_run "$status" "$work/$config.log"; then
    echo "$config: COULD NOT RUN (exit $status after ${secs}s): a process was killed or the container did not start; this is not a test failure. Last lines:"
    tail -n 8 "$work/$config.log" | cut -c1-400
  else
    echo "$config: FAILED (exit $status after ${secs}s). $summary"
    # What failed, and each panic or compile error with the trace under
    # it, within what a reader of only the end is shown. The tests' own
    # warnings, which fill the log, are left out.
    awk '/^error: / { print NR ": " $0; next }
         /panic: |: error: / { more = 15 }
         more > 0 { print NR ": " $0; more-- }' "$work/$config.log" \
      | cut -c1-400 | head -n $((100 / bad))
  fi
done

if [ "$bad" -eq 0 ]; then
  rm -rf "$work"
  exit 0
fi
echo "The whole logs are in $work"
[ "$failed" -eq 0 ] || exit 1
exit 2
