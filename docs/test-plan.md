# Test plan

Status: Phase 1 design, for review. No mainnet keys or funds at any stage. Tests use self-issued stand-in assets:
an issued asset on Liquid testnet and an SPL mint on devnet.

## 1. Environments

| Level | Liquid | Solana |
|---|---|---|
| Unit | Simplicity execution against a constructed `ElementsEnv` | `solana-program-test` or LiteSVM |
| Local integration | `elementsd` regtest (needs Simplicity active) | `solana-test-validator` |
| Public | Liquid testnet (Simplicity is active: our claims and refunds confirmed there) | Devnet (CORS and Agave 4.4.0-beta confirmed) |

Local integration is what makes timeouts testable, since regtest can mine blocks at will and the validator can warp
slots. Restart behaviour needs a `solana-test-validator` or local cluster run with a hard fork, **whether this is
practical is UNVERIFIED**. If not, `LastRestartSlot` is set directly in LiteSVM.

## 2. First feasibility proofs (Phase 2, before anything else)

Status (2026-11-05): F1, F2, F3 and F5 pass, on Elements 23.3.4 regtest, and a claim and a refund also ran on live Liquid testnet. F4 needs real wallets and is still open.
Run them with `ELEMENTSD=... ELEMENTS_CLI=... cargo test -p liq2sol-htlc --test regtest -- --ignored --nocapture` and
`scripts/check-wasm-parity.sh`.

| # | Proof | Why |
|---|---|---|
| F1 | Execute the draft HTLC claim and refund on regtest, including pruned size and cost against budget | Never yet run |
| F2 | Same transaction built and signed in WASM, byte-identical to native | Unverified, and the client depends on it |
| F3 | Compare `nSequence` and `tx_lock_height` behaviour in a refund | Common pitfall |
| F4 | A real Aqua and Green payment to a non-confidential Simplicity address on testnet, if those wallets support testnet | Unverified, and the client depends on it |
| F5 | Fee-coin spend: explicit L-BTC, whole coin to fee | Confirms the D3 design |

## 3. Liquid contract tests

Each must pass, or fail with the expected error, on both the unit and the regtest levels.

| Case | Expected |
|---|---|
| Valid claim | Accepted |
| Shared preimage vector (see contracts.md section 3) | Accepted |
| Wrong preimage | Rejected |
| Wrong recipient signature; sender's key signs | Rejected |
| Signature over a different transaction | Rejected |
| Spent coin: wrong asset, with correct amount | Rejected |
| Spent coin: correct asset, wrong amount (over and under) | Rejected |
| Spent coin: blinded asset, or blinded amount | Rejected |
| Fake asset with the same name and ticker | Rejected |
| Output 0 missing | Rejected |
| Output 0: wrong script, wrong asset, wrong amount, blinded asset or amount | Rejected, one test per case |
| Correct payment at output 1, something else at output 0 | Rejected |
| Claim after `LOCK_HEIGHT`, still unspent | Accepted (documented behaviour) |
| Two HTLC coins, one output | Second input's check still sees only output 0; document what happens, since only the recipient can sign it |
| Refund before `LOCK_HEIGHT`, one block before, at it | Rejected, rejected, accepted |
| Refund with final sequences | Rejected |
| Refund with lock time as a timestamp value | Rejected |
| Refund: wrong key, recipient's key | Rejected |
| Refund of a wrong-asset or wrong-amount coin | Accepted |
| Unpruned program, wrong CMR, wrong control block, wrong leaf version | Rejected by consensus |
| Key-path spend attempt | Impossible (NUMS key) |

**Status:** these run in `contracts/liquid/tests/contract.rs` and `regtest.rs`. Mutation check: removing each of the 11
checks in the contract, one at a time, made at least one test fail.

## 4. Solana program tests

| Case | Expected |
|---|---|
| Lock: valid; zero amount; past deadline; initiator == recipient; wrong mint; Token-2022 mint | Accepted, then rejected in each other case |
| Lock twice with same initiator and hash | Second rejected |
| Same hash, different initiator | Accepted, as they are separate escrows |
| Claim: correct preimage; wrong preimage; wrong length | Accepted, rejected, rejected |
| Claim by third party | Accepted, funds still go to recipient |
| Claim at `deadline_slot − 1`, and at `deadline_slot` | Accepted, rejected |
| Claim to a non-canonical ATA; ATA of another owner; ATA with reassigned owner | Rejected |
| Claim twice | Second rejected |
| Refund at `deadline_slot − 1`, and at `deadline_slot` | Rejected, accepted |
| Refund after claim, claim after refund | Rejected |
| Refund to another account | Rejected |
| Frozen recipient, frozen initiator, frozen vault | Fails, state unchanged, succeeds after thaw |
| `close_on_restart`: restart after creation | Claim rejected and refund accepted, before deadline |
| `close_on_restart`: restart before creation | No effect |
| `close_on_restart = false`: restart after creation | No effect |
| `extend`: valid; twice; non-recipient; on short leg; past deadline; earlier deadline; over the cap | Accepted, all others rejected |
| `release`: recipient; other signers; after settlement | Accepted, rejected, rejected |
| `close`: while open; by initiator; by others before and after retention | Rejected, accepted, rejected, accepted |
| Preimage readable after claim | Yes, until close |
| CPI claim from a wrapper program with heavy logging | Works, and the stored preimage is still correct |
| Vault: withdrawal by operator; over window cap; cap resets; refund returns to vault; rotation | Per contracts.md |
| Bond module, only if later adopted (not in v1, D2) | Claim returns it. Refund forfeits it. Restart returns it. Unbound reclaim. |

**Property tests** (random sequences of instructions, slots and restarts): invariants S1, S2, S4, S5 and S6 hold after each step.

