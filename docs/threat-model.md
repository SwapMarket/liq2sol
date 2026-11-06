# Threat model

Status: Phase 1 design, for review. "Protocol" below refers to [protocol.md](protocol.md).

## 1. Assets

- User funds in the Liquid swap output and the Solana escrow.
- Maker inventory and hot keys.
- Taker per-swap keys, secrets, blinding keys and recovery files.
- Availability: the ability to swap, and above all the ability to refund.

## 2. Malicious counterparty

| Attack | Mitigation | Residual |
|---|---|---|
| Taker funds with a fake asset | **No on-chain check any more.** The maker unblinds the coin and acts only on the exact asset and amount (L5). A forged range proof label doesn't open the asset commitment (L6). | A bug in this check loses the swap, so the signer checks it as well as the engine |
| Taker underpays or overpays | Same. The coin is only refundable. | None |
| Taker pays and never signs | Maker doesn't lock without a stored adapter signature | One reserved fee coin |
| Taker sends a bad share | Verified against the point and the taker's key, and the combination against the output key, before storing | None |
| Maker reuses a nonce | Nonces are random, in memory only, consumed by the type system, and never stored. A crash abandons the session. | **A reuse would reveal the per-swap key.** Limited to one swap. |
| Adapter signature lost after acting on it | Fsynced before the lock or the share | Disk failure can lose one swap, so back up the ledger |
| Fee coin spent elsewhere | Reserved durably, and coins of that value aren't used for payments | A Liquid rollback dropping a fee coin's parent could kill a signature |
| Claim stuck at a fixed fee | Fee coins pay far above the floor, and a child can bump | Sustained fee spikes |
| No relay during a swap | The session can't run, so the swap ends in refunds | Failed swap, no loss |
| Taker reuses a point from an earlier swap | Maker rejects seen points forever | None |
| Taker claims right at the deadline | Maker's 3 h margin, and the revealed value stays in the account | Maker must be online inside it |
| Taker hides the value in a CPI with noisy logs | The value is stored in the escrow account, not read from logs | None |
| Taker griefs: locks, then never claims | Rate limits, spread, caps (D2) | Capital time |
| Taker exploits a Liquid halt (S→L) | `extend` (D1) | Halts over about 7 days, bounded by caps |
| Taker exploits a Solana restart (L→S) | `close_on_restart` | Stalls with no hard fork, bounded by caps |
| Maker never locks | Taker refunds | About 5–6 h of the taker's time, plus any `extend`. Reputation only. |
| Maker quotes with a short `LOCK_HEIGHT` | Client checks the height from its own view of the tip | None |
| Maker gives wrong address | The client derives addresses itself and accepts none | None |
| Maker's lock has a too-close deadline | Client verifies and enforces its claim cutoff before revealing | None |
| Maker abuses `extend` | Capped, once only, recipient only. On-chain evidence. | Up to 7 days' delay |
| Maker refunds Liquid as a late claim propagates | Claim cutoff of 10 blocks | Taker who ignores the cutoff |
| Front-run lock on another's PDA | Seeds include the initiator | None |
| Third party front-runs `claim` | Funds can only go to the fixed recipient, so this just pays the fee for them | None |
| Third party front-runs Liquid claim | Needs the recipient's signature, which commits to the transaction | None |

## 3. Network

| Attack | Mitigation | Residual |
|---|---|---|
| Malicious relay: forges, replays, drops | Signed messages, ids and expiry, several relays. Protocol is safe without relays after `rfq-terms`. | Denial of service |
| Relay metadata | NIP-44 and gift wrap, ephemeral keys | Timing and traffic analysis |
| Lying Esplora or RPC, colluding with the maker, reports a lock that doesn't exist and captures the broadcast `p` | The taker confirms the lock on two independent endpoints before revealing. Maker uses its own node. | **Taker loses the swap if both endpoints collude or the user overrides the check.** |
| RPC withholds a claim transaction | Rebroadcast through multiple endpoints | Delay |
| Mempool watcher reads `p` | Expected in both directions. Cutoffs exist for this. | Thin margin if cutoffs are ignored |
| DNS or TLS attack on the site | HTTPS, CSP, SRI, local and IPFS options | Users on the hosted page rely on HTTPS and GitHub |

## 4. Supply chain

