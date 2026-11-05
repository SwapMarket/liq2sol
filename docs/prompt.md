# Project: Trustless L-USDT ⇄ SOL-USDT atomic swap protocol

You are designing (and later implementing) a trustless swap protocol between **USDt on the Liquid Network** and **USDT on Solana**. Work in phases. **Phase 0 and Phase 1 produce documents only. Stop after Phase 1 and wait for my review before writing implementation code.**

## Goal
Two parties (a *taker* and a *maker*) swap L-USDT for SOL-USDT, or the reverse, with no custodian and no trusted operator. Either both legs complete or both parties can refund unilaterally.

## Background and lessons learned
- Boltz, the main HTLC swap service for Liquid, suspended swaps in Aug 2026. AI-assisted attackers found and exploited flaws in its *operator infrastructure* faster than its small team could patch them. User funds were safe only because the swaps were non-custodial and users could refund unilaterally.
- **Design principle:** minimize trusted/online infrastructure. Put guarantees onchain, keep contracts small enough to formally verify, avoid any central server, and always give users a server-free refund path.

## Trust model (state explicitly in the design)
- Trustless between counterparties.
- Residual trust that we accept and document: Tether (can freeze USDT on either chain) and the Liquid federation (Liquid consensus).
- Non-goals: wrapped assets, lock-and-mint bridges, multisig custody of user funds.

## Components
1. **Liquid HTLC in SimplicityHL** (Simplicity is live on Liquid mainnet)
   - Claim path: sha256 preimage + recipient Schnorr signature.
   - Refund path: sender signature after a lock height.
   - Covenant on the claim: output must be Tether's L-USDT asset ID, with an explicit (unblinded) amount, to the recipient's script.
   - Always check asset ID together with amount, since anyone can issue a fake Liquid asset.
   - Keep the contract minimal, and plan for formal verification.
2. **Solana escrow program (Rust, Anchor)**
   - HTLC escrow: claim with sha256 preimage, refund after a timeout.
   - Maker liquidity deposits and taker reservations.
   - Anti-griefing deposits.
3. **Maker daemon** (in Rust)
   - Watches both chains, publishes quotes, claims and refunds automatically, and rebalances.
   - Holds hot keys, so this is the main attack surface. Keep it minimal and open source. Each maker runs their own; there is no central operator.
4. **Taker client: a fully static web app hosted on GitHub Pages**
   - No backend.
   - Liquid: the page derives the HTLC address from a precompiled SimplicityHL template plus swap parameters. The user pays it from any Liquid wallet (Aqua, Green, etc.) to a non-confidential address. The page holds ephemeral claim/refund keys.
   - Solana: standard wallet adapter (Phantom, etc.).
   - Chain data from public Liquid Esplora APIs and Solana RPC, with user-configurable endpoints.
   - Liquid/Simplicity transaction building in the browser via WASM (e.g. LWK, rust-simplicity). **Verify feasibility early.**
   - State: browser storage plus a mandatory downloadable recovery file *before funding*. The recovery file alone must be enough to refund.
   - Supply-chain hardening: reproducible builds, pinned dependencies, subresource integrity, published release hashes, an IPFS mirror, and a run-locally option.
5. **Discovery: RFQ over Nostr**
   - Makers publish signed quotes; takers request and accept.
   - Define event kinds and message schemas, including replay and spoofing protection.

## Protocol flows (taker is always the secret holder and always locks first)
- **L-USDT → SOL-USDT**
  1. Taker locks L-USDT in the Liquid HTLC (long timeout T1).
  2. Maker locks SOL-USDT in the Solana escrow (short timeout T2 < T1).
  3. Taker claims on Solana, revealing the preimage.
  4. Maker reads the preimage and claims on Liquid.
- **SOL-USDT → L-USDT**
  1. Taker locks SOL-USDT in the Solana escrow (long timeout).
  2. Maker locks L-USDT in the Liquid HTLC (short timeout).
  3. Taker claims on Liquid, revealing the preimage in the witness.
  4. Maker reads it and claims on Solana.
- Derive concrete timeout values from finality: Liquid is final after 2 blocks (~1 min each); use Solana's finalized commitment. Add safety margins for congestion and downtime.

## Open questions to resolve in the design
- **Free-option problem:** the taker can abort after the maker locks. Choose and justify a mitigation (taker bond/fee, short timeouts, reputation, etc.) for both directions.
- **Decimal mismatch:** L-USDT and SOL-USDT may use different precision (verify, likely 8 vs 6). Define amount conversion and rounding.
- **Liquid fees:** who pays L-BTC fees for claims and refunds, and how? Can the covenant allow fees paid from the swapped amount?
- **Preimage encoding:** fixed 32-byte preimage; identical sha256 semantics on both chains.
- **Reorgs, chain halts, and Tether freezes** mid-swap: specify the behavior for each.
- **Watchtowers:** optional design for takers who go offline.
- **Phase 2 extension (design only, do not build yet):** a Solana program that verifies Liquid block headers (federation signatures, including dynafed key rotation) and Merkle proofs of Liquid transactions. This would allow a reservation-based Liquid→Solana flow where the taker pays the maker's registered Liquid address directly.

## Phase 0: verify assumptions (write `docs/assumptions.md`)
Do not rely on memory for anything below. Check current docs and source code, and record each finding with a link:
- SimplicityHL version, syntax, and the exact jet names for sha256, BIP340 verification, lock-height checks, and output asset/amount/script introspection. Check the official HTLC example.
- How Simplicity programs are committed to (CMR → Taproot address) and how witnesses are built.
- Whether the relevant Rust crates compile to WASM for browser use.
- Tether's L-USDT asset ID and precision; the SOL-USDT mint address and decimals.
- The Liquid block signing scheme and the federation key-rotation format.
- Solana syscalls available for sha256 and secp256k1, and compute-budget limits.
- Public Esplora/RPC CORS and rate limits.

Mark anything you couldn't verify as **UNVERIFIED**.

## Phase 1: design documents (then STOP for review)
- `docs/protocol.md`: full state machines for both directions, timeouts, message formats, failure and recovery paths.
- `docs/contracts.md`:
  - SimplicityHL contract spec plus a draft `.simf`.
  - Anchor program accounts, instructions, and invariants.
- `docs/nostr-rfq.md`: event kinds and schemas.
- `docs/client.md`: static-app architecture, key handling, recovery file format, build/release pipeline.
- `docs/maker.md`: daemon architecture, key management, monitoring.
- `docs/threat-model.md`: attackers (counterparty, network, supply chain, infrastructure/AI-assisted probing), mitigations, residual risks.
- `docs/test-plan.md`: Liquid testnet plus Solana devnet end-to-end tests, adversarial cases, fuzzing, and a formal-verification plan.

## Phase 2+ (after my approval)
Monorepo layout:
- `contracts/liquid/`
- `programs/solana-escrow/`
- `maker/` (Rust)
- `client/`
- `shared/` (types, schemas)

Build order:
1. Contracts with tests.
2. A CLI-only end-to-end swap on testnet/devnet.
3. Maker daemon.
4. Static client.

## Working rules
- Testnet/devnet only. Never generate, request, or handle mainnet keys.
- Don't guess APIs. Verify against docs or source, and cite them.
- Prefer small, auditable code over features.
- Every component must have tests. Adversarial tests (refunds, timeouts, wrong asset, wrong amount, replayed quotes) are required.
- When a decision is genuinely mine to make, list the options with trade-offs instead of picking silently.
