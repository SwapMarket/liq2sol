#!/bin/sh
# Runs inside the pinned image: reads `git archive` of one commit on stdin, builds the page as published, and writes it as a tar
# on stdout. Usage: build-inside.sh BUILD_ID
set -eu
build=${1:?build id}
mkdir -p /src && cd /src
tar -xf -
# Fixed paths and times, so nothing about the machine or the moment reaches the output.
export CARGO_TARGET_DIR=/build/target SOURCE_DATE_EPOCH=0 RUSTFLAGS="" CARGO_INCREMENTAL=0
cd client/wasm
cargo build --locked --release --target wasm32-unknown-unknown >&2
out=/src/client/site/wasm
rm -rf "$out" && mkdir -p "$out"
wasm-bindgen --target web --out-dir "$out" "$CARGO_TARGET_DIR/wasm32-unknown-unknown/release/liq2sol_client_wasm.wasm" >&2
wasm-opt -O "$out/liq2sol_client_wasm_bg.wasm" -o "$out/liq2sol_client_wasm_bg.wasm" >&2
cd /src
node client/stamp.mjs client/site "$build" >&2
cd client/site
find . -type f | LC_ALL=C sort | xargs sha256sum > /tmp/SHA256SUMS
cp /tmp/SHA256SUMS SHA256SUMS
tar --sort=name --mtime=@0 --owner=0 --group=0 --numeric-owner -cf - .
