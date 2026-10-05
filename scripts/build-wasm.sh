#!/usr/bin/env bash
# Builds the WebAssembly module embedded in the Go SDK:
#   go/internal/wasm/sqlscope.wasm.gz
#
# Requires a Rust toolchain with the wasm32-wasip1 target
# (`rustup target add wasm32-wasip1`). Set CARGO to pick a specific cargo.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cargo="${CARGO:-cargo}"
out="$root/go/internal/wasm/sqlscope.wasm.gz"

# Remap absolute paths so the module does not depend on the build machine.
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$root=/sqlscope --remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo"

"$cargo" build --manifest-path "$root/Cargo.toml" -p sqlscope-wasm \
  --target wasm32-wasip1 --profile wasm --locked

gzip -9 -n -c "$root/target/wasm32-wasip1/wasm/sqlscope_wasm.wasm" > "$out"
echo "wrote $out ($(wc -c < "$out" | tr -d ' ') bytes)"
