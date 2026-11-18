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

## Limits

- It trusts the pinned image, the Debian snapshot and crates.io's checksums in `Cargo.lock`, as any build does.
- Browser tests build the WebAssembly with `wasm-pack` instead, so they test the same source, not the same bytes.
- `index.html` pins every other file by hash (`client/stamp.mjs`), so checking it alone covers the page in browsers with import-map
  integrity.
