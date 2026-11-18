#!/usr/bin/env bash
# Install an Elements node for a maker on this machine: download a release, check it, and run it as a service.
# Usage: sudo ./install-elements.sh [mainnet|testnet] [--datadir DIR] [--version X.Y.Z]
#        ./install-elements.sh --fetch-only DIR [--version X.Y.Z]     (download, check and unpack into DIR, nothing else)
#
# - The release is used only if its SHA256SUMS.asc carries a good signature by Elements' release key, pinned here by
#   fingerprint and shipped beside this script as elements-release-key.gpg, and the archive matches its line there.
# - Installs to /opt/elements-VERSION, linked from /usr/local/bin. Runs as user `elements`, from DIR (default
#   /var/lib/elements), as elementsd.service.
# - Writes DIR/elements.conf: txindex (the maker looks up confirmed transactions by id), no wallet, RPC on 127.0.0.1 only
#   with a new random password, and validatepegin=0, since there is no bitcoind to check peg-ins against.
# - Never replaces an existing elements.conf, elementsd.service or elements-cli it didn't make.
# A first sync downloads the whole chain and takes hours, or days on a Raspberry Pi. Start the maker only after it ends.
set -euo pipefail

# Elements 23.3.4 fixes the September 2026 exploit, so nothing older is installed.
version=23.3.4
min_version=23.3.4
# Pablo Greco, who signs Elements releases. Named in ElementsProject's own guide,
# https://github.com/ElementsProject/elementsproject.github.io/blob/master/elements-code-tutorial/installing-elements.md
fingerprint=BD0F3062F87842410B06A0432F656B0610604482
network=mainnet datadir=/var/lib/elements fetch_only=''

die() { echo "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    mainnet|testnet) network=$1; shift ;;
    --datadir) datadir=${2:?--datadir needs a folder}; shift 2 ;;
    --version) version=${2:?--version needs X.Y.Z}; shift 2 ;;
    --fetch-only) fetch_only=${2:?--fetch-only needs a folder}; shift 2 ;;
    *) die "unknown argument: $1" ;;
  esac
done

[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "--version wants X.Y.Z"
[ "$(printf '%s\n%s\n' "$min_version" "$version" | sort -V | head -1)" = "$min_version" ] || die "Elements $version is older than $min_version"
[[ "$datadir" =~ ^/[A-Za-z0-9._/-]+$ ]] || die "--datadir wants a full path, of letters, digits and . _ / -"

case "$(uname -m)" in
  x86_64) arch=x86_64 ;;
  aarch64|arm64) arch=aarch64 ;;
  *) die "no Elements release for $(uname -m) here: x86_64 and aarch64 only" ;;
esac
for tool in curl gpgv sha256sum tar; do command -v "$tool" >/dev/null || die "needs $tool: apt install curl gpgv coreutils tar"; done

key="$(cd "$(dirname "$0")" && pwd)/elements-release-key.gpg"
[ -f "$key" ] || die "elements-release-key.gpg must be beside this script"

# Download, check the signature and the archive's hash, and unpack into $1.
fetch() {
  local into=$1 work file base
  work=$(mktemp -d)
  trap 'rm -rf "$work"' RETURN
  file="elements-$version-$arch-linux-gnu.tar.gz"
  base="https://github.com/ElementsProject/elements/releases/download/elements-$version"
  echo "downloading Elements $version for $arch"
  curl -fsSL -o "$work/SHA256SUMS.asc" "$base/SHA256SUMS.asc"
  curl -fsSL -o "$work/$file" "$base/$file"
  # A clearsigned file: gpgv writes out what the signature covers, and only that is read.
  gpgv --keyring "$key" --status-fd 3 --output "$work/sums" "$work/SHA256SUMS.asc" 3>"$work/status" 2>/dev/null \
    || die "SHA256SUMS.asc isn't signed by Elements' release key, so nothing was installed"
  grep -q "^\[GNUPG:\] VALIDSIG $fingerprint " "$work/status" || die "SHA256SUMS.asc is signed, but not by $fingerprint"
  local want
  want=$(awk -v f="$file" '$2 == f || $2 == "*" f { print $1 }' "$work/sums")
  [ -n "$want" ] || die "$file isn't in the signed SHA256SUMS"
  [ "$(sha256sum "$work/$file" | cut -d' ' -f1)" = "$want" ] || die "$file doesn't match the signed SHA256SUMS, so nothing was installed"
  echo "signature and hash check out"
  mkdir -p "$into"
  tar -xzf "$work/$file" -C "$into" --strip-components=1
  [ -x "$into/bin/elementsd" ] && [ -x "$into/bin/elements-cli" ] || die "the archive has no bin/elementsd and bin/elements-cli"
}

