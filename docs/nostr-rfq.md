# RFQ over Nostr

Status: Phase 1 design, for review.

Nostr carries discovery and negotiation only. **No message here can move funds**: a forged, replayed or withheld message
can at worst waste time. Safety comes from the client deriving every address and verifying every lock on-chain.

## 1. Event kinds

Kind ranges are from NIP-01. `38401` and `28401` aren't in the NIP kind list as of 2026-11-05. They
aren't registered, so a later NIP could claim them.

| Kind | Range | Purpose | Visibility |
|---|---|---|---|
| `38401` | Addressable | Maker offer | Public |
| `21059` | Ephemeral gift wrap (NIP-59) | Carries the private messages below | Encrypted |

Private message types, inside the gift wrap's NIP-44 encrypted rumor (kind `28401`):

| `type` | From | Purpose |
|---|---|---|
| `rfq-request` | Taker | Asks for a firm price |
| `rfq-quote` | Maker | Firm price, short expiry |
| `rfq-accept` | Taker | Accepts, and commits to `H` and keys |
| `rfq-terms` | Maker | Final parameters, signed |
| `rfq-reject` | Either | Optional, frees the maker's reservation early |

Why private messages: the payment hash and keys would otherwise link swaps before they happen, and quotes tailored to a
taker shouldn't be public. Why ephemeral: nothing needs to outlive the negotiation. A lost message costs a retry.

## 2. Maker offer (`38401`)

Addressable by kind, pubkey and `d` tag. Republished at least every 10 minutes, with a NIP-40 `expiration` tag no more
than 30 minutes ahead, so a dead maker disappears from clients.

Tags: `["d","liq2sol-v1-<network>"]`, `["expiration","<unix>"]`, `["t","liq2sol"]`.

```json
{
  "v": 1,
  "network": "testnet",
  "program_id": "<base58, Solana escrow program>",
  "template_version": 1,
  "directions": ["L2S", "S2L"],
  "limits": { "min_sol_units": "10000000", "max_sol_units": "1000000000" },
  "indicative": { "L2S_bps": 30, "S2L_bps": 30 },
  "fee_coin_sats": 300,
  "confs": { "default": 2, "above_sol_units": "1000000000", "above": 10 },
  "relays": ["wss://..."]
}
```

All amounts are decimal strings, in order to survive JSON parsers that lose precision on large integers. An offer isn't
binding. It can't be, as no on-chain state backs it.

## 3. Private messages

Every message has this envelope. `id` is 32 random bytes in hex, set by the taker and repeated in all messages of the
negotiation.

```json
{ "v": 1, "type": "...", "id": "<hex32>", "network": "testnet", "sent_at": 1793888350, "body": { } }
```

**`rfq-request`**

```json
{ "direction": "L2S", "amount_side": "send", "amount": "100000000" }
```

**`rfq-quote`**

```json
{
  "send": "100000000", "receive": "99700000",
  "quote_expiry": 1793888410,
  "taker_lock_window": 30
}
```

`send` and `receive` are always in SOL-USDT base units. Liquid-side amounts are those values times 100 and must be
exact.

**`rfq-accept`**

```json
{
  "quote_event": "<nostr event id of rfq-quote>",
  "payment_hash": "<hex32>",
  "taker_liquid_pk": "<hex32, x-only, ephemeral>",
  "taker_solana": "<base58>",
  "taker_liquid_script": "<hex, S2L only: scriptPubKey to receive>",
  "taker_fee_script": "<hex, S2L only: scriptPubKey for the maker's L-BTC fee coin>"
}
```

**`rfq-terms`** (the last message needed)

```json
{
  "accept_event": "<nostr event id of rfq-accept>",
  "h_accept": 2692146,
  "lock_height": 2692446,
  "maker_liquid_pk": "<hex32, x-only, per-swap>",
  "maker_liquid_script": "<hex, L2S only: scriptPubKey to receive>",
  "maker_solana": "<base58>",
  "maker_initiator": "<base58, L2S only: escrow seed, the maker's key or its vault PDA>",
  "usdt_asset": "<hex32>",
  "program_id": "<base58>",
  "template_version": 1,
  "liquid_amount": "10000000000",
  "solana_amount": "99700000",
  "min_deadline_slots": 8400
}
```

The taker checks `lock_height` against the rules for its direction, using its own view of the Liquid tip, plus `usdt_asset`,
`program_id`, amounts and template version against pinned values. It then derives the address locally. Any mismatch
aborts the swap before funding.

## 4. Replay and spoofing protection

| Attack | Protection |
|---|---|
| Spoofed maker | All maker messages are BIP-340 signed by the maker's Nostr key (the seal's signature, see below). Clients show the npub and warn on unknown keys. A fake maker can only waste a taker's time, since it can't redirect funds. |
| Replayed quote | Each quote is tied to a fresh `id` and has `quote_expiry`. Takers reject expired or unknown ids. |
| Replayed accept | The maker stores each `id` and each `payment_hash` forever, and rejects duplicates. |
| Replayed terms | `accept_event` ties it to one accept, and the `id` to one negotiation. |
| Cross-network replay | `network` is in the envelope and the offer, and `program_id` and asset are checked against pinned per-network values. |
| Cross-version replay | `v` and `template_version` are checked. |
| Swapped message type | Each `type` is only accepted in its position in the negotiation, tied together by event ids. |
| Hash reuse by maker | A taker never reuses `p`: it's fresh per swap. A maker must never lock against a known `H`, since a used preimage is public. |
| Relay drops or delays | At worst a retry. Multiple relays are used. `rfq-terms` is idempotent. |
| Relay reads | Contents are NIP-44 encrypted. Relays see the recipient tag and timing. |

### Signature retention

NIP-59 wraps a signed **seal** (kind 13) around an unsigned rumor, which gives deniability. The seal signature does
authenticate the sender to the recipient, and the client must check that the seal's pubkey equals the rumor's pubkey,
and equals the pinned maker key. For the reputation evidence below, the taker keeps the decrypted seal.

Verified: `nostr-tools` 2.25.5 opens our wrappers and checks every layer. **Support for kind `21059` across public relays
is UNVERIFIED.** If relay support is poor, the fallback is the regular gift wrap
(kind `1059`), with an `expiration` tag.

## 5. Rate limits and reputation

- Makers limit `rfq-request` and `rfq-accept` per sender key and per time window. Keys are free, so this only raises the
  cost, and it doesn't stop a determined griefer on its own. The maker's total in-flight cap is the actual bound.
- Reputation is local at first. Clients count outcomes per maker npub in browser storage.
- A maker that signed `rfq-terms` and then didn't lock leaves evidence: the signed terms plus chain state show it failed
  to lock. Publishing such evidence as a Nostr event is deferred, since it invites false reports and needs its own design.
- A client should warn about makers whose npub is new, and should default to small amounts for them.
