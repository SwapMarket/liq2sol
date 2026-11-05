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
  committing or pushing; the user has said "you can commit" per change so far. No GitHub remote yet: creating the
  public repo is the user's call (`gh repo create SwapMarket/liq2sol ...`).
- End commit messages with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Status (2026-11-05)

- **Phase 0: done.** `docs/assumptions.md` has every finding with links and the UNVERIFIED list.
  Highlights: SimplicityHL 0.9.0; jets for sha256 / BIP-340 / `check_lock_height` / output asset-amount-script
  introspection confirmed; leaf version 0xbe with witness `[witness, program, cmr, control_block]`;
  **compiling SimplicityHL in WASM works** (probe matched native CMR); published `lwk_wasm` lacks Simplicity, so we
  build our own WASM; L-USDt precision 8 vs SOL-USDT 6 decimals; Solana USDT has a freeze authority.
- **Phase 1: not started**, except `docs/drafts/liquid_htlc.simf` (+ `.args`), which compiles with `simc 0.9.0`.
  Remaining: `docs/protocol.md`, `docs/contracts.md`, `docs/nostr-rfq.md`, `docs/client.md`, `docs/maker.md`,
  `docs/threat-model.md`, `docs/test-plan.md`.

## Design notes so far (inputs for Phase 1)

- **Draft HTLC** (`docs/drafts/liquid_htlc.simf`): params `PAYMENT_HASH, RECIPIENT_PK, SENDER_PK, LOCK_HEIGHT,
  USDT_ASSET, AMOUNT, RECIPIENT_SCRIPT_HASH`; witness `CLAIM_OR_REFUND: Either<(u256, Signature), Signature>`.
  Claim checks the preimage, the recipient signature, that the spent coin (`current_amount`) is explicit USDt of exactly
  AMOUNT, and that output 0 is explicit USDt AMOUNT to `RECIPIENT_SCRIPT_HASH`. Refund: `check_lock_height` + sender
  signature. Open point: with a recipient signature, the covenant's value is protecting the recipient if its
  ephemeral (browser) key leaks, and making the claim delegable (watchtowers). A sig-less, covenant-only claim is an
  alternative worth presenting as an option.
- **Decimals:** quote in SOL-USDT base units (6 dp); Liquid amount = SOL amount × 100 (exact). Liquid amounts must be
  multiples of 100 base units; reject otherwise rather than rounding.
- **Liquid fees:** fees are L-BTC, so the USDt amount stays exact (output 0 = AMOUNT), and the claimer adds its own L-BTC
  input. A taker who receives L-USDt (SOL→L direction) may hold no L-BTC: options are (a) the maker funds a small L-BTC
  "fee allowance" UTXO under the same contract, (b) the taker must hold L-BTC, or (c) the covenant allows
  `output 0 = AMOUNT - fee` with the fee taken in USDt. That needs some other way to pay the L-BTC fee, so (c) only works if
  Liquid supports non-L-BTC fees, which is **UNVERIFIED**. This is a user decision.
- **Deadlines on Solana: use slot heights, not unix time.** If Solana halts and restarts, `unix_timestamp` jumps
  forward and can expire a deadline instantly. In SOL→L that lets a taker who already claimed on Liquid race a refund
  on Solana. Slots pause during a halt, like Liquid block heights.
- **Free option:**
  - **L→S:** the maker locks on Solana only after the taker's Liquid lock has 2 confirmations, so a taker bond in the
    escrow can be enforced trustlessly. The bond is forfeited to the maker if the maker locked by a deadline the program
    checks and the taker didn't claim before T2; it is returned on claim.
  - **S→L:** the Solana program can't see whether the maker locked on Liquid (that needs the Phase 2 light client), so a
    bond would either let a maker who never locks take it, or never be enforceable.
  - Options for the user: priced-in premium plus short timeouts plus per-key rate limits and Nostr reputation; a
    capped, reputation-backed bond; or defer the trustless bond to Phase 2.
- **Timeouts (starting point, to justify in protocol.md):**
  - L→S: T2 (Solana, maker refund) about 30 min of slots after the maker locks; T1 (Liquid, taker refund) = taker
    lock + about 240 blocks. T1 − T2 must leave the maker hours to claim on Liquid after the preimage is revealed.
  - S→L mirrors this: taker's Solana lock about 4 h of slots; maker's Liquid lock about 120 blocks.
  - Confirmations: Liquid 2 blocks, Solana `finalized`.
- **Watchtowers:** make the Solana claim permissionless (anyone with the preimage claims to the fixed recipient). On
  Liquid, the taker's refund can be pre-signed (`sig_all_hash` fixes outputs) and handed to a watchtower.
- **Phase 2 light client:** Liquid headers are signed 11-of-15 (ECDSA, **UNVERIFIED** encoding). One
  `secp256k1_recover` costs 25k CU; without a recovery id, try both, so about 11 × 2 × 25k ≈ 550k CU per header. That
  fits in a 1.4M-CU transaction. Dynafed: track the active signblockscript per 20,160-block epoch.
- **Prior art to read:** LWK's `lwk_anyswap` and `lwk_boltz`, and the Simplicity taproot builder in LWK bindings (NUMS
  internal key).

## Environment notes

- This shell sets `RUSTFLAGS=-C link-arg=-fuse-ld=lld`, which breaks wasm32 links: build wasm with `RUSTFLAGS=""`.
- Tools present: Rust 1.97.1 with `wasm32-unknown-unknown`, clang 18 (wasm32 backend), wasm-pack, Node, Foundry.
- `simc` (SimplicityHL CLI with `.args`/`.wit` support): `cargo install simplicityhl --version 0.9.0 --features serde --locked`.
  Compile-check the draft: `simc docs/drafts/liquid_htlc.simf --args docs/drafts/liquid_htlc.args`.
- `/tmp` is wiped on reboot. Keep anything worth keeping in the repo.
- Free Solana RPC (publicnode) blocks `getProgramAccounts`/token-account indexing and old history; plan clients around
  that (remember accounts locally, derive PDAs).