if [ -n "$fetch_only" ]; then
  [ ! -e "$fetch_only" ] || die "$fetch_only exists already"
  fetch "$fetch_only"
  echo "unpacked into $fetch_only"
  exit 0
fi

[ "$(id -u)" = 0 ] || die "run it with sudo"
command -v systemctl >/dev/null || die "needs systemd"
chain=$([ "$network" = mainnet ] && echo liquidv1 || echo liquidtestnet)
port=$([ "$network" = mainnet ] && echo 7041 || echo 7039)
unit=/etc/systemd/system/elementsd.service
opt=/opt/elements-$version

# Everything that could stop it is checked before anything changes.
[ ! -e "$unit" ] || die "$unit exists already: this machine has a node set up, so nothing was changed"
[ ! -e "$datadir/elements.conf" ] || die "$datadir/elements.conf exists already, so nothing was changed"
for tool in elementsd elements-cli; do
  link=/usr/local/bin/$tool
  [ ! -e "$link" ] || [ "$(readlink "$link" || true)" = "$opt/bin/$tool" ] || die "$link exists already and isn't this release's, so nothing was changed"
done

[ -d "$opt" ] || fetch "$opt"
ln -sfn "$opt/bin/elementsd" /usr/local/bin/elementsd
ln -sfn "$opt/bin/elements-cli" /usr/local/bin/elements-cli

id elements >/dev/null 2>&1 || useradd --system --home-dir "$datadir" --shell /usr/sbin/nologin elements
install -d -m 750 -o elements -g elements "$datadir"

password=$(head -c 24 /dev/urandom | base64 | tr -d '/+=' | head -c 32)
umask 077
cat > "$datadir/elements.conf" <<EOF
# Written by liq2sol's install-elements.sh for a maker.
chain=$chain

[$chain]
server=1
# The maker looks up confirmed transactions by id.
txindex=1
# The maker needs no wallet in the node.
disablewallet=1
# Checking peg-ins needs a bitcoind, which this machine doesn't run.
validatepegin=0
rpcbind=127.0.0.1
rpcallowip=127.0.0.1
rpcport=$port
rpcuser=liq2sol
rpcpassword=$password
EOF
chown elements:elements "$datadir/elements.conf"

cat > "$unit" <<EOF
[Unit]
Description=Elements node ($chain), for liq2sol makers
After=network-online.target
Wants=network-online.target

[Service]
User=elements
Group=elements
ExecStart=/usr/local/bin/elementsd -datadir=$datadir -conf=$datadir/elements.conf -daemon=0 -printtoconsole=0
Restart=on-failure
RestartSec=30
# Shutting down flushes the chain state, which can take minutes on a Raspberry Pi.
TimeoutStopSec=600
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=$datadir

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable --now elementsd.service

cat <<EOF

Elements $version is running on $chain, from $datadir, as elementsd.service.

It is syncing now, which takes hours, or days on a Raspberry Pi. To see how far it has got:
  sudo -u elements elements-cli -datadir=$datadir getblockchaininfo | grep -E '"(blocks|headers|verificationprogress)"'

Start a maker on it only once it has caught up, when verificationprogress is about 1: a node still syncing answers
with an old chain. For a new maker, install it meanwhile with install.sh's --no-start, and these:
  --elements-rpc liq2sol:$password

For a maker you have already, put the same user and password in the "Node" args of maker.json and signer.json
(deploy/README.md), and restart both. The password is also in $datadir/elements.conf.
EOF
