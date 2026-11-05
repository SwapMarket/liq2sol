//! The confidential Liquid leg of a swap. Specification: docs/confidential.md.
//!
//! The output is a taproot key aggregated from both parties with MuSig2. A claim is a plain key-path signature, so on-chain
//! it shows no hash, no script and no sign of being a swap. Its coin and outputs are blinded, so the amount and asset are
//! hidden too. The one who can claim gets the secret's counterpart through an adapter signature: completing it publishes
//! a signature from which the other party, and nobody else, can compute the secret.
//!
//! A refund is the one path that stands alone: a Simplicity leaf with a lock height and the sender's signature.
//!
//! Nothing on-chain checks the asset or amount any more. Each party must unblind the coin and check it before acting.

#![forbid(unsafe_code)]

use std::sync::Arc;

use musig2::secp::{MaybeScalar, Point, Scalar};
use musig2::{AdaptorSignature, AggNonce, KeyAggContext, LiftedSignature, PartialSignature, PubNonce, SecNonce};
use simplicityhl::elements::secp256k1_zkp::{self, XOnlyPublicKey};
use simplicityhl::elements::sighash::{Prevouts, SighashCache};
use simplicityhl::elements::taproot::{ControlBlock, LeafVersion, TaprootBuilder, TaprootSpendInfo};
use simplicityhl::elements::{
    Address, AddressParams, BlockHash, PeginWitness, RangeProof, SchnorrSighashType, Script, Transaction, TxInWitness,
    TxOut, Witness,
};
use simplicityhl::simplicity::bitcoin::secp256k1::SECP256K1;
use simplicityhl::simplicity::jet::elements::{ElementsEnv, ElementsUtxo};
use simplicityhl::simplicity::{BitMachine, Cmr};
use simplicityhl::{Arguments, CompiledProgram, TemplateAst, UnresolvedValues, WitnessValues};

pub const REFUND_SOURCE: &str = include_str!("refund.simf");
const VALIDATION_WEIGHT_OFFSET: u64 = 50;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Keys(String),
    Compile(String),
    /// The refund doesn't satisfy the leaf: wrong key, or before the lock height.
    Rejected(String),
    BadUtxo(String),
    OverBudget {
        cost_wu: u64,
        budget_wu: u64,
    },
    /// A counterparty's signature share doesn't check out. Stop, and never fund or reveal on the strength of it.
    BadPartialSignature,
    Signing(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for Error {}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The swap's parameters. Keys are x-only, as they appear in signatures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapParams {
    /// Compressed keys of both parties. The order is fixed, so both derive the same aggregate.
    pub maker_key: [u8; 33],
    pub taker_key: [u8; 33],
    /// Who can refund, and from when.
    pub refund_pk: [u8; 32],
    pub lock_height: u32,
}

/// One swap's output. Both parties derive this themselves, and never accept an address from the other.
pub struct SwapOutput {
    program: CompiledProgram,
    cmr: Cmr,
    spend_info: TaprootSpendInfo,
    /// The aggregate of both keys, tweaked to the taproot output key.
    key_ctx: KeyAggContext,
}

