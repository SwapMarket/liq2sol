# Maker daemon

Status: Phase 1 design, for review. Rust. Each maker runs its own; there is no central operator. This is the main
attack surface because it holds hot keys, so it should do little and expose nothing.

## 1. Components

| Module | Job |
|---|---|
| `chain_liquid` | Own `elementsd` (at least v23.3.4, which fixes the September 2026 exploit), with Esplora as a cross-check |
| `chain_solana` | RPC at `finalized`, at least two providers. Polls derived PDAs, so no indexer is needed. A Helius free key is fine for devnet testing. For mainnet, use a paid plan or an own node, since the free plan allows 1 `sendTransaction`/s and has no staked connections. |
| `rfq` | Nostr client: publishes offers, answers requests |
| `engine` | State machines from [protocol.md](protocol.md), as pure functions over (state, chain view) → actions |
| `signer` | Holds keys, signs only requests that pass its own policy check (section 3) |
| `store` | SQLite, write-ahead log, fsync before every irreversible action |
| `rebalance` | Reports imbalance and alerts. See section 5. |
| `monitor` | Metrics and alerts. See section 6. |

**No inbound network ports.** All connections are outbound: to relays, nodes and RPC. Admin access is local only, through
a CLI over a Unix socket. There's no web UI and no HTTP API.

**Crash safety.** Every action is recorded as an intent before it is sent. On restart, the daemon reloads open swaps,
reads both chains and works out where it stands. All actions are idempotent, because chain state decides what is still
needed.

## 2. Key management

| Key | Use | Where |
|---|---|---|
| Nostr key | Signs offers and terms. Identity and reputation. | Hot. Can be rotated at the cost of reputation. |
| Liquid per-swap keys | Half of the claim's aggregate key, and the refund | Derived from a hot seed and the swap id |
| Liquid wallet and blinding keys | Fund locks, pay fees. Every coin is blinded, and explicit coins are ignored. | Hot, balance limited |
| Solana operator key | Calls `lock_from_vault`, `extend`, pays fees | Hot, rate-capped by the vault (D6) |
| Solana vault owner | Withdraws, changes operator | **Cold.** Never on the daemon's host. |
| Liquid treasury | Refills the hot wallet | **Cold.** Never on the daemon's host. |

- The `signer` is a separate process, reached only through a Unix socket in a private directory. It treats whoever is on
  the socket as possibly compromised:
  - **It builds every transaction itself.** Our claims and refunds pay our own wallet, because nothing else can be asked for.
    It signs and returns transactions, and it never broadcasts.
  - **It reads heights, slots, coins and escrows through its own chain connections.** It signs a Solana lock only after seeing
    the taker's exact coin confirmed and read with the swap's blinding key, and with an adapter signature stored, and a Liquid lock only after seeing a matching escrow. Otherwise a
    compromised engine could lock our funds to a colluding taker who never paid.
  - **It runs its own policy and ledger**, and commits before releasing a signature: per-swap and per-window caps, no payout
    above what is received, its own check of the timeouts, and no refund after collecting or release after locking.
  - **A Liquid lock is signed once.** A retry returns the identical stored bytes, so the address can't be paid twice.
  - **It keeps the signing sessions.** Nonces live in memory only, are used once, and a crash abandons the session. An adapter signature is fsynced before
    anything relies on it: before a Solana lock, or before our share is released. One session per swap. Our share is refused once the taker's escrow is gone.
  - **It keeps a pool of blinded fee coins**, reserves one for a swap durably before signing, and never spends coins of that value on anything else.
  - Requests are handled one at a time so policy checks can't race, and read outside that lock so a stalled client blocks nobody.
- Per-swap Liquid keys are derived as SHA-256 of a domain tag, a seed and the swap id, rather than BIP-32. A restored seed
  can still spend every open swap.
- **Limits of this separation.** It needs a separate OS user to mean much, because the same user can read the key files. Root on
  the host defeats it. The signer also trusts its own chain source, so a lying node or RPC could feed it false evidence, which is
  why a maker should run its own node.
- An optional remote signer or HSM can replace it later.
- Keys and nonces are never logged. The swap log contains secrets only after they are public.
- **Testnet and devnet only at this stage.** Mainnet key ceremony is out of scope until after audit.

## 3. Pre-lock checks

All of these must pass, or the maker abandons the swap. The taker can then refund.

