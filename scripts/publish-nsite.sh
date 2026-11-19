#!/usr/bin/env bash
# Publish one of SwapMarket's sites as an nsite (NIP-5A) under SwapMarket's key, signed in the browser by a NIP-07 extension such as
# Alby. The key never comes near this script.
#
#   scripts/publish-nsite.sh liq2sol [COMMIT]      named site liq2sol, built here reproducibly from COMMIT (default HEAD)
#   scripts/publish-nsite.sh sol2base | eth2base | sol2tron   named sites, from their latest GitHub Pages deploy
#   scripts/publish-nsite.sh btc                   BTC Swaps (SwapMarket/swapmarket.github.io) as the root site, from its latest
#                                                  "Deploy main" build, without the docs, which stay on GitHub Pages
#
# A site taken from a deploy is checked file by file against what https://swapmarket.github.io serves now, so the nsite gets exactly
# the bytes GitHub Pages has. Then this serves scripts/nsite/publish.html with the files on http://127.0.0.1:8765 until Ctrl-C. Open
# that page in the browser that holds the key (from another machine: ssh -N -L 8765:127.0.0.1:8765 <this machine>).
# Check liq2sol's result with scripts/check-page.sh <nsite address>.
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
site=${1:-}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# Takes the newest successful run of a workflow, downloads one artifact, and leaves the site's files in $work/site.
from_deploy() {
  local gh_repo=$1 workflow=$2 artifact=$3
  read -r run sha < <(gh run list -R "$gh_repo" -w "$workflow" -s success -L 1 --json databaseId,headSha -q '.[0] | "\(.databaseId) \(.headSha)"') || true
  [ -n "$run" ] || { echo "no successful run of $workflow in $gh_repo" >&2; exit 1; }
  echo "$gh_repo: run $run of \"$workflow\", commit ${sha::12}" >&2
  gh run download "$run" -R "$gh_repo" -n "$artifact" -D "$work/download"
  mkdir "$work/site"
  if [ -f "$work/download/artifact.tar" ]; then tar -x -C "$work/site" -f "$work/download/artifact.tar"; else cp -a "$work/download/." "$work/site/"; fi
}

# Every file must be what GitHub Pages serves at the same path now.
same_as_live() {
  local base=$1 bad=0 rel
  while IFS= read -r -d '' f; do
    rel=${f#"$work/site/"}
    if ! curl -fsSL -o "$work/live" "$base$rel"; then echo "not on GitHub Pages: $rel" >&2; bad=1
    elif ! cmp -s "$work/live" "$f"; then echo "differs from GitHub Pages: $rel" >&2; bad=1; fi
  done < <(find "$work/site" -type f -print0)
  [ "$bad" = 0 ] || { echo "the build is not what $base serves, so nothing is published" >&2; exit 1; }
  echo "every file is what $base serves" >&2
}

case $site in
  liq2sol)
    "$repo/scripts/build-page.sh" "${2:-HEAD}" "$work/site" >/dev/null
    (cd "$work/site" && sha256sum -c --quiet SHA256SUMS)
    meta='{"kind": 35128, "d": "liq2sol", "title": "liq2sol", "description": "Trustless, confidential atomic swaps between USDt on Liquid and USDT on Solana.", "source": "https://github.com/SwapMarket/liq2sol"}'
    ;;
  sol2base)
    from_deploy SwapMarket/sol2base "Deploy Pages" github-pages
    same_as_live https://swapmarket.github.io/sol2base/
    meta='{"kind": 35128, "d": "sol2base", "title": "sol2base", "description": "Move USDC and USDT between Solana and Base with Circle'"'"'s CCTP.", "source": "https://github.com/SwapMarket/sol2base"}'
    ;;
  eth2base)
    from_deploy SwapMarket/eth2base "Deploy Pages" github-pages
    same_as_live https://swapmarket.github.io/eth2base/
    meta='{"kind": 35128, "d": "eth2base", "title": "eth2base", "description": "Move USDC between Ethereum and Base with Circle'"'"'s CCTP.", "source": "https://github.com/SwapMarket/eth2base"}'
    ;;
  sol2tron)
    from_deploy SwapMarket/sol2tron "Deploy Pages" github-pages
    same_as_live https://swapmarket.github.io/sol2tron/
    meta='{"kind": 35128, "d": "sol2tron", "title": "sol2tron", "description": "Move USDT between Solana and Tron.", "source": "https://github.com/SwapMarket/sol2tron"}'
    ;;
  btc)
    from_deploy SwapMarket/swapmarket.github.io "Deploy main" app-dist
    rm -rf "${work:?}/site/docs"
    same_as_live https://swapmarket.github.io/
    meta='{"kind": 15128, "title": "SwapMarket", "description": "Non-custodial swaps between Bitcoin, Lightning, Liquid and Rootstock, through competing providers.", "source": "https://github.com/SwapMarket/swapmarket.github.io"}'
    ;;
  *)
    echo "usage: scripts/publish-nsite.sh liq2sol [COMMIT] | sol2base | eth2base | sol2tron | btc" >&2; exit 2 ;;
esac

python3 -I "$repo/scripts/nsite/files.py" "$work/site" > "$work/files.json"
echo "$meta" > "$work/site.json"
cp "$repo/scripts/nsite/publish.html" "$work/"
echo "open http://127.0.0.1:8765/publish.html in the browser with the key; Ctrl-C when done"
python3 -I -m http.server 8765 --bind 127.0.0.1 --directory "$work"
