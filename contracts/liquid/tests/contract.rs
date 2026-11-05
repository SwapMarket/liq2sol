//! Contract tests from docs/test-plan.md section 3, run on the Simplicity Bit Machine against constructed transactions.
//! Consensus-level cases (mempool, lock-time finality, leaf rules) are in `regtest.rs`.

use std::str::FromStr;

use liq2sol_htlc::{asset_param, extract_preimage, script_hash, Error, Htlc, HtlcParams, Spend};
use simplicityhl::elements::confidential::{Asset, Nonce, Value};
use simplicityhl::elements::secp256k1_zkp::{
    Generator, Keypair, Message, PedersenCommitment, Secp256k1,
};
use simplicityhl::elements::{
    AssetId, AssetIssuance, BlockHash, LockTime, OutPoint, Script, Sequence, Transaction, TxIn,
    TxInWitness, TxOut, TxOutWitness, Txid,
};

/// The shared preimage vector from docs/contracts.md section 3.
const PREIMAGE: [u8; 32] = [0; 32];
const PAYMENT_HASH: &str = "66687aadf862bd776c8fc18b8e9f8e20089714856ee233b3902a591d0d5f2925";
const AMOUNT: u64 = 10_000_000_000;
const LOCK_HEIGHT: u32 = 1000;
/// Tether's L-USDt id as explorers display it.
const USDT: &str = "ce091c998b83c78bb71a632313ba3760f1763d9cfcffae02258ffa9865a37bd2";
const FAKE: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const LBTC: &str = "6f0279e9ed041c3d710a9f57d0c02928416460c4b722ae3457a11eec381c526d";

fn unhex32(s: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).unwrap();
    }
    out
}

fn asset(s: &str) -> AssetId {
    AssetId::from_str(s).unwrap()
}

fn key(seed: u8) -> Keypair {
    Keypair::from_seckey_slice(&Secp256k1::new(), &[seed; 32]).unwrap()
}

fn xonly(k: &Keypair) -> [u8; 32] {
    k.x_only_public_key().0.serialize()
}

fn recipient_script() -> Script {
    Script::from(
        vec![0x51, 0x20]
            .into_iter()
            .chain([0xaa; 32])
            .collect::<Vec<u8>>(),
    )
}

fn genesis() -> BlockHash {
    BlockHash::from_byte_array([7; 32])
}

struct Fx {
    params: HtlcParams,
    htlc: Htlc,
    recipient: Keypair,
    sender: Keypair,
}

fn fixture() -> Fx {
    let recipient = key(1);
    let sender = key(2);
    let params = HtlcParams {
        payment_hash: unhex32(PAYMENT_HASH),
        recipient_pk: xonly(&recipient),
        sender_pk: xonly(&sender),
        lock_height: LOCK_HEIGHT,
        usdt_asset: asset_param(&asset(USDT)),
        amount: AMOUNT,
        recipient_script_hash: script_hash(&recipient_script()),
    };
    let htlc = Htlc::new(&params).unwrap();
    Fx {
        params,
        htlc,
        recipient,
        sender,
    }
}

fn txout(a: Asset, v: Value, script: Script) -> TxOut {
    TxOut {
        asset: a,
        value: v,
        nonce: Nonce::Null,
        script_pubkey: script,
        witness: TxOutWitness::default(),
    }
}

fn explicit_out(a: &str, amount: u64, script: Script) -> TxOut {
    txout(Asset::Explicit(asset(a)), Value::Explicit(amount), script)
}

fn input(n: u8, sequence: Sequence) -> TxIn {
    TxIn {
        previous_output: OutPoint::new(Txid::from_byte_array([n; 32]), 0),
        is_pegin: false,
        script_sig: Script::new(),
        sequence,
        asset_issuance: AssetIssuance::default(),
        witness: TxInWitness::default(),
    }
}

/// A spend of one HTLC coin plus one L-BTC fee coin.
struct Case {
    tx: Transaction,
    utxos: Vec<TxOut>,
}

impl Case {
    fn claim(fx: &Fx) -> Self {
        Self {
            tx: Transaction {
                version: 2,
                lock_time: LockTime::ZERO,
                input: vec![input(1, Sequence::MAX), input(2, Sequence::MAX)],
                output: vec![
                    explicit_out(USDT, AMOUNT, recipient_script()),
                    TxOut::new_fee(300, asset(LBTC)),
                ],
            },
            utxos: vec![
                explicit_out(USDT, AMOUNT, fx.htlc.script_pubkey()),
                explicit_out(
                    LBTC,
                    300,
                    Script::from(
                        vec![0x51, 0x20]
                            .into_iter()
                            .chain([0xbb; 32])
                            .collect::<Vec<u8>>(),
                    ),
                ),
            ],
        }
    }

