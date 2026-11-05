//! The liq2sol Liquid HTLC: instantiate the SimplicityHL template, derive its address, and finalize spends.
//!
//! Specification: `docs/contracts.md`. All byte arrays are in the order the Simplicity jets see them, which for
//! asset ids is the reverse of the hex shown by explorers; use [`asset_param`] to convert.

#![forbid(unsafe_code)]

use std::sync::Arc;

use simplicityhl::elements::secp256k1_zkp::XOnlyPublicKey;
use simplicityhl::elements::taproot::{
    ControlBlock, LeafVersion, TaprootBuilder, TaprootSpendInfo,
};
use simplicityhl::elements::{
    Address, AddressParams, AssetId, BlockHash, PeginWitness, RangeProof, Script, Transaction,
    TxInWitness, TxOut, Witness,
};
use simplicityhl::simplicity::bitcoin::secp256k1::SECP256K1;
use simplicityhl::simplicity::jet::elements::{ElementsEnv, ElementsUtxo};
use simplicityhl::simplicity::{BitMachine, Cmr};
use simplicityhl::{Arguments, CompiledProgram, TemplateAst, UnresolvedValues, WitnessValues};

/// The HTLC source. Changing it changes every address, so its CMR is pinned by a test.
pub const SOURCE: &str = include_str!("htlc.simf");

/// The BIP-341 unspendable internal key, so only the Simplicity leaf can spend.
pub const NUMS_KEY: [u8; 32] = [
    0x50, 0x92, 0x9b, 0x74, 0xc1, 0xa0, 0x49, 0x54, 0xb7, 0x8b, 0x4b, 0x60, 0x35, 0xe9, 0x7a, 0x5e,
    0x07, 0x8a, 0x5a, 0x0f, 0x28, 0xec, 0x96, 0xd5, 0x47, 0xbf, 0xee, 0x9a, 0xce, 0x80, 0x3a, 0xc0,
];

/// Free validation weight added to the serialized witness size to give a Simplicity spend's budget.
const VALIDATION_WEIGHT_OFFSET: u64 = 50;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The program could not be compiled or instantiated.
    Compile(String),
    /// The witness did not fit the program.
    Witness(String),
    /// The spend does not satisfy the contract: bad preimage, signature, asset, amount, output or lock time.
    Rejected(String),
    /// The UTXO list does not describe this input, or does not pay this HTLC.
    BadUtxo(String),
    /// Execution would cost more than the input's budget.
    OverBudget { cost_wu: u64, budget_wu: u64 },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for Error {}

/// The parameters baked into one swap's program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HtlcParams {
    pub payment_hash: [u8; 32],
    pub recipient_pk: [u8; 32],
    pub sender_pk: [u8; 32],
    pub lock_height: u32,
    pub usdt_asset: [u8; 32],
    pub amount: u64,
    pub recipient_script_hash: [u8; 32],
}

/// How an HTLC input is being spent.
#[derive(Debug, Clone, Copy)]
pub enum Spend {
    Claim {
        preimage: [u8; 32],
        signature: [u8; 64],
    },
    Refund {
        signature: [u8; 64],
    },
}

/// Convert an asset id to the byte order the program compares against.
pub fn asset_param(asset: &AssetId) -> [u8; 32] {
    asset.to_byte_array()
}