**L→S:**

1. `T.x` has never been seen.
2. At least one confirmed output at the derived address that unblinds to the exact asset and amount, with
   the confirmations D7 requires. The maker acts on one such coin and ignores everything else there, so a stray coin can't
   block a swap. The sender can refund the others.
3. Both `elementsd` and Esplora agree on that output.
4. `LOCK_HEIGHT − tip ≥ 210`.
5. Solana and Liquid both healthy, per protocol section 7.5.
6. Under the exposure caps.
7. A checked adapter signature for the claim is stored. Without one the maker never locks, so a taker who won't sign costs it nothing.
8. The escrow will be created with `close_on_restart = true` and the correct `deadline_slot`.

**S→L:**

1. `T.x` has never been seen.
2. Escrow at `finalized`: recipient, mint, amount, the full point and `close_on_restart = false`.
3. At least 84,500 slots remain, and `tip ≤ h_accept + 30`.
4. The maker's Solana token account exists and isn't frozen.
5. Both chains healthy, and under the caps.

## 4. Exposure limits

These limit the damage from the risks contracts can't remove.

| Limit | Applies to |
|---|---|
| Per-swap maximum | Everything |
| Total open S→L | Liquid halt or rollback |
| Total open L→S | Solana restart gap, Liquid rollback |
| Per-taker-key open swaps | Griefing |
| Global rate per window | Griefing, compromise |
| Daily loss circuit breaker | Stops new locks after any swap settles at a loss |

Defaults are set after testnet, so they are deliberately absent here. They should scale with what the operator can afford
to lose in a single incident.

**Recovering from a rollback alarm:** check the chain by hand, then run `liq2sol-maker clear-rollback <config.json>`. Nothing else clears it.

**Automatic reactions:**

- **Liquid tip older than 10 min:** stop new locks. With S→L swaps open, call `extend` once the tip is older than
  60 min and within 1 h of an escrow's deadline (D1). Calling it late keeps honest takers' refunds undelayed in short
  stalls, and the chance of missing it is covered because the margin is hours long.
- **Solana stalled, or `LastRestartSlot` changed:** stop new locks, refund any eligible escrows after restart.
- **Liquid tip height decreased, or a confirmed tx disappears:** stop everything and page the operator. Don't try to
  automate recovery from a rollback.
- **Never refund a coin whose Solana escrow this maker has already claimed.** After a rollback, that coin belongs to a
  taker who hasn't rebroadcast yet (protocol section 7.3). The signer's policy enforces this as well as the engine.

## 5. Rebalancing

Each completed swap moves inventory from one chain to the other. Rebalancing needs a venue, and every venue adds trust
and exposes keys, so **v1 automates nothing that moves funds**. The daemon:

- Widens the spread on the direction that drains the scarcer side.
- Stops quoting a direction when inventory falls below a threshold.
- Alerts the operator, who moves funds by hand using cold keys.

The brief asks the daemon to rebalance. A narrow automated option can be added after testnet, such as swapping through
other liq2sol makers. Adding an exchange API key to the host would add a new secret and a new attack path, so it's not
recommended.

## 6. Monitoring

| Signal | Alert when |
|---|---|
| Liquid tip age | Over 5 min warn, over 10 min page |
| Solana finalized slot age | Over 60 s page |
| Tip disagreement between sources | Over 2 blocks |
| Tip height decrease | Always, page |
| Time left on an unclaimed reveal | Under 1 h, page |
| `LastRestartSlot` change | Always, page |
| Hot wallet L-BTC, SOL, USDt, USDT | Below threshold |
| Vault window usage | Over 80% |
| Failed signer policy checks | Any, page |
| Elements or Agave security release | Manual subscription. The September 2026 fix was public 5 days before the exploit, so patch fast. |

Metrics are written to local logs, plus an optional outbound push. A second machine should run a dead-man's switch that
pages if the daemon stops reporting, because downtime during a reveal window is what costs money.

## 7. Dependencies and operations

- Dependencies are pinned and minimal, with `cargo-deny` and `cargo-audit` in CI.
- `#![forbid(unsafe_code)]` in the daemon's own crates.
- Runs under systemd with sandboxing: `NoNewPrivileges`, `ProtectSystem=strict`, a private `/tmp`, syscall filter.
- Parsers for Nostr, Esplora and RPC data are fuzzed, because all of it is attacker-reachable input.
