# liq2sol

Trustless atomic swaps between **USDt on Liquid** and **USDT on Solana**: a MuSig2 taproot output with adapter-signed
claims and a Simplicity refund leaf on Liquid, an Anchor escrow on Solana, a Rust maker, a static taker page, and quotes
over Nostr. No custodian, no central operator, and a refund path for every user that needs no server.

**Status:** live on mainnet since 2026-11-17, with at most 100 USDT a swap. Nothing here has had an outside audit:
`musig2`'s adaptor code has the project's own review (`docs/musig2-review.md`).

- **The page:** https://swapmarket.github.io/liq2sol/. It opens on mainnet, and Settings switches to Liquid testnet and Solana
  devnet, which also have a faucet.
- **The same page on Nostr:** https://1cdzwdkc0uepk5ll8mhpcbyw3xh8gakldbfdq8fphhqnv503bcliq2sol.nsite.lol/, an nsite signed by SwapMarket's key
  (`client/reproducible/README.md`).
- **The mainnet escrow:** `DsgtATJzzejhSZweUsna77kU5vYFqrSojoEYUNmufi6b`. It is immutable, so a fix needs a new program id.
- **Makers:** signed releases for Debian on x86_64 and aarch64. `deploy/README.md` installs one.

| Document | Contents |
|---|---|
| [docs/prompt.md](docs/prompt.md) | Project brief |
| [docs/assumptions.md](docs/assumptions.md) | Verified facts with sources, and what remains unverified |
| [docs/protocol.md](docs/protocol.md) | State machines, timeouts, failure behaviour, open decisions |
| [docs/confidential.md](docs/confidential.md) | The Liquid design in use: MuSig2 output, adapter signatures, blinded coins, refund leaf |
| [docs/contracts.md](docs/contracts.md) | Liquid swap output and Solana escrow specifications |
| [docs/nostr-rfq.md](docs/nostr-rfq.md) | Quote and negotiation messages |
| [docs/client.md](docs/client.md) | Static taker app, keys, recovery file, release pipeline |
| [docs/maker.md](docs/maker.md) | Maker daemon, keys, limits, monitoring |
| [docs/threat-model.md](docs/threat-model.md) | Attackers, mitigations, residual risks |
| [docs/test-plan.md](docs/test-plan.md) | Tests, adversarial scenarios, formal verification |
| [docs/mainnet.md](docs/mainnet.md) | What mainnet needed, what is done and what is left |
| [docs/musig2-review.md](docs/musig2-review.md) | The project's review of `musig2`'s adaptor code |
| [deploy/README.md](deploy/README.md) | Installing and running a maker |

Licences: `client/` is AGPL-3.0-only, because it includes code from `SwapMarket/swapmarket.github.io`. Everything else is MIT.
