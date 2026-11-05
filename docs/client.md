# Taker client

Status: Phase 1 design, for review. A fully static web app on GitHub Pages, with no backend.

## 1. Architecture

| Layer | Contents |
|---|---|
| UI | Small TypeScript app, minimal dependencies, no runtime framework unless justified |
| Protocol core | State machines from [protocol.md](protocol.md). Pure functions, driven by chain reads, resumable from storage. |
| Liquid WASM | Own Rust crate: HTLC instantiation, address derivation, transaction build, signing, witness encoding |
| Solana | WalletConnect through a wallet adapter (section 3), plus reused code from `~/src/eth2base` and `~/src/sol2base` |
| Chain access | Esplora and Solana RPC, user-configurable |
| Nostr | RFQ messages only ([nostr-rfq.md](nostr-rfq.md)) |
| Storage | IndexedDB per swap, plus the recovery file |

**WASM crate.** The published `lwk_wasm` lacks Simplicity. LWK's `lwk_simplicity` also pins `simplicityhl` 0.8.0. The
recommendation is a small crate on `simplicityhl 0.9.0` and `elements`, exporting about six functions. Compiling in WASM
is verified (2.5 MB, CMR identical to native). **Signing a full spend in WASM is UNVERIFIED**, and it is the first thing
to prove in Phase 2.

**Solana transaction format.** The chain now has version 1 transactions, which `@solana/web3.js` 1.95 can't decode. The
client must use a library that can, or decode the few fields it needs from account state rather than transactions,
which the escrow's stored preimage already favors.

## 2. Chain access

| Need | Method |
|---|---|
| Find an HTLC coin | Esplora `/scripthash/:hash/utxo` (or `/txs`) |
| Read a preimage | Esplora spend lookup on the outpoint, then the witness (only relevant for makers and auditors) |
| Read escrow state | `getAccountInfo` on the derived PDA |
| Tip and slot | Esplora `/blocks/tip/height`, `getSlot` with `finalized` |

Free RPCs block `getProgramAccounts` and old history, so the client never scans. It derives every address from data it
has.

**Before revealing a preimage, the counterparty's lock must be confirmed on two independent endpoints** (protocol
section 4). If the second endpoint isn't available, the client requires the explicit override and warning.

Rate limits on public Esplora are **UNVERIFIED**, so poll no faster than every 15 seconds.

| Chain | First endpoint | Second endpoint |
|---|---|---|
| Liquid | `blockstream.info` (CORS `*`) | `liquid.network` (CORS `*`) |
| Solana mainnet | `solana-rpc.publicnode.com` | Helius, `https://mainnet.helius-rpc.com/?api-key=<key>` |
| Solana devnet | `api.devnet.solana.com` | Helius, `https://devnet.helius-rpc.com/?api-key=<key>` |

All endpoints remain user-configurable.

**Helius key.** Helius rejects requests without a key (HTTP 401), so it is active only once the taker enters their own.
The key is optional, and the app ships with none, because any key in a static page is public and its shared quota could
be drained by anyone.

- The swap screen offers the field before funding, with a link to Helius's free plan, and explains that without a key
  the two-endpoint check needs the override.
- The key is stored only in this browser's storage and sent only to Helius.
- The key sits in the URL's query string, so the app never logs or displays full endpoint URLs. It never goes in the
  recovery file, a Nostr message or an error report.
- The standalone recovery tools (section 4) don't need it. A refund needs no preimage reveal, so no second endpoint is
  required.

The free plan's limits (10 requests/s, 1 `sendTransaction`/s, 1M credits a month) comfortably cover one taker. Whether a
free key works on devnet is **UNVERIFIED**.

## 3. Key handling

- Ephemeral Liquid key and preimage are generated with `crypto.getRandomValues`, one set per swap. They are never reused
  and are never derived from the user's wallet.
- **The Solana wallet is never given a Liquid key, and the app never sees the wallet's key.**
- The Liquid wallet (Aqua, Green and so on) is external. The user pays the address, and the app never touches that
  wallet's keys.
- Secrets stay in browser storage only until the swap is settled, then are deleted. The recovery file may be kept by the
  user.
- Show the amount, asset, address and refund time before funding. Also show the build hash, so a user can compare it
  with the release, while being clear that a tampered page could display anything.

### Solana wallet connection

The user chose WalletConnect on 2026-11-05, and chose against a key held by the page. The app holds no Solana key.

| Topic | Design |
|---|---|
| Library | A WalletConnect adapter for the Solana wallet adapter library: `@walletconnect/solana-adapter` (Reown) or `@solana/wallet-adapter-walletconnect` (Anza). To be chosen in the client phase. Pinned, and loaded only when the user connects. |
| Project ID | One Reown project ID per deployment. It is public by nature, because a static page can't hide it, so it isn't treated as a secret. It is pinned in the released code. |
| Allowed origins | The GitHub Pages origin is on Reown's allowlist. Localhost is always allowed. **IPFS gateways and other mirrors are refused unless listed**, so those users need a listed origin or their own project ID, which the app accepts as a setting. |
| Signing | Use `solana_signTransaction`, then send from the app, **never `solana_signAndSendTransaction` for a claim**. The app must control when the preimage is broadcast: after the two-endpoint check and simulation, inside the cutoff. |
| Preimage exposure | A claim passes through the user's wallet for signing, so the wallet sees `p` before broadcast. WalletConnect messages are end-to-end encrypted, and the wallet is already trusted with the user's funds. A malicious wallet colluding with the maker could still steal the swap. |
| Relay dependency | WalletConnect needs Reown's relay, and self-hosting isn't available. This is the one central service in the taker's Solana path. See below. |
| Time limits | A phone approval takes time. The client asks for the signature early enough that the claim cutoff still holds, and it rechecks the cutoff before sending. |

