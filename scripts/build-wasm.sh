#!/bin/sh
# Build the WASM modules: crates/zen-wasm (@zen/client's core) and
# crates/zen-svc-wasm (@zen/svc's protocol engines, loaded only by services
# that use them), compiled for wasm32, then wasm-bindgen's JS glue and
# TypeScript declarations into packages/zen-wasm/pkg and
# packages/svc-wasm/pkg (git-ignored).
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

dir=$profile
[ "$profile" = dev ] && dir=debug

# build <crate> <package dir>
build() {
  name=$(echo "$1" | tr - _)
  cargo build -p "$1" --target wasm32-unknown-unknown --profile "$profile"
  out=packages/$2/pkg
  rm -rf "$out"
  wasm-bindgen --target web --out-dir "$out" --out-name "$name" \
    "target/wasm32-unknown-unknown/$dir/$name.wasm"
  ls -l "$out/${name}_bg.wasm" | awk -v n="$name" '{printf "%s_bg.wasm: %d bytes\n", n, $5}'
}

build zen-wasm zen-wasm
build zen-svc-wasm svc-wasm
