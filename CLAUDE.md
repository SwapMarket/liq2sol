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
  `threat-model.md`, `test-plan.md`, plus `docs/drafts/liquid_htlc.simf`. The user accepted all decisions D1–D10
  (`docs/protocol.md` section 9) on 2026-11-05. **Do not write implementation code until the user explicitly approves
  starting Phase 2.**

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
- **The second Solana endpoint is Helius, using an optional key the taker provides** (user's decision, 2026-11-05). Never
  ship a key in the static app.

## Environment notes

- This shell sets `RUSTFLAGS=-C link-arg=-fuse-ld=lld`, which breaks wasm32 links: build wasm with `RUSTFLAGS=""`.
- Tools present: Rust 1.97.1 with `wasm32-unknown-unknown`, clang 18 (wasm32 backend), wasm-pack, Node, Foundry.
- `simc` (SimplicityHL CLI with `.args`/`.wit` support): `cargo install simplicityhl --version 0.9.0 --features serde --locked`.
  Compile-check the draft: `simc docs/drafts/liquid_htlc.simf --args docs/drafts/liquid_htlc.args`. It has no `--version` flag.
- `/tmp` is wiped on reboot. Keep anything worth keeping in the repo.
- Free Solana RPC (publicnode) blocks `getProgramAccounts`/token-account indexing and old history; plan clients around
  that (remember accounts locally, derive PDAs).
