# Taker client

Status: built in [client/](../client), and tested in a real browser against the real daemon. A fully static web app with no backend. Hosting and release hardening (section 6) are not done yet.

## 1. Architecture

| Layer | Contents |
|---|---|
| UI | Plain JavaScript modules in `client/site`, no framework and no build step for the page, styled after swapmarket.github.io with system fonts |
| Protocol core | `js/swap.js`: state machines from [protocol.md](protocol.md), driven by chain reads, resumable from storage. `js/plan.js`: how an order fills from the book. |
| Protocol logic in WASM | `client/wasm`, built on the same crates as the maker: addresses, unblinding, adapter signing, NIP-59 and the RFQ rules, plus Solana transactions and accounts, which tests compare byte for byte with the native builders |
| Solana | WalletConnect and Wallet Standard extensions (section 3), with no Solana library in the page |
| Chain access | Esplora and Solana RPC, user-configurable |
| Nostr | RFQ messages only ([nostr-rfq.md](nostr-rfq.md)) |
| Storage | `localStorage`, plus the recovery file |

**WASM crate.** The published `lwk_wasm` lacks Simplicity. LWK's `lwk_simplicity` also pins `simplicityhl` 0.8.0. So this is our own crate on
`simplicityhl 0.9.0` and `elements`. The module is 6.9 MB, or about 4.4 MB compressed. It runs in headless Chromium 145. Firefox and Safari are **UNVERIFIED**.

**Solana transaction format.** The chain now has version 1 transactions, which `@solana/web3.js` 1.95 can't decode. The
client must use a library that can, or decode the few fields it needs from account state rather than transactions,
which the escrow's stored value already favors.

## 2. Chain access

| Need | Method |
|---|---|
| Find the swap's coin | Esplora `/scripthash/:hash/utxo` (or `/txs`), then unblind each output with the swap's blinding key. Check the asset and amount here: nothing on-chain does. |
| See a spend | Esplora spend lookup on the outpoint. One witness item is a claim, and four is a refund. |
| Read escrow state | `getAccountInfo` on the derived PDA |
| Tip and slot | Esplora `/blocks/tip/height`, `getSlot` with `finalized` |

Free RPCs block `getProgramAccounts` and old history, so the client never scans. It derives every address from data it
has.

**Before revealing the secret, the counterparty's lock must be confirmed on two independent endpoints** (protocol
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
- The standalone recovery tools (section 4) don't need it. A refund reveals no secret, so no second endpoint is
  required.

The free plan's limits (10 requests/s, 1 `sendTransaction`/s, 1M credits a month) comfortably cover one taker. Whether a
free key works on devnet is **UNVERIFIED**.

## 3. Key handling

- The per-swap Liquid key, the secret `t` and the blinding key are generated with `crypto.getRandomValues`, one set per swap. They are never reused
  and are never derived from the user's wallet.
- **The Solana wallet is never given a Liquid key, and the app never sees the wallet's key.**
- The Liquid wallet (Aqua, Green and so on) is external. The user pays the confidential address, and the app never touches that
  wallet's keys. Whether real wallets pay this address form is **UNVERIFIED**. Paying from an explicit coin leaks a bound on the amount, and the app
  should say so.
- **Nonces are generated fresh for each session, held in memory, used once and never stored.** Signing two messages with one nonce reveals the
  key. A reload abandons the session, and a new one starts with new randomness.
- Secrets stay in browser storage only until the swap is settled, then are deleted. The recovery file may be kept by the
  user.
- Show the amount, asset, address and refund time before funding. Also show the build hash, so a user can compare it
  with the release, while being clear that a tampered page could display anything.

### Solana wallet connection

The user chose these on 2026-11-05, and chose against a key held by the page. The app holds no Solana key.

| Option | Reaches | Needs the relay? |
|---|---|---|
| **WalletConnect** | Phone wallets by QR code, such as Trust Wallet. `sol2base` documents that Phantom isn't reachable this way. | Yes |
| **Extension wallets** | Phantom, Solflare, Backpack and others, through the Wallet Standard. The older injected objects need a Solana library's transaction type, so they aren't supported. | No |

