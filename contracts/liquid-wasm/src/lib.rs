//! Feasibility proof F2: build and sign a full Liquid spend in the browser's WASM runtime.
//!
//! `parity_vector` builds a fixed claim from fixed test keys, with a Simplicity input and a taproot key-path L-BTC
//! fee input, and returns it as text. `scripts/check-wasm-parity.sh` compares the WASM result with the native one.
//! The keys are constants for testing only and never hold funds.

#![forbid(unsafe_code)]

use liq2sol_htlc::{asset_param, script_hash, Htlc, HtlcParams, Spend};
use simplicityhl::elements::confidential::{Asset, Nonce, Value};
use simplicityhl::elements::encode::serialize_hex;
use simplicityhl::elements::hashes::sha256;
use simplicityhl::elements::schnorr::{TapTweak, UntweakedKeypair};
use simplicityhl::elements::secp256k1_zkp::{Keypair, Message, Secp256k1};
use simplicityhl::elements::sighash::{Prevouts, SighashCache};
use simplicityhl::elements::{
    Address, AddressParams, AssetId, AssetIssuance, BlockHash, LockTime, OutPoint,
    SchnorrSighashType, Script, Sequence, Transaction, TxIn, TxInWitness, TxOut, TxOutWitness,
    Txid, Witness,
};
use wasm_bindgen::prelude::wasm_bindgen;

const AMOUNT: u64 = 10_000_000_000;
const FEE: u64 = 300;

fn txout(asset: AssetId, amount: u64, script_pubkey: Script) -> TxOut {
    TxOut {
        asset: Asset::Explicit(asset),
        value: Value::Explicit(amount),
        nonce: Nonce::Null,
        script_pubkey,
        witness: TxOutWitness::default(),
    }
}

fn input(n: u8) -> TxIn {
    TxIn {
        previous_output: OutPoint::new(Txid::from_byte_array([n; 32]), 0),
        is_pegin: false,
        script_sig: Script::new(),
        sequence: Sequence::MAX,
        asset_issuance: AssetIssuance::default(),
        witness: TxInWitness::default(),
    }
}

/// The address, CMR and fully signed claim for fixed inputs.
#[wasm_bindgen]
pub fn parity_vector() -> String {
    let secp = Secp256k1::new();
    let keypair =
        |seed: u8| Keypair::from_seckey_slice(&secp, &[seed; 32]).expect("valid test key");
    let (recipient, sender, fee_key) = (keypair(1), keypair(2), keypair(3));
    let usdt = AssetId::from_byte_array([0xce; 32]);
    let lbtc = AssetId::from_byte_array([0x6f; 32]);
    let genesis = BlockHash::from_byte_array([7; 32]);
    let params_net = &AddressParams::LIQUID_TESTNET;

    let preimage = [0xa5; 32];
    let recipient_script = Address::p2tr(
        &secp,
        keypair(4).x_only_public_key().0,
        None,
        None,
        params_net,
    )
    .script_pubkey();
    let params = HtlcParams {
        payment_hash: sha256::Hash::hash(&preimage).to_byte_array(),
        recipient_pk: recipient.x_only_public_key().0.serialize(),
        sender_pk: sender.x_only_public_key().0.serialize(),
        lock_height: 1000,
        usdt_asset: asset_param(&usdt),
        amount: AMOUNT,
        recipient_script_hash: script_hash(&recipient_script),
    };
    let htlc = Htlc::new(&params).expect("template compiles");
    let fee_script =
        Address::p2tr(&secp, fee_key.x_only_public_key().0, None, None, params_net).script_pubkey();

    let utxos = vec![
        txout(usdt, AMOUNT, htlc.script_pubkey()),
        txout(lbtc, FEE, fee_script),
    ];
    let tx = Transaction {
        version: 2,
        lock_time: LockTime::ZERO,
        input: vec![input(1), input(2)],
        output: vec![
            txout(usdt, AMOUNT, recipient_script),
            TxOut::new_fee(FEE, lbtc),
        ],
    };

    let digest = htlc
        .sighash_all(&tx, &utxos, 0, genesis)
        .expect("input pays the HTLC");
    let signature = *secp
        .sign_schnorr_no_aux_rand(&Message::from_digest(digest), &recipient)
        .as_ref();
    let (mut tx, report) = htlc
        .finalize(
            tx,
            &utxos,
            0,
            genesis,
            Spend::Claim {
                preimage,
                signature,
            },
        )
        .expect("valid claim");

    let hash = SighashCache::new(&tx)
        .taproot_key_spend_signature_hash(
            1,
            &Prevouts::All(&utxos),
            SchnorrSighashType::All,
            genesis,
        )
        .expect("valid fee input");
    let tweaked = UntweakedKeypair::tap_tweak(fee_key, &secp, None);
    let sig = secp.sign_schnorr_no_aux_rand(
        &Message::from_digest(hash.to_byte_array()),
        &tweaked.to_inner(),
    );
    let mut fee_sig = sig.as_ref().to_vec();
    fee_sig.push(SchnorrSighashType::All as u8);
    tx.input[1].witness.script_witness = Witness::from_slice(&[fee_sig]);

    let cmr: String = htlc.cmr().iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "cmr {cmr}\naddress {}\nprogram_bytes {}\ntx {}",
        htlc.address(params_net),
        report.program_bytes,
        serialize_hex(&tx)
    )
}

#[cfg(test)]
mod tests {
    /// Fails if the contract or the compiler output changes, since that changes every address.
    #[test]
    fn program_commitment_is_pinned() {
        let vector = super::parity_vector();
        assert!(
            vector.starts_with(
                "cmr 32d7f610a4b71805b04dd4af62fde841b31934b2950b1b6ec5e06f88d35e60bb\n"
            ),
            "{vector}"
        );
    }
}
