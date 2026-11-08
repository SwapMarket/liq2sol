#!/usr/bin/env python3
"""Remove each confidential-swap rule in turn, and report whether a test notices. Run from the repo root, on a clean tree.

ELEMENTSD=... ELEMENTS_CLI=... SOLANA_RPC=... RUSTFLAGS="" python3 -I -u scripts/mutate-confidential.py [name filter]
"""
import subprocess
import sys

SIGNER = "maker/signer/src/lib.rs"
RFQ = "maker/daemon/src/rfq.rs"
POLICY = "maker/core/src/policy.rs"
ENGINE = "maker/core/src/engine.rs"
CHAIN = "shared/chain/src/liquid.rs"
SWAP = "shared/chain/src/swap.rs"

CORE = ["cargo", "test", "-p", "liq2sol-maker-core"]
PROCESS = ["cargo", "test", "-p", "liq2sol-signer", "--test", "process", "--", "--ignored", "--nocapture"]
def e2e(s):
    return ["env", f"SCENARIO={s}", "cargo", "test", "-p", "liq2sol-maker", "--test", "e2e", "--", "--ignored", "--nocapture"]

MUTATIONS = [
    ("fresh: pay the funding address", SIGNER, "        if !self.fresh_addresses {\n            return Ok(self.wallet.address());", "        if true {\n            return Ok(self.wallet.address());", e2e("RFQ L2S")),
    ("fresh: over a public API", SIGNER, "if cfg.fresh_addresses && !matches!(cfg.liquid, LiquidSource::Node { .. }) {", "if false {", PROCESS),
    ("close: an open escrow", SIGNER, "if escrow.state == EscrowState::Open {\n            return Err(\"the escrow is still open\".into());", "if false {\n            return Err(\"the escrow is still open\".into());", e2e("RFQ L2S")),
    ("close: straight after settling", SIGNER, "if self.sol.slot()? < escrow.settled_slot.saturating_add(self.close_after_slots) {", "if false {", e2e("RFQ L2S")),
    ("priority: sign what fails simulation", "shared/chain/src/sol.rs", "        if let Some(e) = sim.err {\n            return Err(format!(\"simulation failed: {e:?}\\n{:#?}\", sim.logs));\n        }\n        let mut accounts", "        let mut accounts", PROCESS),
    ("early refund: lock anyway", POLICY, "if record.early_refunded {\n                return Err(Refusal::EarlyRefunded);\n            }\n            if !record.adaptor_held {", "if !record.adaptor_held {", CORE),
    ("early refund: after signing a lock", POLICY, "if record.direction.is_some() {\n                return Err(Refusal::AlreadyLocked);\n            }\n            record.early_refunded = true;", "record.early_refunded = true;", CORE),
    ("early refund: forgets its promise", POLICY, "return Err(Refusal::AlreadyLocked);\n            }\n            record.early_refunded = true;", "return Err(Refusal::AlreadyLocked);\n            }", CORE),
    ("release: lock on Liquid anyway", POLICY, "if record.early_refunded {\n                return Err(Refusal::EarlyRefunded);\n            }\n            let amount = liquid_amount", "let amount = liquid_amount", CORE),
    ("release: forgets its promise", POLICY, "// And never lock on Liquid afterwards.\n            record.early_refunded = true;", "", CORE),
    ("release: after a Liquid lock", POLICY, "if record.direction.is_some() {\n                return Err(Refusal::ReleaseAfterLock);", "if false {\n                return Err(Refusal::ReleaseAfterLock);", CORE),
    ("early refund: any coin signed", SIGNER, "if !coins.iter().any(|c| Some(c.coin.outpoint) == first) {", "if false {", e2e("early refund")),
    ("early refund: abandoned can be undone", "maker/core/src/store.rs", "if current.as_deref() == Some(json(&State::Abandoned)?.as_str()) {", "if false {", CORE),
    ("lock without an adapter signature", POLICY, "if !record.adaptor_held {\n                return Err(Refusal::NoAdaptor);", "if false {\n                return Err(Refusal::NoAdaptor);", CORE),
    ("engine locks without a signature", ENGINE, "l2s_coin_acceptable(cfg, terms, v) && v.adaptor_held", "l2s_coin_acceptable(cfg, terms, v)", CORE),
    ("share after the escrow is gone", POLICY, "if !taker_escrow_open {", "if false {", CORE),
    ("second share allowed", POLICY, "if record.direction != Some(Direction::S2L) {\n                return Err(Refusal::UnknownSwap);\n            }\n            if record.adaptor_held {", "if record.direction != Some(Direction::S2L) {\n                return Err(Refusal::UnknownSwap);\n            }\n            if false {", CORE),
    ("wrong secret accepted", POLICY, "if !lock_point.opened_by(secret) {\n                return Err(Refusal::WrongSecret);\n            }\n        }\n        ClaimSolana", "if false {\n                return Err(Refusal::WrongSecret);\n            }\n        }\n        ClaimSolana", CORE),
    ("stranger joins a session", RFQ, "(details.taker_nostr == sender.to_hex() && details.negotiation == id", "(details.negotiation == id", e2e("RFQ S2L")),
    ("another negotiation id accepted", RFQ, "&& details.negotiation == id && details.terms", "&& details.terms", e2e("RFQ S2L")),
    ("session during a rollback alarm", RFQ, "if self.store.rollback().unwrap_or(true) {", "if false {", e2e("rollback")),
    ("fee coins used for payments", SIGNER, "if !self.is_fee_coin(&c) && !self.ledger", "if !self.ledger", PROCESS),
    ("signed coins used again", SIGNER, "if !self.is_fee_coin(&c) && !self.ledger.is_used(&outpoint_text(&c)).map_err(|e| e.to_string())? {", "if !self.is_fee_coin(&c) {", PROCESS),
    ("top-up not idempotent", SIGNER, "if !self.chain.is_confirmed(&tx.txid()).unwrap_or(false) {", "if false {", PROCESS),
    ("explicit coins taken as our own", SIGNER, "Ok(coins.into_iter().filter(|c| !c.coin.out.asset.is_explicit() && !c.coin.out.value.is_explicit()).collect())", "Ok(coins)", PROCESS),
    ("no confirmation check on a session", SIGNER, "if confirmations < needed {", "if false {", PROCESS),
    ("coin amount not checked", CHAIN, "c.secrets.asset == asset && c.secrets.value == amount && c", "c.secrets.asset == asset && c", PROCESS),
    ("coin asset not checked", CHAIN, "c.secrets.asset == asset && c.secrets.value == amount && c", "c.secrets.value == amount && c", PROCESS),
    ("claim shape not checked", SWAP, 'if tx.input[0].previous_output != *coin {', 'if false {', PROCESS),
    ("session terms may change", SIGNER, "if open.terms != *t {", "if false {", PROCESS),
]

