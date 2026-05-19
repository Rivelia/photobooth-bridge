#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"

IMAGE=docker.io/library/rust:latest
TARGET=x86_64-pc-windows-gnu
OUT_DIR=target-windows

podman run --rm -v "$PWD":/work -w /work "$IMAGE" bash -c "
  set -e
  apt-get update -qq && apt-get install -y -qq mingw-w64 >/dev/null
  rustup target add $TARGET
  mkdir -p .cargo
  cat > .cargo/config.toml <<EOF
[target.$TARGET]
linker = \"x86_64-w64-mingw32-gcc\"
ar = \"x86_64-w64-mingw32-ar\"
EOF
  CARGO_TARGET_DIR=$OUT_DIR cargo build --release --target $TARGET
"

rm -rf .cargo
podman unshare chown -R 0:0 "$OUT_DIR"

EXE="$OUT_DIR/$TARGET/release/photobooth-bridge.exe"
ls -lh "$EXE"
echo "Built: $EXE"
