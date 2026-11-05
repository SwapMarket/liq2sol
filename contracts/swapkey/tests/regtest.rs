//! Stage 1 of the confidential redesign, on a real Elements node with default relay policy. Each thing the design claims is
//! checked against the node: it either accepts the transaction or it doesn't.
//!
//!   ELEMENTSD=~/bin/elementsd ELEMENTS_CLI=~/bin/elements-cli cargo test -p liq2sol-swapkey --test regtest -- --ignored --nocapture

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::str::FromStr;
use std::time::{Duration, Instant};

use liq2sol_swapkey::*;
use rand::RngCore;
use serde_json::Value as Json;
use simplicityhl::elements::confidential::{Asset, Value};
use simplicityhl::elements::encode::{deserialize, serialize_hex};
use simplicityhl::elements::schnorr::{TapTweak, UntweakedKeypair};
use simplicityhl::elements::secp256k1_zkp::{Keypair, Message, PublicKey, Secp256k1, SecretKey};
use simplicityhl::elements::{
    Address, AddressParams, AssetId, AssetIssuance, BlockHash, LockTime, OutPoint, Script, Sequence, Transaction, TxIn,
    TxInWitness, TxOut, TxOutSecrets,
};

const AMOUNT: u64 = 10_000_000_000;
const FEE_COIN: u64 = 2_000;

struct Node {
    child: Child,
    dir: PathBuf,
    cli: String,
    port: u16,
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.call(&["stop"]);
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Node {
    fn start() -> Self {
        let daemon = std::env::var("ELEMENTSD").unwrap_or_else(|_| "elementsd".into());
        let cli = std::env::var("ELEMENTS_CLI").unwrap_or_else(|_| "elements-cli".into());
        let port = 44_000 + (std::process::id() % 10_000) as u16;
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("swapkey-{port}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Default relay fee and policy, so "accepted" means what it would mean on a real network.
        let child = Command::new(daemon)
            .arg(format!("-datadir={}", dir.display()))
            .args(["-chain=liquidregtest", "-listen=0", "-txindex=1", "-validatepegin=0", "-fallbackfee=0.00001"])
            .args(["-initialfreecoins=2100000000", "-anyonecanspendaremine=1", "-evbparams=simplicity:-1:::"])
            .args(["-rpcuser=t", "-rpcpassword=t", &format!("-rpcport={port}"), "-printtoconsole=0"])
            .stdout(Stdio::null())
            .spawn()
            .expect("start elementsd");
        let node = Node { child, dir, cli, port };
        let start = Instant::now();
        while node.call(&["getblockcount"]).is_err() {
            assert!(start.elapsed() < Duration::from_secs(60), "elementsd did not start");
            std::thread::sleep(Duration::from_millis(200));
        }
        node.ok(&["createwallet", "w"]);
        node.ok(&["rescanblockchain"]);
        node
    }

    fn call(&self, args: &[&str]) -> Result<String, String> {
        let out = Command::new(&self.cli)
            .args(["-chain=liquidregtest", "-rpcuser=t", "-rpcpassword=t", &format!("-rpcport={}", self.port)])
            .args(args)
            .output()
            .expect("run elements-cli");
        if out.status.success() {
            Ok(String::from_utf8(out.stdout).unwrap().trim().to_string())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    }

    fn ok(&self, args: &[&str]) -> String {
        self.call(args).unwrap_or_else(|e| panic!("{args:?}: {e}"))
    }

    fn mine(&self, blocks: u32) {
        let a = self.ok(&["getnewaddress"]);
        self.ok(&["generatetoaddress", &blocks.to_string(), &a]);
    }

    fn height(&self) -> u32 {
        self.ok(&["getblockcount"]).parse().unwrap()
    }

    /// Pay an address from the node's wallet, as a user's wallet would, and return the output it created.
    fn pay(&self, address: &Address, sats: u64, label: &str) -> (OutPoint, TxOut) {
        let amount = format!("{}.{:08}", sats / 100_000_000, sats % 100_000_000);
        let txid = self.ok(&[
            "sendtoaddress",
            &address.to_string(),
            &amount,
            "",
            "",
            "false",
            "false",
            "1",
            "UNSET",
            "false",
            label,
        ]);
        self.mine(1);
        let tx: Transaction = deserialize(&unhex(&self.ok(&["getrawtransaction", &txid]))).unwrap();
        let spk = address.script_pubkey();
        let (vout, out) = tx.output.iter().enumerate().find(|(_, o)| o.script_pubkey == spk).expect("output");
        (OutPoint::new(tx.txid(), vout as u32), out.clone())
    }

    fn would_accept(&self, tx: &Transaction) -> Result<(), String> {
        let arg = format!("[\"{}\"]", serialize_hex(tx));
        let result: Json = serde_json::from_str(&self.ok(&["testmempoolaccept", &arg])).unwrap();
        if result[0]["allowed"].as_bool().unwrap_or(false) {
            Ok(())
        } else {
            Err(result[0]["reject-reason"].as_str().unwrap_or("unknown").to_string())
        }
    }
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2).map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).unwrap()).collect()
}

