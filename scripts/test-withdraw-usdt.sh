#!/usr/bin/env bash
# deploy/withdraw-usdt.sh against a stand-in for liq2sol-maker, which logs each call and reports open swaps from a counter
# that drops by one each time it is read. Nothing real is run.
set -euo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
fail() { echo "FAIL: $*" >&2; exit 1; }

mkdir -p "$work/root/main"
echo '{}' > "$work/root/main/maker.json"
cat > "$work/maker" <<STUB
#!/usr/bin/env bash
echo "\$*" >> "$work/calls"
case "\$1" in
  report)
    open=\$(cat "$work/open"); echo "\$open open, 0 never started."
    [ -f "$work/paused" ] && echo "Paused: no new swaps until \\\`resume\\\`."
    [ "\$open" -gt 0 ] && [ ! -f "$work/stuck" ] && echo \$((open - 1)) > "$work/open"; exit 0 ;;
  pause) touch "$work/paused" ;;
  resume) rm -f "$work/paused" ;;
  withdraw) [ -f "$work/withdraw-fails" ] && { echo "the signer didn't withdraw" >&2; exit 1; }; echo "sent" ;;
esac
STUB
chmod +x "$work/maker"
run() {
  : > "$work/calls"
  LIQ2SOL_MAKER="$work/maker" LIQ2SOL_USER="$(id -un)" LIQ2SOL_ROOT="$work/root" POLL_SECS=0 WAIT_SECS="${WAIT:-60}" \
    "$repo/deploy/withdraw-usdt.sh" "$@" > "$work/out" 2>&1
}
calls() { cut -d' ' -f1 "$work/calls" | tr '\n' ' '; }

echo 2 > "$work/open"
run solana 50 || fail "it should have withdrawn: $(cat "$work/out")"
[ "$(calls)" = "report pause report report withdraw resume " ] || fail "calls: $(calls)"
grep -q "withdraw $work/root/main/maker.json solana usdt 50" "$work/calls" || fail "withdrew something else"
[ ! -f "$work/paused" ] || fail "left paused"
echo "ok: it pauses, waits for open swaps to finish, withdraws, and resumes"

echo 0 > "$work/open"; touch "$work/paused"
run liquid all || fail "$(cat "$work/out")"
[ "$(calls)" = "report report withdraw " ] || fail "calls: $(calls)"
[ -f "$work/paused" ] || fail "resumed a maker the operator had paused"
rm -f "$work/paused"
echo "ok: a maker already paused stays paused"

echo 0 > "$work/open"; touch "$work/withdraw-fails"
if run solana all; then fail "a failed withdrawal reported success"; fi
[ ! -f "$work/paused" ] || fail "a failed withdrawal left the maker paused"
rm -f "$work/withdraw-fails"
echo "ok: a failed withdrawal still resumes the maker"

echo 3 > "$work/open"; touch "$work/stuck"
if WAIT=0 run solana 50; then fail "withdrew with swaps open"; fi
grep -q withdraw "$work/calls" && fail "called withdraw with swaps open"
[ ! -f "$work/paused" ] || fail "a timeout left the maker paused"
grep -q "still open" "$work/out" || fail "no reason given: $(cat "$work/out")"
rm -f "$work/stuck"
echo "ok: with swaps still open at the time limit, nothing is withdrawn and the maker resumes"

for bad in "ethereum 5" "solana -5" "solana 5 ../x"; do
  # shellcheck disable=SC2086
  if run $bad; then fail "accepted: $bad"; fi
done
echo "ok: a wrong chain, amount or maker name is refused"
echo PASS
