#!/bin/sh
# Run the project's unit tests on Linux, in a container, from the checkout in
# the current directory. For a machine that is not Linux, such as the Mac the
# factory runs on: GitHub's CI tests Linux, and a failure there cannot
# otherwise be seen to fail, or to pass, before a change is pushed.
#
#   test-linux.sh [Debug|ReleaseSafe] [more `zig build test` arguments...]
#
# It needs Docker. The container is the architecture Docker runs natively
# (arm64 on Apple silicon) and is set up as the project's CI sets up its
# Linux runners: Ubuntu 24.04, the Zig release build.zig.zon names, and the
# Rust toolchain deps/rust-toolchain.toml pins. The checkout is copied in as
# it stands, uncommitted changes included, and nothing is written back to
# it. Builds are kept between runs in Docker volumes, so the first run is
# slow (it compiles the Rust core) and later ones are not.
#
# Exit status: that of `zig build test`, or 2 if it could not be run.
set -eu

optimize=Debug
if [ $# -gt 0 ]; then
  optimize=$1
  shift
fi
case "$optimize" in
  Debug|ReleaseSafe|ReleaseFast|ReleaseSmall) ;;
  *) echo "test-linux: the first argument is the optimization mode, not \`$optimize\`" >&2; exit 2 ;;
esac
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
image="zig-linux-test:zig$zig_version-rust$rust_channel-$arch-1"
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
 && rm -rf /var/lib/apt/lists/*
RUN curl -fsSL "https://ziglang.org/download/${ZIG_VERSION}/zig-${ARCH}-linux-${ZIG_VERSION}.tar.xz" \
      | tar -xJ -C /opt \
 && ln -s "/opt/zig-${ARCH}-linux-${ZIG_VERSION}/zig" /usr/local/bin/zig
ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:$PATH
RUN curl -fsSL https://sh.rustup.rs \
      | sh -s -- -y --profile minimal --default-toolchain "${RUST_CHANNEL}"
WORKDIR /work
DOCKERFILE
fi

# The checkout goes in through a pipe: a build over a shared folder is slow,
# and this way the container cannot change the checkout. The caches are the
# container's own: a Linux build must not share a cargo target directory
# with the host's.
export COPYFILE_DISABLE=1
tar -cf - \
    --exclude ./.git --exclude ./.zig-cache --exclude ./zig-out --exclude ./worktrees \
    . \
  | docker run --rm -i \
      -v zig-linux-test-local:/work/.zig-cache \
      -v zig-linux-test-global:/root/.cache/zig \
      -v zig-linux-test-cargo-registry:/opt/cargo/registry \
      -v zig-linux-test-cargo-git:/opt/cargo/git \
      -e "OPTIMIZE=$optimize" \
      "$image" sh -euc '
        tar -xf - -C /work 2>/dev/null
        # Copying stamps every file with the current time, and cargo would
        # rebuild the core on every run. Its sources are pinned: give them
        # a fixed old time, as CI does.
        find deps -exec touch -h -t 200001010000 {} +
        echo "test-linux: $(uname -sm), zig $(zig version), $(rustc --version), -Doptimize=$OPTIMIZE" >&2
        exec zig build test "-Doptimize=$OPTIMIZE" --summary all "$@"
      ' sh "$@"
