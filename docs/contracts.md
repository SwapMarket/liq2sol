# Contracts

Status: Phase 1 design, for review. Decisions `Dn` refer to [protocol.md](protocol.md) section 9.

## 1. Liquid HTLC (SimplicityHL)

The contract is at [contracts/liquid/src/htlc.simf](../contracts/liquid/src/htlc.simf), and has been executed on an
Elements 23.3.4 regtest node (see [test-plan.md](test-plan.md) section 2). It is unaudited.

### Parameters

Baked into the program at compile time, so each swap has its own CMR and address.

| Parameter | Type | Meaning |
|---|---|---|
| `PAYMENT_HASH` | u256 | `SHA256(p)` |
| `RECIPIENT_PK` | Pubkey | x-only key of the claimer |
| `SENDER_PK` | Pubkey | x-only key of the refunder |
| `LOCK_HEIGHT` | Height (u32) | Absolute height for the refund |
| `USDT_ASSET` | u256 | Tether's asset id, or the stand-in on testnet, in **internal byte order: the reverse of the hex explorers show**. Display-order hex yields a program no real coin can satisfy. Always use `asset_param`. |
| `AMOUNT` | u64 | Exact locked amount, base units |
| `RECIPIENT_SCRIPT_HASH` | u256 | SHA-256 of the recipient's scriptPubKey |

### Witness

`CLAIM_OR_REFUND: Either<(u256, Signature), Signature>`

### Spend rules

**Claim** (`Left`) must satisfy all of:

1. `SHA256(preimage) == PAYMENT_HASH`, where the witness type forces a 32-byte preimage.
2. BIP-340 signature by `RECIPIENT_PK` over `sig_all_hash`.
3. The spent coin is explicit `USDT_ASSET`, explicit `AMOUNT`.
4. Output 0 is explicit `USDT_ASSET`, explicit `AMOUNT`.
5. Output 0's script hash equals `RECIPIENT_SCRIPT_HASH`.

Claim has no height limit. A program can't assert "before height X", because a spender may set any lock time below the
current height. See [protocol.md](protocol.md) section 7.1.

**Refund** (`Right`) must satisfy both of:

1. `check_lock_height(LOCK_HEIGHT)`.
2. BIP-340 signature by `SENDER_PK` over `sig_all_hash`.

The refund has no covenant, so any coin at the address, of any asset or amount, can be recovered by the sender.

**Refund transactions must set a non-final `nSequence`.** `tx_lock_height` returns 0 when all sequences are final, so
`check_lock_height` fails on a transaction that sets the lock time but leaves sequences at the maximum. Confirmed on a
node ("Assertion failed inside jet").

**Earliest refund.** With lock time `LOCK_HEIGHT`, the node accepts the refund once the tip reaches `LOCK_HEIGHT`, so it
mines in block `LOCK_HEIGHT + 1`. Confirmed on a node.

**Reading the preimage.** The preimage isn't byte-aligned in the witness, because a 1-bit branch tag comes first.
`extract_preimage` scans each bit offset and accepts only a value that hashes to `PAYMENT_HASH`.

### Invariants

| # | Invariant |
|---|---|
| L1 | Claim requires the preimage. |
| L2 | Claim requires the recipient's signature (D4). |
| L3 | A coin that isn't exactly explicit `AMOUNT` of `USDT_ASSET` can't be claimed. Fake assets and blinded coins fail. |
| L4 | A successful claim pays exactly `AMOUNT` of explicit `USDT_ASSET` to the fixed script, in output 0. |
| L5 | Refund is impossible before `LOCK_HEIGHT`. |
| L6 | Refund requires the sender's signature. |
| L7 | Only these two paths exist. Tap tree has one leaf and the internal key is unspendable. |

### Address derivation

- Tapleaf version `0xbe`; the leaf script is the 32-byte CMR.
- Internal key: `50929b74c1a04954b78b4b6035e97a5e078a5a0f28ec96d547bfee9ace803ac0`, the standard BIP-341 NUMS point,
  as used in LWK's Simplicity taproot test. Clients must derive it themselves and never accept an address from the maker.
- Spend witness: `[simplicity_witness, simplicity_program, cmr, control_block]`.
- Payment must go to the **non-confidential** address. A blinded payment isn't claimable (L3), and is only refundable.

### Size and fees

Measured on pruned programs, as consensus requires (a node refuses the unpruned form: "Program has FAIL node"):

| Spend | Program | Witness | Cost | Budget |
|---|---|---|---|---|
| Claim | 408 bytes | 97 bytes | 122 WU | 627 WU |
| Refund | 121 bytes | 65 bytes | 62 WU | 306 WU |

Annex padding isn't needed. A claim with one L-BTC fee input measures **379 vB** (1,514 weight), about 38 sats at the
prevailing 0.1 sat/vB. The fee coin is 300 sats, spent entirely as fee with no change output. That is about 0.8 sat/vB,
tested and accepted, and it could be lowered.

### Formal verification plan

See [test-plan.md](test-plan.md) section 6.

## 2. Solana escrow (Anchor)

Anchor v1.2.2, legacy Token program only. The mint is a compile-time constant selected by a Cargo feature: the real
USDT mint for a mainnet build, a stand-in for devnet.

### Accounts

**`Escrow`**, PDA `["escrow", initiator, payment_hash]`:

| Field | Type | Notes |
|---|---|---|
| `initiator` | Pubkey | Funded the lock. Receives refund and rent. |
| `recipient` | Pubkey | Receives the claim. |
| `amount` | u64 | Base units, greater than 0. |
| `payment_hash` | [u8; 32] | |
| `created_slot` | u64 | |
| `deadline_slot` | u64 | Claim closes and refund opens here. |
| `close_on_restart` | bool | True for short-leg locks. |
| `extended` | bool | `extend` has been used. |
| `state` | enum | `Open`, `Claimed`, `Refunded`. |
| `preimage` | [u8; 32] | Set on claim. Zero otherwise. |
| `settled_slot` | u64 | Slot of claim or refund. |
| `bump` | u8 | |