filter_ = sys.argv[1] if len(sys.argv) > 1 else ""
results = {}
for name, path, old, new, test in MUTATIONS:
    if filter_ not in name:
        continue
    text = open(path).read()
    if text.count(old) != 1:
        print(f"NOT-APPLIED {name}: pattern found {text.count(old)} times", flush=True)
        results["NOT-APPLIED"] = results.get("NOT-APPLIED", 0) + 1
        continue
    open(path, "w").write(text.replace(old, new))
    try:
        built = subprocess.run(["cargo", "build"], cwd="maker", capture_output=True, text=True)
        if built.returncode != 0:
            outcome, why = "NO-BUILD", built.stderr[-300:]
        else:
            run = subprocess.run(test, cwd="maker", capture_output=True, text=True, timeout=1500)
            out = run.stdout + run.stderr
            if run.returncode == 0:
                outcome, why = "SURVIVED", ""
            elif "signer did not start" in out or "error: could not compile" in out:
                outcome, why = "NO-RESULT", out[-200:]
            else:
                fails = [l for l in out.splitlines() if "FAILED:" in l or "panicked" in l or l.startswith("FAIL ")]
                outcome, why = "CAUGHT", (fails[0] if fails else "")[:150]
    finally:
        open(path, "w").write(text)
    print(f"{outcome:9} {name}: {why}", flush=True)
    results[outcome] = results.get(outcome, 0) + 1
print(results)
subprocess.run(["cargo", "build"], cwd="maker", capture_output=True)
