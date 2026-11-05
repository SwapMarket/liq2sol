# liq2sol

Trustless atomic swaps between USDt on Liquid and USDT on Solana: a SimplicityHL HTLC on Liquid, an Anchor
escrow on Solana, a Rust maker daemon, a static taker web app, and RFQ over Nostr. Sister projects:
`~/src/eth2base` and `~/src/sol2base` (static CCTP bridges; their Solana/Base wallet code is reusable reference).

The full brief is in `docs/prompt.md`. Read it first; it governs scope and working rules.

## Rules (from the brief)

- **Phases 0 and 1 are documents only. Stop after Phase 1 and wait for the user's review before any implementation code.**
- Testnet/devnet only. Never generate, request or handle mainnet keys.
- Don't guess APIs: verify against docs/source and cite them. Mark anything unverified **UNVERIFIED**.
- Small, auditable code over features. Every component needs tests, including adversarial ones.
- When a decision is the user's, list options with trade-offs instead of picking silently.
- The maker daemon is in **Rust** (user's choice).

## Repo conventions

- Commits are signed by SwapMarket (repo-local git config: user `SwapMarket`, key `01151A25AAE22F55`). Ask before
  committing or pushing; the user has said "you can commit" per change so far.
- Public repo: https://github.com/SwapMarket/liq2sol (`origin`, branch `main`).
- End commit messages with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Status (2026-11-05)

- **Phase 0: done.** `docs/assumptions.md` has every finding with links and the UNVERIFIED list. Section 10 holds later
  findings, some of which update earlier sections.
- **Phase 1: documents written.** `docs/protocol.md`, `contracts.md`, `nostr-rfq.md`, `client.md`, `maker.md`,
  `threat-model.md`, `test-plan.md`, plus `contracts/liquid/src/htlc.simf`. The user accepted all decisions D1–D10
  (`docs/protocol.md` section 9) on 2026-11-05. 
- **Phase 2: started, with approval (2026-11-05).** Build order: contracts with tests, CLI end-to-end, maker, client.
  `contracts/liquid` (HTLC library and tests) and `contracts/liquid-wasm` are done, and proofs F1, F2, F3 and F5 pass. `programs/solana-escrow` has all core and vault instructions with tests. `cli/` ran full swaps and aborts in both
  directions on Liquid testnet and Solana devnet, using stand-in assets. `cli/tests/failures.rs` runs the failure scenarios on regtest plus devnet. `maker/core` has the
  engine, signer policy and store, with tests and mutation checks, but no daemon runs yet. Next: the signer process (which must derive amounts, destinations and
  chain heights itself, because `policy::Request` fields are currently caller-supplied), the chain adapter, then RFQ, then the static client. E14 still needs a real cluster restart. E16 only runs on a local validator.

## Design points that are easy to get wrong

Details and sources are in the docs. These are recorded because older assumptions were wrong.

- **Solana slots are about 213 ms now, not 400 ms** (Alpenglow era). Never hard-code the rate.
- **`unix_timestamp` doesn't jump after a Solana restart under Alpenglow.** It catches up at up to twice nominal. Deadlines
  use slots, and short-leg escrows also close on `LastRestartSlot`.
- **Halts are asymmetric.** A halt on the chain holding the short leg lets the taker steal, because the long leg's clock
  keeps running: Liquid halt in S→L (mitigated by `extend`), Solana restart in L→S (mitigated by `close_on_restart`).
- **Liquid's 2-block finality is not absolute.** The federation rolled back the chain in September 2026, after a roughly
  4-day stoppage.
- **USDt can't pay Liquid fees** in practice: the mempool counts only the policy asset.
- **Never reveal the preimage before** confirming the counterparty's lock on two independent endpoints and simulating
  the claim. A failed transaction still leaks it.
- **The taker's Solana wallet connects through WalletConnect or an extension wallet, with no page-held key** (user's
  decision, 2026-11-05). The Reown project ID is shared with `sol2base`, whose `app.js` is the reference code. Sign with
  `solana_signTransaction` and broadcast from the app, so the app controls when the preimage goes out.
- **Anchor is v1.2.2, repo `otter-sec/anchor`.**
- **The second Solana endpoint is Helius, using an optional key the taker provides** (user's decision, 2026-11-05). Never
  ship a key in the static app.

## Environment notes

- This shell sets `RUSTFLAGS=-C link-arg=-fuse-ld=lld`, which breaks wasm32 links: build wasm with `RUSTFLAGS=""`.
- Tools present: Rust 1.97.1 with `wasm32-unknown-unknown`, clang 18 (wasm32 backend), wasm-pack, Node, Foundry.
- Regtest tests: `ELEMENTSD=~/bin/elementsd ELEMENTS_CLI=~/bin/elements-cli cargo test -p liq2sol-htlc --test regtest -- --ignored`.
  WASM parity: `scripts/check-wasm-parity.sh`.
- Solana program, from `programs/`: `cargo build-sbf --manifest-path solana-escrow/Cargo.toml && cargo test`. It is a separate
  workspace pinned to Rust 1.95.0 and to Anchor's template lockfile, because LiteSVM's newest dependencies need a newer Rust.
  PATH needs `~/.local/share/solana/install/active_release/bin` and `~/.avm/bin`. `programs/.keys/` holds devnet-only keypairs
  and is gitignored; losing them means changing `declare_id!` and `USDT_MINT`.
- `simc` (SimplicityHL CLI with `.args`/`.wit` support): `cargo install simplicityhl --version 0.9.0 --features serde --locked`.
  Compile-check the draft: `simc contracts/liquid/src/htlc.simf --args contracts/liquid/htlc.args`. It has no `--version` flag.
- End-to-end, from `cli/`: `SOLANA_RPC=https://api.devnet.solana.com cargo run -- swap l2s` (also `swap s2l`, `abort l2s|s2l`, `liquid-check`, `balances`). It refuses mainnet.
  Test wallets are in `cli/.keys/` (gitignored). The Liquid faucet is rate-limited, and the devnet CLI faucet often refuses this machine, so use faucet.solana.com.
- Maker rules, from `maker/`: `cargo test`. No chain or network needed.
- Failure scenarios, from `cli/`: `ELEMENTSD=~/bin/elementsd ELEMENTS_CLI=~/bin/elements-cli SOLANA_RPC=https://api.devnet.solana.com cargo test --test failures -- --ignored --nocapture`.
- `solana-test-validator` aborts here without AVX, prebuilt or built from source (`reed-solomon-erasure` runs AVX2 code unchecked). It runs under WSL on the Windows host, with `--bind-address <WSL IP>` (not `0.0.0.0`, which panics) and a Windows `netsh` port proxy; use `SOLANA_RPC=http://<Windows LAN IP>:8899`.
- `/tmp` is wiped on reboot. Keep anything worth keeping in the repo.
- Free Solana RPC (publicnode) blocks `getProgramAccounts`/token-account indexing and old history; plan clients around
  that (remember accounts locally, derive PDAs).
