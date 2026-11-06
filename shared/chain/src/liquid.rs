//! The Liquid side, with no I/O, so the browser client runs the same code: a wallet of blinded coins, reading coins, and building
//! spends. Coin lists come from [`crate::esplora`] natively, and from JavaScript in the browser.

use std::str::FromStr;

use simplicityhl::elements::confidential::{Asset, AssetBlindingFactor, Nonce, Value, ValueBlindingFactor};
use simplicityhl::elements::issuance::{AssetBlindingNonce, AssetEntropy, ContractHash};
use simplicityhl::elements::schnorr::{TapTweak, UntweakedKeypair};
use simplicityhl::elements::secp256k1_zkp::{Keypair, Message, PublicKey, Secp256k1, SecretKey};
use simplicityhl::elements::sighash::{Prevouts, SighashCache};
use simplicityhl::elements::{
    Address, AddressParams, AssetId, AssetIssuance, BlockHash, LockTime, OutPoint, SchnorrSighashType, Script,
    Sequence, Transaction, TxIn, TxInWitness, TxOut, TxOutSecrets, TxOutWitness, Witness,
};

/// The network this process works on, chosen once before any address is derived.
pub struct NetworkConfig {
    pub params: &'static AddressParams,
    pub usdt: AssetId,
    pub lbtc: AssetId,
}

static NETWORK: std::sync::OnceLock<NetworkConfig> = std::sync::OnceLock::new();

/// Use regtest assets instead of the Liquid testnet defaults. Only the test harness does this.
pub fn use_network(config: NetworkConfig) {
    let _ = NETWORK.set(config);
}

pub fn network() -> &'static NetworkConfig {
    NETWORK.get_or_init(|| NetworkConfig {
        params: &AddressParams::LIQUID_TESTNET,
        usdt: AssetId::from_str(TEST_ASSET).expect("constant"),
        lbtc: AssetId::from_str(TEST_LBTC).expect("constant"),
    })
}

pub const LIQUID_TESTNET_GENESIS: &str = "a771da8e52ee6ad581ed1e9a99825e5b3b7992225534eaa2ae23244fe26ab1c1";
/// Liquid mainnet. Nothing in this project may run against it.
pub const LIQUID_MAINNET_GENESIS: &str = "1466275836220db2944ca059a3a10ef6fd2ea684b0688d2c379296888a206003";
/// USDt on Liquid testnet, "PEGx USDt Testnet" in the registry, with 8 decimals. Not issued by Tether.
pub const TEST_ASSET: &str = "b612eb46313a2cd6ebabd8b7a8eed5696e29898b87a43bff41c94f51acef9d73";
/// L-BTC on Liquid testnet.
pub const TEST_LBTC: &str = "144c654344aa716d6f3abcc1ca90e5641e4e2a7f633bc09fe3baf64585819a49";
/// What a fee coin holds by default, all of it paid as the fee of a claim or refund. A maker can change it in its signer config.
pub const FEE: u64 = 30;

pub type Res<T> = Result<T, String>;

pub fn usdt() -> AssetId {
    network().usdt
}

pub fn lbtc() -> AssetId {
    network().lbtc
}

/// A coin on-chain, confirmed or not, blinded or not.
#[derive(Clone, Debug)]
pub struct Coin {
    pub outpoint: OutPoint,
    pub out: TxOut,
    pub height: Option<u32>,
}

fn input(previous_output: OutPoint, sequence: Sequence) -> TxIn {
    TxIn {
        previous_output,
        is_pegin: false,
        script_sig: Script::new(),
        sequence,
        asset_issuance: AssetIssuance::default(),
        witness: TxInWitness::default(),
    }
}

pub fn keypair_from(seed: &[u8; 32]) -> Res<Keypair> {
    Keypair::from_seckey_slice(&Secp256k1::new(), seed).map_err(|e| e.to_string())
}