    fn refund(fx: &Fx, lock_time: u32, sequence: Sequence) -> Self {
        let mut case = Self::claim(fx);
        case.tx.lock_time = LockTime::from_consensus(lock_time);
        case.tx.input[0].sequence = sequence;
        case.tx.input[1].sequence = sequence;
        case
    }

    fn sign(&self, fx: &Fx, signer: &Keypair) -> [u8; 64] {
        let digest = fx
            .htlc
            .sighash_all(&self.tx, &self.utxos, 0, genesis())
            .unwrap();
        let sig = Secp256k1::new().sign_schnorr_no_aux_rand(&Message::from_digest(digest), signer);
        *sig.as_ref()
    }

    fn run(&self, fx: &Fx, spend: Spend) -> Result<liq2sol_htlc::SpendReport, Error> {
        fx.htlc
            .finalize(self.tx.clone(), &self.utxos, 0, genesis(), spend)
            .map(|(_, report)| report)
    }

    fn run_claim(&self, fx: &Fx) -> Result<liq2sol_htlc::SpendReport, Error> {
        let signature = self.sign(fx, &fx.recipient);
        self.run(
            fx,
            Spend::Claim {
                preimage: PREIMAGE,
                signature,
            },
        )
    }

    fn run_refund(&self, fx: &Fx) -> Result<liq2sol_htlc::SpendReport, Error> {
        let signature = self.sign(fx, &fx.sender);
        self.run(fx, Spend::Refund { signature })
    }
}

#[track_caller]
fn assert_rejected(result: Result<liq2sol_htlc::SpendReport, Error>) {
    match result {
        Err(Error::Rejected(_)) => {}
        other => panic!("expected the contract to reject, got {other:?}"),
    }
}

fn blinded_asset() -> Asset {
    let secp = Secp256k1::new();
    Asset::Confidential(Generator::new_unblinded(&secp, asset(USDT).into_tag()))
}

fn blinded_value() -> Value {
    let secp = Secp256k1::new();
    let generator = Generator::new_unblinded(&secp, asset(USDT).into_tag());
    Value::Confidential(PedersenCommitment::new_unblinded(&secp, AMOUNT, generator))
}

// ---- Claim ----

#[test]
fn valid_claim_is_accepted_and_within_budget() {
    let fx = fixture();
    let report = Case::claim(&fx).run_claim(&fx).unwrap();
    assert!(report.cost_wu <= report.budget_wu);
    println!("claim: {report:?}");
}

#[test]
fn claim_after_lock_height_is_still_accepted() {
    // Documented behaviour: a program cannot bound the claim from above.
    let fx = fixture();
    let mut case = Case::claim(&fx);
    case.tx.lock_time = LockTime::from_consensus(LOCK_HEIGHT + 500);
    case.tx.input[0].sequence = Sequence::ZERO;
    case.tx.input[1].sequence = Sequence::ZERO;
    case.run_claim(&fx).unwrap();
}

#[test]
fn wrong_preimage_is_rejected() {
    let fx = fixture();
    let case = Case::claim(&fx);
    let signature = case.sign(&fx, &fx.recipient);
    assert_rejected(case.run(
        &fx,
        Spend::Claim {
            preimage: [1; 32],
            signature,
        },
    ));
}

#[test]
fn claim_signed_by_sender_is_rejected() {
    let fx = fixture();
    let case = Case::claim(&fx);
    let signature = case.sign(&fx, &fx.sender);
    assert_rejected(case.run(
        &fx,
        Spend::Claim {
            preimage: PREIMAGE,
            signature,
        },
    ));
}

#[test]
fn claim_signature_from_another_transaction_is_rejected() {
    let fx = fixture();
    let original = Case::claim(&fx);
    let signature = original.sign(&fx, &fx.recipient);
    let mut changed = Case::claim(&fx);
    changed.tx.output[1] = TxOut::new_fee(299, asset(LBTC));
    assert_rejected(changed.run(
        &fx,
        Spend::Claim {
            preimage: PREIMAGE,
            signature,
        },
    ));
}

#[test]
fn spent_coin_with_fake_asset_is_rejected() {
    let fx = fixture();
    let mut case = Case::claim(&fx);
    case.utxos[0] = explicit_out(FAKE, AMOUNT, fx.htlc.script_pubkey());
    assert_rejected(case.run_claim(&fx));
}

#[test]
fn spent_coin_with_wrong_amount_is_rejected() {
    let fx = fixture();
    for amount in [AMOUNT - 1, AMOUNT + 1] {
        let mut case = Case::claim(&fx);
        case.utxos[0] = explicit_out(USDT, amount, fx.htlc.script_pubkey());
        assert_rejected(case.run_claim(&fx));
    }
}

