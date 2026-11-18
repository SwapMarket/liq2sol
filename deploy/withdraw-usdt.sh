#!/usr/bin/env bash
# Withdraw a maker's USDt or USDT to the signer's withdraw_to, safely: pause it, wait until no swap is open, withdraw, resume.
# Usage: sudo ./withdraw-usdt.sh <liquid|solana> <amount|all> [maker name, default main]
#   sudo ./withdraw-usdt.sh solana 50          50.00 USDT from maker main's Solana wallet
#   sudo ./withdraw-usdt.sh liquid all second  all the USDt of maker second
# The amount is in whole units, such as 12.5. It waits up to WAIT_SECS (default 3600) for open swaps to finish.
# The maker is resumed afterwards, even if the withdrawal fails or this is interrupted, unless it was paused already.
set -euo pipefail

maker_bin=${LIQ2SOL_MAKER:-/opt/liq2sol/liq2sol-maker}
run_as=${LIQ2SOL_USER:-liq2sol}
wait_secs=${WAIT_SECS:-3600}
poll_secs=${POLL_SECS:-30}

die() { echo "$*" >&2; exit 1; }
if [ $# -lt 2 ] || [ $# -gt 3 ]; then die "usage: sudo $0 <liquid|solana> <amount|all> [maker name, default main]"; fi
chain=$1 amount=$2 name=${3:-main}
[[ "$chain" = liquid || "$chain" = solana ]] || die "the chain is liquid or solana"
[[ "$amount" = all || "$amount" =~ ^[0-9]*\.?[0-9]+$ ]] || die "the amount is a number, such as 12.5, or all"
[[ "$name" =~ ^[A-Za-z0-9_-]+$ ]] || die "a maker name is letters, digits, _ and -"
config=${LIQ2SOL_ROOT:-/var/lib/liq2sol}/$name/maker.json
[ -f "$config" ] || die "no maker $name: $config isn't there"

maker() {
  if [ "$(id -un)" = "$run_as" ]; then "$maker_bin" "$@"; else sudo -u "$run_as" "$maker_bin" "$@"; fi
}
open_swaps() { maker report "$config" | sed -n 's/^\([0-9]*\) open, .*/\1/p'; }

was_paused=no
maker report "$config" | grep -q '^Paused' && was_paused=yes
if [ "$was_paused" = no ]; then
  maker pause "$config" >/dev/null
  trap 'maker resume "$config" >/dev/null && echo "maker $name resumed"' EXIT
  echo "maker $name paused: it takes no new swaps while this runs"
fi

waited=0
while :; do
  open=$(open_swaps)
  [ -n "$open" ] || die "couldn't read how many swaps are open from the report"
  [ "$open" = 0 ] && break
  [ "$waited" -lt "$wait_secs" ] || die "$open swaps are still open after ${wait_secs}s, so nothing was withdrawn"
  echo "waiting for $open open swaps to finish (${waited}s so far, Ctrl-C to stop)"
  sleep "$poll_secs"
  waited=$((waited + poll_secs))
done

maker withdraw "$config" "$chain" usdt "$amount"
