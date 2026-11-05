//! Feasibility proofs F1, F3 and F5 from docs/test-plan.md, against a real `elementsd` regtest node with Simplicity on.
//!
//! Needs a node, so it is ignored by default:
//!   ELEMENTSD=elementsd ELEMENTS_CLI=elements-cli cargo test -p liq2sol-htlc --test regtest -- --ignored --nocapture

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::str::FromStr;

use liq2sol_htlc::{asset_param, extract_preimage, script_hash, Htlc, HtlcParams, Spend};
use serde_json::Value as Json;
use simplicityhl::elements::confidential::{Asset, Nonce, Value};
use simplicityhl::elements::encode::{deserialize, serialize_hex};
use simplicityhl::elements::hashes::sha256;
use simplicityhl::elements::schnorr::{TapTweak, UntweakedKeypair};
use simplicityhl::elements::secp256k1_zkp::{Keypair, Message, Secp256k1};
use simplicityhl::elements::sighash::{Prevouts, SighashCache};
use simplicityhl::elements::{
    Address, AddressParams, AssetId, AssetIssuance, BlockHash, LockTime, OutPoint,
    SchnorrSighashType, Script, Sequence, Transaction, TxIn, TxInWitness, TxOut, TxOutWitness,
    Witness,
};

const AMOUNT: u64 = 10_000_000_000; // 100.0 of the 8-decimal test asset.
const FEE: u64 = 300;

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
        let port = 20_000 + (std::process::id() % 20_000) as u16;
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("regtest-{port}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let child = Command::new(daemon)
            .arg(format!("-datadir={}", dir.display()))
            .args([
                "-chain=liquidregtest",
                "-listen=0",
                "-txindex=1",
                "-validatepegin=0",
                "-fallbackfee=0.00001",
            ])
            .args([
                "-initialfreecoins=2100000000",
                "-anyonecanspendaremine=1",
                "-evbparams=simplicity:-1:::",
            ])
            .args([
                "-rpcuser=t",
                "-rpcpassword=t",
                &format!("-rpcport={port}"),
                "-printtoconsole=0",
            ])
            .stdout(Stdio::null())
            .spawn()
            .expect("start elementsd; set ELEMENTSD to its path");
        let node = Node {
            child,
            dir,
            cli,
            port,
        };
        for _ in 0..120 {
            if node.call(&["getblockcount"]).is_ok() {
                return node;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        panic!("elementsd did not start");
    }

    fn call(&self, args: &[&str]) -> Result<String, String> {
        let out = Command::new(&self.cli)
            .args([
                "-chain=liquidregtest",
                "-rpcuser=t",
                "-rpcpassword=t",
                &format!("-rpcport={}", self.port),
            ])
            .args(args)
            .output()
            .expect("run elements-cli; set ELEMENTS_CLI to its path");
        if out.status.success() {
            Ok(String::from_utf8(out.stdout).unwrap().trim().to_string())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    }

    fn ok(&self, args: &[&str]) -> String {
        self.call(args).unwrap_or_else(|e| panic!("{args:?}: {e}"))
    }

    fn json(&self, args: &[&str]) -> Json {
        serde_json::from_str(&self.ok(args)).unwrap()
    }

    fn mine(&self, blocks: u32) {
        let address = self.ok(&["getnewaddress"]);
        self.ok(&["generatetoaddress", &blocks.to_string(), &address]);
    }

    fn height(&self) -> u32 {
        self.ok(&["getblockcount"]).parse().unwrap()
    }

    /// Pay `amount` of `asset` explicitly to `address`, mine it, and return the outpoint and the output.
    fn fund(&self, address: &Address, sats: u64, asset_label: &str) -> (OutPoint, TxOut) {
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
            asset_label,
        ]);
        self.mine(1);
        let raw = self.ok(&["getrawtransaction", &txid]);
        let tx: Transaction = deserialize(&from_hex(&raw)).unwrap();
        let script = address.script_pubkey();
        let (vout, out) = tx
            .output
            .iter()
            .enumerate()
            .find(|(_, o)| o.script_pubkey == script)
            .expect("funded output");
        assert!(
            out.asset.is_explicit() && out.value.is_explicit(),
            "funding output must be explicit"
        );
        (OutPoint::new(tx.txid(), vout as u32), out.clone())
    }

    /// Whether the mempool would accept this transaction, and the reason if it would not.
    fn would_accept(&self, tx: &Transaction) -> Result<(), String> {
        let arg = format!("[\"{}\"]", serialize_hex(tx));
        let result = &self.json(&["testmempoolaccept", &arg])[0];
        if result["allowed"].as_bool().unwrap_or(false) {
            Ok(())
        } else {
            Err(result["reject-reason"]
                .as_str()
                .unwrap_or("unknown")
                .to_string())
        }
    }
}