impl SwapOutput {
    pub fn new(p: &SwapParams) -> Result<Self, Error> {
        let keys_err = |e: String| Error::Keys(e);
        let parse = |b: &[u8; 33]| Point::from_slice(b).map_err(|e| keys_err(e.to_string()));
        let ctx =
            KeyAggContext::new([parse(&p.maker_key)?, parse(&p.taker_key)?]).map_err(|e| keys_err(e.to_string()))?;

        let template = TemplateAst::new(REFUND_SOURCE, Box::new(simplicityhl::ast::ElementsJetHinter::new()))
            .map_err(|e| Error::Compile(e.to_string()))?;
        let json = serde_json::json!({
            "SENDER_PK": format!("0x{}", hex(&p.refund_pk)),
            "LOCK_HEIGHT": p.lock_height.to_string(),
        });
        let unresolved: UnresolvedValues =
            serde_json::from_str(&json.to_string()).map_err(|e| Error::Compile(e.to_string()))?;
        let arguments: Arguments = unresolved.resolve(template.parameters()).map_err(Error::Compile)?;
        let program = template.instantiate(arguments, false).map_err(Error::Compile)?;
        let cmr = program.commit().cmr();

        // The internal key is the aggregate itself, so a key-path spend needs both parties.
        let internal_bytes: [u8; 32] = {
            let point: Point = ctx.aggregated_pubkey();
            point.serialize_xonly()
        };
        let internal = XOnlyPublicKey::from_slice(&internal_bytes).map_err(|e| keys_err(e.to_string()))?;
        let spend_info = TaprootBuilder::new()
            .add_leaf_with_ver(0, Script::from(cmr.as_ref().to_vec()), simplicityhl::simplicity::leaf_version())
            .map_err(|e| Error::Compile(format!("{e:?}")))?
            .finalize(SECP256K1, internal)
            .map_err(|e| Error::Compile(format!("{e:?}")))?;

        // Elements uses its own tag, "TapTweak/elements", so take the tweak from rust-elements. The library's built-in
        // taproot tweak uses Bitcoin's tag, and would give a key no Liquid node accepts.
        let tweak = Scalar::from_slice(spend_info.tap_tweak().as_ref()).map_err(|e| keys_err(e.to_string()))?;
        let key_ctx = ctx.with_xonly_tweak(tweak).map_err(|e| keys_err(e.to_string()))?;
        let output: Point = key_ctx.aggregated_pubkey();
        if output.serialize_xonly() != spend_info.output_key().as_inner().serialize() {
            return Err(Error::Keys("the signing key doesn't match the taproot output key".into()));
        }
        Ok(Self { program, cmr, spend_info, key_ctx })
    }

    /// The confidential address to fund. The blinding key is shared by both parties, so each can read the coin.
    pub fn address(&self, blinder: secp256k1_zkp::PublicKey, params: &'static AddressParams) -> Address {
        Address::p2tr(SECP256K1, self.spend_info.internal_key(), self.spend_info.merkle_root(), Some(blinder), params)
    }

    pub fn script_pubkey(&self) -> Script {
        self.address_unblinded().script_pubkey()
    }

    fn address_unblinded(&self) -> Address {
        Address::p2tr(
            SECP256K1,
            self.spend_info.internal_key(),
            self.spend_info.merkle_root(),
            None,
            &AddressParams::LIQUID,
        )
    }

    pub fn key_ctx(&self) -> &KeyAggContext {
        &self.key_ctx
    }

    pub fn cmr(&self) -> [u8; 32] {
        self.cmr.to_byte_array()
    }

    fn control_block(&self) -> Result<ControlBlock, Error> {
        self.spend_info
            .control_block(&(Script::from(self.cmr.as_ref().to_vec()), Self::leaf_version()))
            .ok_or_else(|| Error::Compile("no control block".into()))
    }

    fn leaf_version() -> LeafVersion {
        simplicityhl::simplicity::leaf_version()
    }

    fn env(
        &self,
        tx: &Transaction,
        utxos: &[TxOut],
        index: usize,
        genesis: BlockHash,
    ) -> Result<ElementsEnv<Arc<Transaction>>, Error> {
        if utxos.len() != tx.input.len() {
            return Err(Error::BadUtxo(format!("{} utxos for {} inputs", utxos.len(), tx.input.len())));
        }
        if utxos.get(index).map(|u| &u.script_pubkey) != Some(&self.script_pubkey()) {
            return Err(Error::BadUtxo(format!("input {index} does not pay this output")));
        }
        let all = utxos
            .iter()
            .map(|u| ElementsUtxo { script_pubkey: u.script_pubkey.clone(), asset: u.asset, value: u.value })
            .collect();
        Ok(ElementsEnv::new(Arc::new(tx.clone()), all, index as u32, self.cmr, self.control_block()?, None, genesis))
    }

    /// The message the refund signature signs.
    pub fn refund_sighash(
        &self,
        tx: &Transaction,
        utxos: &[TxOut],
        index: usize,
        genesis: BlockHash,
    ) -> Result<[u8; 32], Error> {
        Ok(self.env(tx, utxos, index, genesis)?.c_tx_env().sighash_all().to_byte_array())
    }