**Vault token account**, PDA `["vault", escrow]`, owned by the escrow PDA. One per swap, so a freeze affects one swap.

The seeds include the initiator, so nobody can front-run a lock with the same hash and occupy another party's PDA.

**The preimage stays in the account after claim.** Reading the transaction instead would be fragile: a claim made
through CPI with deliberately noisy logs could hide it, and free RPCs drop history. A plain `getAccountInfo` on a PDA
the maker can derive always works, and only the initiator can close the account early.

### Instructions

Time rule used below, with `can_claim` and `can_refund` always exact opposites:

```text
restarted  = close_on_restart && LastRestartSlot > created_slot
can_claim  = state == Open && slot < deadline_slot && !restarted
can_refund = state == Open && !can_claim
```

| Instruction | Signer | Checks | Effect |
|---|---|---|---|
| `lock(hash, recipient, amount, deadline_slot, close_on_restart)` | initiator | Mint matches. `amount > 0`. `deadline_slot > slot`. `recipient != initiator`. | Creates escrow and vault, moves `amount` in. |
| `claim(preimage)` | anyone | `can_claim`. `sha256(preimage) == hash`. Recipient token account is the canonical ATA for the mint and owned by `recipient`. | Moves funds to recipient, stores preimage, sets `Claimed`, closes vault. |
| `refund` | anyone | `can_refund`. Destination token account is owned by `initiator`. | Moves funds to the initiator, sets `Refunded`, closes vault. |
| `extend(new_deadline)` | recipient | `state == Open`. `!close_on_restart`. `!extended`. `slot < deadline_slot`. `deadline_slot < new_deadline ≤ deadline_slot + MAX_EXTENSION` (3,000,000 slots, about 7 days). | Moves the deadline, sets `extended` (D1). |
| `release` | recipient | `state == Open` | Returns funds to the initiator at once, sets `Refunded`. For a maker who can't fill the swap. |
| `close` | initiator any time, anyone after `RETENTION` (3,000,000 slots) | `state != Open` | Closes the escrow, rent goes to the initiator. |

The claimer adds an idempotent ATA creation before `claim` when needed. A refund destination that doesn't exist or is
frozen makes the refund fail with no state change, so it can be retried. It never pays someone else.

### Invariants

| # | Invariant |
|---|---|
| S1 | Vault balance equals `amount` while `Open`, and the vault is gone afterwards. |
| S2 | Funds only ever leave to the recipient's account or the initiator's account. |
| S3 | Claim requires the 32-byte preimage of `payment_hash`. |
| S4 | `can_claim` and `can_refund` are never both true, and for an `Open` escrow exactly one is. |
| S5 | `state` moves from `Open` to `Claimed` or `Refunded` and never changes again. |
| S6 | Once `Claimed`, the preimage stays readable until `close`, and only the initiator can close before `RETENTION`. |
| S7 | The deadline moves only through `extend`: once, later only, by the recipient, on a long-leg escrow, and capped. |
| S8 | Only the configured mint and the legacy Token program are accepted. Token-2022 is rejected. |
| S9 | The program holds no global authority, admin key or fee switch. |

### Compute and rent

Claim is one SHA-256 (under 200 CU in syscalls) plus a token transfer CPI and a close. It should comfortably fit the
default limit, **to be measured in Phase 2**. Rent for the escrow and vault is returned at settlement or `close`.

### Modules beyond the core

These live behind separate instructions so the core stays auditable. A maker can lock directly with `lock` and never use
the vault.

**Maker vault (D6, accepted: in scope).** PDA `["maker_vault", owner]`, with a token account.

| Instruction | Signer | Effect |
|---|---|---|
| `vault_init(operator, window_slots, window_cap)` | owner (cold) | Creates the vault |
| `vault_deposit(amount)` | anyone | Adds funds |
| `vault_withdraw(amount)` | owner | Withdraws, cold key only |
| `vault_set_operator(operator, window_slots, window_cap)` | owner | Rotates the hot key or changes the cap |
| `lock_from_vault(...)` | operator | As `lock`, with `initiator` = the vault PDA, limited to `window_cap` per `window_slots`. Funds from refund return to the vault. |

The operator can't withdraw, but it can lock vault funds to a recipient it controls and claim them, so **the cap, not
the key separation, limits a stolen hot key.** A vault is one freezable token account, unlike per-swap vaults, so the
maker should keep its balance near operating needs.

**L→S taker bond (D2: not in v1, kept here as a design for later).** Lamports in PDA `["bond", taker, hash]`.

| Instruction | Signer | Effect |
|---|---|---|
| `post_bond(hash, maker, lamports, expiry_slot)` | taker | Creates the bond |
| `lock_bonded(...)` | maker | As `lock`, and binds the bond |
| `reclaim_bond` | anyone | Returns an unbound bond after expiry |

On a bound escrow: `claim` returns the bond to the taker. `refund` pays it to the maker, unless `restarted` is true, in
which case it returns to the taker, since a chain restart isn't the taker's fault.

## 3. Shared test vector

| Item | Value |
|---|---|
| Preimage | 32 zero bytes |
| SHA-256 | `66687aadf862bd776c8fc18b8e9f8e20089714856ee233b3902a591d0d5f2925` |

Both contracts must accept this pair and reject any other length or value. The value is the one used in the official
SimplicityHL HTLC example and draft arguments, and it is the standard SHA-256 of 32 zero bytes.
