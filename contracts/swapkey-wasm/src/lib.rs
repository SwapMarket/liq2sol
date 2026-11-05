//! Stage 1 proof: does the whole confidential claim work in a browser's WASM runtime? It takes a real funded coin from a node,
//! unblinds it, runs both parties' adapter signing, blinds the output with range and surjection proofs, and signs. The result
//! goes back to the node, which is the judge.

#![forbid(unsafe_code)]

use liq2sol_swapkey::*;
use simplicityhl::elements::confidential::{AssetBlindingFactor, ValueBlindingFactor};
use simplicityhl::elements::encode::{deserialize, serialize_hex};
use simplicityhl::elements::schnorr::{TapTweak, UntweakedKeypair};
use simplicityhl::elements::secp256k1_zkp::{Keypair, Message, PublicKey, Secp256k1, SecretKey};
use simplicityhl::elements::{
    Address, AddressParams, AssetId, AssetIssuance, BlockHash, LockTime, OutPoint, Script, Sequence, Transaction, TxIn,
    TxInWitness, TxOut, TxOutSecrets,
};
use std::str::FromStr;
use wasm_bindgen::prelude::wasm_bindgen;

fn unhex(s: &str) -> Result<Vec<u8>, String> {
    (0..s.len() / 2).map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(|e| e.to_string())).collect()
}

fn arr<const N: usize>(s: &str) -> Result<[u8; N], String> {
    unhex(s)?.try_into().map_err(|_| "wrong length".to_string())
}

fn input(previous_output: OutPoint) -> TxIn {
    TxIn {
        previous_output,
        is_pegin: false,
        script_sig: Script::new(),
        sequence: Sequence::MAX,
        asset_issuance: AssetIssuance::default(),
        witness: TxInWitness::default(),
    }
}

