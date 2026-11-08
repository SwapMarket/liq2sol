#!/usr/bin/env bash
# Set up a fresh local solana-test-validator for the suites: SOL for every test wallet, both programs, the stand-in mint
# and the faucet's vault. A restarted validator is a new chain, so run this after each restart. Devnet keys only.
# Usage: SOLANA_RPC=http://<Windows LAN IP>:8899 scripts/local-validator.sh
set -euo pipefail
: "${SOLANA_RPC:?set SOLANA_RPC to the local validator}"
case "$SOLANA_RPC" in *devnet*|*mainnet*|*testnet*) echo "refusing a public cluster: $SOLANA_RPC" >&2; exit 1 ;; esac
root=$(cd "$(dirname "$0")/.." && pwd)
export PATH="$HOME/.local/share/solana/install/active_release/bin:$HOME/.avm/bin:$PATH"
keys="$root/programs/.keys"
deployer="$keys/devnet-deployer.json"

for k in "$deployer" "$root"/cli/.keys/{taker-solana,maker-solana,mint-authority}.json; do
  solana airdrop 100 "$(solana-keygen pubkey "$k")" -u "$SOLANA_RPC" >/dev/null
done
# Airdrops must finalize before anything spends them.
sleep 20

cd "$root/programs"
cargo build-sbf --arch v3 --manifest-path solana-escrow/Cargo.toml
cargo build-sbf --arch v3 --manifest-path devnet-faucet/Cargo.toml
solana program deploy --use-rpc -u "$SOLANA_RPC" -k "$deployer" \
  --program-id "$keys/escrow-confidential-program.json" target/deploy/solana_escrow.so
solana program deploy --use-rpc -u "$SOLANA_RPC" -k "$deployer" \
  --program-id "$keys/devnet-faucet-program.json" target/deploy/devnet_faucet.so

cd "$root/cli"
mint=$(cargo run -q -- setup ../programs/.keys/devnet-usdt-mint.json | awk '/stand-in mint/ {print $3}')
vault=$(cargo run -q -- faucet-init "$(solana-keygen pubkey "$keys/devnet-faucet-program.json")" | awk '/faucet vault/ {print $3}')
spl-token mint "$mint" 100000 "$vault" -u "$SOLANA_RPC" \
  --mint-authority "$root/cli/.keys/mint-authority.json" --fee-payer "$root/cli/.keys/mint-authority.json"
echo "local validator ready: escrow, faucet, mint $mint, faucet vault $vault"
