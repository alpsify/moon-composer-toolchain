#!/usr/bin/env bash
# Builds and tests the plugin in the official Rust image, so that the host
# needs nothing but Docker.
#
#   scripts/build.sh         builds dist/composer_toolchain.wasm
#   scripts/build.sh test    runs the unit tests of the composer.json logic
set -euo pipefail

cd "$(dirname "$0")/.."

IMAGE="${RUST_IMAGE:-rust:1.98}"
TARGET=wasm32-wasip1

run() {
  docker run --rm \
    --volume "$PWD":/plugin \
    --volume moon-composer-toolchain-registry:/usr/local/cargo/registry \
    --volume moon-composer-toolchain-target:/target \
    --env CARGO_TARGET_DIR=/target \
    --workdir /plugin \
    "$IMAGE" sh -ec "$1"
}

case "${1:-build}" in
  test)
    # Without the default `wasm` feature, only the composer.json logic is
    # compiled: it runs natively and needs none of the moon crates.
    run "cargo test --no-default-features"
    ;;
  build)
    mkdir -p dist
    run "rustup target add $TARGET >/dev/null 2>&1 && cargo build --release --target $TARGET && cp /target/$TARGET/release/composer_toolchain.wasm dist/"
    ls -lh dist/composer_toolchain.wasm
    ;;
  *)
    echo "Usage: $0 [build|test]" >&2
    exit 1
    ;;
esac