`sol2base` (`app.js`) has a working version of both, which is the starting point.

| Topic | Design |
|---|---|
| Library | `@walletconnect/sign-client` directly, rather than a wallet adapter library. **Bundled into `site/vendor` at a pinned version, not loaded from `esm.sh`** as `sol2base` does, because this client allows no CDN. Loaded only when the user picks WalletConnect. **UNVERIFIED:** no real phone wallet has connected yet. |
| Project ID | `c001d9e3d0c6308a5ce7e5932d40064a`, the Reown project shared with `sol2base` and `eth2base` (user's decision). It is public by nature, because a static page can't hide it. Reown advises one ID per project, and sharing means sharing quota and allowlist. |
| Allowed origins | **`swapmarket.github.io` must be on the project's allowlist.** It serves `sol2base`, and the allowlist can't be checked from here. Localhost is always allowed. IPFS gateways and other mirrors are refused unless listed, so those users need extension wallets, a listed origin, or their own project ID, which the app accepts as a setting. |
| Chain ids | CAIP-2: mainnet `solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp`, devnet `solana:EtWTRABZaYq6iMfeYKouRu166VU2xqa1`. |
| Signing | Use `solana_signTransaction` or the Wallet Standard's `solana:signTransaction`, then send from the app. **Never let the wallet broadcast a claim**, including `solana_signAndSendTransaction`. The app must control when the secret is broadcast: after the two-endpoint check and simulation, inside the cutoff. |
| Secret exposure | A claim passes through the user's wallet for signing, so the wallet sees the revealed value before broadcast. WalletConnect messages are end-to-end encrypted, and the wallet is already trusted with the user's funds. A malicious wallet colluding with the maker could still steal the swap. |
| Relay dependency | WalletConnect needs Reown's relay, and self-hosting isn't available. Extension wallets avoid it. |
| Time limits | A phone approval takes time. The client asks for the signature early enough that the claim cutoff still holds, and it rechecks the cutoff before sending. |
| Storage gotcha | WalletConnect moves `localStorage` keys whose names contain `walletconnect`, `wc@`, `wc_` or `wallet_connect` into IndexedDB and deletes them, so app keys must avoid those strings (found in `sol2base`). |

**If the relay is down:**

- Before funding: a WalletConnect user can't start a swap, since the client tests the wallet connection before showing the
  funding address. No funds are at risk. Extension wallets aren't affected.
- L→S, after funding, before claiming: the taker can't claim, so the secret stays hidden. Funds come back through the Liquid refund, which doesn't use WalletConnect.
- S→L refund: `refund` is permissionless, so anyone can send it. The standalone recovery tools accept any funded fee-payer key, for instance from the Solana CLI.

**UNVERIFIED, to test in the client phase:** claims through each wallet type, whether wallets accept version 1 transactions, and devnet use through WalletConnect (`sol2base` only uses mainnet).

### The makers book and splitting an order

Offers show each maker's price, per-swap limits and what it can pay now. A taker can trade any amount inside those limits, not only whole offers. Prices show to 4 places
and sizes to 2, with minimums rounded up and maximums and availability rounded down. Quotes and payments stay exact, because a coin must match to the last unit.

When an order is larger than the best offer, `js/plan.js` fills it best price first, across makers and across several swaps with one maker, up to 5 swaps. What the book can't cover is shown before accepting.

**The fills are not atomic together.** Each is its own swap, with its own secret, keys and refund, so some can complete while others refund, and the review screen says so. An L→S taker makes one exact payment per fill, and an S→L taker approves one lock per fill. One recovery file covers the whole order. In S→L every fill pays the same Liquid address, so an observer can see those payments went to one place, though not their amounts.

## 4. Recovery file

**The funding address is not revealed until the file has been downloaded and read back.** "Read back" means the user
picks the file and the app parses and checks it, which catches a failed or blocked download. It isn't proof the file
is safely kept, so the UI says so plainly.

```json
{
  "format": "liq2sol-recovery",
  "version": 2,
  "created_at": 1793888350,
  "swaps": [
    {
      "id": "<hex32>", "direction": "L2S", "maker": "<hex32, Nostr key>",
      "secrets": { "lock_secret": "<hex32>", "taker_sk": "<hex32>", "blinding_key": "<hex32>", "lock_point": "<hex33>", "lock_point_64": "<hex64>", "taker_key": "<hex33>" },
      "nostr": { "secret": "<hex32, throwaway>", "public": "<hex32>" },
      "terms": { "the maker's rfq-terms": "..." },
      "info": { "address": "<confidential address>", "fee_address": "<confidential address>", "cmr": "<hex32>" },
      "escrow_address": "<base58>", "solana_owner": "<base58>", "state": "fund"
    }
  ],
  "checksum": "<sha256 of the sorted JSON, excluding this field>"
}
```

- `secrets` holds what can spend funds: anyone with the file can refund (to any address) or complete the swap. The UI says
  this file is equivalent to money. Optional passphrase encryption: **scrypt + AES-256-GCM**, with the plaintext form
  above. It is off by default (D10). Encryption protects a leaked file, but a forgotten passphrase loses funds in a
  lost-browser case.
- The template source and version are enough to rebuild the program, because the compiled program for a version is
  deterministic. `cmr` and `address` are checked against the rebuild, and a mismatch is an error.
- S→L needs no Solana key in the file, since refund is permissionless and pays the initiator.

**Rescue.** The page's Rescue tab picks a swap up from the file on any device, and refunds need no maker or relay. Addresses are derived again from the file, not read from it.

**Not built yet:** a single-file `recovery.html` that runs from disk, and a `liq2sol-recover` CLI. Until then a local copy served over HTTP is the way to recover if the site is gone.

## 5. L-BTC handling

Per D3:

- S→L: the maker's fee coin pays a blinded key-path output on the taker's per-swap key. The claim spends it entirely as fee.
- L→S refund: the page shows a fee-coin address on that key, for the user to pay at least 30 sats of L-BTC, **only if a refund is needed**. All of it pays the fee, and any amount from there up works. A refund needs 29 sats at the relay floor. Wallets have their own minimums, and the page says so: Aqua refuses to send under 100 (seen by the user, 2026-11-06).

**Wallet behaviour is UNVERIFIED and must be tested with real wallets before release:**

- Can Aqua and Green pay an exact L-USDt amount to a confidential taproot address of this form?
- A real wallet has paid USDt to a `tlq1p…` taproot address of this form (the user, 2026-11-06). Whether every wallet does is untested. The Blockstream wallet was seen offering to send 30 sats to an `lq1q…` address, for a 26-sat fee at 0.10 sat/vB (seen by the user, 2026-11-06).
- If the user has no L-BTC, how does Aqua pay fees, and what does that do to a payment to an arbitrary address?

## 6. Build and release

| Requirement | Approach |
|---|---|
| Reproducible builds | Docker image pinned by digest. Rust toolchain and clang pinned. `SOURCE_DATE_EPOCH` set. CI builds twice and compares hashes. |
| Pinned dependencies | `Cargo.lock` and a package lockfile with integrity hashes. `--locked` and `npm ci`. Dependency changes require review. Minimal count. |
| SRI | Every script, style and WASM file gets a hash in the entry HTML. WASM loading verifies its hash. |
| CSP | `default-src 'self'; connect-src` limited to `'self'`, `https:` and `wss:`, since endpoints are user-configurable; `script-src 'self' 'wasm-unsafe-eval'`; no inline scripts. Set by meta tag, because GitHub Pages can't set headers. |
| Release hashes | Signed `SHA256SUMS` with the SwapMarket key on each GitHub release. Includes the CMR of each template version. |
| IPFS mirror | Each release is pinned, with its CID in the release notes. |
| Run locally | Release zip, `python3 -m http.server`, or `recovery.html` from disk. |
| Hosting risk | A compromised GitHub account can replace the site. The page can't verify itself, so users who care should use a hash-checked local copy or an IPFS CID. The UI links to these instructions. |

No analytics, no external fonts, no CDN. Program ID, asset ID, mint and template CMR are pinned per network in the
released code. The user can edit endpoints but not these, unless they use a clearly marked developer mode.
