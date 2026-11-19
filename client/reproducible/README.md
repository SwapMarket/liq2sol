# Reproducible page build

The published page at https://swapmarket.github.io/liq2sol/ is built here, in a container pinned to exact tools, so anyone can rebuild
it from the commit it names and get the same bytes.

- `Dockerfile`: Rust 1.99.1 by image digest, `clang`, `llvm` and Node from one Debian snapshot date, `wasm-bindgen-cli` at the version in
  `client/wasm/Cargo.lock`, and binaryen 117's `wasm-opt`, checked by hash. Always built as `linux/amd64`.
- `build-inside.sh`: builds one commit's `git archive` at fixed paths, runs `client/stamp.mjs`, and writes `SHA256SUMS`.

## Check the live page

With Docker and this repository:

```sh
scripts/check-page.sh
```

It reads the commit from the page's footer, rebuilds it with `scripts/build-page.sh`, and compares every file the site serves with the
rebuild. The first run builds the toolchain image, which takes about 20 minutes. Later runs take about 5.

## Build one commit

```sh
scripts/build-page.sh <commit> <out folder>
```

The deploy (`.github/workflows/pages.yml`) runs this twice, on two machines, and publishes only if both agree byte for byte.

## The nsite on Nostr

The page is also an nsite (NIP-5A, a draft): its files sit on Blossom servers, and a signed kind 35128 event, the manifest, maps
each path to its sha256. It is SwapMarket's named site `liq2sol`, under
`npub1xhklzztd83v34zsf9da2gvfrctj5wcsvsx09er9v26jy7r0uq8yq4vme7d` (hex `35edf109…f0dfc01c8`), at

  https://1cdzwdkc0uepk5ll8mhpcbyw3xh8gakldbfdq8fphhqnv503bcliq2sol.nsite.lol/

- **Whose it is.** The address's first 50 characters are the public key in base36, then the site name:
  `'%064x' % int('1cdzwdkc0uepk5ll8mhpcbyw3xh8gakldbfdq8fphhqnv503bc', 36)` is the hex key above. A gateway serves that key's manifest,
  so the address alone says whose site it is, not that the gateway serves it faithfully.
- **Check it** like the main site: `scripts/check-page.sh <nsite address>`. The nsite has no `SHA256SUMS`, since NIP-5A paths need an
  extension. The manifest's `path` tags carry the same hashes.
- **Gateways** (checked 2026-11-19): nsite.lol, nwb.tf and nsite.cloud serve it correctly. nsite.cloud tells browsers to cache it for a
  year, so returning visitors may not get a fix. nsite.run adds its own Content-Security-Policy, which blocks the WebAssembly, so the
  page can't start there. Each gateway is its own origin: swaps saved on one don't show on another, though recovery files work on all.
- **WalletConnect** works only on origins in the Reown project's allowlist: `swapmarket.github.io` and the nsite.lol address.
- **Blossom servers:** blossom.yakihonne.com and nostr.download hold every file. nostr.download labels HTML and JavaScript
  `text/plain`, so it is listed second. Many servers refuse HTML, JavaScript or WebAssembly.
- **Publishing** is by hand, after a release, like signing it: `scripts/publish-nsite.sh [commit]` builds the commit and serves
  `scripts/nsite/publish.html` on `http://127.0.0.1:8765`, which signs through a NIP-07 extension (Alby) holding SwapMarket's key.
  The key never reaches the repository, the script or CI, and the page refuses any other key. Gateways read the manifest from the
  key's own relays (kind 10002), which the page publishes to as well.

## Limits

- It trusts the pinned image, the Debian snapshot and crates.io's checksums in `Cargo.lock`, as any build does.
- Browser tests build the WebAssembly with `wasm-pack` instead, so they test the same source, not the same bytes.
- `index.html` pins every other file by hash (`client/stamp.mjs`), so checking it alone covers the page in browsers with import-map
  integrity.
