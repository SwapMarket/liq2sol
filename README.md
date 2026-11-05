# liq2sol

Trustless atomic swaps between **USDt on Liquid** and **USDT on Solana**: a SimplicityHL HTLC on Liquid, an
Anchor escrow on Solana, a Rust maker daemon, a static taker web app, and quotes over Nostr. No custodian, no
central operator, and a server-free refund path for every user.

Status: early implementation, for testnet and devnet only. Nothing here is audited or handles real funds. The Liquid
contract library, the Solana program and a CLI exist. The CLI has completed swaps in both directions on Liquid testnet and Solana
devnet, using stand-in assets. The maker daemon and client don't exist yet.

| Document | Contents |
|---|---|
| [docs/prompt.md](docs/prompt.md) | Project brief |
| [docs/assumptions.md](docs/assumptions.md) | Verified facts with sources, and what remains unverified |
| [docs/protocol.md](docs/protocol.md) | State machines, timeouts, failure behaviour, open decisions |
| [docs/contracts.md](docs/contracts.md) | Liquid HTLC and Solana escrow specifications |
| [docs/nostr-rfq.md](docs/nostr-rfq.md) | Quote and negotiation messages |
| [docs/client.md](docs/client.md) | Static taker app, keys, recovery file, release pipeline |
| [docs/maker.md](docs/maker.md) | Maker daemon, keys, limits, monitoring |
| [docs/threat-model.md](docs/threat-model.md) | Attackers, mitigations, residual risks |
| [docs/test-plan.md](docs/test-plan.md) | Tests, adversarial scenarios, formal verification |
