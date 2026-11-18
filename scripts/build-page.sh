#!/usr/bin/env bash
# Build the page exactly as published, from one commit, in the pinned image (client/reproducible). Anyone with Docker gets the same
# bytes, and so the same SHA256SUMS, as the site. Usage: scripts/build-page.sh [COMMIT] OUT
# COMMIT defaults to HEAD. Only what the commit holds is built: uncommitted changes are left out. Crates download inside the
# container, checked against Cargo.lock.
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
if [ $# -eq 2 ]; then commit=$1 out=$2; elif [ $# -eq 1 ]; then commit=HEAD out=$1; else
  echo "usage: scripts/build-page.sh [COMMIT] OUT" >&2; exit 2
fi
[ ! -e "$out" ] || { echo "$out exists already" >&2; exit 1; }
full=$(git -C "$repo" rev-parse --verify "$commit^{commit}")
bindgen=$(git -C "$repo" show "$full:client/wasm/Cargo.lock" | awk '/^name = "wasm-bindgen"$/ { getline; gsub(/[^0-9.]/, ""); print; exit }')
[ -n "$bindgen" ] || { echo "no wasm-bindgen version in client/wasm/Cargo.lock" >&2; exit 1; }

# The image comes from the commit's own recipe, so an old commit builds with the tools it was published with.
ctx=$(mktemp -d)
trap 'rm -rf "$ctx"' EXIT
git -C "$repo" archive "$full" client/reproducible | tar -x -C "$ctx"
# Named by its recipe, so an image already here is used again, and CI can keep one between runs.
recipe=$(cat "$ctx/client/reproducible/Dockerfile" "$ctx/client/reproducible/build-inside.sh" <(echo "$bindgen") | sha256sum | cut -c1-16)
image="liq2sol-page-builder:$recipe"
if ! docker image inspect "$image" >/dev/null 2>&1; then
  echo "building the toolchain image $image, which takes a while the first time" >&2
  docker build -q --platform linux/amd64 --build-arg "WASM_BINDGEN_VERSION=$bindgen" -t "$image" "$ctx/client/reproducible" >/dev/null
fi

mkdir -p "$out"
git -C "$repo" archive "$full" | docker run --rm -i --platform linux/amd64 "$image" "${full::12}" | tar -x -C "$out"
echo "built ${full::12} into $out" >&2
cat "$out/SHA256SUMS"
