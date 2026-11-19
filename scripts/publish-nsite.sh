#!/usr/bin/env bash
# Publish one commit's page as SwapMarket's named nsite (NIP-5A, d tag liq2sol), signed in the browser by a NIP-07 extension such as
# Alby. The key never comes near this script. Usage: scripts/publish-nsite.sh [COMMIT]   (default HEAD)
# It builds the commit reproducibly, then serves scripts/nsite/publish.html with the build on http://127.0.0.1:8765 until Ctrl-C.
# Open that page in the browser that holds the key (from another machine: ssh -N -L 8765:127.0.0.1:8765 <this machine>), then check
# the result with scripts/check-page.sh <nsite address>.
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
commit=${1:-HEAD}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

"$repo/scripts/build-page.sh" "$commit" "$work/site" >/dev/null
(cd "$work/site" && sha256sum -c --quiet SHA256SUMS)
python3 -I "$repo/scripts/nsite/files.py" "$work/site" > "$work/files.json"
cp "$repo/scripts/nsite/publish.html" "$work/"
echo "open http://127.0.0.1:8765/publish.html in the browser with the key; Ctrl-C when done"
python3 -I -m http.server 8765 --bind 127.0.0.1 --directory "$work"