pub fn random32() -> Res<[u8; 32]> {
    let mut b = [0u8; 32];
    getrandom::getrandom(&mut b).map_err(|e| e.to_string())?;
    Ok(b)
}

pub fn sign_keypath(tx: &mut Transaction, utxos: &[TxOut], index: usize, key: &Keypair, genesis: BlockHash) -> Res<()> {
    let secp = Secp256k1::new();
    let hash = SighashCache::new(&*tx)
        .taproot_key_spend_signature_hash(index, &Prevouts::All(utxos), SchnorrSighashType::All, genesis)
        .map_err(|e| e.to_string())?;
    let tweaked = UntweakedKeypair::tap_tweak(*key, &secp, None);
    let sig = secp.sign_schnorr_no_aux_rand(&Message::from_digest(hash.to_byte_array()), &tweaked.to_inner());
    let mut bytes = sig.as_ref().to_vec();
    bytes.push(SchnorrSighashType::All as u8);
    tx.input[index].witness.script_witness = Witness::from_slice(&[bytes]);
    Ok(())
}

/// A wallet holding only blinded coins. If it held explicit ones, anyone could read an input's value and so bound the
/// blinded outputs of any transaction spending it.
#[derive(Clone)]
pub struct Wallet {
    pub key: Keypair,
    pub blinding: SecretKey,
}

impl Wallet {
    pub fn blinder(&self) -> PublicKey {
        PublicKey::from_secret_key(&Secp256k1::new(), &self.blinding)
    }

    pub fn address(&self) -> Address {
        Address::p2tr(&Secp256k1::new(), self.key.x_only_public_key().0, None, Some(self.blinder()), network().params)
    }
}

/// A coin we can read: either explicit, or blinded to a key we hold.
#[derive(Clone, Debug)]
pub struct SecretCoin {
    pub coin: Coin,
    pub secrets: TxOutSecrets,
}

/// Read one output, or `None` if it isn't readable with this key. Anyone can send junk to any address, so that isn't an error.
pub fn read_output(out: &TxOut, blinding: &SecretKey) -> Option<TxOutSecrets> {
    match (out.asset, out.value) {
        (Asset::Explicit(asset), Value::Explicit(value)) => {
            Some(TxOutSecrets::new(asset, AssetBlindingFactor::zero(), value, ValueBlindingFactor::zero()))
        }
        _ => out.unblind(&Secp256k1::new(), *blinding).ok(),
    }
}

/// The coin of exactly this asset and amount, at the required depth. The chain no longer enforces this, so this check is
/// what stands between a party and a fake asset or a short payment.
pub fn exact_secret_coin(
    coins: &[SecretCoin],
    asset: AssetId,
    amount: u64,
    tip: u32,
    confs: u32,
) -> Option<SecretCoin> {
    coins
        .iter()
        .find(|c| {
            c.secrets.asset == asset && c.secrets.value == amount && c.coin.height.is_some_and(|h| tip + 1 >= h + confs)
        })
        .cloned()
}

/// One payment. A confidential address hides its asset and amount.
pub struct Payment {
    pub asset: AssetId,
    pub amount: u64,
    pub address: Address,
}

/// A fee of 0.2 sat per discount vbyte, twice the relay floor. Liquid and its testnet charge confidential transactions on
/// discount vsize, which leaves out most of the proofs' weight (`-acceptdiscountct`, on by default on those networks and off
/// on regtest). Measured on a node: 2 inputs and 4 blinded outputs are 487 dvB against 4,986 vB.
fn pay_fee(inputs: usize, blinded_outputs: usize) -> u64 {
    (60 + 60 * inputs as u64 + 80 * blinded_outputs as u64) / 5
}

