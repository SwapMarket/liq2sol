#!/usr/bin/env bash
# Install the liq2sol maker on Debian, set up one maker, and start it.
#
#   sudo ./install.sh NAME [mainnet|testnet] [--helius KEY] [--page-to NPUB --page-relay URL ...]
#                         [--elements-rpc USER:PASSWORD [--elements-cli PATH] [--elements-port N]] [--no-start]
#
# NAME is the maker's folder under /var/lib/liq2sol, such as "main". The network defaults to mainnet.
# - Downloads the newest release for this machine, and installs it only if SwapMarket's signature checks out, against a key
#   whose hash is written below. If the maker is already installed, it leaves the binaries alone: update.sh does updates.
# - Makes the maker's folder from the example configs for that network, with the Helius key and pages if given.
# - With --elements-rpc, reads Liquid from your own Elements node on this machine, through elements-cli, after checking it
#   answers on the right chain. The maker keeps a public API as its second source, to check the node against.
# - Makes the Solana wallet, solana.json, on this machine. The signer makes the other keys on its first start. No key
#   leaves this machine: back up the keys folder yourself.
# - Starts the signer and the maker, and prints the addresses to fund and the maker's Nostr key.
# It never touches an existing maker's folder.
set -euo pipefail

repo=SwapMarket/liq2sol
opt=${LIQ2SOL_OPT:-/opt/liq2sol}
data=${LIQ2SOL_DATA:-/var/lib/liq2sol}
units=/etc/systemd/system
# sha256 of deploy/release-key.gpg, SwapMarket's release key.
key_sha256=28846ef1fcfac9b8b1b5c7b34bf14844db01488436f98928f3a70b2507dbd8b1

usage() { sed -n '3,5p' "$0" | sed 's/^# *//' >&2; exit 2; }
die() { echo "install: $*" >&2; exit 1; }

name=${1:-}
[ -n "$name" ] && [ "${name#-}" = "$name" ] || usage
shift
network=mainnet
if [ "${1:-}" = mainnet ] || [ "${1:-}" = testnet ]; then network=$1; shift; fi
helius='' page_to='' page_relays=() start=1 rpc='' cli=/usr/local/bin/elements-cli rpc_port=''
while [ $# -gt 0 ]; do
  case "$1" in
    --helius) helius=${2:?--helius needs a key}; shift 2 ;;
    --page-to) page_to=${2:?--page-to needs an npub}; shift 2 ;;
    --page-relay) page_relays+=("${2:?--page-relay needs a URL}"); shift 2 ;;
    --elements-rpc) rpc=${2:?--elements-rpc needs USER:PASSWORD}; shift 2 ;;
    --elements-cli) cli=${2:?--elements-cli needs a path}; shift 2 ;;
    --elements-port) rpc_port=${2:?--elements-port needs a port}; shift 2 ;;
    --no-start) start=0; shift ;;
    *) usage ;;
  esac
done

