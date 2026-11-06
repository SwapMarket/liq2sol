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

- Commits are signed by SwapMarket (repo-local git config: user `SwapMarket`, key `01151A25AAE22F55`). Work directly on
  `main`. Commit when it makes sense, but ask before each push, because deploys are slow (user's decision, 2026-11-06).
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
  directions on Liquid testnet and Solana devnet, using stand-in assets. `cli/tests/failures.rs` runs the failure scenarios on regtest plus devnet. `maker/` has the
  engine, signer process and daemon loop, tested with real processes under crashes, halts and rollbacks, with the maker vault on. `shared/rfq` and `maker/daemon/src/rfq.rs` add quotes over Nostr, tested on a mock relay and through `relay.coinos.pro`, the user's chosen relay. Next: the static client. E14 still needs a real cluster restart. E16 only runs on a local validator.

**Confidential design (on `main`, 2026-11-06):** the redesign in `docs/confidential.md` replaces the Simplicity HTLC with a MuSig2 taproot output, adapter-signed claims and blinded coins. Contracts, signer, daemon, RFQ and CLI are all moved over and tested. The docs describe that design. The program is on devnet at `EcK819kqJT6pHzQVLbzbUnK6ieaiS9JVGubPhC6Q2puU`; the key is `programs/.keys/escrow-confidential-program.json`. The static client is in `client/`, deployed by `.github/workflows/pages.yml` to https://swapmarket.github.io/liq2sol/. Next: real wallets, other browsers, and release hardening (`docs/client.md` section 6).

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
- **Nothing on-chain checks a swap coin's asset or amount.** Each party unblinds it and checks. That check is the whole defence (`exact_secret_coin`).
- **An adapter signature must be durable before anything relies on it:** before a Solana lock (L→S), or before the maker's share goes out (S→L). One session per swap.
- **A nonce signs once and is never stored.** A crash abandons the session.
- **Liquid charges confidential transactions on discount vsize,** about a tenth of vsize. `-acceptdiscountct` is on by default on `liquidv1` and `liquidtestnet`, and off on regtest, so regtest nodes must set it.
- **Wallets must hold only blinded coins:** an explicit input bounds the blinded outputs.
- **A fee coin is 30 sats (user's decision, 2026-11-06),** just above a refund's 29-sat floor, because Liquid has no fee spikes. It leaves no margin if the floor rises.
- **In the page, every change to a swap goes through `withSwap`.** A slow tick saving an old copy lost a lock and the maker's one-time share. Inputs stay the same elements across renders, or typing is lost when offers arrive.
- **The page's CSP allows `unsafe-eval`, on testnet and mainnet** (user's decision, 2026-11-06), or Phantom can't connect: its injected scripts use `new Function`. The test wallet doesn't, so browser tests won't show this.
- **Mainnet client builds must be obfuscated,** as `SwapMarket/swapmarket.github.io` does (user's decision, 2026-11-06). Testnet builds aren't. See `docs/client.md` section 6.
- **A recovery file is saved before a swap starts, so it says nothing about how far the swap got.** A restored swap reads both chains before offering any step that spends.
- **An early refund and a Solana lock must exclude each other** (L→S). The signer refuses one after the other, and records its promise before its share leaves. It is no defence against griefing, since a griefing maker just says no.
- **The program no longer logs instruction names** (`no-log-ix-name`, on by default). That only hides labels from casual viewers.
- **The public test makers run on the user's Debian VPS** (`liq2sol-maker@first` and `@second`, from the release), with their keys there. Their Nostr keys are on the vetted list (`client/site/js/vetted.js`). `scripts/run-test-maker.sh` still runs a local one, whose key isn't listed.
- **Nothing in the Settings logs may hold a secret.** People paste them into chats. `js/logs.js` scrubs them, and the browser test checks after a real swap.
- **A refund's fee coin can be any amount from 30 sats.** Aqua won't send under 100, and the Blockstream wallet sends 30.
- **The signer never reuses a coin it has signed away.** A retried lock must send the same bytes, so a reused coin could never land.
- **Kind 21059 wrappers carry the real time.** Relays drop ephemeral events more than a few seconds old.
- **The second Solana endpoint is Helius, using an optional key the taker provides** (user's decision, 2026-11-05). Never
  ship a key in the static app.

## Environment notes

- This shell sets `RUSTFLAGS=-C link-arg=-fuse-ld=lld`, which breaks wasm32 links: build wasm with `RUSTFLAGS=""`.
- Tools present: Rust 1.97.1 with `wasm32-unknown-unknown`, clang 18 (wasm32 backend), wasm-pack, Node, Foundry.
- Regtest tests: `ELEMENTSD=~/bin/elementsd ELEMENTS_CLI=~/bin/elements-cli cargo test -p liq2sol-swapkey --test regtest -- --ignored --nocapture`.
- Solana program, from `programs/`: `cargo build-sbf --manifest-path solana-escrow/Cargo.toml && cargo test`. It is a separate
  workspace pinned to Rust 1.95.0 and to Anchor's template lockfile, because LiteSVM's newest dependencies need a newer Rust.
  PATH needs `~/.local/share/solana/install/active_release/bin` and `~/.avm/bin`. `programs/.keys/` holds devnet-only keypairs
  and is gitignored; losing them means changing `declare_id!` and `USDT_MINT`.
- `simc` (SimplicityHL CLI with `.args`/`.wit` support): `cargo install simplicityhl --version 0.9.0 --features serde --locked`.
- End-to-end, from `cli/`: `SOLANA_RPC=https://api.devnet.solana.com cargo run -- swap l2s` (also `swap s2l`, `abort l2s|s2l`, `liquid-check`, `balances`). It refuses mainnet.
  Test wallets are in `cli/.keys/` (gitignored). The Liquid faucet is rate-limited, and the devnet CLI faucet often refuses this machine, so use faucet.solana.com.
- Maker rules, from `maker/`: `cargo test`. Signer attack suite: `ELEMENTSD=~/bin/elementsd ELEMENTS_CLI=~/bin/elements-cli SOLANA_RPC=https://api.devnet.solana.com cargo test -p liq2sol-signer --test process -- --ignored --nocapture`.
- Daemon scenarios, from `maker/`: same env, `cargo test -p liq2sol-maker --test e2e -- --ignored --nocapture` (about 13 minutes; `SCENARIO=rollback` runs one). `api.devnet.solana.com` sometimes times out here; the WSL validator also works, but slower.
- Run `cargo build` in `maker/` before the daemon suite, which starts the signer binary from the same workspace. A crashed run strands stand-in tokens in a throwaway vault; `liq2sol-cli setup` tops wallets up, and the deployer in `programs/.keys` has devnet SOL.
- When cleaning up test processes, match exact names with `pgrep -x`: `pgrep -f` patterns match the running shell's own command line and kill it.
- Failure scenarios, from `cli/`: `ELEMENTSD=~/bin/elementsd ELEMENTS_CLI=~/bin/elements-cli SOLANA_RPC=https://api.devnet.solana.com cargo test --test failures -- --ignored --nocapture`.
- `solana-test-validator` aborts here without AVX, prebuilt or built from source (`reed-solomon-erasure` runs AVX2 code unchecked). It runs under WSL on the Windows host, with `--bind-address <WSL IP>` (not `0.0.0.0`, which panics) and a Windows `netsh` port proxy; use `SOLANA_RPC=http://<Windows LAN IP>:8899`.
- `/tmp` is wiped on reboot. Keep anything worth keeping in the repo.
- `solana-test-validator` has SIMD-0500 active, which refuses to deploy SBPF v0: build with `cargo build-sbf --arch v3`, and deploy with `--use-rpc` through the port proxy. A restarted validator is a new chain, so redeploy and rerun `liq2sol-cli setup`, after the airdrops finalize.
- Client: build WASM with `client/build.sh`. Unit tests: `node --test client/test/*.test.mjs`. WASM parity: `cargo test` in `client/wasm`. Real browser against the real daemon: `npm install` in `client/`, then from `maker/`, `cargo build` and `CLIENT_TEST=1 SCENARIO=browser` with the daemon suite's environment. Use the local validator for that: the fast polling gets rate-limited on public devnet. Run it more than once, because timing bugs have shown up here.
- Devnet faucet: `programs/devnet-faucet`, at `GAcP2avhVggTqeHSEoKGmGXw4wpJCTasRBjbUfrC1Xpu`, key in `programs/.keys/devnet-faucet-program.json`. A local validator needs it deployed too (`--arch v3 --use-rpc`), then `liq2sol-cli faucet-init <program>` and `spl-token mint` to its vault, or the browser's faucet scenario fails.
- Failed daemon runs strand stand-in tokens in throwaway vaults. `liq2sol-cli setup` tops wallets up, once airdrops have finalized.
- Mutation checks for this design: `RUSTFLAGS="" python3 -I -u scripts/mutate-confidential.py`, on a clean tree, with the same environment as the suites.
- Free Solana RPC (publicnode) blocks `getProgramAccounts`/token-account indexing and old history; plan clients around
  that (remember accounts locally, derive PDAs).
