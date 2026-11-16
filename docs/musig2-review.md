# Review of `musig2` 0.4.1's adaptor code

Every swap's Liquid leg is a 2-of-2 MuSig2 output, claimed with an adapter signature made by the `musig2` crate (conduition, version 0.4.1 exactly). We found no outside audit of its adaptor code. This is a review by the project, done on 2026-11-16 for mainnet (user's decision). **It is not an independent audit**, which is why mainnet swaps are capped at 100 USDT.

## What was checked

| Check | Result |
|---|---|
| The published crate is the tagged source | `src/` from crates.io is identical to the `v0.4.1` tag of `github.com/conduition/musig2`, apart from test vectors, which the crate leaves out. |
| The crate's own tests, including the BIP327 and BIP340 vectors | All pass on both backends we use: `secp256k1` (libsecp256k1, the maker) and `k256` (the page). |
| Key aggregation and tweak | `SwapOutput::new` refuses unless the aggregated, tweaked key equals the taproot output key rust-elements derives by its own code. |
| Nonce generation | `SecNonce::build` follows BIP327's NonceGen: the secret key masked with hashed randomness, then the public key, aggregate key and message. We pass 32 bytes from the OS, the key, the aggregate and the message. |
| Nonce coefficient and final nonce | `b = H(R1, R2, Q, m)`, and `G` replaces a final nonce at infinity, as BIP327 says. |
| Adaptor signing | Read by hand. With `R' = R + T`: the challenge uses `R'`, and each nonce is negated when `R'` has odd y. The adaptor point is added after `b` is computed, as libsecp256k1-zkp's MuSig module does. |
| Adapting and extracting | Read by hand, for both parities: `adapt` gives `(R', s + t)` or `(-R', s - t)`, each valid under BIP340, and `reveal_secret` returns `t` in both. |
| Aggregation | Refuses unless `s·G = ±R + e·Q`. We also call `adaptor::verify_single` before relying on a signature. |

## Our own tests

`contracts/swapkey/tests/adaptor.rs` runs the whole flow and trusts nothing the crate says about its own output:

- A completed signature is accepted by libsecp256k1's BIP340 verifier, through rust-secp256k1, under the output key read from the script. It is accepted for the agreed message only.
- Before the secret, nothing verifies: not the stored value, and not a completion with another secret.
- The published signature gives the other party exactly the secret. Another session's signature, another point or a changed signature gives nothing.
- Both parities of the adapted nonce come up in every run, and the test fails if either doesn't.
- A share is refused for any other message, point, key or nonce. Shares that don't belong together don't combine.
- A key outside the swap can't sign, and a nonce is bound to its own key.

They run in CI on both backends (`unit` in `.github/workflows/suites.yml`). The regtest and daemon suites also have a real Elements node accept these signatures.

## How we use it

- A nonce signs once: `Session::sign` consumes the session, and no nonce is ever stored.
- The adaptor point is fixed by the agreed terms before any nonce exists, so neither party can choose it after seeing nonces.
- The other party's share is verified before ours is relied on, and the aggregate is verified before a lock.
- Partial signatures are not treated as proof of anything alone, which the crate warns about.

## What this leaves open

- **Side channels.** The page signs with `k256`, whose big-integer code isn't constant time (RUSTSEC-2026-0387). It signs with a key made for one swap, in the user's own browser. The maker uses libsecp256k1.
- **The `secp` crate** underneath, by the same author, wraps both backends. Its arithmetic is covered by the vectors above, and was not read line by line.
- **Protocol-level proofs.** MuSig2 with adaptors has published security arguments. This review checks that the code matches the construction, not the construction itself.
- A dependency update needs this review again. The version is pinned with `=0.4.1`.