    /// Check the refund against the leaf, and set the input's witness.
    pub fn finalize_refund(
        &self,
        mut tx: Transaction,
        utxos: &[TxOut],
        index: usize,
        genesis: BlockHash,
        signature: [u8; 64],
    ) -> Result<Transaction, Error> {
        let env = self.env(&tx, utxos, index, genesis)?;
        let json =
            serde_json::json!({ "SENDER_SIG": { "value": format!("0x{}", hex(&signature)), "type": "Signature" } });
        let unresolved: UnresolvedValues =
            serde_json::from_str(&json.to_string()).map_err(|e| Error::Compile(e.to_string()))?;
        let values: WitnessValues = unresolved.resolve(self.program.witness_types()).map_err(Error::Compile)?;
        let satisfied = self.program.satisfy_with_env(values, Some(&env)).map_err(Error::Rejected)?;
        let pruned = satisfied.redeem();
        let mut machine = BitMachine::for_program(pruned).map_err(|e| Error::Rejected(e.to_string()))?;
        machine.exec(pruned, &env).map_err(|e| Error::Rejected(e.to_string()))?;
        let (program_bytes, witness_bytes) = pruned.to_vec_with_witness();
        let items = [witness_bytes, program_bytes, self.cmr.as_ref().to_vec(), self.control_block()?.serialize()];
        let size = 1 + items.iter().map(|i| 3 + i.len() as u64).sum::<u64>();
        let budget_wu = size + VALIDATION_WEIGHT_OFFSET;
        let cost_wu = simplicityhl::simplicity::bitcoin::Weight::from(pruned.bounds().cost).to_wu();
        if cost_wu > budget_wu {
            return Err(Error::OverBudget { cost_wu, budget_wu });
        }
        tx.input[index].witness = TxInWitness {
            amount_rangeproof: RangeProof::EMPTY,
            inflation_keys_rangeproof: RangeProof::EMPTY,
            script_witness: Witness::from_slice(&items),
            pegin_witness: PeginWitness::EMPTY,
        };
        Ok(tx)
    }
}

/// The taproot key-path message for one input, under Elements' own sighash.
pub fn key_spend_sighash(
    tx: &Transaction,
    utxos: &[TxOut],
    index: usize,
    genesis: BlockHash,
) -> Result<[u8; 32], Error> {
    let hash = SighashCache::new(tx)
        .taproot_key_spend_signature_hash(index, &Prevouts::All(utxos), SchnorrSighashType::All, genesis)
        .map_err(|e| Error::Signing(e.to_string()))?;
    Ok(hash.to_byte_array())
}

/// Set a key-path witness from a finished signature.
pub fn set_key_spend_witness(tx: &mut Transaction, index: usize, signature: &[u8; 64]) {
    let mut bytes = signature.to_vec();
    bytes.push(SchnorrSighashType::All as u8);
    tx.input[index].witness.script_witness = Witness::from_slice(&[bytes]);
}

/// One party's nonce for one signing session.
///
/// The secret half must be used once and never stored. Signing two different messages with one nonce reveals the key, so a
/// crash simply abandons the session, and a new one starts with fresh randomness.
pub struct Session {
    secnonce: SecNonce,
    pub public: PubNonce,
}

impl Session {
    pub fn start(seckey: &[u8; 32], ctx: &KeyAggContext, message: &[u8; 32]) -> Result<Self, Error> {
        let key = Scalar::from_slice(seckey).map_err(|e| Error::Keys(e.to_string()))?;
        let aggregated: Point = ctx.aggregated_pubkey();
        let mut randomness = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut randomness);
        let secnonce = SecNonce::build_with_seckey(randomness, key)
            .with_aggregated_pubkey(aggregated)
            .with_message(message)
            .build();
        let public = secnonce.public_nonce();
        Ok(Self { secnonce, public })
    }

    /// Our share of the adapter signature. Consumes the session, so its nonce can't sign anything else.
    pub fn sign(
        self,
        seckey: &[u8; 32],
        ctx: &KeyAggContext,
        their_nonce: &PubNonce,
        adaptor_point: &[u8; 33],
        message: &[u8; 32],
    ) -> Result<(PartialSignature, AggNonce), Error> {
        let key = Scalar::from_slice(seckey).map_err(|e| Error::Keys(e.to_string()))?;
        let point = Point::from_slice(adaptor_point).map_err(|e| Error::Keys(e.to_string()))?;
        let agg: AggNonce = [self.public.clone(), their_nonce.clone()].iter().sum();
        let partial: PartialSignature = musig2::adaptor::sign_partial(ctx, key, self.secnonce, &agg, point, message)
            .map_err(|e| Error::Signing(e.to_string()))?;
        Ok((partial, agg))
    }
}

