//! The taker's half of a confidential swap's signing session. The maker's half lives in its signer. See
//! docs/confidential.md section 3.
//!
//! The taker holds the secret, so its signature share is what's at risk. It signs only a transaction it checked.

use liq2sol_swapkey::{
    combine, complete, key_spend_sighash, set_key_spend_witness, verify_share, Nonce, Session, Share, SwapOutput,
};
use simplicityhl::elements::secp256k1_zkp::Keypair;
use simplicityhl::elements::{BlockHash, OutPoint, Transaction, TxOut};

use crate::liquid::{sign_keypath, Esplora, Res};

/// The outputs a transaction spends, in input order, read from the chain rather than from anyone's word for it.
pub fn spent_outputs(chain: &Esplora, tx: &Transaction) -> Res<Vec<TxOut>> {
    tx.input
        .iter()
        .map(|i| {
            let prev = chain.tx(&i.previous_output.txid)?;
            prev.output.get(i.previous_output.vout as usize).cloned().ok_or_else(|| "input's output is missing".into())
        })
        .collect()
}

/// Checks a claim's shape: the swap coin is input 0, as both parties sign for, and one fee coin is input 1.
pub fn check_claim_shape(tx: &Transaction, coin: &OutPoint) -> Res<()> {
    if tx.input.len() != 2 {
        return Err(format!("a claim has 2 inputs, this has {}", tx.input.len()));
    }
    if tx.input[0].previous_output != *coin {
        return Err("input 0 isn't the swap coin".into());
    }
    if tx.input[1].previous_output == *coin {
        return Err("the swap coin can't pay its own fee".into());
    }
    Ok(())
}

fn seckey(key: &Keypair) -> [u8; 32] {
    key.secret_bytes()
}

/// L→S: the maker claims our coin, so we sign its claim, locked to our point. Nothing is usable until we reveal the secret
/// on Solana, which pays us first. Returns what to send back.
pub fn answer_claim(
    chain: &Esplora,
    key: &Keypair,
    output: &SwapOutput,
    coin: &OutPoint,
    lock_point: &[u8; 33],
    maker_nonce: &Nonce,
    tx: &Transaction,
    genesis: BlockHash,
) -> Res<(Nonce, Share)> {
    check_claim_shape(tx, coin)?;
    let utxos = spent_outputs(chain, tx)?;
    if utxos[0].script_pubkey != output.script_pubkey() {
        return Err("input 0 doesn't spend this swap's output".into());
    }
    let message = key_spend_sighash(tx, &utxos, 0, genesis).map_err(|e| e.to_string())?;
    let ctx = output.key_ctx();
    let session = Session::start(&seckey(key), ctx, &message).map_err(|e| e.to_string())?;
    let nonce = session.public.clone();
    let (share, _) = session.sign(&seckey(key), ctx, maker_nonce, lock_point, &message).map_err(|e| e.to_string())?;
    Ok((nonce, share))
}

/// S→L: our claim of the maker's coin, part way through its signing session.
pub struct TakerClaim {
    pub tx: Transaction,
    utxos: Vec<TxOut>,
    message: [u8; 32],
    session: Session,
}

impl TakerClaim {
    /// Step one, for the unsigned claim: the nonce to send with it.
    pub fn start(
        chain: &Esplora,
        key: &Keypair,
        output: &SwapOutput,
        tx: Transaction,
        genesis: BlockHash,
    ) -> Res<Self> {
        let utxos = spent_outputs(chain, &tx)?;
        let message = key_spend_sighash(&tx, &utxos, 0, genesis).map_err(|e| e.to_string())?;
        let session = Session::start(&seckey(key), output.key_ctx(), &message).map_err(|e| e.to_string())?;
        Ok(Self { tx, utxos, message, session })
    }

    pub fn nonce(&self) -> Nonce {
        self.session.public.clone()
    }

    /// Step two: our share, given the maker's nonce. The maker sends its own share only once it holds ours.
    pub fn sign(
        self,
        key: &Keypair,
        output: &SwapOutput,
        lock_point: &[u8; 33],
        maker_nonce: &Nonce,
    ) -> Res<(Share, SignedClaim)> {
        let (share, aggregate) = self
            .session
            .sign(&seckey(key), output.key_ctx(), maker_nonce, lock_point, &self.message)
            .map_err(|e| e.to_string())?;
        let signed = SignedClaim {
            tx: self.tx,
            utxos: self.utxos,
            message: self.message,
            aggregate,
            share,
            maker_nonce: maker_nonce.clone(),
        };
        Ok((share, signed))
    }
}

/// S→L: waiting for the maker's share.
pub struct SignedClaim {
    tx: Transaction,
    utxos: Vec<TxOut>,
    message: [u8; 32],
    aggregate: liq2sol_swapkey::AggregateNonce,
    share: Share,
    maker_nonce: Nonce,
}

impl SignedClaim {
    /// Step three: finish the claim with the secret, and pay the fee from our fee coin. Publishing this reveals the secret
    /// to the maker, so the caller first confirms the maker's coin and that the transaction would be accepted.
    pub fn finish(
        self,
        fee_key: &Keypair,
        output: &SwapOutput,
        maker_key: &[u8; 33],
        maker_share: Share,
        lock_point: &[u8; 33],
        secret: &[u8; 32],
        genesis: BlockHash,
    ) -> Res<Transaction> {
        let ctx = output.key_ctx();
        verify_share(ctx, maker_share, &self.aggregate, maker_key, &self.maker_nonce, lock_point, &self.message)
            .map_err(|e| e.to_string())?;
        let adaptor = combine(ctx, &self.aggregate, [self.share, maker_share], lock_point, &self.message)
            .map_err(|e| e.to_string())?;
        let signature = complete(&adaptor, secret).map_err(|e| e.to_string())?;
        let mut tx = self.tx;
        set_key_spend_witness(&mut tx, 0, &signature);
        sign_keypath(&mut tx, &self.utxos, 1, fee_key, genesis)?;
        Ok(tx)
    }
}