/// Random bytes from the browser, to prove randomness works there.
#[wasm_bindgen]
pub fn random_key() -> String {
    use rand::RngCore;
    let mut b = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The address for a swap, derived in the browser. The arguments are hex.
#[wasm_bindgen]
pub fn swap_address(maker_sk: &str, taker_sk: &str, blinding_sk: &str, lock_height: u32) -> Result<String, String> {
    let (output, blinder) = output_for(maker_sk, taker_sk, blinding_sk, lock_height)?;
    Ok(output.address(blinder, &AddressParams::ELEMENTS).to_string())
}

fn output_for(
    maker_sk: &str,
    taker_sk: &str,
    blinding_sk: &str,
    lock_height: u32,
) -> Result<(SwapOutput, PublicKey), String> {
    let secp = Secp256k1::new();
    let key = |h: &str| -> Result<PublicKey, String> {
        Ok(PublicKey::from_secret_key(&secp, &SecretKey::from_slice(&arr::<32>(h)?).map_err(|e| e.to_string())?))
    };
    let (maker, taker) = (key(maker_sk)?, key(taker_sk)?);
    let mut refund_pk = [0u8; 32];
    refund_pk.copy_from_slice(&taker.serialize()[1..]);
    let params = SwapParams { maker_key: maker.serialize(), taker_key: taker.serialize(), refund_pk, lock_height };
    Ok((SwapOutput::new(&params).map_err(|e| e.to_string())?, key(blinding_sk)?))
}

/// The plain key-path address that holds the test fee coin.
#[wasm_bindgen]
pub fn fee_address(fee_sk: &str) -> Result<String, String> {
    let secp = Secp256k1::new();
    let key = Keypair::from_seckey_slice(&secp, &arr::<32>(fee_sk)?).map_err(|e| e.to_string())?;
    Ok(Address::p2tr(&secp, key.x_only_public_key().0, None, None, &AddressParams::ELEMENTS).to_string())
}

/// A complete, signed confidential claim. Returns the transaction, then the secret the signature gave up, then timings.
#[allow(clippy::too_many_arguments)]
#[wasm_bindgen]
pub fn build_claim(
    maker_sk: &str,
    taker_sk: &str,
    blinding_sk: &str,
    lock_height: u32,
    coin_tx: &str,
    fee_tx: &str,
    fee_sk: &str,
    genesis: &str,
    lbtc: &str,
    t: &str,
) -> Result<String, String> {
    let secp = Secp256k1::new();
    let e = |e: Error| e.to_string();
    let (output, _) = output_for(maker_sk, taker_sk, blinding_sk, lock_height)?;
    let (maker, taker, t): ([u8; 32], [u8; 32], [u8; 32]) = (arr(maker_sk)?, arr(taker_sk)?, arr(t)?);
    let blinding = SecretKey::from_slice(&arr::<32>(blinding_sk)?).map_err(|e| e.to_string())?;
    let fee_key = Keypair::from_seckey_slice(&secp, &arr::<32>(fee_sk)?).map_err(|e| e.to_string())?;
    let genesis = BlockHash::from_str(genesis).map_err(|e| e.to_string())?;
    let lbtc = AssetId::from_str(lbtc).map_err(|e| e.to_string())?;

    // Find the coin, and read it with the shared blinding key.
    let coin_tx: Transaction = deserialize(&unhex(coin_tx)?).map_err(|e| e.to_string())?;
    let spk = output.script_pubkey();
    let (vout, coin) = coin_tx.output.iter().enumerate().find(|(_, o)| o.script_pubkey == spk).ok_or("no coin")?;
    let secrets = coin.unblind(&secp, blinding).map_err(|e| e.to_string())?;
    let fee_tx: Transaction = deserialize(&unhex(fee_tx)?).map_err(|e| e.to_string())?;
    let fee_spk =
        Address::p2tr(&secp, fee_key.x_only_public_key().0, None, None, &AddressParams::ELEMENTS).script_pubkey();
    let (fee_vout, fee) =
        fee_tx.output.iter().enumerate().find(|(_, o)| o.script_pubkey == fee_spk).ok_or("no fee coin")?;
    let fee_value = fee.value.explicit().ok_or("blinded fee coin")?;

    // A blinded output, with its range and surjection proofs.
    let to = Keypair::from_seckey_slice(&secp, &arr::<32>(fee_sk)?).map_err(|e| e.to_string())?;
    let to_blinder = PublicKey::from_secret_key(&secp, &blinding);
    let to_script = Address::p2tr(&secp, to.x_only_public_key().0, None, Some(to_blinder), &AddressParams::ELEMENTS)
        .script_pubkey();
    let spent = [secrets, TxOutSecrets::new(lbtc, AssetBlindingFactor::zero(), fee_value, ValueBlindingFactor::zero())];
    let (paid, _, _, _) = TxOut::new_last_confidential(
        &mut rand::rngs::OsRng,
        &secp,
        secrets.value,
        secrets.asset,
        to_script,
        to_blinder,
        &spent,
        &[],
    )
    .map_err(|e| e.to_string())?;
    let mut tx = Transaction {
        version: 2,
        lock_time: LockTime::ZERO,
        input: vec![
            input(OutPoint::new(coin_tx.txid(), vout as u32)),
            input(OutPoint::new(fee_tx.txid(), fee_vout as u32)),
        ],
        output: vec![paid, TxOut::new_fee(fee_value, lbtc)],
    };
    let utxos = vec![coin.clone(), fee.clone()];

    // Both parties' adapter signing.
    let message = key_spend_sighash(&tx, &utxos, 0, genesis).map_err(e)?;
    let big_t = point_of(&t).map_err(e)?;
    let ctx = output.key_ctx();
    let (a, b) = (Session::start(&maker, ctx, &message).map_err(e)?, Session::start(&taker, ctx, &message).map_err(e)?);
    let (na, nb) = (a.public.clone(), b.public.clone());
    let (sa, agg) = a.sign(&maker, ctx, &nb, &big_t, &message).map_err(e)?;
    let (sb, _) = b.sign(&taker, ctx, &na, &big_t, &message).map_err(e)?;
    let adaptor = combine(ctx, &agg, [sa, sb], &big_t, &message).map_err(e)?;
    let finished = complete(&adaptor, &t).map_err(e)?;
    set_key_spend_witness(&mut tx, 0, &finished);

    let fee_digest = key_spend_sighash(&tx, &utxos, 1, genesis).map_err(e)?;
    let tweaked = UntweakedKeypair::tap_tweak(fee_key, &secp, None);
    let sig = secp.sign_schnorr_no_aux_rand(&Message::from_digest(fee_digest), &tweaked.to_inner());
    set_key_spend_witness(&mut tx, 1, sig.as_ref());

    let recovered = extract_secret(&adaptor, &finished, &big_t).ok_or("the secret didn't come out")?;
    let recovered: String = recovered.iter().map(|x| format!("{x:02x}")).collect();
    Ok(format!("{}\n{recovered}", serialize_hex(&tx)))
}