#[test]
fn spent_coin_with_blinded_asset_or_amount_is_rejected() {
    let fx = fixture();
    let mut case = Case::claim(&fx);
    case.utxos[0] = txout(
        blinded_asset(),
        Value::Explicit(AMOUNT),
        fx.htlc.script_pubkey(),
    );
    assert_rejected(case.run_claim(&fx));
    let mut case = Case::claim(&fx);
    case.utxos[0] = txout(
        Asset::Explicit(asset(USDT)),
        blinded_value(),
        fx.htlc.script_pubkey(),
    );
    assert_rejected(case.run_claim(&fx));
}

#[test]
fn output_0_with_wrong_script_asset_or_amount_is_rejected() {
    let fx = fixture();
    let bad_outputs = [
        explicit_out(USDT, AMOUNT, Script::from(vec![0x51])),
        explicit_out(FAKE, AMOUNT, recipient_script()),
        explicit_out(USDT, AMOUNT - 1, recipient_script()),
        explicit_out(USDT, AMOUNT + 1, recipient_script()),
        txout(blinded_asset(), Value::Explicit(AMOUNT), recipient_script()),
        txout(
            Asset::Explicit(asset(USDT)),
            blinded_value(),
            recipient_script(),
        ),
    ];
    for bad in bad_outputs {
        let mut case = Case::claim(&fx);
        case.tx.output[0] = bad;
        assert_rejected(case.run_claim(&fx));
    }
}

#[test]
fn correct_payment_only_at_output_1_is_rejected() {
    let fx = fixture();
    let mut case = Case::claim(&fx);
    let pay = case.tx.output[0].clone();
    case.tx.output[0] = explicit_out(LBTC, 1, Script::from(vec![0x51]));
    case.tx.output.insert(1, pay);
    assert_rejected(case.run_claim(&fx));
}

#[test]
fn missing_output_0_is_rejected() {
    let fx = fixture();
    let mut case = Case::claim(&fx);
    case.tx.output.clear();
    assert_rejected(case.run_claim(&fx));
}

#[test]
fn two_coins_can_share_output_0_but_only_with_the_recipients_signature() {
    // Documented in protocol.md: each input sees only output 0, so a recipient signing both loses one coin.
    // Nobody else can do this, because the signature commits to the transaction. This is why a covenant-only
    // claim (D4) would need an extra rule.
    let fx = fixture();
    let mut case = Case::claim(&fx);
    case.utxos[1] = explicit_out(USDT, AMOUNT, fx.htlc.script_pubkey());
    case.run_claim(&fx).unwrap();
    let wrong_signer = case.sign(&fx, &fx.sender);
    assert_rejected(case.run(
        &fx,
        Spend::Claim {
            preimage: PREIMAGE,
            signature: wrong_signer,
        },
    ));
}

// ---- Refund ----

#[test]
fn valid_refund_is_accepted_and_within_budget() {
    let fx = fixture();
    let report = Case::refund(&fx, LOCK_HEIGHT, Sequence::ZERO)
        .run_refund(&fx)
        .unwrap();
    assert!(report.cost_wu <= report.budget_wu);
    println!("refund: {report:?}");
}

#[test]
fn refund_with_lock_time_below_lock_height_is_rejected() {
    let fx = fixture();
    assert_rejected(Case::refund(&fx, LOCK_HEIGHT - 1, Sequence::ZERO).run_refund(&fx));
    assert_rejected(Case::refund(&fx, 0, Sequence::ZERO).run_refund(&fx));
}

#[test]
fn refund_with_final_sequences_is_rejected() {
    let fx = fixture();
    assert_rejected(Case::refund(&fx, LOCK_HEIGHT, Sequence::MAX).run_refund(&fx));
}

#[test]
fn refund_with_timestamp_lock_time_is_rejected() {
    let fx = fixture();
    assert_rejected(Case::refund(&fx, 1_800_000_000, Sequence::ZERO).run_refund(&fx));
}

#[test]
fn refund_signed_by_recipient_is_rejected() {
    let fx = fixture();
    let case = Case::refund(&fx, LOCK_HEIGHT, Sequence::ZERO);
    let signature = case.sign(&fx, &fx.recipient);
    assert_rejected(case.run(&fx, Spend::Refund { signature }));
}

#[test]
fn wrong_asset_or_amount_coin_can_be_refunded() {
    let fx = fixture();
    for coin in [
        explicit_out(FAKE, AMOUNT, fx.htlc.script_pubkey()),
        explicit_out(USDT, AMOUNT - 1, fx.htlc.script_pubkey()),
    ] {
        let mut case = Case::refund(&fx, LOCK_HEIGHT, Sequence::ZERO);
        case.utxos[0] = coin;
        case.run_refund(&fx).unwrap();
    }
}