fn from_hex(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).unwrap())
        .collect()
}

/// A copy of `tx` with one byte of the HTLC input's witness stack changed.
fn flip_witness_bit(tx: &Transaction, item: usize, byte: usize, mask: u8) -> Transaction {
    let mut items: Vec<Vec<u8>> = (0..4)
        .map(|i| tx.input[0].witness.script_witness[i].to_vec())
        .collect();
    items[item][byte] ^= mask;
    let mut out = tx.clone();
    out.input[0].witness.script_witness = Witness::from_slice(&items);
    out
}

fn key(seed: u8) -> Keypair {
    Keypair::from_seckey_slice(&Secp256k1::new(), &[seed; 32]).unwrap()
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

struct Swap {
    params: HtlcParams,
    htlc: Htlc,
    preimage: [u8; 32],
    recipient: Keypair,
    sender: Keypair,
    recipient_script: Script,
}

struct Env {
    node: Node,
    genesis: BlockHash,
    usdt: AssetId,
    lbtc: AssetId,
    params: &'static AddressParams,
    fee_key: Keypair,
    fee_address: Address,
}

impl Env {
    fn start() -> Self {
        let node = Node::start();
        node.ok(&["createwallet", "w"]);
        node.ok(&["rescanblockchain"]);
        node.mine(1);
        let issued = node.json(&["issueasset", "1000", "0", "false"]);
        let usdt = AssetId::from_str(issued["asset"].as_str().unwrap()).unwrap();
        node.mine(1);
        let lbtc = AssetId::from_str(
            node.json(&["getsidechaininfo"])["pegged_asset"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        let genesis = BlockHash::from_str(&node.ok(&["getblockhash", "0"])).unwrap();
        let secp = Secp256k1::new();
        let fee_key = key(9);
        let params = &AddressParams::ELEMENTS;
        let fee_address = Address::p2tr(&secp, fee_key.x_only_public_key().0, None, None, params);
        Env {
            node,
            genesis,
            usdt,
            lbtc,
            params,
            fee_key,
            fee_address,
        }
    }

    fn swap(&self, seed: u8, lock_height: u32) -> Swap {
        let mut preimage = [seed; 32];
        preimage[0] = 0x80 | seed;
        let recipient = key(seed);
        let sender = key(seed + 100);
        let recipient_script = Address::p2tr(
            &Secp256k1::new(),
            key(seed + 50).x_only_public_key().0,
            None,
            None,
            self.params,
        )
        .script_pubkey();
        let params = HtlcParams {
            payment_hash: sha256::Hash::hash(&preimage).to_byte_array(),
            recipient_pk: recipient.x_only_public_key().0.serialize(),
            sender_pk: sender.x_only_public_key().0.serialize(),
            lock_height,
            usdt_asset: asset_param(&self.usdt),
            amount: AMOUNT,
            recipient_script_hash: script_hash(&recipient_script),
        };
        let htlc = Htlc::new(&params).unwrap();
        Swap {
            params,
            htlc,
            preimage,
            recipient,
            sender,
            recipient_script,
        }
    }

    /// Build an unsigned spend of the HTLC coin plus a whole L-BTC fee coin, per decision D3.
    fn spend(
        &self,
        coin: (OutPoint, TxOut),
        fee_coin: (OutPoint, TxOut),
        outputs: Vec<TxOut>,
        lock_time: u32,
        sequence: Sequence,
    ) -> (Transaction, Vec<TxOut>) {
        let tx = Transaction {
            version: 2,
            lock_time: LockTime::from_consensus(lock_time),
            input: vec![input(coin.0, sequence), input(fee_coin.0, sequence)],
            output: outputs,
        };
        (tx, vec![coin.1, fee_coin.1])
    }

    fn sign_fee_input(&self, tx: &mut Transaction, utxos: &[TxOut]) {
        let secp = Secp256k1::new();
        let hash = SighashCache::new(&*tx)
            .taproot_key_spend_signature_hash(
                1,
                &Prevouts::All(utxos),
                SchnorrSighashType::All,
                self.genesis,
            )
            .unwrap();
        let tweaked = UntweakedKeypair::tap_tweak(self.fee_key, &secp, None);
        let msg = Message::from_digest(hash.to_byte_array());
        let sig = secp.sign_schnorr_no_aux_rand(&msg, &tweaked.to_inner());
        let mut bytes = sig.as_ref().to_vec();
        bytes.push(SchnorrSighashType::All as u8);
        tx.input[1].witness.script_witness = Witness::from_slice(&[bytes]);
    }

    fn sign_simplicity(
        &self,
        swap: &Swap,
        tx: &Transaction,
        utxos: &[TxOut],
        signer: &Keypair,
    ) -> [u8; 64] {
        let digest = swap.htlc.sighash_all(tx, utxos, 0, self.genesis).unwrap();
        *Secp256k1::new()
            .sign_schnorr_no_aux_rand(&Message::from_digest(digest), signer)
            .as_ref()
    }

    fn claim_tx(
        &self,
        swap: &Swap,
        coin: (OutPoint, TxOut),
        fee_coin: (OutPoint, TxOut),
    ) -> (Transaction, Vec<TxOut>) {
        let pay = TxOut {
            asset: Asset::Explicit(self.usdt),
            value: Value::Explicit(AMOUNT),
            nonce: Nonce::Null,
            script_pubkey: swap.recipient_script.clone(),
            witness: TxOutWitness::default(),
        };
        let (tx, utxos) = self.spend(
            coin,
            fee_coin,
            vec![pay, TxOut::new_fee(FEE, self.lbtc)],
            0,
            Sequence::MAX,
        );
        let signature = self.sign_simplicity(swap, &tx, &utxos, &swap.recipient);
        let (mut tx, report) = swap
            .htlc
            .finalize(
                tx,
                &utxos,
                0,
                self.genesis,
                Spend::Claim {
                    preimage: swap.preimage,
                    signature,
                },
            )
            .unwrap();
        println!("claim report: {report:?}");
        self.sign_fee_input(&mut tx, &utxos);
        (tx, utxos)
    }

    fn refund_tx(
        &self,
        swap: &Swap,
        coin: (OutPoint, TxOut),
        fee_coin: (OutPoint, TxOut),
        lock_time: u32,
        sequence: Sequence,
    ) -> Transaction {
        let (asset, value) = (coin.1.asset, coin.1.value);
        let back = TxOut {
            asset,
            value,
            nonce: Nonce::Null,
            script_pubkey: swap.recipient_script.clone(),
            witness: TxOutWitness::default(),
        };
        let (tx, utxos) = self.spend(
            coin,
            fee_coin,
            vec![back, TxOut::new_fee(FEE, self.lbtc)],
            lock_time,
            sequence,
        );
        let signature = self.sign_simplicity(swap, &tx, &utxos, &swap.sender);
        let (mut tx, report) = swap
            .htlc
            .finalize(tx, &utxos, 0, self.genesis, Spend::Refund { signature })
            .unwrap();
        println!("refund report: {report:?}");
        self.sign_fee_input(&mut tx, &utxos);
        tx
    }

    fn fund_fee_coin(&self) -> (OutPoint, TxOut) {
        self.node.fund(&self.fee_address, FEE, "bitcoin")
    }

    fn unspent(&self, outpoint: &OutPoint) -> bool {
        !self
            .node
            .ok(&[
                "gettxout",
                &outpoint.txid.to_string(),
                &outpoint.vout.to_string(),
            ])
            .is_empty()
    }
}

#[test]
#[ignore = "needs elementsd; see the module docs"]
fn htlc_on_a_real_node() {
    let env = Env::start();
    let usdt_label = env.usdt.to_string();
    println!(
        "{}",
        env.node
            .ok(&["getnetworkinfo"])
            .lines()
            .find(|l| l.contains("subversion"))
            .unwrap_or("")
    );

    // ---- Claim: mined, and the preimage is recoverable from the chain. ----
    let swap = env.swap(1, env.node.height() + 1000);
    let coin = env
        .node
        .fund(&swap.htlc.address(env.params), AMOUNT, &usdt_label);
    let fee_coin = env.fund_fee_coin();
    let (tx, _) = env.claim_tx(&swap, coin.clone(), fee_coin.clone());

    let decoded = env
        .node
        .json(&["decoderawtransaction", &serialize_hex(&tx)]);
    println!(
        "claim vsize {} weight {} fee {FEE} sats",
        decoded["vsize"], decoded["weight"]
    );

    // Consensus runs the program: every tampered copy of the valid claim must be refused.
    let mut tampered = tx.clone();
    tampered.output[1] = TxOut::new_fee(FEE - 1, env.lbtc);
    assert!(env.node.would_accept(&tampered).is_err(), "changed fee");

    let wrong_cmr = flip_witness_bit(&tx, 2, 0, 1);
    println!(
        "wrong CMR: {:?}",
        env.node.would_accept(&wrong_cmr).unwrap_err()
    );

    let wrong_control = flip_witness_bit(&tx, 3, 1, 1);
    println!(
        "wrong control block: {:?}",
        env.node.would_accept(&wrong_control).unwrap_err()
    );

    let wrong_leaf = flip_witness_bit(&tx, 3, 0, 0x02);
    println!(
        "wrong leaf version: {:?}",
        env.node.would_accept(&wrong_leaf).unwrap_err()
    );

    let signature = {
        let mut unsigned = tx.clone();
        unsigned.input[0].witness = TxInWitness::default();
        env.sign_simplicity(
            &swap,
            &unsigned,
            &[coin.1.clone(), fee_coin.1.clone()],
            &swap.recipient,
        )
    };
    let (program, witness) = swap
        .htlc
        .unpruned_for_test(Spend::Claim {
            preimage: swap.preimage,
            signature,
        })
        .unwrap();
    let mut unpruned = tx.clone();
    let cmr = tx.input[0].witness.script_witness[2].to_vec();
    let control_block = tx.input[0].witness.script_witness[3].to_vec();
    unpruned.input[0].witness.script_witness =
        Witness::from_slice(&[witness, program, cmr, control_block]);
    println!(
        "unpruned: {:?}",
        env.node.would_accept(&unpruned).unwrap_err()
    );

    env.node.would_accept(&tx).expect("valid claim");
    let txid = env.node.ok(&["sendrawtransaction", &serialize_hex(&tx)]);
    env.node.mine(1);
    assert!(!env.unspent(&coin.0), "HTLC coin spent");

    let mined: Transaction =
        deserialize(&from_hex(&env.node.ok(&["getrawtransaction", &txid]))).unwrap();
    let recovered = extract_preimage(
        &mined.input[0].witness.script_witness[0],
        &swap.params.payment_hash,
    );
    assert_eq!(recovered, Some(swap.preimage));
    assert_eq!(mined.output[0].value, Value::Explicit(AMOUNT));
    assert_eq!(mined.output[0].asset, Asset::Explicit(env.usdt));

    // ---- Refund: refused early, accepted once the lock height has passed. ----
    let tip = env.node.height();
    let lock_height = tip + 6;
    let swap = env.swap(2, lock_height);
    let coin = env
        .node
        .fund(&swap.htlc.address(env.params), AMOUNT, &usdt_label);
    let fee_coin = env.fund_fee_coin();

    let refund = env.refund_tx(
        &swap,
        coin.clone(),
        fee_coin.clone(),
        lock_height,
        Sequence::ZERO,
    );
    println!(
        "early refund: {:?}",
        env.node.would_accept(&refund).unwrap_err()
    );

    // Final sequences make `tx_lock_height` read zero, so the contract refuses even at the height.
    let mut final_seq = Transaction {
        lock_time: LockTime::from_consensus(lock_height),
        ..refund.clone()
    };
    final_seq.input[0].sequence = Sequence::MAX;
    final_seq.input[1].sequence = Sequence::MAX;

    while env.node.height() < lock_height - 1 {
        env.node.mine(1);
    }
    println!(
        "at height {} (next block {}): lock_time {lock_height} {:?}",
        env.node.height(),
        env.node.height() + 1,
        env.node.would_accept(&refund)
    );
    assert!(
        env.node.would_accept(&refund).is_err(),
        "not yet: a lock time needs a later block"
    );
    env.node.mine(1);
    println!(
        "at height {} : {:?}",
        env.node.height(),
        env.node.would_accept(&refund)
    );
    assert_eq!(env.node.height(), lock_height);
    println!(
        "final sequences: {:?}",
        env.node.would_accept(&final_seq).unwrap_err()
    );
    env.node
        .would_accept(&refund)
        .expect("refund is valid once the tip reaches the lock height");
    env.node
        .ok(&["sendrawtransaction", &serialize_hex(&refund)]);
    env.node.mine(1);
    assert!(!env.unspent(&coin.0));

    // ---- A wrong-amount coin at a valid address is recoverable through the refund path. ----
    let swap = env.swap(3, env.node.height() + 1);
    let lock_height = swap.params.lock_height;
    let coin = env
        .node
        .fund(&swap.htlc.address(env.params), AMOUNT - 100, &usdt_label);
    let fee_coin = env.fund_fee_coin();
    assert!(env.node.height() >= lock_height);
    let refund = env.refund_tx(&swap, coin.clone(), fee_coin, lock_height, Sequence::ZERO);
    env.node
        .ok(&["sendrawtransaction", &serialize_hex(&refund)]);
    env.node.mine(1);
    assert!(!env.unspent(&coin.0));
}
