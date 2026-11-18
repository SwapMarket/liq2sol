#!/usr/bin/env bash
# Check the published page against its source: rebuild the commit it names, in the pinned image, and compare every file the site
# serves with the rebuild, byte for byte. Usage: scripts/check-page.sh [SITE_URL]   (default https://swapmarket.github.io/liq2sol/)
# Needs Docker, and this repository with that commit fetched.
set -euo pipefail

site=${1:-https://swapmarket.github.io/liq2sol/}
site=${site%/}/
repo=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

curl -fsSL -o "$work/index.html" "${site}index.html"
build=$(grep -o 'id="build" href="https://github.com/SwapMarket/liq2sol/commit/[0-9a-f]\{12\}"' "$work/index.html" | grep -o '[0-9a-f]\{12\}"$' | tr -d '"')
[ -n "$build" ] || { echo "the page names no build commit" >&2; exit 1; }
echo "the page says it is commit $build"
git -C "$repo" rev-parse --verify -q "$build^{commit}" >/dev/null || git -C "$repo" fetch -q origin
"$repo/scripts/build-page.sh" "$build" "$work/built" >/dev/null

# Every file the rebuild has, as the site serves it now.
bad=0
while read -r want file; do
  file=${file#./}
  if ! curl -fsSL -o "$work/live" "$site$file"; then echo "MISSING $file"; bad=1; continue; fi
  got=$(sha256sum "$work/live" | cut -d' ' -f1)
  if [ "$got" = "$want" ]; then echo "same    $file"; else echo "DIFFERS $file"; bad=1; fi
done < "$work/built/SHA256SUMS"
curl -fsSL -o "$work/live" "${site}SHA256SUMS"
if cmp -s "$work/live" "$work/built/SHA256SUMS"; then echo "same    SHA256SUMS"; else echo "DIFFERS SHA256SUMS"; bad=1; fi

if [ "$bad" = 0 ]; then echo "the published page is exactly commit $build, rebuilt here"; else echo "the published page is NOT what commit $build builds" >&2; exit 1; fi
