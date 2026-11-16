# Going to mainnet

Mainnet support was approved on 2026-11-16, with four decisions by the user: a guarded network switch in the page's Settings, an escrow program that is immutable from the start, the project's own review of `musig2` (`docs/musig2-review.md`), and at most 100 USDT a swap. This page tracks what is done and what is left.

**No mainnet key is ever made, asked for or handled in this repository or by its tooling.** Whoever deploys the program and runs a maker makes and funds their own keys.

## Done

| Part | State |
|---|---|
| Network choice in the maker | `"network": "mainnet"` in `signer.json` and `maker.json`. Both chains must agree with it, before any key is loaded (`sol::enter`). Per-swap limits above 100 USDT and regtest settings are refused. |
| Network choice in the page | A switch in Settings. Each network has its own swaps, settings and logs, and switching reloads the page. It waits while a swap is in progress. Both chains must agree with the choice (`checkGenesis`, and again in the WebAssembly). |
| Mainnet constants | Liquid genesis, Tether's USDt asset, L-BTC, Solana genesis and Tether's USDT mint, each checked against the chain. |
| The cap | 100 USDT a swap: `MAINNET_MAX_PER_SWAP` for the maker, `maxSwapUnits` for the page, checked on the agreed terms. |
| `musig2` | Reviewed, with tests against libsecp256k1's verifier, on both backends. |
| Page integrity | The deploy pins every script, the style sheet and the WebAssembly by hash in `index.html` (`client/stamp.mjs`). A changed file is refused, so `index.html` alone is what to check. |
| Recovery files | Carry their network. A test network's file can't be restored on mainnet, or the reverse. |

Mainnet can't be selected anywhere until the program id below is set. Until then the switch is disabled and a maker told `mainnet` refuses to start.

## Left to do

### 1. Deploy the escrow program (the user, with their own keys)

The program needs an address of its own on mainnet. Its keypair is only used for the deploy.

1. Make the program keypair, on your own machine: `solana-keygen new -o escrow-mainnet-program.json`. Keep the file private. Send only its public address (`solana-keygen pubkey escrow-mainnet-program.json`).
2. The address is `DsgtATJzzejhSZweUsna77kU5vYFqrSojoEYUNmufi6b`, set in `declare_id!` under the program's `mainnet` feature. Only after step 5 confirms the deploy does it go into `sol::MAINNET_PROGRAM` and `MAINNET.programId` in `client/site/js/config.js`, which is what turns mainnet on.
3. Build it from that commit, with the feature that pins Tether's mint: from `programs/`, `cargo build-sbf --manifest-path solana-escrow/Cargo.toml --features mainnet`. Note the file's `sha256sum`.
4. Deploy it as final, which is what makes it immutable: `solana program deploy --url mainnet-beta --program-id escrow-mainnet-program.json --final programs/target/deploy/solana_escrow.so`. The rent is about 1.6 SOL, and the deploy also needs about as much again for a buffer, which comes back.
5. Check what is on chain: `solana program show <address> --url mainnet-beta` must say it is not upgradeable, and `solana program dump <address> onchain.so --url mainnet-beta` must match the build's hash once trailing zero padding is removed.

An immutable program can never be fixed. A bug means a new program, a new address and a new release of the page and the maker. Swaps already open on the old one finish there.

### 2. Before the switch is turned on

- **A reproducible page build**, so anyone can check a published `index.html` against the source. The pinning itself is done (below).
- **The page's signing backend.** It uses `k256`, which isn't constant time (`client/wasm/.cargo/audit.toml`). The key lasts one swap and stays in the user's browser.
- **Real wallets on real phones**, and Firefox and Safari, on test networks first.
- **Vetted makers.** The mainnet list in `client/site/js/vetted.js` is empty.

### 3. Running a mainnet maker

- New keys and a new folder: never copy a test maker's keys or databases.
- `"network": "mainnet"` in both configs, mainnet endpoints, and limits at or under 100 USDT.
- Our own Liquid node (`LiquidSource::Node`) and a paid Solana RPC: public endpoints rate-limit and keep little history, and the closed-escrow check reads history after a deadline.
- `page_to` set and tested, and something outside the machine that notices if the maker stops.

### 4. Not yet proven on real chains

- A real Solana restart (E14).
- The closed-escrow history read after a deadline.
- A swap with a real wallet paying a confidential mainnet address.
