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
# container of its own, as far as the memory Docker has allows: compiling
# the tests takes about 3 GiB in Debug and 6 GiB in ReleaseSafe, and those
# that do not fit together run in turn. With 18 GiB or more all four run at
# once and cost little more time than the slowest. TEST_LINUX_JOBS, if set,
# is the most to run at once.
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
# Exit status: 0 if every configuration passed, 1 if any failed its tests,
# 2 if any could not be run.
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

work=$(mktemp -d "${TMPDIR:-/tmp}/test-linux.XXXXXX")
run=$(basename "$work" | tr -c 'A-Za-z0-9\n' '-')

# Containers outlive the script that started them unless they are stopped.
stop() {
  for config in $configs; do
    docker kill "$run-$config" >/dev/null 2>&1 || true
  done
}
trap 'stop; rm -rf "$work"; exit 2' INT TERM HUP

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
# once and the others wait for it.
one() {
  config=$1
  shift
  case "$config" in
    musl-*) target=$arch-linux-musl; optimize=${config#musl-} ;;
    *) target=; optimize=$config ;;
  esac
  began=$(date +%s)
  status=0
  docker run --rm -i --init --name "$run-$config" \
      -v zig-linux-test-local:/work/.zig-cache \
      -v zig-linux-test-global:/root/.cache/zig \
      -v zig-linux-test-cargo-registry:/opt/cargo/registry \
      -v zig-linux-test-cargo-git:/opt/cargo/git \
      -e "OPTIMIZE=$optimize" -e "TARGET=$target" -e "ARCH=$arch" \
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
        echo "test-linux: $(uname -sm), zig $(zig version), $(rustc --version), -Doptimize=$OPTIMIZE ${TARGET:+-Dtarget=$TARGET}" >&2
        exec zig build test "-Doptimize=$OPTIMIZE" --summary all "$@"
      ' sh "$@" <"$work/src.tar" >"$work/$config.log" 2>&1 || status=$?
  echo "$status $(( $(date +%s) - began ))" >"$work/$config.status"
}

# How much memory compiling a configuration's tests takes at its worst, in
# GiB, rounded up from what was measured (2.8 and 5.8).
need() {
  case "$1" in
    *ReleaseSafe) echo 6 ;;
    *) echo 3 ;;
  esac
}

# Start each configuration as soon as it fits beside those running. One
# that is killed for want of memory looks like a failure of the change, so
# they are not all simply started together.
budget=$(( $(docker info --format '{{.MemTotal}}') / 1073741824 ))
jobs=${TEST_LINUX_JOBS:-0}
wanted=0
for config in $configs; do
  wanted=$((wanted + $(need "$config")))
done
if [ "$wanted" -gt "$budget" ]; then
  echo "test-linux: Docker has $budget GiB and these would take $wanted GiB at once; those that do not fit together run in turn" >&2
fi
pending=$configs
running=
while [ -n "$pending$running" ]; do
  still=
  used=0
  count=0
  for config in $running; do
    if [ ! -f "$work/$config.status" ]; then
      still="$still $config"
      used=$((used + $(need "$config")))
      count=$((count + 1))
    fi
  done
  running=$still
  rest=
  for config in $pending; do
    fits=$((used + $(need "$config")))
    if [ -z "$running" ] || { [ "$fits" -le "$budget" ] && { [ "$jobs" -le 0 ] || [ "$count" -lt "$jobs" ]; }; }; then
      one "$config" "$@" &
      running="$running $config"
      used=$fits
      count=$((count + 1))
    else
      rest="$rest $config"
    fi
  done
  pending=$rest
  [ -z "$running" ] || sleep 1
done
wait
rm -f "$work/src.tar"

# A process the kernel killed, or a container that never ran, says nothing
# about the change under test.
could_not_run() {
  [ "$1" -ge 125 ] || grep -Eq 'signal KILL|Out of memory|No space left on device' "$2"
}

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
[ "$unrun" -eq 0 ] || exit 2
exit 1
