#!/usr/bin/env bash
# Feasibility proof F2: the WASM build must produce a byte-identical signed spend to the native build.
set -euo pipefail
cd "$(dirname "$0")/.."
export RUSTFLAGS=""
native="$(cargo run -q -p liq2sol-htlc-wasm --example vector)"
(cd contracts/liquid-wasm && wasm-pack --quiet build --target nodejs --out-dir pkg)
wasm="$(node -e "process.stdout.write(require('./contracts/liquid-wasm/pkg').parity_vector())")"
echo "wasm size: $(stat -c %s contracts/liquid-wasm/pkg/liq2sol_htlc_wasm_bg.wasm) bytes"
echo "$native" | cut -c1-120
if [ "$native" = "$wasm" ]; then echo "PARITY OK"; else echo "PARITY FAILED"; echo "$wasm" | cut -c1-120; exit 1; fi
