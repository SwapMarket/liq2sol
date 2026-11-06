//! The confidential wallet against a real node: blinding has to balance across every output, or the node refuses it.
//!
//!   ELEMENTSD=~/bin/elementsd ELEMENTS_CLI=~/bin/elements-cli cargo test --test confidential -- --ignored --nocapture

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::str::FromStr;
use std::time::{Duration, Instant};

use liq2sol_chain::esplora::*;
use liq2sol_chain::liquid::*;
use serde_json::Value as Json;
use simplicityhl::elements::confidential::{Asset, Value};
use simplicityhl::elements::encode::serialize_hex;
use simplicityhl::elements::secp256k1_zkp::SecretKey;
use simplicityhl::elements::{AddressParams, AssetId, BlockHash, Sequence, TxOut};

struct Node {
    child: Child,
    rpc: Rpc,
    dir: PathBuf,
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.rpc.call(&["stop"]);
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn start() -> Node {
    let port = 46_000 + (std::process::id() % 5_000) as u16;
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("conf-{port}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let daemon = std::env::var("ELEMENTSD").unwrap_or_else(|_| "elementsd".into());
    let cli = std::env::var("ELEMENTS_CLI").unwrap_or_else(|_| "elements-cli".into());
    let base: Vec<String> = ["-chain=liquidregtest", "-rpcuser=t", "-rpcpassword=t", &format!("-rpcport={port}")]
        .map(String::from)
        .to_vec();
    let child = Command::new(daemon)
        .arg(format!("-datadir={}", dir.display()))
        .args(["-listen=0", "-txindex=1", "-validatepegin=0", "-fallbackfee=0.00001", "-initialfreecoins=2100000000"])
        .args([
            "-anyonecanspendaremine=1",
            "-evbparams=simplicity:-1:::",
            "-printtoconsole=0",
            "-acceptdiscountct=1",
            "-creatediscountct=1",
        ])
        .args(&base)
        .stdout(Stdio::null())
        .spawn()
        .expect("start elementsd");
    let rpc = Rpc { cli, args: base };
    let t = Instant::now();
    while rpc.call(&["getblockcount"]).is_err() {
        assert!(t.elapsed() < Duration::from_secs(60));
        std::thread::sleep(Duration::from_millis(200));
    }
    rpc.call(&["createwallet", "w"]).unwrap();
    rpc.call(&["rescanblockchain"]).unwrap();
    Node { child, rpc, dir }
}

fn wallet() -> Wallet {
    Wallet {
        key: keypair_from(&random32().unwrap()).unwrap(),
        blinding: SecretKey::from_slice(&random32().unwrap()).unwrap(),
    }
}

fn hidden(out: &TxOut) -> bool {
    matches!(out.asset, Asset::Confidential(_)) && matches!(out.value, Value::Confidential(_))
}

#[test]
#[ignore = "needs elementsd"]
fn a_wallet_with_only_blinded_coins_works_on_a_real_node() {
    let node = start();
    let mine = |n: u32| {
        let a = node.rpc.call(&["getnewaddress"]).unwrap();
        node.rpc.call(&["generatetoaddress", &n.to_string(), &a]).unwrap();
    };
    let fund = |a: &str, sats: u64, label: &str| {
        let amount = format!("{}.{:08}", sats / 100_000_000, sats % 100_000_000);
        node.rpc.call(&["sendtoaddress", a, &amount, "", "", "false", "false", "1", "UNSET", "false", label]).unwrap();
    };
    mine(1);
    let issued: Json = serde_json::from_str(&node.rpc.call(&["issueasset", "1000", "0", "false"]).unwrap()).unwrap();
    let usdt_id = issued["asset"].as_str().unwrap().to_string();
    mine(1);
    let info: Json = serde_json::from_str(&node.rpc.call(&["getsidechaininfo"]).unwrap()).unwrap();
    use_network(NetworkConfig {
        params: &AddressParams::ELEMENTS,
        usdt: AssetId::from_str(&usdt_id).unwrap(),
        lbtc: AssetId::from_str(info["pegged_asset"].as_str().unwrap()).unwrap(),
    });
    let chain = Esplora::Node(node.rpc.clone());
    let genesis = BlockHash::from_str(&chain.genesis().unwrap()).unwrap();

    let (ours, theirs, fee_wallet) = (wallet(), wallet(), wallet());
    fund(&ours.address().to_string(), 200_000, "bitcoin");
    fund(&ours.address().to_string(), 5_000_000, &usdt_id);
    mine(1);
    let coins = chain.secret_coins(&ours.address(), &ours.blinding).unwrap();
    assert_eq!(coins.len(), 2, "both blinded coins are readable");
    assert!(coins.iter().all(|c| hidden(&c.coin.out)), "and hidden from everyone else");
    assert!(chain.secret_coins(&ours.address(), &theirs.blinding).unwrap().is_empty(), "the wrong key reads nothing");
    println!("    ok: a wallet reads its own blinded coins, and a wrong key reads nothing");

    // Several blinded outputs and change in both assets, which is where the blinding has to balance.
    let tx = build_confidential_pay(
        &chain,
        &ours,
        vec![
            Payment { asset: usdt(), amount: 1_000_000, address: theirs.address() },
            Payment { asset: lbtc(), amount: FEE, address: fee_wallet.address() },
        ],
        genesis,
    )
    .unwrap();
    assert_eq!(tx.output.len(), 5, "two payments, two change outputs, and the fee");
    assert!(tx.output.iter().filter(|o| !o.is_fee()).all(hidden), "everything but the fee is hidden");
    let info: Json =
        serde_json::from_str(&node.rpc.call(&["decoderawtransaction", &serialize_hex(&tx)]).unwrap()).unwrap();
    println!(
        "    pay: {} inputs, {} outputs, vsize {}, discountvsize {}",
        tx.input.len(),
        tx.output.len(),
        info["vsize"],
        info["discountvsize"]
    );
    let paid: u64 = tx.output.iter().filter(|o| o.is_fee()).filter_map(|o| o.value.explicit()).sum();
    assert!(paid >= info["discountvsize"].as_u64().unwrap() / 10, "at least the relay floor");
    assert!(paid <= info["discountvsize"].as_u64().unwrap() / 2, "and it isn't paying for the undiscounted size");
    let txid = chain.broadcast(&tx).expect("the node accepts it");
    mine(1);
    println!("    ok: the node accepted {} with 4 blinded outputs", &txid.to_string()[..12]);

    let got = chain.secret_coins(&theirs.address(), &theirs.blinding).unwrap();
    assert_eq!((got.len(), got[0].secrets.asset, got[0].secrets.value), (1, usdt(), 1_000_000));
    let change = chain.secret_coins(&ours.address(), &ours.blinding).unwrap();
    let total = |a| change.iter().filter(|c| c.secrets.asset == a).map(|c| c.secrets.value).sum::<u64>();
    assert_eq!(total(usdt()), 4_000_000, "the right change came back");
    assert_eq!(total(lbtc()), 200_000 - FEE - paid, "after the fee coin and the fee");
    println!("    ok: the recipient and the change read the exact amounts");

    // Blinded change spends again.
    let again = build_confidential_pay(
        &chain,
        &ours,
        vec![Payment { asset: usdt(), amount: 4_000_000, address: theirs.address() }],
        genesis,
    )
    .unwrap();
    assert!(again.output.iter().filter(|o| !o.is_fee()).all(hidden));
    chain.broadcast(&again).expect("spending blinded change works");
    mine(1);
    println!("    ok: blinded change can be spent again");

    // Issuing a stand-in from one coin, with its change blinded.
    let accepts = |tx: &simplicityhl::elements::Transaction| -> Json {
        let arg = format!("[\"{}\"]", serialize_hex(tx));
        serde_json::from_str(&node.rpc.call(&["testmempoolaccept", &arg]).unwrap()).unwrap()
    };
    let issuer = wallet();
    fund(&issuer.address().to_string(), 50_000, "bitcoin");
    let plain_issuer = wallet();
    fund(&plain_issuer.address().to_unconfidential().to_string(), 50_000, "bitcoin");
    mine(1);
    for (name, w) in [("a blinded coin", &issuer), ("an explicit coin", &plain_issuer)] {
        let coin = chain.secret_coins(&w.address(), &w.blinding).unwrap().remove(0);
        let (issue, _) = build_issuance(&coin, w, 1_000_000, genesis).unwrap();
        let verdict = accepts(&issue);
        println!("    issuing from {name}: {verdict}");
        assert!(verdict[0]["allowed"].as_bool().unwrap(), "{name}: {verdict}");
    }
    println!("    ok: an asset can be issued from a blinded or an explicit coin");

    // A payment using up a whole coin has only one blinded output, which has to balance by itself. A maker's wallet can hit this.
    let exact = wallet();
    fund(&exact.address().to_string(), 50_000, "bitcoin");
    mine(1);
    let spend_all = Payment { asset: lbtc(), amount: 50_000 - 56, address: theirs.address() };
    let tx = build_confidential_pay(&chain, &exact, vec![spend_all], genesis).unwrap();
    assert_eq!(tx.output.len(), 2, "one blinded output, and the fee");
    let verdict = accepts(&tx);
    assert!(verdict[0]["allowed"].as_bool().unwrap(), "{verdict}");
    println!("    ok: a payment with no change, and so one blinded output, is accepted");

    // A swap spend's shape, with the fee coin spent whole.
    let tip = chain.tip().unwrap();
    let mine_now = chain.secret_coins(&theirs.address(), &theirs.blinding).unwrap();
    let coin = exact_secret_coin(&mine_now, usdt(), 1_000_000, tip, 1).expect("the exact coin");
    let fee_coin = chain.secret_coins(&fee_wallet.address(), &fee_wallet.blinding).unwrap().remove(0);
    assert_eq!(fee_coin.secrets.value, FEE);
    let one = [coin.clone()];
    assert!(exact_secret_coin(&one, usdt(), 1_000_001, tip, 1).is_none(), "the wrong amount isn't accepted");
    assert!(exact_secret_coin(&one, lbtc(), 1_000_000, tip, 1).is_none(), "the wrong asset isn't accepted");
    assert!(exact_secret_coin(&one, usdt(), 1_000_000, tip, 99).is_none(), "too few confirmations aren't accepted");
    let (mut spend, utxos) = build_swap_spend(&coin, &fee_coin, &ours.address(), 0, Sequence::MAX).unwrap();
    sign_keypath(&mut spend, &utxos, 0, &theirs.key, genesis).unwrap();
    sign_keypath(&mut spend, &utxos, 1, &fee_wallet.key, genesis).unwrap();
    let arg = format!("[\"{}\"]", serialize_hex(&spend));
    let accept: Json = serde_json::from_str(&node.rpc.call(&["testmempoolaccept", &arg]).unwrap()).unwrap();
    assert!(accept[0]["allowed"].as_bool().unwrap(), "{accept}");
    let info: Json =
        serde_json::from_str(&node.rpc.call(&["decoderawtransaction", &serialize_hex(&spend)]).unwrap()).unwrap();
    println!("    swap spend: vsize {}, discountvsize {}", info["vsize"], info["discountvsize"]);
    println!("    ok: a spend of a blinded coin and a blinded fee coin is accepted at {} vB", accept[0]["vsize"]);
    assert!(is_key_path_spend(&spend, &coin.coin.outpoint).unwrap());
    assert!(published_signature(&spend, &coin.coin.outpoint).is_some());
}
