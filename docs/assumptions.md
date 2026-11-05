# Phase 0: verified assumptions

Checked on 2026-11-05 against current source and live endpoints. Each item links to where it was verified.
Anything not confirmed from a primary source is marked **UNVERIFIED**.

## 1. SimplicityHL

| Item | Finding | Source |
|---|---|---|
| Repo / version | `BlockstreamResearch/SimplicityHL` (formerly Simfony). Latest release **0.9.0** (2026-11-04); 0.8.0 (2026-09-27). Compiler binary `simc` (needs the `serde` feature to read `.args`/`.wit` files). | [releases](https://github.com/BlockstreamResearch/SimplicityHL/releases), [Cargo.toml](https://github.com/BlockstreamResearch/SimplicityHL/blob/simplicityhl-0.9.0/Cargo.toml) |
| Version directive | Files should start with `simc "0.9.0";` (compiler warns otherwise). | `simc` output when compiling `contracts/liquid/src/htlc.simf` |
| Official HTLC example | `examples/htlc.simf`: `witness::COMPLETE_OR_CANCEL: Either<(u256, Signature), Signature>`; claim = sha256 preimage + BIP-340 sig; cancel = `check_lock_height` + BIP-340 sig. | [htlc.simf](https://github.com/BlockstreamResearch/SimplicityHL/blob/simplicityhl-0.9.0/examples/htlc.simf), [htlc.complete.wit](https://github.com/BlockstreamResearch/SimplicityHL/blob/simplicityhl-0.9.0/examples/htlc.complete.wit) |
| Parameters | Compile-time parameters are `param::NAME`, supplied in a `.args` JSON file (values as strings, e.g. `"LOCK_HEIGHT": "1000"`). Parameters are baked into the program, so each swap has its own CMR/address. | [p2pk.simf](https://github.com/BlockstreamResearch/SimplicityHL/blob/simplicityhl-0.9.0/examples/p2pk.simf) + `p2pk.args`; verified by compiling the draft |
| Witness values | `witness::NAME`, supplied in a `.wit` JSON file (`{"NAME": {"value": "...", "type": "..."}}`). | `htlc.complete.wit` |
| SHA-256 jets | `jet::sha_256_ctx_8_init()`, `jet::sha_256_ctx_8_add_32(ctx, u256)`, `jet::sha_256_ctx_8_finalize(ctx) -> u256` | `htlc.simf` |
| BIP-340 | `jet::bip_0340_verify((pk: Pubkey, msg: u256), sig: Signature)`; message `jet::sig_all_hash()` | `htlc.simf`, [docs/jet.rs](https://github.com/BlockstreamResearch/SimplicityHL/blob/simplicityhl-0.9.0/src/docs/jet.rs) |
| Lock height | `jet::check_lock_height(h: Height)` asserts `tx_lock_height >= h` (absolute height; `Height = u32`). Relative-lock jets are named `BrokenDoNotUseCheckLockDistance/Duration`: **do not use relative timelocks**. | `src/docs/jet.rs` lines ~610–625 |
| Output introspection | `jet::output_asset(u32) -> Option<Asset1>`, `jet::output_amount(u32) -> Option<(Asset1, Amount1)>`, `jet::output_script_hash(u32) -> Option<u256>` (SHA-256 of scriptPubKey), `jet::num_outputs()`, `jet::output_is_fee(u32)`, `jet::total_fee(asset)`. Inputs: `jet::current_index()`, `jet::current_amount() -> (Asset1, Amount1)`, `jet::current_asset()`, `jet::current_script_hash()`. | `src/docs/jet.rs`, [src/jet/elements.rs](https://github.com/BlockstreamResearch/SimplicityHL/blob/simplicityhl-0.9.0/src/jet/elements.rs) lines ~1100–1130 |
| Confidential vs explicit | `Asset1 = Either<(u1, u256), u256>` (blinded commitment \| explicit asset id); `Amount1 = Either<(u1, u256), u64>` (blinded \| explicit). The covenant must require `Right(...)` for both. | [src/types/aliased.rs](https://github.com/BlockstreamResearch/SimplicityHL/blob/simplicityhl-0.9.0/src/types/aliased.rs) lines ~385–395 |
| Other jets used | `jet::eq_256`, `jet::eq_64`, `assert!`, `panic!()` | examples, `src/docs/jet.rs` |
| Draft compiles | `contracts/liquid/src/htlc.simf` compiles with `simc 0.9.0` using `contracts/liquid/htlc.args`. Not yet executed against a transaction. | local run |

## 2. Commitment (CMR → Taproot address) and witness

| Item | Finding | Source |
|---|---|---|
| Activation | Simplicity is live on Liquid mainnet (activated 2025-08-01). | [Blockstream: Simplicity launches on Liquid mainnet](https://blog.blockstream.com/simplicity-launches-on-liquid-mainnet/) |
| Leaf version | Tapleaf version **0xbe** (`TAPROOT_LEAF_TAPSIMPLICITY`). | [elements interpreter.h](https://github.com/ElementsProject/elements/blob/elements-23.3.4/src/script/interpreter.h) line 280, [rust-simplicity lib.rs](https://github.com/BlockstreamResearch/rust-simplicity/blob/master/src/lib.rs) `leaf_version()` |
| Leaf "script" | The tapleaf script is the 32-byte **CMR** of the program. | `interpreter.cpp` (`script_bytes.size() != 32` check) |
| Spend witness | Witness stack = `[simplicity_witness, simplicity_program, cmr(32 bytes), control_block]`; anything else fails with `SCRIPT_ERR_SIMPLICITY_WRONG_LENGTH`. Cost budget = serialized witness size + offset. | [elements interpreter.cpp](https://github.com/ElementsProject/elements/blob/elements-23.3.4/src/script/interpreter.cpp) ~line 3355 |
| Address | P2TR output whose tap tree has the 0xbe leaf. Internal key: the usual choice is an unspendable NUMS point, so only the Simplicity leaf can spend. **UNVERIFIED:** the exact NUMS point used by LWK/SimplicityHL tooling (check `lwk_simplicity` taproot builder before Phase 2). | LWK `lwk_bindings/tests/bindings/simplicity_taproot_builder.py` (not yet read) |
| Elements version | Latest release elements-23.3.4 (2026-09-09). | [releases](https://github.com/ElementsProject/elements/releases) |

## 3. WASM feasibility (browser)

| Item | Finding | Source |
|---|---|---|
| rust-simplicity | `simplicity-lang` 0.9.0 has `[target.wasm32-unknown-unknown.dependencies]` and a `wasm-sysroot` for its C core. | [Cargo.toml](https://github.com/BlockstreamResearch/rust-simplicity/blob/master/Cargo.toml), [simplicity-sys/build.rs](https://github.com/BlockstreamResearch/rust-simplicity/blob/master/simplicity-sys/build.rs) |
| **Compiler in WASM: verified** | A probe crate depending on `simplicityhl = "=0.9.0"` built for `wasm32-unknown-unknown` (2.5 MB) and run under Node with wasm-pack compiled the official HTLC and returned CMR `1cf9b3bf158655c66c4c2fbe3812e849fea98266a2dfca58622c31be7b473651`, identical to the native build. The C cores (`secp256k1-zkp`, `simplicity-sys`) compile with clang 18's wasm32 backend. | local build (Rust 1.97.1, wasm-pack) |
| Build gotcha | This machine's shell sets `RUSTFLAGS=-C link-arg=-fuse-ld=lld`, which breaks wasm32 linking. Build wasm with `RUSTFLAGS=""`. | local |
| LWK | `Blockstream/lwk` 0.20.0 has `lwk_simplicity` (depends on `simplicityhl` **0.8.0**) and `lwk_wasm`. The published npm `lwk_wasm@0.20.0` (9.9 MB wasm) does **not** include Simplicity (`simplicity` is an optional, non-default feature). We'd build our own WASM (either `lwk_wasm --features simplicity` or a smaller crate on rust-simplicity + `elements`). | [lwk_wasm/Cargo.toml](https://github.com/Blockstream/lwk/blob/master/lwk_wasm/Cargo.toml), [lwk_simplicity/Cargo.toml](https://github.com/Blockstream/lwk/blob/master/lwk_simplicity/Cargo.toml), npm package contents |
| LWK swap code | LWK also has `lwk_boltz` and `lwk_anyswap` crates: possible prior art. **UNVERIFIED:** what `lwk_anyswap` does. | [lwk tree](https://github.com/Blockstream/lwk) |
| Transaction signing in WASM | **UNVERIFIED:** building and signing a full Liquid spend (sighash, control block, witness serialization) in WASM. Expected to work (same crates), but not yet exercised. | — |

## 4. Assets

| Item | Finding | Source |
|---|---|---|
| L-USDt | Asset id `ce091c998b83c78bb71a632313ba3760f1763d9cfcffae02258ffa9865a37bd2`, name "Tether USD", ticker USDt, **precision 8**, entity domain `tether.to`, issuer pubkey `0337cceec0beea0232eb…`. | [Esplora asset](https://blockstream.info/liquid/api/asset/ce091c998b83c78bb71a632313ba3760f1763d9cfcffae02258ffa9865a37bd2) (same on liquid.network) |
| SOL USDT | Mint `Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB`, legacy **Token program** (`TokenkegQ…`), **6 decimals**, mint and **freeze authority** `Q6XprfkF8RQQKoQVG33xT88H7wi8Uk1B1CC7YAs69Gi` (Tether can freeze token accounts). | Solana RPC `getAccountInfo` (jsonParsed), mainnet |
| Conversion | 1 SOL-USDT base unit = 100 L-USDt base units. | derived from the above |
| Testnets | Tether does not issue on Liquid testnet or Solana devnet: tests use self-issued stand-in assets. **UNVERIFIED** that no official test assets exist. | — |
| Tether freeze on Liquid | **UNVERIFIED:** whether/how Tether can freeze L-USDt UTXOs (accepted residual trust per prompt). | — |

## 5. Liquid consensus

| Item | Finding | Source |
|---|---|---|
| Block time | `nPowTargetSpacing = 60` (1 minute). | [chainparams.cpp](https://github.com/ElementsProject/elements/blob/elements-23.3.4/src/chainparams.cpp) `CLiquidV1Params` |
| Signing | Federation multisig, "11 signatures, 15 pubkeys"; `max_block_signature_size = 12*74+16*33`. | same |
| Dynafed | `dynamic_epoch_length = 20160` blocks (~14 days), `total_valid_epochs = 2`. Header has `DYNAFED_HF_MASK` (bit 31) in version; `DynaFedParams` with current/proposed `DynaFedParamEntry` (signblockscript, signblock_witness_limit, fedpeg program/script, extension space; or pruned with `m_elided_root`); signatures in `m_signblock_witness`. | [block.h](https://github.com/ElementsProject/elements/blob/elements-23.3.4/src/primitives/block.h), [block.cpp](https://github.com/ElementsProject/elements/blob/elements-23.3.4/src/primitives/block.cpp) |
| Header hash | `GetHash()` serializes without the signblock witness. | `block.cpp` |
| Finality | "2 blocks" finality. **UNVERIFIED from Blockstream primary docs**; only secondary sources so far. | [Rootstock comparison](https://rootstock.io/blog/the-cutting-edge-of-sidechains-liquid-and-rsk/) |
| signblockscript form | **UNVERIFIED:** exact script template (CHECKMULTISIG layout), signature encoding (ECDSA DER?), and how the active signblockscript is derived across dynafed epochs. Needed only for the Phase 2 light client. | — |

## 6. Solana

| Item | Finding | Source |
|---|---|---|
| Validator version | Agave v4.4.0 (2026-10-28). | [releases](https://github.com/anza-xyz/agave/releases) |
| Syscalls | `sol_sha256`, `sol_secp256k1_recover`, `sol_keccak256`, `sol_blake3`, `sol_get_clock_sysvar`, `sol_remaining_compute_units`, … | [syscalls/src/lib.rs](https://github.com/anza-xyz/agave/blob/v4.4.0/syscalls/src/lib.rs) ~line 383 |
| Compute budget | `MAX_COMPUTE_UNIT_LIMIT = 1_400_000` per tx; `DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT = 200_000`; heap 32 KiB default, `MAX_HEAP_FRAME_BYTES = 256 KiB`. | [program-runtime/src/execution_budget.rs](https://github.com/anza-xyz/agave/blob/v4.4.0/program-runtime/src/execution_budget.rs) |
| Syscall costs | `sha256_base_cost = 85`, `sha256_byte_cost = 1`, `secp256k1_recover_cost = 25_000`. | same file |
| Anchor | **Corrected in Phase 2:** the repo is now `otter-sec/anchor` (the old `solana-foundation/anchor` name redirects). GitHub's latest is **v1.2.2** (2026-10-28), minimum Rust 1.89. v0.32.3 (2026-11-03) is a patch to the older 0.32 line, published the same day as v0.31.3 and v0.30.4. v0.32.3's changelog section is empty, so no fix is listed. | [releases](https://github.com/otter-sec/anchor/releases), [installation](https://www.anchor-lang.com/docs/installation) |
| Finalized commitment | **UNVERIFIED** current typical time-to-finalized (historically ~12–13 s). | — |
| Transaction format | Solana now has a **version 1** transaction format; `@solana/web3.js` 1.95 can't decode it (seen while scanning mainnet). Clients must handle it. | observed via RPC |

## 7. Public endpoints (browser)

| Endpoint | CORS | Notes |
|---|---|---|
| `https://blockstream.info/liquid/api`, `/liquidtestnet/api` | `Access-Control-Allow-Origin: *` | Rate limits: **UNVERIFIED** (no rate-limit headers returned) |
| `https://liquid.network/api`, `/liquidtestnet/api` | `access-control-allow-origin: *` | same |
| `https://solana-rpc.publicnode.com` | `*` | Works for basic reads and sends; **blocks** "indexed" methods (`getProgramAccounts`, `getTokenAccountsByOwner`) and older history ("archive requests require a personal token"). |
| `https://api.mainnet-beta.solana.com` | `*` | Returned 403 "Access forbidden" from this network. |
| Solana devnet RPC | **UNVERIFIED** | — |

## 8. Nostr

| Item | Finding | Source |
|---|---|---|
| Event kinds | Regular; replaceable `10000–19999`; ephemeral `20000–29999`; addressable `30000–39999` (keyed by kind + pubkey + `d` tag). | [NIP-01](https://github.com/nostr-protocol/nips/blob/master/01.md) |
| Related NIPs | NIP-09 (deletion), NIP-40 (expiration), NIP-44 (encryption), NIP-59 (gift wrap), NIP-17 (private DMs) all exist. | [nips repo](https://github.com/nostr-protocol/nips) |

## 9. Background

| Item | Finding | Source |
|---|---|---|
| Boltz suspension | Boltz suspended all Bitcoin, Lightning and Liquid swaps on 2026-08-03, citing months of AI-assisted attacks; no user funds lost (non-custodial). Blockstream launched in-house swaps around 2026-08-11. | [crypto.news](https://crypto.news/boltz-halts-swaps-as-ai-attacks-outpace-its-team/), [TFTC](https://www.tftc.io/boltz-bitcoin-swaps-suspended-ai-attacks), [bitbo](https://bitbo.io/news/blockstream-launches-swaps-boltz/) |

## 10. Phase 1 additions (checked 2026-11-05)

These were checked while writing the design. Several update or close items above.

### Liquid

| Item | Finding | Source |
|---|---|---|
| **September 2026 exploit** | A range-proof cache bug in Elements let one transaction create about 4,000 unbacked L-BTC at block 4,050,336 on Sept 6. The fix had been public since Sept 1. USDt and other issued assets were "unaffected", though unavailable during the pause. | [Blockstream assessment](https://blog.blockstream.com/liquid-network-security-incident-assessment/) |
| **Halt length** | Blocks resumed Sept 9 21:05 UTC. User transactions reappeared Sept 10 12:31 UTC, so about 4 days of unavailability. The assessment doesn't state a total. An earlier halt of about 22 hours is reported by a secondary source, its date inferred as about 2021. | Same; [secondary](https://tradingclubhouse.com/liquid-network-block-production-resumes-following-transaction-processing-issues/) |
| **Rollback** | The exploit block was replaced ("replacement block 4,050,336") and transactions verified as valid were replayed. So 2-block finality is a working assumption, not an absolute. **UNVERIFIED:** rollback depth and whether all valid transactions were replayed. | Blockstream assessment |
| Peg status | Blockstream's report (latest update Sept 17) has pegs still suspended. One secondary report says L-BTC is fully backed again by about Nov 4. **UNVERIFIED** from a primary source. | [CryptoTicker](https://cryptoticker.io/en/liquid-network-l-btc-fully-backed-again/) |
| Non-L-BTC fees | Consensus tracks fees per asset (`fee_map`), but mempool acceptance counts only `policyAsset`: "We only consider policyAsset". A USDt-only fee counts as zero and fails relay. Closes the open point in `CLAUDE.md`. | [validation.cpp](https://github.com/ElementsProject/elements/blob/elements-23.3.4/src/validation.cpp) line 893 |
| Fee rate | Esplora estimates 0.1 sat/vB at all targets. `DEFAULT_MIN_RELAY_TX_FEE = 100` sat/kvB. | live `/fee-estimates`; [validation.h](https://github.com/ElementsProject/elements/blob/elements-23.3.4/src/validation.h) line 59 |
| NUMS key | LWK's Simplicity taproot test uses `50929b74c1a04954b78b4b6035e97a5e078a5a0f28ec96d547bfee9ace803ac0`. Closes the item in section 2. | [lwk test](https://github.com/Blockstream/lwk/blob/master/lwk_bindings/tests/bindings/simplicity_taproot_builder.py) |
| `lwk_anyswap` | A thin wrapper over the `anyswap-sdk` crate. Not relevant prior art. Closes the item in section 3. | [lwk_anyswap](https://github.com/Blockstream/lwk/tree/master/lwk_anyswap) |
| Lock-time semantics | `tx_lock_height` returns zeroes when `tx_is_final`. A refund must set a non-final sequence. `IsFinalTx` also treats a transaction as final if all sequences are maxed. | `src/docs/jet.rs` lines 626–629, [tx_verify.cpp](https://github.com/ElementsProject/elements/blob/elements-23.3.4/src/consensus/tx_verify.cpp) line 18 |
| **Signblockscript form** | Decoded live block 4,131,983: compact current params entry with a P2WSH signblockscript and elided root, no proposed entry. Witness has 13 items: empty, 11 DER ECDSA signatures with sighash `01`, and a 513-byte script `OP_11 <15 pubkeys> OP_15 OP_CHECKMULTISIG`, whose SHA-256 matches the P2WSH. Closes the form and encoding part of section 5. **UNVERIFIED:** exact signed digest. | Live header from Esplora `/block/:hash/header` |
| Draft size and cost | 473-byte program, 97-byte claim witness, 65-byte refund witness, cost bound 121,123 milliWU, before pruning. Budget = witness stack size + 50 WU, in weight units. | Local probe with `simplicityhl 0.9.0`, [analysis.rs](https://github.com/BlockstreamResearch/rust-simplicity/blob/master/src/analysis.rs), `interpreter.cpp` line 3360 |

### Solana

| Item | Finding | Source |
|---|---|---|
| **Alpenglow is live** | Mainnet consensus switched on 2026-10-21. Reported finality about 150 ms, and confirmed and finalized become equivalent. Secondary sources; the primary page returned HTTP 429 when fetched. **UNVERIFIED** from a primary source. | [solana.com/upgrades/alpenglow](https://solana.com/upgrades/alpenglow), [Crowdfund Insider](https://www.crowdfundinsider.com/2026/10/319911-anza-activates-alpenglow-on-solana-sol-mainnet-slashing-finality-to-150-milliseconds/) |
| **Slot time** | Measured 208–220 ms, average 213 ms, over 60 one-minute samples. Nominal 200 ms per SIMD-0525 (secondary source). Replaces the 400 ms assumption. | Live `getRecentPerformanceSamples` |
| **`unix_timestamp` under Alpenglow** | Set by the leader's block footer, bounded to `(parent, parent + 2 × elapsed slot duration]`. No jump after a halt, catches up at up to twice nominal. Replaces the stake-weighted calculation, which is still in the code for pre-Alpenglow banks. | [block_component_processor.rs](https://github.com/anza-xyz/agave/blob/v4.4.0/runtime/src/block_component_processor.rs) line 770, [bank.rs](https://github.com/anza-xyz/agave/blob/v4.4.0/runtime/src/bank.rs) line 3614 |
| **`LastRestartSlot`** | Sysvar holding the latest hard-fork slot at or below the current slot, with syscall `sol_get_last_restart_slot`. Changes only on hard-fork restarts. Live on mainnet and devnet (the account exists). Mainnet holds slot 246,464,040, and **from memory, not checked,** that matches the February 2024 restart. **UNVERIFIED:** how a stall without a hard fork behaves under Alpenglow. | `bank.rs` line 2678, [syscalls/src/lib.rs](https://github.com/anza-xyz/agave/blob/v4.4.0/syscalls/src/lib.rs) line 453 |
| Outage history | A secondary report says Solana has passed 1,000 days without an outage. | [coinalertnews](https://coinalertnews.com/news/2026/11/02/solana-1000-days-uptime-milestone) |
| Devnet RPC | `api.devnet.solana.com` echoes the request origin in CORS, and reports `solana-core 4.4.0-beta.1`. Closes the item in section 7. | Live request |
| Helius RPC | Mainnet and devnet send `access-control-allow-origin: *` and return HTTP 401 without a valid key. The key is an `api-key` query parameter. Free plan: 1M credits a month, 10 RPC requests/s, 1 `sendTransaction`/s, archival data included. Helius also describes masked, IP-limited "Secure RPC" URLs as safe for frontends. **UNVERIFIED:** a request with a real key, a free key on devnet, and whether the free plan includes Secure RPC URLs. | Live requests, [pricing](https://www.helius.dev/pricing), [endpoints](https://www.helius.dev/docs/api-reference/endpoints) |

### Phase 2 proofs

| Item | Finding | Source |
|---|---|---|
| **HTLC on a node** | Claim mined and refund mined on Elements 23.3.4 regtest with `-evbparams=simplicity:-1:::`. The node refused a wrong CMR, control block or leaf version, an unpruned program, an early refund, and a refund with final sequences. | `contracts/liquid/tests/regtest.rs` |
| **Signing in WASM** | Closes the item in section 3. A full claim with a Simplicity input and a taproot key-path fee input, built and signed in WASM under Node, is byte-identical to native. The module is 4.7 MB unoptimized for size. **UNVERIFIED** in a real browser, as opposed to Node. | `scripts/check-wasm-parity.sh` |
| Asset byte order | The jets compare internal byte order, the reverse of explorer display. | `asset_param_is_the_reverse_of_display_order` test |
| Claim size | 379 vB with one fee input. | Regtest measurement |

### Public networks

| Item | Finding | Source |
|---|---|---|
| Simplicity on Liquid testnet | Active. A third-party spend was seen at height 2,692,247, and our own claim and refund confirmed. | [claim](https://blockstream.info/liquidtestnet/tx/bd400dbfdead7d43aae3ef4c6f5bef63f4786e5a7979d691be6787b27e05aa51), [refund](https://blockstream.info/liquidtestnet/tx/06c42327edf7b8dbe02457bf5e8897cb90cb18775215d5916b7e69ea5b3525bf) |
| Liquid testnet test assets | Faucet gives explicit coins to non-confidential addresses: 100,000 sats of L-BTC and 5,000 units of `TEST` (`38fca2d9…50a5`). It rate-limits with HTTP 429. Official Tether test assets remain **UNVERIFIED**. | [faucet](https://liquidtestnet.com/faucet) |
| Devnet deployment | Escrow at `DxVq5iSzKfRMAyis8RyLUeLggidM2Jvk1FZyyEauCRQ1`, upgradeable (D8), 332,664 bytes, about 1.7 SOL of rent. Stand-in mint `8BXimW8mCEfhdD1fhv9aoCfQgSj3KxFc52jMAbLvaFwi`. | `solana program show` |
| Devnet slot time | About 240 ms, slower than mainnet's 213 ms. | Live `getRecentPerformanceSamples` |
| `solana-test-validator` | The prebuilt binary aborts on CPUs without AVX. The check is set at build time by `perf/build.rs`, so a local build should avoid it. Not yet tried. | Local run, [perf/build.rs](https://github.com/anza-xyz/agave/blob/v4.4.0/perf/build.rs) |

### WalletConnect

| Item | Finding | Source |
|---|---|---|
| Solana methods | `solana_getAccounts`, `solana_requestAccounts`, `solana_signMessage`, `solana_signTransaction`, `solana_signAllTransactions`, `solana_signAndSendTransaction`. | [Reown RPC reference](https://docs.reown.com/advanced/multichain/rpc-reference/solana-rpc) |
| Adapters | `@walletconnect/solana-adapter` 0.0.9 and `@solana/wallet-adapter-walletconnect` 0.1.22 both exist. | [npm](https://www.npmjs.com/package/@walletconnect/solana-adapter), [npm](https://www.npmjs.com/package/@solana/wallet-adapter-walletconnect) |
| Origins | A project can allowlist origins, and others are denied. Localhost is always allowed, and an empty list allows all. | [Reown relay docs](https://docs.reown.com/cloud/relay) |
| Relay | Self-hosting isn't available yet. | [Reown FAQ](https://docs.reown.com/advanced/faq) |

### Nostr

| Item | Finding | Source |
|---|---|---|
| Ephemeral gift wrap | Kind `21059` is in the NIP kind list under NIP-59. **UNVERIFIED:** relay support. | [NIP list](https://github.com/nostr-protocol/nips/blob/master/README.md) |
