#!/usr/bin/env bash
# deploy/install-elements.sh must install nothing that isn't Elements' signed release. Its --fetch-only path is run with a
# stand-in curl serving files from a folder: the real signed SHA256SUMS.asc (downloaded once), and archives made here.
# Then once against GitHub for real, unless OFFLINE=1. Needs no root, and installs nothing.
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
version=23.3.4
arch=$(uname -m | sed 's/arm64/aarch64/')
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
fail() { echo "FAIL: $*" >&2; exit 1; }

served=$work/served bin=$work/bin
mkdir -p "$served" "$bin"
curl -fsSL -o "$served/SHA256SUMS.asc" \
  "https://github.com/ElementsProject/elements/releases/download/elements-$version/SHA256SUMS.asc"
# Serves the last part of the URL from $served, as curl -fsSL -o FILE URL.
cat > "$bin/curl" <<EOF
#!/usr/bin/env bash
out=\$3 url=\$4
cp "$served/\${url##*/}" "\$out"
EOF
chmod +x "$bin/curl"

# Runs the script from a copy of deploy/ with the key given, and passes if it refuses with the message given.
refuses() {
  local name=$1 key=$2 message=$3 copy=$work/copy-$RANDOM out
  mkdir -p "$copy"
  cp "$repo/deploy/install-elements.sh" "$copy/"
  cp "$key" "$copy/elements-release-key.gpg"
  if out=$(PATH="$bin:$PATH" "$copy/install-elements.sh" --fetch-only "$work/out-$RANDOM" 2>&1); then
    fail "$name: it installed"
  fi
  grep -q "$message" <<<"$out" || fail "$name: refused, but with: $out"
  echo "ok: $name"
}

# An archive that isn't the release, under the release's name.
file=elements-$version-$arch-linux-gnu.tar.gz
mkdir -p "$work/fake/elements-$version/bin"
printf '#!/bin/sh\necho planted\n' > "$work/fake/elements-$version/bin/elementsd"
cp "$work/fake/elements-$version/bin/elementsd" "$work/fake/elements-$version/bin/elements-cli"
chmod +x "$work/fake/elements-$version/bin/"*
tar -czf "$served/$file" -C "$work/fake" "elements-$version"
refuses 'a changed archive' "$repo/deploy/elements-release-key.gpg" "doesn't match the signed SHA256SUMS"

# The signed sums with the archive's line changed to the planted one's hash.
cp "$served/SHA256SUMS.asc" "$work/real.asc"
sed -i "s/^[0-9a-f]\{64\}  $file\$/$(sha256sum "$served/$file" | cut -d' ' -f1)  $file/" "$served/SHA256SUMS.asc"
cmp -s "$served/SHA256SUMS.asc" "$work/real.asc" && fail "the test didn't change the sums"
refuses 'changed sums' "$repo/deploy/elements-release-key.gpg" "isn't signed by Elements' release key"
cp "$work/real.asc" "$served/SHA256SUMS.asc"

# The real sums, checked against another key: this project's own release key.
refuses 'another key' "$repo/deploy/release-key.gpg" "isn't signed by Elements' release key"

# Too old.
if out=$("$repo/deploy/install-elements.sh" --fetch-only "$work/old" --version 23.3.3 2>&1); then fail 'installed 23.3.3'; fi
grep -q 'older than' <<<"$out" || fail "refused 23.3.3, but with: $out"
echo 'ok: an older version'

if [ "${OFFLINE:-}" != 1 ]; then
  "$repo/deploy/install-elements.sh" --fetch-only "$work/real" >/dev/null
  "$work/real/bin/elementsd" -version | grep -q "v$version" || fail 'the real release did not unpack'
  echo 'ok: the real release downloads, checks out and runs'
fi
echo PASS
