//! Nothing on-chain checks a swap coin's asset or amount, so reading the coin is the whole defence. A payer controls the range
//! proof's message, so this writes "USDt" into one over a worthless asset, as a dishonest taker would.

use liq2sol_chain::liquid::read_output;
use std::str::FromStr;

use simplicityhl::elements::confidential::{
    Asset, AssetBlindingFactor, Nonce, RangeProof, SurjectionProof, Value, ValueBlindingFactor,
};
use simplicityhl::elements::secp256k1_zkp::rand::rngs::OsRng;
use simplicityhl::elements::secp256k1_zkp::{Generator, PedersenCommitment, PublicKey, Secp256k1, SecretKey};
use simplicityhl::elements::{AssetId, RangeProofMessage, Script, TxOut, TxOutWitness};

struct Forged {
    out: TxOut,
    key: SecretKey,
}

/// A valid proof over the asset `real`, with a message claiming `claimed` and a blinding factor of `claimed_abf`.
fn forge(real: AssetId, real_abf: AssetBlindingFactor, claimed: AssetId, claimed_abf: AssetBlindingFactor) -> Forged {
    let secp = Secp256k1::new();
    let key = SecretKey::new(&mut OsRng);
    let blinder = PublicKey::from_secret_key(&secp, &key);
    let script = Script::new();
    let generator = Generator::new_blinded(&secp, real.into_tag(), real_abf.into_inner());
    let vbf = ValueBlindingFactor::new(&mut OsRng);
    let amount = 1_000_000;
    let commitment = PedersenCommitment::new(&secp, amount, vbf.into_inner(), generator);
    let (nonce, shared) = Nonce::with_ephemeral_sk(&secp, SecretKey::new(&mut OsRng), &blinder);
    let message = RangeProofMessage::new(claimed, claimed_abf).to_byte_array();
    let proof = simplicityhl::elements::secp256k1_zkp::RangeProof::new(
        &secp,
        1,
        commitment,
        amount,
        vbf.into_inner(),
        &message,
        script.as_bytes(),
        shared,
        0,
        52,
        generator,
    )
    .unwrap();
    let out = TxOut {
        asset: Asset::Confidential(generator),
        value: Value::Confidential(commitment),
        nonce,
        script_pubkey: script,
        witness: TxOutWitness {
            surjection_proof: SurjectionProof::EMPTY,
            rangeproof: RangeProof::from_slice(&proof.serialize()).unwrap(),
        },
    };
    Forged { out, key }
}

#[test]
fn a_coin_of_another_asset_labelled_usdt_is_not_read_as_usdt() {
    let usdt = AssetId::from_str(&"01".repeat(32)).unwrap();
    let worthless = AssetId::from_str(&"02".repeat(32)).unwrap();
    let abf = AssetBlindingFactor::new(&mut OsRng);

    // The control: the same construction, told the truth, reads back. So the refusal below is about the lie.
    let honest = forge(usdt, abf, usdt, abf);
    let read = read_output(&honest.out, &honest.key).expect("an honest output reads back");
    assert_eq!((read.asset, read.value), (usdt, 1_000_000));

    let lying = forge(worthless, abf, usdt, abf);
    assert!(read_output(&lying.out, &lying.key).is_none(), "the label doesn't match the commitment");

    let other_factor = forge(worthless, abf, usdt, AssetBlindingFactor::new(&mut OsRng));
    assert!(read_output(&other_factor.out, &other_factor.key).is_none());
}

#[test]
fn half_blinded_outputs_are_not_read() {
    let usdt = AssetId::from_str(&"01".repeat(32)).unwrap();
    let abf = AssetBlindingFactor::new(&mut OsRng);
    let f = forge(usdt, abf, usdt, abf);
    let mut explicit_value = f.out.clone();
    explicit_value.value = Value::Explicit(1_000_000);
    assert!(read_output(&explicit_value, &f.key).is_none());
    let mut explicit_asset = f.out.clone();
    explicit_asset.asset = Asset::Explicit(usdt);
    assert!(read_output(&explicit_asset, &f.key).is_none());
}