**If the relay is down:**

- Before funding: no swap starts, since the client tests the wallet connection before showing the funding address. No funds are at risk.
- L→S, after funding, before claiming: the taker can't claim, so `p` stays secret. Funds come back through the Liquid refund, which doesn't use WalletConnect.
- S→L refund: `refund` is permissionless, so anyone can send it. The standalone recovery tools accept any funded fee-payer key, for instance from the Solana CLI.

**UNVERIFIED, to test in the client phase:** the WalletConnect chain IDs for Solana mainnet and devnet, which wallets support the Solana namespace (**including whether Phantom does**, since the brief names it), and whether the adapter handles version 1 transactions.

## 4. Recovery file

**The funding address is not revealed until the file has been downloaded and read back.** "Read back" means the user
picks the file and the app parses and checks it, which catches a failed or blocked download. It isn't proof the file
is safely kept, so the UI says so plainly.

```json
{
  "format": "liq2sol-recovery",
  "version": 1,
  "network": "testnet",
  "created_at": 1793888350,
  "swap_id": "<hex32>",
  "direction": "L2S",
  "secret": { "preimage": "<hex32>", "liquid_sk": "<hex32>" },
  "liquid": {
    "template_version": 1,
    "params": {
      "PAYMENT_HASH": "<hex32>", "RECIPIENT_PK": "<hex32>", "SENDER_PK": "<hex32>",
      "LOCK_HEIGHT": 2692446, "USDT_ASSET": "<hex32>", "AMOUNT": "10000000000",
      "RECIPIENT_SCRIPT_HASH": "<hex32>"
    },
    "cmr": "<hex32>",
    "address": "<non-confidential address>"
  },
  "solana": {
    "program_id": "<base58>", "initiator": "<base58>", "recipient": "<base58>",
    "mint": "<base58>", "amount": "99700000", "escrow_pda": "<base58>"
  },
  "maker_npub": "npub1...",
  "checksum": "<sha256 of the canonical JSON, excluding this field>"
}
```

- `secret` holds what can spend funds: anyone with the file can refund (to any address) or complete the swap. The UI says
  this file is equivalent to money. Optional passphrase encryption: **scrypt + AES-256-GCM**, with the plaintext form
  above. It is off by default (D10). Encryption protects a leaked file, but a forgotten passphrase loses funds in a
  lost-browser case.
- The template source and version are enough to rebuild the program, because the compiled program for a version is
  deterministic. `cmr` and `address` are checked against the rebuild, and a mismatch is an error.
- S→L needs no Solana key in the file, since refund is permissionless and pays the initiator.

**Standalone recovery.** A file alone must be enough, without the site:

- A single-file `recovery.html` (with embedded WASM), included in every release, runs from disk.
- A CLI (`liq2sol-recover`) built from the same Rust crate.

Both take the file, a destination address, and an Esplora URL, and they tell the user what's needed: the L-BTC fee coin
address and amount, then the earliest refund height. No Nostr, no maker, no liq2sol server.

## 5. L-BTC handling

Per D3:

- S→L: the maker's fee coin pays to a key-path taproot output on the taker's ephemeral key. The claim spends it entirely
  as fee.
- L→S refund: the client shows a fee-coin address on the ephemeral key, for the user to pay a few hundred sats of L-BTC,
  **only if a refund is needed**. All L-BTC used is explicit, so the transaction carries no range proofs.

**Wallet behaviour is UNVERIFIED and must be tested with real wallets before release:**

- Can Aqua and Green pay an exact L-USDt amount to a non-confidential taproot address?
- Do they display an incoming explicit USDt output?
- If the user has no L-BTC, how does Aqua pay fees, and what does that do to a payment to an arbitrary address?

## 6. Build and release

| Requirement | Approach |
|---|---|
| Reproducible builds | Docker image pinned by digest. Rust toolchain and clang pinned. `SOURCE_DATE_EPOCH` set. CI builds twice and compares hashes. |
| Pinned dependencies | `Cargo.lock` and a package lockfile with integrity hashes. `--locked` and `npm ci`. Dependency changes require review. Minimal count. |
| SRI | Every script, style and WASM file gets a hash in the entry HTML. WASM loading verifies its hash. |
| CSP | `default-src 'self'; connect-src` limited to `https:` and `wss:`, since endpoints are user-configurable; `script-src 'self' 'wasm-unsafe-eval'`; no inline scripts. Set by meta tag, because GitHub Pages can't set headers. |
| Release hashes | Signed `SHA256SUMS` with the SwapMarket key on each GitHub release. Includes the CMR of each template version. |
| IPFS mirror | Each release is pinned, with its CID in the release notes. |
| Run locally | Release zip, `python3 -m http.server`, or `recovery.html` from disk. |
| Hosting risk | A compromised GitHub account can replace the site. The page can't verify itself, so users who care should use a hash-checked local copy or an IPFS CID. The UI links to these instructions. |

No analytics, no external fonts, no CDN. Program ID, asset ID, mint and template CMR are pinned per network in the
released code. The user can edit endpoints but not these, unless they use a clearly marked developer mode.