/// A signed payment from these coins, which the caller chose, such as a wallet's coins without those promised to a swap. Every
/// output but the L-BTC fee is blinded.
pub fn pay_from_coins(
    coins: &[SecretCoin],
    wallet: &Wallet,
    payments: Vec<Payment>,
    genesis: BlockHash,
) -> Res<Transaction> {
    let address = wallet.address();
    let mut needed: Vec<(AssetId, u64)> = vec![(lbtc(), 0)];
    for p in &payments {
        match needed.iter_mut().find(|(a, _)| *a == p.asset) {
            Some((_, total)) => *total = total.checked_add(p.amount).ok_or("amount overflow")?,
            None => needed.push((p.asset, p.amount)),
        }
    }
    // Sized for change in every asset. L-BTC goes last, because each coin it takes raises the fee it has to cover.
    needed.rotate_left(1);
    let blinded = payments.len() + needed.len();
    let mut chosen: Vec<SecretCoin> = vec![];
    let mut changes: Vec<(AssetId, u64)> = vec![];
    let mut fee = 0;
    for (asset, need) in &needed {
        let mut total = 0u64;
        let mut candidates = coins.iter().filter(|c| c.secrets.asset == *asset);
        loop {
            fee = pay_fee(chosen.len(), blinded);
            let target = if *asset == lbtc() { need.checked_add(fee).ok_or("amount overflow")? } else { *need };
            if total >= target {
                if total > target {
                    changes.push((*asset, total - target));
                }
                break;
            }
            let Some(c) = candidates.next() else {
                return Err(format!("wallet {address} has {total} of {asset}, needs {target}"));
            };
            total = total.checked_add(c.secrets.value).ok_or("amount overflow")?;
            chosen.push(c.clone());
        }
    }

    let spent: Vec<TxOutSecrets> = chosen.iter().map(|c| c.secrets).collect();
    let utxos: Vec<TxOut> = chosen.iter().map(|c| c.coin.out.clone()).collect();
    let mut all: Vec<Payment> = payments;
    for (asset, amount) in changes {
        all.push(Payment { asset, amount, address: address.clone() });
    }

    // The last blinded output is what makes the blinding factors balance, so it is built separately.
    let secp = Secp256k1::new();
    let mut rng = rand::rngs::OsRng;
    let last = all.pop().ok_or("nothing to pay")?;
    let mut output = vec![];
    let mut out_secrets = vec![];
    for p in &all {
        let (out, abf, vbf, _) =
            TxOut::new_not_last_confidential(&mut rng, &secp, p.amount, &p.address, p.asset, &spent)
                .map_err(|e| e.to_string())?;
        output.push(out);
        out_secrets.push(TxOutSecrets::new(p.asset, abf, p.amount, vbf));
    }
    let refs: Vec<&TxOutSecrets> = out_secrets.iter().collect();
    let blinder = last.address.blinding_pubkey.ok_or("every payment here must go to a confidential address")?;
    let (out, _, _, _) = TxOut::new_last_confidential(
        &mut rng,
        &secp,
        last.amount,
        last.asset,
        last.address.script_pubkey(),
        blinder,
        &spent,
        &refs,
    )
    .map_err(|e| e.to_string())?;
    output.push(out);
    output.push(TxOut::new_fee(fee, lbtc()));

    let mut tx = Transaction {
        version: 2,
        lock_time: LockTime::ZERO,
        input: chosen.iter().map(|c| input(c.coin.outpoint, Sequence::MAX)).collect(),
        output,
    };
    for i in 0..tx.input.len() {
        sign_keypath(&mut tx, &utxos, i, &wallet.key, genesis)?;
    }
    Ok(tx)
}

