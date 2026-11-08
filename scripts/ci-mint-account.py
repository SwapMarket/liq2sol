"""Write the stand-in USDT mint as an account file for `solana-test-validator --account`, so CI needs no mint keypair.

The mint's address is fixed by the program (`USDT_MINT`), and only its keypair could create it there. A test validator can
start with the account already in place instead. Its mint and freeze authority is a key generated in the CI job.

Usage: python3 -I scripts/ci-mint-account.py <authority keypair json> <output json>
"""
import base64
import json
import struct
import sys

MINT = "8BXimW8mCEfhdD1fhv9aoCfQgSj3KxFc52jMAbLvaFwi"
TOKEN_PROGRAM = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
# Rent-exempt minimum for an 82-byte account.
RENT = 1_461_600


def main():
    keypair, out = sys.argv[1], sys.argv[2]
    authority = bytes(json.load(open(keypair))[32:64])
    # SPL Token's Mint: COption<Pubkey> mint authority, supply, decimals, initialized, COption<Pubkey> freeze authority.
    data = struct.pack("<I32sQBBI32s", 1, authority, 0, 6, 1, 1, authority)
    assert len(data) == 82
    account = {
        "pubkey": MINT,
        "account": {
            "lamports": RENT,
            "data": [base64.b64encode(data).decode(), "base64"],
            "owner": TOKEN_PROGRAM,
            "executable": False,
            "rentEpoch": 0,
            "space": 82,
        },
    }
    json.dump(account, open(out, "w"))


main()
