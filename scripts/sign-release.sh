#!/usr/bin/env bash
# Sign a maker release's SHA256SUMS with the SwapMarket key, once release.yml has published it, and add the signature to
# the release. Makers' update.sh installs nothing without it. Run it on the machine holding the key.
# Usage: scripts/sign-release.sh v0.06
set -euo pipefail
tag=${1:?usage: $0 vX.YY}
key=E7947F8DBC700926D8A8175101151A25AAE22F55
repo=SwapMarket/liq2sol
root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cd "$work"

# The tag itself must be ours, or we'd be vouching for a build of someone else's code.
git -C "$root" fetch -q origin tag "$tag"
git -C "$root" verify-tag "$tag"
gh release download "$tag" --repo "$repo" --pattern 'SHA256SUMS' --pattern '*.tar.gz'
sha256sum -c SHA256SUMS
gpg --local-user "$key" --armor --detach-sign SHA256SUMS
gpgv --keyring "$root/deploy/release-key.gpg" SHA256SUMS.asc SHA256SUMS
gh release upload "$tag" SHA256SUMS.asc --repo "$repo" --clobber
echo "signed $tag"