/// Check the other party's share. Without this, a bad share would only show up after we had funded.
pub fn verify_share(
    ctx: &KeyAggContext,
    share: PartialSignature,
    agg: &AggNonce,
    their_key: &[u8; 33],
    their_nonce: &PubNonce,
    adaptor_point: &[u8; 33],
    message: &[u8; 32],
) -> Result<(), Error> {
    let key = Point::from_slice(their_key).map_err(|e| Error::Keys(e.to_string()))?;
    let point = Point::from_slice(adaptor_point).map_err(|e| Error::Keys(e.to_string()))?;
    musig2::adaptor::verify_partial(ctx, share, agg, point, key, their_nonce, message)
        .map_err(|_| Error::BadPartialSignature)
}

/// Combine both shares into the adapter signature, and check it against the output key before relying on it.
pub fn combine(
    ctx: &KeyAggContext,
    agg: &AggNonce,
    shares: [PartialSignature; 2],
    adaptor_point: &[u8; 33],
    message: &[u8; 32],
) -> Result<AdaptorSignature, Error> {
    let point = Point::from_slice(adaptor_point).map_err(|e| Error::Keys(e.to_string()))?;
    let sig: AdaptorSignature = musig2::adaptor::aggregate_partial_signatures(ctx, agg, point, shares, message)
        .map_err(|e| Error::Signing(e.to_string()))?;
    let output: Point = ctx.aggregated_pubkey();
    musig2::adaptor::verify_single(output, &sig, message, point).map_err(|_| Error::BadPartialSignature)?;
    Ok(sig)
}

/// Finish the signature with the secret, for the party who holds it. This is what publishes the secret.
pub fn complete(sig: &AdaptorSignature, secret: &[u8; 32]) -> Result<[u8; 64], Error> {
    let t = MaybeScalar::Valid(Scalar::from_slice(secret).map_err(|e| Error::Keys(e.to_string()))?);
    let finished: LiftedSignature = sig.adapt(t).ok_or_else(|| Error::Signing("the secret doesn't fit".into()))?;
    Ok(finished.serialize())
}

/// Read the secret out of a published signature, for the party holding the adapter signature.
pub fn extract_secret(sig: &AdaptorSignature, published: &[u8; 64], adaptor_point: &[u8; 33]) -> Option<[u8; 32]> {
    let finished = LiftedSignature::from_bytes(published).ok()?;
    let t: MaybeScalar = sig.reveal_secret(&finished)?;
    let point = Point::from_slice(adaptor_point).ok()?;
    if t.base_point_mul() != musig2::secp::MaybePoint::Valid(point) {
        return None;
    }
    match t {
        MaybeScalar::Valid(s) => Some(s.serialize()),
        MaybeScalar::Zero => None,
    }
}

/// The x coordinate of the generator, as a scalar. The Solana program's point check is built around it.
fn gx() -> Result<Scalar, Error> {
    Scalar::from_hex("79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798")
        .map_err(|e| Error::Keys(e.to_string()))
}

/// What a claim reveals on Solana for this secret: `s = t * Gx mod n`.
pub fn solana_value(secret: &[u8; 32]) -> Result<[u8; 32], Error> {
    let t = Scalar::from_slice(secret).map_err(|e| Error::Keys(e.to_string()))?;
    Ok((t * gx()?).serialize())
}

/// The secret, from what a claim revealed on Solana: `t = s / Gx mod n`.
pub fn secret_from_solana(revealed: &[u8; 32]) -> Result<[u8; 32], Error> {
    let s = Scalar::from_slice(revealed).map_err(|e| Error::Keys(e.to_string()))?;
    Ok((s * gx()?.invert()).serialize())
}

/// The point as the Solana program stores it: x then y.
pub fn solana_point(secret: &[u8; 32]) -> Result<[u8; 64], Error> {
    let t = Scalar::from_slice(secret).map_err(|e| Error::Keys(e.to_string()))?;
    let uncompressed = t.base_point_mul().serialize_uncompressed();
    let mut out = [0u8; 64];
    out.copy_from_slice(&uncompressed[1..]);
    Ok(out)
}

/// The compressed public point for a secret.
pub fn point_of(secret: &[u8; 32]) -> Result<[u8; 33], Error> {
    let t = Scalar::from_slice(secret).map_err(|e| Error::Keys(e.to_string()))?;
    Ok(t.base_point_mul().serialize())
}