**Status:** these run in `programs/solana-escrow/tests/escrow.rs` against the compiled binary, with the real Token program.
Mutation check: each of 26 removed or weakened checks made at least one test fail. Not yet written: the CPI wrapper with heavy logging (needs a second test program), and the bond module (not in v1). Accounts
are frozen and restarts are set directly in LiteSVM, so neither a real freeze authority nor a real cluster restart is exercised yet.

## 5. End-to-end and adversarial scenarios

**Status (2026-11-05):** E1 ran in both directions on the public networks with `cli/` (`swap l2s`, `swap s2l`), and the abort runs cover E2 and E3
(`abort l2s`, `abort s2l`), including a refused early refund. Funds were conserved. These runs use the faucet's `TEST` asset and a stand-in mint, one
process plays both roles, and RFQ is not involved. The L→S reveal used the single-endpoint override.

E4, E5, E9, E11, E12, E13 and E15 run in `cli/tests/failures.rs`, on an Elements regtest node and real devnet: not mining is a halt, and `invalidateblock` with
an emptied mempool is a rollback. E12 and E15 pass only if the predicted loss actually happens. They exercise the chains, not the daemon, so the daemon's
reactions (calling `extend`, refusing to refund) are still to be tested. A real federation rollback may also differ, since it replays transactions.

Still open: E14 (needs a cluster restart), E16 (needs a mint with a real freeze authority; the devnet stand-in has none), blinded coins in E9 (Bit Machine only), E6–E8 and E10 (need RFQ and client
logic), E17 onward (need the daemon and client). Also open: `solana-test-validator` from source still dies on this CPU, because `reed-solomon-erasure` runs AVX2 code unchecked.

Each runs both directions where applicable, first locally and then on testnet and devnet.

| # | Scenario | Expected |
|---|---|---|
| E1 | Happy path | Both settled, exact amounts |
| E2 | Maker never locks | Taker refunds with only the recovery file and no site |
| E3 | Taker never claims | Both refund after timeouts |
| E4 | Taker reveals at the last slot before the deadline | Maker still claims |
| E5 | Maker offline through the reveal, back within the margin | Maker still claims |
| E6 | Replayed quote, accept, terms; cross-network replay | Rejected |
| E7 | Forged maker messages | Rejected |
| E8 | Maker terms: short `LOCK_HEIGHT`, wrong asset, wrong program, wrong amount | Client aborts before funding |
| E9 | Taker funds wrong asset, wrong amount, blinded, or twice | Maker doesn't lock (or acts on one exact coin), and taker refunds |
| E10 | Fake lock reported by one malicious endpoint (Esplora or RPC) | Client refuses to reveal `p` |
| E11 | Liquid stops mining with S→L open, maker calls `extend` | Honest flow finishes after resume |
| E12 | Same as E11, malicious taker, no `extend` | Demonstrates the theft, proving why D1 exists |
| E13 | Liquid stops mining with L→S open | Safe, delayed |
| E14 | Restart with L→S open, taker tries a late claim after Liquid refund | Claim rejected |
| E15 | Regtest reorg dropping a lock | Daemon pages, stops locking. Documents the loss case. |
| E16 | Taker's token account frozen before claim | Preflight stops the reveal, both refund |
| E17 | Daemon killed at every intent boundary | Resumes correctly, no double action |
| E18 | Browser storage cleared mid-swap | Recovered from the file |
| E19 | Taker has no L-BTC (both directions) | Clear flow per D3 |
| E20 | Taker has no SOL (L→S) | Blocked before funding per D5 |
| E21 | Griefing at the rate limits | Caps hold, service degrades, no loss |
| E22 | Signer policy: engine asks for an out-of-policy signature | Refused, alert |

## 6. Formal verification

| Component | Approach | Status |
|---|---|---|
| Solana state machine | Kani or a Rust model checker on the pure `can_claim` / `can_refund` logic: S4, S5, S7 | Feasible. Keep that logic in pure functions to allow it. |
| Solana whole program | Property tests plus audit. Full verification of token CPI is out of scope. | Planned |
| Liquid HTLC | Prove L1–L7 over the compiled Simplicity program in Rocq, using Simplicity's formal semantics | **UNVERIFIED:** how much is tooled for Elements jets today. Requires investigation with Blockstream's Simplicity repo and likely a specialist. |
| Liquid fallback | Exhaustive path tests over the small witness space, plus a CMR pinned to the audited source | Minimum bar, whatever else is done |
| Cross-chain timing | TLA+ model of both state machines, with halts, restarts and rollbacks as environment actions. Check: no honest party loses funds under the documented assumptions, and each assumption's removal yields the documented counterexample. | Recommended. It is the most likely way to find a mistake in this design, and this review already found two by hand. |

## 7. Fuzzing

| Target | Tool |
|---|---|
| Nostr message parsers (maker and client) | `cargo-fuzz` |
| Esplora and RPC response parsers | `cargo-fuzz` |
| Recovery file parser | `cargo-fuzz`, and a JS property test |
| Witness and transaction encoding round trip | `cargo-fuzz` |
| Solana instructions and account orderings | Trident (**UNVERIFIED** current Anchor support) or a LiteSVM-based harness |
| Engine state machine with random chain views | Property testing (`proptest`) |

## 8. Exit criteria before any mainnet discussion

All of these need to be met, and mainnet remains the user's explicit decision:

1. All sections above pass in CI, and all public-network scenarios have run.
2. Independent audit of both contracts and the signer.
3. Formal targets in section 6 either completed or explicitly waived by the user.
4. Reproducible build verified by a second party.
5. Real-wallet tests completed (F4).
