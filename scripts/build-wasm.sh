#!/bin/sh
# Build the WASM module of @zen/client: crates/zen-wasm compiled for
# wasm32, then wasm-bindgen's JS glue and TypeScript declarations into
# packages/zen-wasm/pkg (git-ignored).
#
#   scripts/build-wasm.sh [--dev]
#
# Needs the wasm32-unknown-unknown target and wasm-bindgen-cli at exactly the
# version of the wasm-bindgen crate in Cargo.lock:
#   rustup target add wasm32-unknown-unknown
#   cargo install wasm-bindgen-cli --version <that version> --locked
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

profile=release
[ "${1:-}" = --dev ] && profile=dev

want=$(awk '/^name = "wasm-bindgen"$/ { getline; gsub(/version = |"/, ""); print; exit }' Cargo.lock)
have=$(wasm-bindgen --version 2>/dev/null | awk '{print $2}') || true
if [ "$want" != "$have" ]; then
  echo "build-wasm.sh: wasm-bindgen-cli ${have:-missing}, Cargo.lock has $want:" >&2
  echo "  cargo install wasm-bindgen-cli --version $want --locked" >&2
  exit 1
fi

cargo build -p zen-wasm --target wasm32-unknown-unknown --profile "$profile"
dir=$profile
[ "$profile" = dev ] && dir=debug
out=packages/zen-wasm/pkg
rm -rf "$out"
wasm-bindgen --target web --out-dir "$out" --out-name zen_wasm \
  "target/wasm32-unknown-unknown/$dir/zen_wasm.wasm"
ls -l "$out"/zen_wasm_bg.wasm | awk '{printf "zen_wasm_bg.wasm: %d bytes\n", $5}'