/// SHA-256 of a scriptPubKey, as `RECIPIENT_SCRIPT_HASH` expects.
pub fn script_hash(script: &Script) -> [u8; 32] {
    use simplicityhl::simplicity::hashes::sha256;
    sha256::Hash::hash(script.as_bytes()).to_byte_array()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// An instantiated HTLC.
pub struct Htlc {
    program: CompiledProgram,
    cmr: Cmr,
    spend_info: TaprootSpendInfo,
}

impl Htlc {
    pub fn new(params: &HtlcParams) -> Result<Self, Error> {
        let template = TemplateAst::new(
            SOURCE,
            Box::new(simplicityhl::ast::ElementsJetHinter::new()),
        )
        .map_err(|e| Error::Compile(e.to_string()))?;
        let json = serde_json::json!({
            "PAYMENT_HASH": format!("0x{}", hex(&params.payment_hash)),
            "RECIPIENT_PK": format!("0x{}", hex(&params.recipient_pk)),
            "SENDER_PK": format!("0x{}", hex(&params.sender_pk)),
            "LOCK_HEIGHT": params.lock_height.to_string(),
            "USDT_ASSET": format!("0x{}", hex(&params.usdt_asset)),
            "AMOUNT": params.amount.to_string(),
            "RECIPIENT_SCRIPT_HASH": format!("0x{}", hex(&params.recipient_script_hash)),
        });
        let unresolved: UnresolvedValues =
            serde_json::from_str(&json.to_string()).map_err(|e| Error::Compile(e.to_string()))?;
        let arguments: Arguments = unresolved
            .resolve(template.parameters())
            .map_err(Error::Compile)?;
        let program = template
            .instantiate(arguments, false)
            .map_err(Error::Compile)?;
        let cmr = program.commit().cmr();

        let internal =
            XOnlyPublicKey::from_slice(&NUMS_KEY).map_err(|e| Error::Compile(e.to_string()))?;
        let spend_info = TaprootBuilder::new()
            .add_leaf_with_ver(0, Self::leaf_script(cmr), Self::leaf_version())
            .map_err(|e| Error::Compile(format!("{e:?}")))?
            .finalize(SECP256K1, internal)
            .map_err(|e| Error::Compile(format!("{e:?}")))?;
        Ok(Self {
            program,
            cmr,
            spend_info,
        })
    }

    fn leaf_script(cmr: Cmr) -> Script {
        Script::from(cmr.as_ref().to_vec())
    }

    fn leaf_version() -> LeafVersion {
        simplicityhl::simplicity::leaf_version()
    }

    pub fn cmr(&self) -> [u8; 32] {
        self.cmr.to_byte_array()
    }

    /// The non-confidential address to fund. Blinded payments cannot be claimed.
    pub fn address(&self, params: &'static AddressParams) -> Address {
        Address::p2tr(
            SECP256K1,
            self.spend_info.internal_key(),
            self.spend_info.merkle_root(),
            None,
            params,
        )
    }

    pub fn script_pubkey(&self) -> Script {
        self.address(&AddressParams::LIQUID).script_pubkey()
    }

    fn control_block(&self) -> Result<ControlBlock, Error> {
        self.spend_info
            .control_block(&(Self::leaf_script(self.cmr), Self::leaf_version()))
            .ok_or_else(|| Error::Compile("no control block for the Simplicity leaf".into()))
    }

    fn env(
        &self,
        tx: &Transaction,
        utxos: &[TxOut],
        index: usize,
        genesis: BlockHash,
    ) -> Result<ElementsEnv<Arc<Transaction>>, Error> {
        if utxos.len() != tx.input.len() {
            return Err(Error::BadUtxo(format!(
                "{} utxos for {} inputs",
                utxos.len(),
                tx.input.len()
            )));
        }
        let utxo = utxos
            .get(index)
            .ok_or_else(|| Error::BadUtxo(format!("no input {index}")))?;
        if utxo.script_pubkey != self.script_pubkey() {
            return Err(Error::BadUtxo(format!(
                "input {index} does not pay this HTLC"
            )));
        }
        let utxos = utxos
            .iter()
            .map(|u| ElementsUtxo {
                script_pubkey: u.script_pubkey.clone(),
                asset: u.asset,
                value: u.value,
            })
            .collect();
        Ok(ElementsEnv::new(
            Arc::new(tx.clone()),
            utxos,
            index as u32,
            self.cmr,
            self.control_block()?,
            None,
            genesis,
        ))
    }

    /// The message to sign with BIP-340 for this input. It commits to all inputs and outputs.
    pub fn sighash_all(
        &self,
        tx: &Transaction,
        utxos: &[TxOut],
        index: usize,
        genesis: BlockHash,
    ) -> Result<[u8; 32], Error> {
        Ok(self
            .env(tx, utxos, index, genesis)?
            .c_tx_env()
            .sighash_all()
            .to_byte_array())
    }

    /// Run the contract against the transaction and, if it passes, set the input's witness.
    ///
    /// This is a local check of the same program consensus runs, so an invalid spend fails here.
    pub fn finalize(
        &self,
        mut tx: Transaction,
        utxos: &[TxOut],
        index: usize,
        genesis: BlockHash,
        spend: Spend,
    ) -> Result<(Transaction, SpendReport), Error> {
        let env = self.env(&tx, utxos, index, genesis)?;
        let witness_values = self.witness_values(spend)?;
        let satisfied = self
            .program
            .satisfy_with_env(witness_values, Some(&env))
            .map_err(Error::Rejected)?;
        let pruned = satisfied.redeem();
        let mut machine =
            BitMachine::for_program(pruned).map_err(|e| Error::Rejected(e.to_string()))?;
        machine
            .exec(pruned, &env)
            .map_err(|e| Error::Rejected(e.to_string()))?;

        let (program_bytes, witness_bytes) = pruned.to_vec_with_witness();
        let report = SpendReport {
            program_bytes: program_bytes.len(),
            witness_bytes: witness_bytes.len(),
            ..Default::default()
        };
        let items = [
            witness_bytes,
            program_bytes,
            self.cmr.as_ref().to_vec(),
            self.control_block()?.serialize(),
        ];
        let budget_wu = serialized_stack_len(&items) + VALIDATION_WEIGHT_OFFSET;
        let stack = Witness::from_slice(&items);
        let cost_wu = simplicityhl::simplicity::bitcoin::Weight::from(pruned.bounds().cost).to_wu();
        if cost_wu > budget_wu {
            return Err(Error::OverBudget { cost_wu, budget_wu });
        }
        tx.input[index].witness = TxInWitness {
            amount_rangeproof: RangeProof::EMPTY,
            inflation_keys_rangeproof: RangeProof::EMPTY,
            script_witness: stack,
            pegin_witness: PeginWitness::EMPTY,
        };
        Ok((
            tx,
            SpendReport {
                cost_wu,
                budget_wu,
                ..report
            },
        ))
    }

    /// The unpruned (program, witness) encoding. Consensus rejects it; only the negative test uses this.
    #[doc(hidden)]
    pub fn unpruned_for_test(&self, spend: Spend) -> Result<(Vec<u8>, Vec<u8>), Error> {
        let satisfied = self
            .program
            .satisfy(self.witness_values(spend)?)
            .map_err(Error::Rejected)?;
        Ok(satisfied.redeem().to_vec_with_witness())
    }

    fn witness_values(&self, spend: Spend) -> Result<WitnessValues, Error> {
        let value = match spend {
            Spend::Claim {
                preimage,
                signature,
            } => {
                format!("Left((0x{}, 0x{}))", hex(&preimage), hex(&signature))
            }
            Spend::Refund { signature } => format!("Right(0x{})", hex(&signature)),
        };
        let json = serde_json::json!({
            "CLAIM_OR_REFUND": { "value": value, "type": "Either<(u256, Signature), Signature>" }
        });
        // The deserializer borrows its strings, so it needs text rather than a `serde_json::Value`.
        let unresolved: UnresolvedValues =
            serde_json::from_str(&json.to_string()).map_err(|e| Error::Witness(e.to_string()))?;
        unresolved
            .resolve(self.program.witness_types())
            .map_err(Error::Witness)
    }
}

/// Recover the preimage from a claim's Simplicity witness, the first item of the input's witness stack.
///
/// The witness is a bit string and the 1-bit branch tag comes first, so the preimage is not byte-aligned. This scans
/// every bit offset and accepts only a value that hashes to `payment_hash`, so it cannot return a wrong preimage and
/// does not depend on the encoder's layout.
pub fn extract_preimage(simplicity_witness: &[u8], payment_hash: &[u8; 32]) -> Option<[u8; 32]> {
    use simplicityhl::simplicity::hashes::sha256;
    let bits = simplicity_witness.len().checked_mul(8)?;
    for offset in 0..=bits.checked_sub(256)? {
        let (byte, shift) = (offset / 8, offset % 8);
        let mut candidate = [0u8; 32];
        for (i, out) in candidate.iter_mut().enumerate() {
            let high = simplicity_witness[byte + i] << shift;
            let low = if shift == 0 {
                0
            } else {
                simplicity_witness.get(byte + i + 1).copied().unwrap_or(0) >> (8 - shift)
            };
            *out = high | low;
        }
        if &sha256::Hash::hash(&candidate).to_byte_array() == payment_hash {
            return Some(candidate);
        }
    }
    None
}

/// Serialized size of a witness stack: a compact-size count, then each item with a compact-size length.
fn serialized_stack_len(items: &[Vec<u8>]) -> u64 {
    fn compact_size(n: usize) -> u64 {
        match n {
            0..=0xfc => 1,
            0xfd..=0xffff => 3,
            0x1_0000..=0xffff_ffff => 5,
            _ => 9,
        }
    }
    compact_size(items.len())
        + items
            .iter()
            .map(|i| compact_size(i.len()) + i.len() as u64)
            .sum::<u64>()
}

/// Measurements of a finalized spend.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpendReport {
    pub program_bytes: usize,
    pub witness_bytes: usize,
    pub cost_wu: u64,
    pub budget_wu: u64,
}