/// An unsigned spend of a swap coin plus one L-BTC fee coin, spent whole, paying one blinded output.
pub fn build_swap_spend(
    coin: &SecretCoin,
    fee_coin: &SecretCoin,
    to: &Address,
    lock_time: u32,
    sequence: Sequence,
) -> Res<(Transaction, Vec<TxOut>)> {
    if fee_coin.secrets.asset != lbtc() {
        return Err("the fee coin isn't L-BTC".into());
    }
    let blinder = to.blinding_pubkey.ok_or("swap proceeds must go to a confidential address")?;
    let spent = [coin.secrets, fee_coin.secrets];
    let (paid, _, _, _) = TxOut::new_last_confidential(
        &mut rand::rngs::OsRng,
        &Secp256k1::new(),
        coin.secrets.value,
        coin.secrets.asset,
        to.script_pubkey(),
        blinder,
        &spent,
        &[],
    )
    .map_err(|e| e.to_string())?;
    let tx = Transaction {
        version: 2,
        lock_time: LockTime::from_consensus(lock_time),
        input: vec![input(coin.coin.outpoint, sequence), input(fee_coin.coin.outpoint, sequence)],
        output: vec![paid, TxOut::new_fee(fee_coin.secrets.value, lbtc())],
    };
    Ok((tx, vec![coin.coin.out.clone(), fee_coin.coin.out.clone()]))
}

/// A new asset, issued from one L-BTC coin, for a test network's stand-in. The issued coin is explicit, and the change goes back blinded.
pub fn build_issuance(
    coin: &SecretCoin,
    wallet: &Wallet,
    units: u64,
    genesis: BlockHash,
) -> Res<(Transaction, AssetId)> {
    const ISSUE_FEE: u64 = 200;
    let contract = ContractHash::from_byte_array([0; 32]);
    let asset = AssetId::new_issuance(coin.coin.outpoint, contract);
    let mut spend = input(coin.coin.outpoint, Sequence::MAX);
    spend.asset_issuance = AssetIssuance {
        asset_blinding_nonce: AssetBlindingNonce::from_byte_array([0; 32]),
        asset_entropy: AssetEntropy::from_byte_array([0; 32]),
        amount: Value::Explicit(units),
        inflation_keys: Value::Null,
    };
    let change_value = coin.secrets.value.checked_sub(ISSUE_FEE).ok_or("too little L-BTC")?;
    let address = wallet.address();
    // The library wants what an input issues counted among the inputs, and each explicit output among the outputs.
    let minted = TxOutSecrets::new(asset, AssetBlindingFactor::zero(), units, ValueBlindingFactor::zero());
    let (change, _, _, _) = TxOut::new_last_confidential(
        &mut rand::rngs::OsRng,
        &Secp256k1::new(),
        change_value,
        lbtc(),
        address.script_pubkey(),
        wallet.blinder(),
        &[coin.secrets, minted],
        &[&minted],
    )
    .map_err(|e| e.to_string())?;
    let issued = TxOut {
        asset: Asset::Explicit(asset),
        value: Value::Explicit(units),
        nonce: Nonce::Null,
        script_pubkey: address.script_pubkey(),
        witness: TxOutWitness::default(),
    };
    let mut tx = Transaction {
        version: 2,
        lock_time: LockTime::ZERO,
        input: vec![spend],
        output: vec![issued, change, TxOut::new_fee(ISSUE_FEE, lbtc())],
    };
    sign_keypath(&mut tx, std::slice::from_ref(&coin.coin.out), 0, &wallet.key, genesis)?;
    Ok((tx, asset))
}

/// How a swap coin was spent, from its witness. A key-path claim has one item, and a refund has the Simplicity leaf's four.
pub fn is_key_path_spend(tx: &Transaction, outpoint: &OutPoint) -> Option<bool> {
    let spend = tx.input.iter().find(|i| i.previous_output == *outpoint)?;
    Some(spend.witness.script_witness.len() == 1)
}

/// The signature from a key-path spend.
pub fn published_signature(tx: &Transaction, outpoint: &OutPoint) -> Option<[u8; 64]> {
    let spend = tx.input.iter().find(|i| i.previous_output == *outpoint)?;
    if spend.witness.script_witness.len() != 1 {
        return None;
    }
    spend.witness.script_witness[0].get(..64)?.try_into().ok()
}