| Attack | Mitigation | Residual |
|---|---|---|
| GitHub Pages site replaced | Signed hashes, IPFS, run-locally | **Serious: a replaced page can steal keys.** The page can't attest to itself. Users of the hosted copy are exposed. |
| Malicious npm or crate | Lockfiles, minimal dependencies, reviewed updates, `cargo-deny` | A compromised pinned dependency before review |
| Compiler or CMR tampering | Pin CMR per template in released code. Reproducible build. | Compiler bugs: formal verification targets the compiled program, not the source |
| Compromised maintainer keys | Hardware-backed signing, immutable mainnet program (D8) | Future releases only. Deployed programs can't change. |
| Compromised wallet adapter or WalletConnect library | Pinned, loaded only on connect. The user approves each transaction in its own wallet. | Blind signing, and a larger dependency tree than the rest of the client |
| WalletConnect relay outage or censorship | Messages are end-to-end encrypted. Funds stay recoverable without it (client.md section 3). | WalletConnect users can't start swaps or make L→S claims during an outage. Extension wallets aren't affected. |
| Malicious browser extension | Not addressable from the page. Ephemeral keys limit what's at stake to current swaps. | Open-swap funds |

## 5. Infrastructure and AI-assisted probing

Boltz stopped because automated attackers found flaws in operator infrastructure faster than a small team could fix
them. Liquid's September 2026 exploit followed a fix that was public for 5 days. The working assumption here is that
**any public interface is continuously probed, and that disclosed patches are weaponized within days.**

| Target | Mitigation | Residual |
|---|---|---|
| Central operator | None exists | N/A |
| Maker RFQ surface | No inbound ports. Strict schemas. Fuzzed parsers. Rate limits. | Parser bugs, limited by process separation |
| Maker signer | Separate process with its own policy | Host compromise defeats process separation |
| Maker hot keys | Vault cap, limited hot balances, cold owner keys | Loss up to the cap per window, until the operator reacts |
| Business-logic abuse (cheap quotes, race conditions) | Pure state machines, simulation, adversarial tests | Unknown logic bugs |
| Automated griefing at scale | Global caps | Service unavailable during attack. Funds safe. |
| Slow patching | Alerts on security releases. Small dependency tree. | Operator discipline |
| Contracts | Small enough to verify. Formal plan. Immutable on mainnet. | **A deployed bug can't be patched,** only abandoned |

A compromised maker can only lose its own funds. Takers are still protected by the contracts, because the maker never
holds a key that can spend taker funds without the secret.

## 6. Consensus and issuer

| Risk | Effect | Mitigation |
|---|---|---|
| Liquid halt | Protocol section 7.1 | `extend`, caps, stop-new-locks |
| Liquid rollback | Protocol section 7.3. **Can cause loss to either party.** | Confirmation depth, caps. Accepted federation trust. |
| Elements consensus bug | Sept 2026 precedent. L-BTC inflation. USDt was unaffected, but halted. Claims and refunds need L-BTC fees, so a broken L-BTC peg could affect fee availability. | **Now in our path:** blinding, range proofs and surjection proofs, through `secp256k1-zkp`. Keep nodes patched. |
| Solana halt or restart | Protocol section 7.2 | `close_on_restart`, slots |
| Tether freeze | Protocol section 7.4 | Preflight, per-swap vaults, accepted trust |
| Tether changes token (migration, Token-2022) | A compiled mint constant no longer applies | New program deploy |
| Simplicity consensus bug or jet change | The refund leaf may misbehave. Claims don't use Simplicity. | Keep amounts capped early. Rely on Blockstream's Simplicity verification work. |
| `musig2` crate bug | Its adaptor code is unaudited, version 0.4.1, and claims rest on it. | Audit before mainnet. Every signature is checked against the output key before it is relied on. |
| Slot time change | SIMD-0525 halved the slot time this year. Deadlines measured in slots shorten in real time. | Deadlines are computed at lock time from live data. Swaps are short. A halving during a swap still leaves the ordering intact. |

## 7. Residual risks to state to users

1. A Liquid rollback can cost either party a swap in flight.
2. A Liquid halt beyond about 7 days can cost a maker, and `extend` can delay a taker's refund.
3. A Solana stall with no restart, beyond the 3 h margin, can cost a maker.
4. Tether can freeze funds. A freeze after a reveal can cost a maker.
5. A compromised website can steal a taker's keys. Use a verified copy for meaningful amounts.
6. A taker who loses the recovery file and browser state loses L→S funds. For S→L, refund is permissionless, but it
   needs the escrow address, which is visible in the taker's lock transaction.
7. An L→S taker needs a little L-BTC to refund. S→L takers rely on the maker's fee coin to claim.
8. These contracts are unaudited until stated otherwise.
