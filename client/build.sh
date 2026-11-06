#!/bin/sh
# Builds the browser's WebAssembly into client/site/wasm. Needs wasm-pack and the wasm32-unknown-unknown target.
set -eu
cd "$(dirname "$0")/wasm"
RUSTFLAGS="" wasm-pack --quiet build --target web --out-dir ../site/wasm --no-pack --release
rm -f ../site/wasm/.gitignore ../site/wasm/README.md
