#!/usr/bin/env bash
# Update the maker on this machine to the newest release, or to the one named, and restart every maker that was running.
# Usage: sudo ./update.sh [vX.YY]      sudo ./update.sh --rollback
# Configs, keys and databases are never touched. The binaries it replaces are kept in /opt/liq2sol/previous.
# A release is installed only if its SHA256SUMS carries a signature by the release key already on this machine, in
# /opt/liq2sol/release-key.gpg. On a first run, that is the copy beside this script.
set -euo pipefail

repo=SwapMarket/liq2sol
opt=/opt/liq2sol
units=/etc/systemd/system

[ "$(id -u)" = 0 ] || { echo "run it with sudo" >&2; exit 1; }

# Makers running now, so exactly these come back afterwards.
running=$(systemctl list-units --type=service --state=active --plain --no-legend 'liq2sol-maker@*' | awk '{print $1}' | sed 's/^liq2sol-maker@//; s/\.service$//')

stop_all() {
  for m in $running; do systemctl stop "liq2sol-maker@$m"; done
  for m in $running; do systemctl stop "liq2sol-signer@$m"; done
}

start_all() {
  systemctl daemon-reload
  local since
  since=$(date '+%Y-%m-%d %H:%M:%S')
  # Two seconds apart, so makers sharing this IP don't all hit the chain APIs at once.
  local first=1
  for m in $running; do
    [ "$first" = 1 ] || sleep 2
    first=0
    systemctl start "liq2sol-signer@$m" "liq2sol-maker@$m"
  done
  # A maker says so once both chains answer. It retries every 30 seconds, and makers sharing an IP can meet a public
  # API's rate limit, so this waits a few minutes. Interrupting it is safe: everything is already installed and started.
  for m in $running; do
    echo "waiting for maker $m to say it is running (Ctrl-C is safe)"
    for _ in $(seq 180); do
      journalctl -u "liq2sol-maker@$m" --since "$since" -q | grep -q 'maker running' && break
      sleep 1
    done
    if journalctl -u "liq2sol-maker@$m" --since "$since" -q | grep -q 'maker running'; then
      echo "maker $m is running"
    else
      echo "maker $m hasn't said it is running yet. Its last lines:" >&2
      journalctl -u "liq2sol-maker@$m" --since "$since" -q -n 5 --no-pager >&2 || true
    fi
  done
}

if [ "${1:-}" = --rollback ]; then
  [ -x "$opt/previous/liq2sol-maker" ] || { echo "there is no previous version in $opt/previous" >&2; exit 1; }
  stop_all
  trap 'echo "the rollback failed part way, so starting the makers again" >&2; start_all' ERR
  install -m 755 "$opt/previous/liq2sol-signer" "$opt/previous/liq2sol-maker" "$opt/"
  cp "$opt/previous/VERSION" "$opt/VERSION" 2>/dev/null || rm -f "$opt/VERSION"
  echo "back to $(cat "$opt/VERSION" 2>/dev/null || echo 'the previous version')"
  start_all
  exit 0
fi

command -v curl >/dev/null || { echo "needs curl: apt install curl" >&2; exit 1; }
command -v gpgv >/dev/null || { echo "needs gpgv: apt install gpgv" >&2; exit 1; }
key="$opt/release-key.gpg"
[ -f "$key" ] || key="$(cd "$(dirname "$0")" && pwd)/release-key.gpg"
[ -f "$key" ] || { echo "no release key: put release-key.gpg in $opt or beside this script" >&2; exit 1; }
# Releases are pre-releases, which GitHub's "latest" skips, so ask for the newest of any kind.
# Read whole before searching: grep stopping early would cut curl off, and pipefail would end the script.
if [ -n "${1:-}" ]; then
  tag=$1
else
  releases=$(curl -fsSL "https://api.github.com/repos/$repo/releases?per_page=1")
  tag=$(printf '%s\n' "$releases" | sed -n '/"tag_name"/{s/.*"tag_name": *"\([^"]*\)".*/\1/p;q;}')
fi
[[ "$tag" =~ ^v[0-9]+\.[0-9]+$ ]] || { echo "not a release tag: '$tag'" >&2; exit 1; }
current=$(cat "$opt/VERSION" 2>/dev/null || echo unknown)
if [ "$current" = "$tag" ]; then
  echo "already at $tag"
  exit 0
fi
echo "updating from $current to $tag"

arch=$(uname -m)
case "$arch" in x86_64|aarch64) ;; *) echo "no release for $arch: a Raspberry Pi needs a 64-bit OS" >&2; exit 1 ;; esac
name="liq2sol-maker-$tag-$arch-linux"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cd "$work"
curl -fsSLO "https://github.com/$repo/releases/download/$tag/$name.tar.gz"
curl -fsSLO "https://github.com/$repo/releases/download/$tag/SHA256SUMS"
# The signature is added by hand after the release is built, so a new release can lack it for a while.
curl -fsSLO "https://github.com/$repo/releases/download/$tag/SHA256SUMS.asc" || { echo "$tag isn't signed yet. Try again later." >&2; exit 1; }
gpgv --keyring "$key" SHA256SUMS.asc SHA256SUMS || { echo "$tag's signature doesn't check out against $key, so nothing was installed" >&2; exit 1; }
# The list covers every architecture, and this machine downloaded only its own.
sha256sum -c --ignore-missing SHA256SUMS
grep -q " $name.tar.gz\$" SHA256SUMS || { echo "SHA256SUMS doesn't list $name.tar.gz" >&2; exit 1; }
tar xzf "$name.tar.gz"
if [ ! -x "$name/liq2sol-maker" ] || [ ! -x "$name/liq2sol-signer" ]; then
  echo "the archive is missing a binary" >&2
  exit 1
fi

stop_all
# Whatever fails from here, the makers come back, on whichever binaries are in place.
trap 'echo "the update failed part way, so starting the makers again" >&2; start_all' ERR
install -d "$opt/previous"
for b in liq2sol-signer liq2sol-maker; do [ -f "$opt/$b" ] && cp -p "$opt/$b" "$opt/previous/"; done
[ -f "$opt/VERSION" ] && cp "$opt/VERSION" "$opt/previous/VERSION"
install -m 755 "$name/liq2sol-signer" "$name/liq2sol-maker" "$opt/"
install -m 644 "$name/liq2sol-signer@.service" "$name/liq2sol-maker@.service" "$units/"
# New settings come with defaults, so an existing config keeps working. These show what there is to set.
install -d "$opt/examples"
install -m 644 "$name/signer.example.json" "$name/maker.example.json" "$name/README.md" "$opt/examples/"
[ -f "$name/update.sh" ] && install -m 755 "$name/update.sh" "$opt/update.sh"
# Signed by the key we trust, so a new key it carries is trusted next time.
[ -f "$name/release-key.gpg" ] && install -m 644 "$name/release-key.gpg" "$opt/release-key.gpg"
echo "$tag" > "$opt/VERSION"
trap - ERR
start_all
echo "now at $tag. To go back: sudo $opt/update.sh --rollback"