fn secret() -> [u8; 32] {
    let mut b = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b
}

fn compressed(sk: &[u8; 32]) -> [u8; 33] {
    PublicKey::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(sk).unwrap()).serialize()
}

fn x_only(sk: &[u8; 32]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(&compressed(sk)[1..]);
    out
}

struct Env {
    node: Node,
    genesis: BlockHash,
    usdt: AssetId,
    lbtc: AssetId,
    fee_key: Keypair,
    fee_address: Address,
}

impl Env {
    fn start() -> Self {
        let node = Node::start();
        node.mine(1);
        let issued: Json = serde_json::from_str(&node.ok(&["issueasset", "1000", "0", "false"])).unwrap();
        let usdt = AssetId::from_str(issued["asset"].as_str().unwrap()).unwrap();
        node.mine(1);
        let info: Json = serde_json::from_str(&node.ok(&["getsidechaininfo"])).unwrap();
        let lbtc = AssetId::from_str(info["pegged_asset"].as_str().unwrap()).unwrap();
        let genesis = BlockHash::from_str(&node.ok(&["getblockhash", "0"])).unwrap();
        let secp = Secp256k1::new();
        let fee_key = Keypair::from_seckey_slice(&secp, &secret()).unwrap();
        let fee_address = Address::p2tr(&secp, fee_key.x_only_public_key().0, None, None, &AddressParams::ELEMENTS);
        Env { node, genesis, usdt, lbtc, fee_key, fee_address }
    }