[[ "$name" =~ ^[a-z0-9][a-z0-9-]{0,30}$ ]] || die "a maker's name is lowercase letters, digits and dashes: '$name'"
[[ "$helius" =~ ^[A-Za-z0-9-]*$ ]] || die "that doesn't look like a Helius key"
if [ -n "$page_to" ]; then
  [[ "$page_to" =~ ^(npub1[a-z0-9]{58}|[0-9a-f]{64})$ ]] || die "--page-to wants an npub or a hex key"
  [ ${#page_relays[@]} -gt 0 ] || die "--page-to needs at least one --page-relay, from your app's DM relays"
  for r in "${page_relays[@]}"; do [[ "$r" =~ ^wss://[A-Za-z0-9./:_-]+$ ]] || die "not a relay URL: $r"; done
fi
if [ -n "$rpc" ]; then
  # Kept to characters that need no escaping in JSON or sed.
  [[ "$rpc" =~ ^[A-Za-z0-9._~-]+:[A-Za-z0-9._~-]{12,}$ ]] || die "--elements-rpc wants USER:PASSWORD, letters, digits and . _ ~ -, with a password of 12 or more"
  [[ "$cli" =~ ^/[A-Za-z0-9._/-]+$ ]] && [ -x "$cli" ] || die "no elements-cli at $cli: give its full path with --elements-cli"
  [[ "$rpc_port" =~ ^[0-9]*$ ]] || die "--elements-port wants a number"
fi
[ "$(id -u)" = 0 ] || die "run it with sudo"
for c in curl gpgv sha256sum openssl tar od; do command -v "$c" >/dev/null || die "needs $c: sudo apt install curl gpgv openssl"; done
folder="$data/$name"
[ ! -e "$folder" ] || die "$folder exists already. To update the binaries, run sudo $opt/update.sh"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cd "$work"

# ---- The release ----
if [ -x "$opt/liq2sol-maker" ] && [ -x "$opt/liq2sol-signer" ]; then
  echo "the maker is installed already, at $(cat "$opt/VERSION" 2>/dev/null || echo 'an unknown version')"
  release="$opt/examples"
else
  # Releases are pre-releases, which GitHub's "latest" skips, so ask for the newest of any kind.
  releases=$(curl -fsSL "https://api.github.com/repos/$repo/releases?per_page=1")
  tag=$(printf '%s\n' "$releases" | sed -n '/"tag_name"/{s/.*"tag_name": *"\([^"]*\)".*/\1/p;q;}')
  [[ "$tag" =~ ^v[0-9]+\.[0-9]+$ ]] || die "couldn't find the newest release: '$tag'"
  arch=$(uname -m)
  case "$arch" in x86_64|aarch64) ;; *) die "no release for $arch: a Raspberry Pi needs a 64-bit OS" ;; esac
  archive="liq2sol-maker-$tag-$arch-linux"
  echo "installing $tag for $arch"
  curl -fsSL -o release-key.gpg "https://raw.githubusercontent.com/$repo/$tag/deploy/release-key.gpg"
  echo "$key_sha256  release-key.gpg" | sha256sum -c --quiet - || die "the release key isn't the one this script expects"
  curl -fsSLO "https://github.com/$repo/releases/download/$tag/$archive.tar.gz"
  curl -fsSLO "https://github.com/$repo/releases/download/$tag/SHA256SUMS"
  curl -fsSLO "https://github.com/$repo/releases/download/$tag/SHA256SUMS.asc" || die "$tag isn't signed yet. Try again later."
  gpgv --keyring ./release-key.gpg SHA256SUMS.asc SHA256SUMS || die "$tag's signature doesn't check out, so nothing was installed"
  grep -q " $archive.tar.gz\$" SHA256SUMS || die "SHA256SUMS doesn't list $archive.tar.gz"
  sha256sum -c --ignore-missing --quiet SHA256SUMS
  tar xzf "$archive.tar.gz"
  release="$work/$archive"
  [ -x "$release/liq2sol-maker" ] && [ -x "$release/liq2sol-signer" ] || die "the archive is missing a binary"

  id liq2sol >/dev/null 2>&1 || useradd --system --home "$data" --shell /usr/sbin/nologin liq2sol
  install -d "$opt" "$opt/examples"
  install -m 755 "$release/liq2sol-signer" "$release/liq2sol-maker" "$release/update.sh" "$opt/"
  install -m 644 release-key.gpg "$opt/release-key.gpg"
  install -m 644 "$release"/*.example.json "$release/README.md" "$opt/examples/"
  install -m 644 "$release/liq2sol-signer@.service" "$release/liq2sol-maker@.service" "$units/"
  echo "$tag" > "$opt/VERSION"
fi

suffix=$([ "$network" = mainnet ] && echo .mainnet || true)
signer_example="$release/signer$suffix.example.json"
maker_example="$release/maker$suffix.example.json"
[ -f "$signer_example" ] && [ -f "$maker_example" ] || die "this release has no $network example configs. Update it first: sudo $opt/update.sh"

# ---- The maker's folder ----
umask 077
install -d -m 700 -o liq2sol -g liq2sol "$data"
install -d -m 700 "$folder" "$folder/keys" "$folder/run"
cp "$signer_example" "$folder/signer.json"
cp "$maker_example" "$folder/maker.json"
if [ -n "$helius" ]; then
  sed -i "s/YOUR_HELIUS_KEY/$helius/" "$folder/signer.json" "$folder/maker.json"
elif grep -q YOUR_HELIUS_KEY "$folder/signer.json"; then
  echo "note: no --helius key, so solana_rpc in both configs still says YOUR_HELIUS_KEY. Set it before starting." >&2
  start=0
fi
if [ -n "$page_to" ]; then
  relays=$(printf '"%s", ' "${page_relays[@]}")
  sed -i "0,/^{/s||{\n \"page_to\": { \"nostr\": \"$page_to\", \"relays\": [${relays%, }] },|" "$folder/maker.json"
fi

# Your own node, through elements-cli. Its data folder must be one the services may write, inside the maker's folder.
if [ -n "$rpc" ]; then
  install -d -m 700 "$folder/elements-cli"
  args="\"-chain=$([ "$network" = mainnet ] && echo liquidv1 || echo liquidtestnet)\", \"-datadir=$folder/elements-cli\", \"-rpcuser=${rpc%%:*}\", \"-rpcpassword=${rpc#*:}\""
  [ -z "$rpc_port" ] || args="$args, \"-rpcport=$rpc_port\""
  node="\"liquid\": { \"Node\": { \"cli\": \"$cli\", \"args\": [$args] } },"
  for f in signer.json maker.json; do
    sed -i "/^ \"liquid\": {/,/^ },/c\\ $node" "$folder/$f"
  done
  # Our own node sees every address we ask about, so a fresh one per payment links nothing.
  sed -i 's/"fresh_addresses": false/"fresh_addresses": true/' "$folder/signer.json"
fi

# The Solana wallet: an ed25519 key, in solana-keygen's format, the 32-byte secret then the 32-byte public key.
openssl genpkey -algorithm ed25519 -out "$work/solana.pem" 2>/dev/null
{ openssl pkey -in "$work/solana.pem" -outform DER | tail -c 32
  openssl pkey -in "$work/solana.pem" -pubout -outform DER | tail -c 32; } | od -An -tu1 -v \
  | tr -s ' \n' ' ' | sed 's/^ //; s/ $//; s/ /,/g; s/.*/[&]/' > "$folder/keys/solana.json"
rm -f "$work/solana.pem"
[ "$(tr -cd , < "$folder/keys/solana.json" | wc -c)" = 63 ] || die "making the Solana wallet failed"
chown -R liq2sol:liq2sol "$folder"
if [ -n "$rpc" ]; then
  want=$([ "$network" = mainnet ] && echo liquidv1 || echo liquidtestnet)
  check=(-chain="$want" -datadir="$folder/elements-cli" -rpcuser="${rpc%%:*}" -rpcpassword="${rpc#*:}")
  [ -z "$rpc_port" ] || check+=(-rpcport="$rpc_port")
  info=$(runuser -u liq2sol -- "$cli" "${check[@]}" getblockchaininfo 2>&1) || {
    echo "note: the node doesn't answer yet: ${info:0:200}" >&2
    echo "the maker is set up for it, but not started. Start it once the node answers (see the end)." >&2
    start=0
  }
  if [ "$start" = 1 ]; then
    chain=$(printf '%s' "$info" | sed -n 's/.*"chain": *"\([^"]*\)".*/\1/p' | head -1)
    # Made just now, with keys never used, so nothing is lost, and a second run can start clean.
    [ "$chain" = "$want" ] || { rm -rf "$folder"; die "the node is on '$chain', not $want, so $folder was removed again"; }
    progress=$(printf '%s' "$info" | sed -n 's/.*"verificationprogress": *\([0-9.e-]*\).*/\1/p' | head -1)
    echo "the node answers on $chain, synced $(awk "BEGIN { printf \"%.1f\", ${progress:-0} * 100 }")%"
  fi
fi
chmod 600 "$folder/signer.json" "$folder/maker.json" "$folder/keys/solana.json"
echo "made $folder for $network"

if [ "$start" = 0 ]; then
  echo "not started. When ready: sudo systemctl daemon-reload && sudo systemctl enable --now liq2sol-signer@$name liq2sol-maker@$name"
  exit 0
fi

# ---- Start it ----
since=$(date '+%Y-%m-%d %H:%M:%S')
systemctl daemon-reload
systemctl enable --now "liq2sol-signer@$name" "liq2sol-maker@$name"
echo "waiting for the maker to start (Ctrl-C is safe: it keeps running)"
for _ in $(seq 180); do
  journalctl -u "liq2sol-maker@$name" --since "$since" -q | grep -q 'maker running' && break
  sleep 1
done
funding=$(journalctl -u "liq2sol-signer@$name" --since "$since" -q | grep -o 'signer ready.*' | tail -1 || true)
nostr=$(journalctl -u "liq2sol-maker@$name" --since "$since" -q | grep -o 'maker answering quotes as .*' | tail -1 || true)
if [ -z "$nostr" ]; then
  echo "the maker hasn't said it is running yet. Its last lines:" >&2
  journalctl -u "liq2sol-maker@$name" --since "$since" -q -n 8 --no-pager >&2 || true
  echo "watch it with: journalctl -fu liq2sol-maker@$name" >&2
fi
# Quotes inside a ${...:-...} default break bash's parsing in a here-document, so the defaults are set first.
[ -n "$funding" ] || funding="The signer hasn't printed its addresses yet: journalctl -u liq2sol-signer@$name | grep 'signer ready'"
cat <<EOF

$funding
$nostr

Next:
 1. Back up $folder/keys somewhere safe and offline. On $network, these keys hold the maker's funds.
 2. Fund the addresses above: USDt and L-BTC on Liquid, USDT and SOL on Solana. Start small.
 3. Send SwapMarket the key after "maker answering quotes as", to be listed on the page.
 4. Set prices in $folder/maker.json, then: sudo systemctl restart liq2sol-maker@$name
EOF
