#!/bin/sh
# A maker on public test networks: Liquid testnet and Solana devnet, quoting on public relays, so the live client has someone to
# trade with. It uses the test keys in cli/.keys and refuses mainnet. Run from the repo root. Stop it with: scripts/run-test-maker.sh stop
set -eu
ROOT=$(pwd)
DIR="$ROOT/maker/.testnet"
if [ "${1:-}" = stop ]; then
  [ -f "$DIR/daemon.pid" ] && kill "$(cat "$DIR/daemon.pid")" 2>/dev/null || true
  [ -f "$DIR/signer.pid" ] && kill "$(cat "$DIR/signer.pid")" 2>/dev/null || true
  echo stopped
  exit 0
fi
mkdir -p "$DIR/keys" "$DIR/run" "$DIR/bin"
chmod 700 "$DIR" "$DIR/keys" "$DIR/run"
[ -e "$DIR/keys/liquid-wallet.key" ] || cp cli/.keys/maker-liquid.key "$DIR/keys/liquid-wallet.key"
[ -e "$DIR/keys/liquid-blinding.key" ] || cp cli/.keys/maker-blinding.key "$DIR/keys/liquid-blinding.key"
[ -e "$DIR/keys/solana.json" ] || cp cli/.keys/maker-solana.json "$DIR/keys/solana.json"
chmod 600 "$DIR/keys/"*

# Copies, so that rebuilding for a test never replaces what is running.
(cd maker && RUSTFLAGS="" cargo build --release -q)
cp maker/target/release/liq2sol-signer maker/target/release/liq2sol-maker "$DIR/bin/"

ENGINE='"confirmations":2,"large_above":1000000000,"confirmations_large":10,"max_per_swap":1000000000,"max_open_l2s":5000000000,"max_open_s2l":5000000000,"l2s_gate_blocks":210,"l2s_fund_window_blocks":30,"l2s_deadline_slots":8400,"s2l_gate_slots":84500,"s2l_lock_window_blocks":30,"liquid_stale_secs":600,"solana_stale_secs":60,"extend_after_stall_secs":3600,"extend_within_slots":16900,"extend_by_slots":3000000,"urgent_blocks":60'
PRIMARY='https://liquid.network/liquidtestnet/api'
SECOND='https://blockstream.info/liquidtestnet/api'
SOLANA="${SOLANA_RPC:-https://api.devnet.solana.com}"
cat > "$DIR/signer.json" <<EOF
{"socket":"$DIR/run/signer.sock","ledger":"$DIR/signer.db","keys_dir":"$DIR/keys","liquid":{"Esplora":"$PRIMARY"},"solana_rpc":"$SOLANA",
 "limits":{"max_per_swap":1000000000,"window_secs":3600,"max_per_window":5000000000,"confirmations":2,"large_above":1000000000,"confirmations_large":10,"s2l_gate_slots":84500,"l2s_gate_blocks":210,"l2s_max_deadline_slots":8400,"s2l_min_blocks":30,"s2l_max_blocks":120,"extend_after_stall_secs":3600,"extend_by_slots":3000000},
 "regtest_assets":null,"vault_owner":null,"fee_coin_sats":30}
EOF
cat > "$DIR/maker.json" <<EOF
{"db":"$DIR/maker.db","signer_socket":"$DIR/run/signer.sock","pages":"$DIR/pages.log","engine":{$ENGINE},"liquid":{"Esplora":"$PRIMARY"},"second_liquid":{"Esplora":"$SECOND"},"solana_rpc":"$SOLANA","tick_millis":20000,"retry_secs":60,"regtest_assets":null,
 "rfq":{"nostr_key":"$DIR/keys/nostr.key","relays":["wss://relay.primal.net","wss://relay.coinos.pro"],"network":"testnet","min_sol_units":100000,"max_sol_units":25000000,"l2s_bps":30,"s2l_bps":30,"quote_secs":60,"l2s_lock_blocks":300,"s2l_lock_blocks":90,"min_deadline_slots":101400,"offer_every_secs":300,"offer_ttl_secs":1800,"max_requests_per_sender":60,"max_requests_per_window":2000,"window_secs":300,"max_negotiations":500}}
EOF
setsid nohup "$DIR/bin/liq2sol-signer" "$DIR/signer.json" >> "$DIR/signer.log" 2>&1 < /dev/null &
echo $! > "$DIR/signer.pid"
sleep 8
setsid nohup "$DIR/bin/liq2sol-maker" "$DIR/maker.json" >> "$DIR/daemon.log" 2>&1 < /dev/null &
echo $! > "$DIR/daemon.pid"
echo "started. Logs: $DIR/daemon.log and $DIR/signer.log. Pages an operator should see: $DIR/pages.log"