// ---- Parameters and addresses ----

#[test]
fn asset_param_is_the_reverse_of_display_order() {
    // Passing explorer hex straight through gives a program no real coin can satisfy.
    let fx = fixture();
    let mut params = fx.params.clone();
    params.usdt_asset = unhex32(USDT);
    assert_ne!(params.usdt_asset, fx.params.usdt_asset);
    let mut reversed = params.usdt_asset;
    reversed.reverse();
    assert_eq!(reversed, fx.params.usdt_asset);

    let wrong = Fx {
        htlc: Htlc::new(&params).unwrap(),
        params,
        recipient: key(1),
        sender: key(2),
    };
    assert_rejected(Case::claim(&wrong).run_claim(&wrong));
}

#[test]
fn each_parameter_changes_the_address() {
    let fx = fixture();
    let base = fx.htlc.script_pubkey();
    let mut variants = vec![];
    let mut p = fx.params.clone();
    p.payment_hash[0] ^= 1;
    variants.push(p);
    let mut p = fx.params.clone();
    p.recipient_pk = xonly(&key(3));
    variants.push(p);
    let mut p = fx.params.clone();
    p.sender_pk = xonly(&key(3));
    variants.push(p);
    let mut p = fx.params.clone();
    p.lock_height += 1;
    variants.push(p);
    let mut p = fx.params.clone();
    p.usdt_asset[0] ^= 1;
    variants.push(p);
    let mut p = fx.params.clone();
    p.amount += 1;
    variants.push(p);
    let mut p = fx.params.clone();
    p.recipient_script_hash[0] ^= 1;
    variants.push(p);
    for variant in variants {
        assert_ne!(
            Htlc::new(&variant).unwrap().script_pubkey(),
            base,
            "{variant:?}"
        );
    }
}

#[test]
fn utxo_not_paying_this_htlc_is_refused() {
    let fx = fixture();
    let mut case = Case::claim(&fx);
    case.utxos[0].script_pubkey = Script::from(vec![0x51]);
    let digest = fx.htlc.sighash_all(&case.tx, &case.utxos, 0, genesis());
    assert!(matches!(digest, Err(Error::BadUtxo(_))));
}

#[test]
fn address_derivation_is_deterministic() {
    let fx = fixture();
    let again = Htlc::new(&fx.params).unwrap();
    assert_eq!(fx.htlc.cmr(), again.cmr());
    assert_eq!(fx.htlc.script_pubkey(), again.script_pubkey());
    // A witness v1 program: OP_1 PUSH32 <output key>.
    let script = fx.htlc.script_pubkey();
    assert_eq!(script.len(), 34);
    assert_eq!(&script.as_bytes()[..2], &[0x51, 0x20]);
}

// ---- Reading the preimage back ----

fn claim_witness(fx: &Fx, preimage: [u8; 32], case: &Case) -> Vec<u8> {
    let signature = case.sign(fx, &fx.recipient);
    let (tx, _) = fx
        .htlc
        .finalize(
            case.tx.clone(),
            &case.utxos,
            0,
            genesis(),
            Spend::Claim {
                preimage,
                signature,
            },
        )
        .unwrap();
    tx.input[0].witness.script_witness[0].to_vec()
}

#[test]
fn preimage_is_recovered_from_a_claim_witness() {
    let fx = fixture();
    let witness = claim_witness(&fx, PREIMAGE, &Case::claim(&fx));
    assert_eq!(
        extract_preimage(&witness, &fx.params.payment_hash),
        Some(PREIMAGE)
    );
}

#[test]
fn preimage_extraction_finds_a_nonzero_preimage_at_its_bit_offset() {
    let preimage = [0xa5; 32];
    let fx = {
        use simplicityhl::elements::hashes::sha256;
        let mut fx = fixture();
        fx.params.payment_hash = sha256::Hash::hash(&preimage).to_byte_array();
        fx.htlc = Htlc::new(&fx.params).unwrap();
        fx
    };
    let witness = claim_witness(&fx, preimage, &Case::claim(&fx));
    assert_ne!(
        &witness[..32],
        &preimage,
        "the preimage is not byte-aligned"
    );
    assert_eq!(
        extract_preimage(&witness, &fx.params.payment_hash),
        Some(preimage)
    );
}

#[test]
fn preimage_extraction_rejects_data_with_no_preimage() {
    let hash = unhex32(PAYMENT_HASH);
    assert_eq!(extract_preimage(&[], &hash), None);
    assert_eq!(extract_preimage(&[0xff; 31], &hash), None);
    assert_eq!(extract_preimage(&[0xff; 97], &hash), None);
}