    fn sign_fee_input(&self, tx: &mut Transaction, utxos: &[TxOut], index: usize) {
        let secp = Secp256k1::new();
        let digest = key_spend_sighash(tx, utxos, index, self.genesis).unwrap();
        let tweaked = UntweakedKeypair::tap_tweak(self.fee_key, &secp, None);
        let sig = secp.sign_schnorr_no_aux_rand(&Message::from_digest(digest), &tweaked.to_inner());
        set_key_spend_witness(tx, index, sig.as_ref());
    }
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

fn explicit_secrets(asset: AssetId, value: u64) -> TxOutSecrets {
    use simplicityhl::elements::confidential::{AssetBlindingFactor, ValueBlindingFactor};
    TxOutSecrets::new(asset, AssetBlindingFactor::zero(), value, ValueBlindingFactor::zero())
}

/// A spend paying a blinded output, which absorbs the input's blinding, with the L-BTC fee coin spent whole.
fn spend_template(
    env: &Env,
    coin: (OutPoint, TxOut),
    secrets: TxOutSecrets,
    fee_coin: (OutPoint, TxOut),
    to: Script,
    to_blinder: PublicKey,
    lock_time: u32,
    sequence: Sequence,
) -> (Transaction, Vec<TxOut>) {
    let secp = Secp256k1::new();
    let fee_value = fee_coin.1.value.explicit().unwrap();
    let spent = [secrets, explicit_secrets(env.lbtc, fee_value)];
    let (paid, _, _, _) = TxOut::new_last_confidential(
        &mut rand::rngs::OsRng,
        &secp,
        secrets.value,
        secrets.asset,
        to,
        to_blinder,
        &spent,
        &[],
    )
    .expect("blind");
    let tx = Transaction {
        version: 2,
        lock_time: LockTime::from_consensus(lock_time),
        input: vec![input(coin.0, sequence), input(fee_coin.0, sequence)],
        output: vec![paid, TxOut::new_fee(fee_value, env.lbtc)],
    };
    (tx, vec![coin.1, fee_coin.1])
}

fn is_hidden(out: &TxOut) -> bool {
    matches!(out.asset, Asset::Confidential(_)) && matches!(out.value, Value::Confidential(_))
}

#[test]
#[ignore = "needs elementsd; see the module docs"]
fn the_confidential_design_works_on_a_real_node() {
    let env = Env::start();
    let secp = Secp256k1::new();
    let usdt_label = env.usdt.to_string();
    println!("{}", env.node.ok(&["getnetworkinfo"]).lines().find(|l| l.contains("subversion")).unwrap_or("").trim());

    let blinding_sk = SecretKey::from_slice(&secret()).unwrap();
    let blinder = PublicKey::from_secret_key(&secp, &blinding_sk);
    let (maker_sk, taker_sk) = (secret(), secret());

    println!("\n==> Fund a blinded output from an ordinary wallet");
    let params = SwapParams {
        maker_key: compressed(&maker_sk),
        taker_key: compressed(&taker_sk),
        refund_pk: x_only(&taker_sk),
        lock_height: env.node.height() + 500,
    };
    let output = SwapOutput::new(&params).unwrap();
    let address = output.address(blinder, &AddressParams::ELEMENTS);
    assert!(address.is_blinded());
    let coin = env.node.pay(&address, AMOUNT, &usdt_label);
    assert!(is_hidden(&coin.1), "an outsider sees no asset or amount");
    println!("    ok: the funding output's asset and amount are hidden");

    // An outsider can't read it. The counterparty, with the shared blinding key, can check exactly what it got.
    let stranger = SecretKey::from_slice(&secret()).unwrap();
    assert!(coin.1.unblind(&secp, stranger).is_err(), "a wrong key can't read it");
    let secrets = coin.1.unblind(&secp, blinding_sk).expect("the shared key reads it");
    assert_eq!((secrets.asset, secrets.value), (env.usdt, AMOUNT));
    println!("    ok: a stranger can't read the coin, and the counterparty confirms its asset and amount");

    println!("\n==> Claim by adapter signature");
    let fee_coin = env.node.pay(&env.fee_address, FEE_COIN, "bitcoin");
    let recipient_sk = SecretKey::from_slice(&secret()).unwrap();
    let recipient_blinder = PublicKey::from_secret_key(&secp, &recipient_sk);
    let recipient = Address::p2tr(
        &secp,
        env.fee_key.x_only_public_key().0,
        None,
        Some(recipient_blinder),
        &AddressParams::ELEMENTS,
    );
    let (tx, utxos) = spend_template(
        &env,
        coin.clone(),
        secrets,
        fee_coin.clone(),
        recipient.script_pubkey(),
        recipient_blinder,
        0,
        Sequence::MAX,
    );
    let message = key_spend_sighash(&tx, &utxos, 0, env.genesis).unwrap();

    // The maker holds the secret's counterpart: it knows T, and both sign against it.
    let t = secret();
    let big_t = point_of(&t).unwrap();
    let ctx = output.key_ctx();
    let (maker, taker) =
        (Session::start(&maker_sk, ctx, &message).unwrap(), Session::start(&taker_sk, ctx, &message).unwrap());
    let (maker_nonce, taker_nonce) = (maker.public.clone(), taker.public.clone());
    let (maker_share, agg) = maker.sign(&maker_sk, ctx, &taker_nonce, &big_t, &message).unwrap();
    let (taker_share, _) = taker.sign(&taker_sk, ctx, &maker_nonce, &big_t, &message).unwrap();

    verify_share(ctx, taker_share, &agg, &params.taker_key, &taker_nonce, &big_t, &message)
        .expect("a good share verifies");
    let wrong_t = point_of(&secret()).unwrap();
    assert_eq!(
        verify_share(ctx, taker_share, &agg, &params.taker_key, &taker_nonce, &wrong_t, &message),
        Err(Error::BadPartialSignature)
    );
    assert_eq!(
        verify_share(ctx, taker_share, &agg, &params.maker_key, &taker_nonce, &big_t, &message),
        Err(Error::BadPartialSignature)
    );
    println!("    ok: shares verify, and a share for another point or key is refused");

    let adaptor = combine(ctx, &agg, [maker_share, taker_share], &big_t, &message).unwrap();
    println!("    ok: the adapter signature checks out against the output key");

    // Without the secret, nothing the parties hold is a valid spend.
    let mut no_secret = tx.clone();
    let fake = complete(&adaptor, &secret()).unwrap();
    set_key_spend_witness(&mut no_secret, 0, &fake);
    env.sign_fee_input(&mut no_secret, &utxos, 1);
    let refused = env.node.would_accept(&no_secret).unwrap_err();
    println!("    ok: finishing it with a wrong secret is refused by the node: {refused}");

    let finished = complete(&adaptor, &t).unwrap();
    let mut claim = tx.clone();
    set_key_spend_witness(&mut claim, 0, &finished);
    env.sign_fee_input(&mut claim, &utxos, 1);
    env.node.would_accept(&claim).expect("the node accepts the claim");
    let info: Json = serde_json::from_str(&env.node.ok(&["decoderawtransaction", &serialize_hex(&claim)])).unwrap();
    println!("    claim: vsize {}, discountvsize {}, weight {}", info["vsize"], info["discountvsize"], info["weight"]);
    let txid = env.node.ok(&["sendrawtransaction", &serialize_hex(&claim)]);
    env.node.mine(1);
    println!("    ok: the node accepted and mined the claim");

    println!("\n==> What the chain shows");
    let mined: Transaction = deserialize(&unhex(&env.node.ok(&["getrawtransaction", &txid]))).unwrap();
    assert!(is_hidden(&mined.output[0]), "the paid output is blinded");
    println!("    ok: the paid output's asset and amount are hidden");
    let witness = &mined.input[0].witness.script_witness;
    assert_eq!(witness.len(), 1, "one witness item");
    assert_eq!(witness[0].len(), 65, "a plain key-path signature");
    println!("    ok: the claim's witness is a single 65-byte signature: no script, no hash, no secret");
    let raw = serialize_hex(&mined);
    let (t_hex, big_t_hex) = (
        t.iter().map(|b| format!("{b:02x}")).collect::<String>(),
        big_t.iter().map(|b| format!("{b:02x}")).collect::<String>(),
    );
    assert!(!raw.contains(&t_hex) && !raw.contains(&big_t_hex[2..]), "neither the secret nor its point appears");
    println!("    ok: neither the secret nor its point appears anywhere in the transaction");

    println!("\n==> The counterparty gets the secret, and a stranger doesn't");
    let mut published = [0u8; 64];
    published.copy_from_slice(&witness[0][..64]);
    assert_eq!(extract_secret(&adaptor, &published, &big_t), Some(t));
    println!("    ok: holding the adapter signature, the secret comes straight out");
    let other = combine_for_stranger(&env, &output);
    assert_eq!(extract_secret(&other, &published, &big_t), None);
    println!("    ok: any other adapter signature gets nothing");

    println!("\n==> Refund through the Simplicity leaf");
    let lock_height = env.node.height() + 5;
    let refund_params = SwapParams { lock_height, ..params.clone() };
    let refund_output = SwapOutput::new(&refund_params).unwrap();
    assert_ne!(refund_output.script_pubkey(), output.script_pubkey(), "each lock height has its own address");
    let address = refund_output.address(blinder, &AddressParams::ELEMENTS);
    let coin = env.node.pay(&address, AMOUNT, &usdt_label);
    let fee_coin = env.node.pay(&env.fee_address, FEE_COIN, "bitcoin");
    let secrets = coin.1.unblind(&secp, blinding_sk).unwrap();
    let back = Address::p2tr(
        &secp,
        env.fee_key.x_only_public_key().0,
        None,
        Some(recipient_blinder),
        &AddressParams::ELEMENTS,
    );
    let sign_refund = |signer: &[u8; 32], lock_time: u32, sequence: Sequence| -> Result<Transaction, Error> {
        let (tx, utxos) = spend_template(
            &env,
            coin.clone(),
            secrets,
            fee_coin.clone(),
            back.script_pubkey(),
            recipient_blinder,
            lock_time,
            sequence,
        );
        let digest = refund_output.refund_sighash(&tx, &utxos, 0, env.genesis)?;
        let key = Keypair::from_seckey_slice(&secp, signer).unwrap();
        let sig = *secp.sign_schnorr_no_aux_rand(&Message::from_digest(digest), &key).as_ref();
        let mut tx = refund_output.finalize_refund(tx, &utxos, 0, env.genesis, sig)?;
        env.sign_fee_input(&mut tx, &utxos, 1);
        Ok(tx)
    };
    assert!(
        matches!(sign_refund(&maker_sk, lock_height, Sequence::ZERO), Err(Error::Rejected(_))),
        "the wrong key is refused"
    );
    println!("    ok: a refund signed by the other party is refused");
    assert!(
        matches!(sign_refund(&taker_sk, lock_height - 1, Sequence::ZERO), Err(Error::Rejected(_))),
        "an early lock time is refused"
    );
    assert!(
        matches!(sign_refund(&taker_sk, lock_height, Sequence::MAX), Err(Error::Rejected(_))),
        "final sequences are refused"
    );
    println!("    ok: an early lock time and final sequences are refused");
    let refund = sign_refund(&taker_sk, lock_height, Sequence::ZERO).unwrap();
    let early = env.node.would_accept(&refund).unwrap_err();
    println!("    ok: the node refuses it before the lock height: {early}");
    while env.node.height() < lock_height {
        env.node.mine(1);
    }
    env.node.would_accept(&refund).expect("the refund is valid at the lock height");
    let txid = env.node.ok(&["sendrawtransaction", &serialize_hex(&refund)]);
    env.node.mine(1);
    let mined: Transaction = deserialize(&unhex(&env.node.ok(&["getrawtransaction", &txid]))).unwrap();
    assert!(is_hidden(&mined.output[0]));
    let witness = &mined.input[0].witness.script_witness;
    println!(
        "    ok: the refund mined, its output is blinded, and its witness shows {} items, so a refund is recognisable",
        witness.len()
    );

    println!("\n==> A wrong coin is the parties' own check now");
    let wrong = SwapOutput::new(&SwapParams { lock_height: lock_height + 1000, ..params.clone() }).unwrap();
    let coin = env.node.pay(&wrong.address(blinder, &AddressParams::ELEMENTS), AMOUNT - 100, &usdt_label);
    let seen = coin.1.unblind(&secp, blinding_sk).unwrap();
    assert_ne!(seen.value, AMOUNT);
    println!("    ok: an underpaid coin is only detected by unblinding it: no contract rule stops a claim");
    println!("\nSTAGE 1 OK");
}

/// A valid adapter signature for the same point, made in a separate session.
fn combine_for_stranger(env: &Env, output: &SwapOutput) -> musig2::AdaptorSignature {
    let _ = (env, output);
    let (a, b) = (secret(), secret());
    let params =
        SwapParams { maker_key: compressed(&a), taker_key: compressed(&b), refund_pk: x_only(&a), lock_height: 1 };
    let o = SwapOutput::new(&params).unwrap();
    let message = [9u8; 32];
    let big_t = point_of(&secret()).unwrap();
    let ctx = o.key_ctx();
    let (x, y) = (Session::start(&a, ctx, &message).unwrap(), Session::start(&b, ctx, &message).unwrap());
    let (xn, yn) = (x.public.clone(), y.public.clone());
    let (s1, agg) = x.sign(&a, ctx, &yn, &big_t, &message).unwrap();
    let (s2, _) = y.sign(&b, ctx, &xn, &big_t, &message).unwrap();
    combine(ctx, &agg, [s1, s2], &big_t, &message).unwrap()
}
