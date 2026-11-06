//! The taker's half of a confidential swap's signing session. The maker's half lives in its signer. See
//! docs/confidential.md section 3.
//!
//! The taker holds the secret, so its signature share is what's at risk. It signs only a transaction it checked.

use liq2sol_swapkey::{
    combine, combine_plain, complete, key_spend_sighash, set_key_spend_witness, verify_share, Nonce, Session, Share, SwapOutput,
    SwapParams,
};
use simplicityhl::elements::secp256k1_zkp::{Keypair, PublicKey, Secp256k1, SecretKey, XOnlyPublicKey};
use simplicityhl::elements::{Address, BlockHash, OutPoint, Transaction, TxOut};

#[cfg(feature = "native")]
use crate::esplora::Esplora;
use crate::liquid::{network, sign_keypath, Res};

/// The swap's output, as both parties derive it. Whoever funded the coin is the one who can refund it.
pub fn swap_output(maker_key: &[u8; 33], taker_key: &[u8; 33], taker_funds: bool, lock_height: u32) -> Res<SwapOutput> {
    let funder = if taker_funds { taker_key } else { maker_key };
    let mut refund_pk = [0u8; 32];
    refund_pk.copy_from_slice(&funder[1..]);
    SwapOutput::new(&SwapParams { maker_key: *maker_key, taker_key: *taker_key, refund_pk, lock_height })
        .map_err(|e| e.to_string())
}

/// The confidential address that funds it.
pub fn swap_address(output: &SwapOutput, blinding: &SecretKey) -> Address {
    output.address(PublicKey::from_secret_key(&Secp256k1::new(), blinding), network().params)
}

/// Where a fee coin for the taker goes. Derived, so a separate address can't be swapped in.
pub fn taker_fee_address(taker_key: &[u8; 33], blinding: &SecretKey) -> Res<Address> {
    let taker = XOnlyPublicKey::from_slice(&taker_key[1..]).map_err(|e| e.to_string())?;
    let secp = Secp256k1::new();
    Ok(Address::p2tr(&secp, taker, None, Some(PublicKey::from_secret_key(&secp, blinding)), network().params))
}

/// The outputs a transaction spends, in input order, read from the chain rather than from anyone's word for it.
#[cfg(feature = "native")]
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
#[cfg(feature = "native")]
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
    answer_claim_with(key, output, coin, lock_point, maker_nonce, tx, &utxos, genesis)
}

/// The same, given the spent outputs in input order.
pub fn answer_claim_with(
    key: &Keypair,
    output: &SwapOutput,
    coin: &OutPoint,
    lock_point: &[u8; 33],
    maker_nonce: &Nonce,
    tx: &Transaction,
    utxos: &[TxOut],
    genesis: BlockHash,
) -> Res<(Nonce, Share)> {
    check_claim_shape(tx, coin)?;
    if utxos.len() != tx.input.len() {
        return Err("one spent output is needed per input".into());
    }
    if utxos[0].script_pubkey != output.script_pubkey() {
        return Err("input 0 doesn't spend this swap's output".into());
    }
    let message = key_spend_sighash(tx, utxos, 0, genesis).map_err(|e| e.to_string())?;
    let ctx = output.key_ctx();
    let session = Session::start(&seckey(key), ctx, &message).map_err(|e| e.to_string())?;
    let nonce = session.public.clone();
    let (share, _) = session.sign(&seckey(key), ctx, maker_nonce, lock_point, &message).map_err(|e| e.to_string())?;
    Ok((nonce, share))
}

/// L→S: getting our coin back early, with the maker's agreement. Spent by the key path, it looks like any claim on-chain.
pub struct EarlyRefund {
    tx: Transaction,
    utxos: Vec<TxOut>,
    message: [u8; 32],
    session: Session,
}

impl EarlyRefund {
    pub fn start_with(key: &Keypair, output: &SwapOutput, tx: Transaction, utxos: Vec<TxOut>, genesis: BlockHash) -> Res<Self> {
        if utxos.first().map(|u| &u.script_pubkey) != Some(&output.script_pubkey()) {
            return Err("input 0 doesn't spend this swap's output".into());
        }
        let message = key_spend_sighash(&tx, &utxos, 0, genesis).map_err(|e| e.to_string())?;
        let session = Session::start(&seckey(key), output.key_ctx(), &message).map_err(|e| e.to_string())?;
        Ok(Self { tx, utxos, message, session })
    }

    pub fn nonce(&self) -> Nonce {
        self.session.public.clone()
    }

    pub fn tx(&self) -> &Transaction {
        &self.tx
    }

    /// The refund with input 0 signed, or an error if the maker's share is no good. The fee coin's input is still to sign.
    pub fn finish(
        self,
        key: &Keypair,
        output: &SwapOutput,
        maker_key: &[u8; 33],
        maker_nonce: &Nonce,
        maker_share: Share,
    ) -> Res<(Transaction, Vec<TxOut>)> {
        let (ours, aggregate) = self
            .session
            .sign_plain(&seckey(key), output.key_ctx(), maker_nonce, &self.message)
            .map_err(|e| e.to_string())?;
        let signature =
            combine_plain(output.key_ctx(), &aggregate, ours, maker_share, maker_key, maker_nonce, &self.message)
                .map_err(|e| e.to_string())?;
        let mut tx = self.tx;
        set_key_spend_witness(&mut tx, 0, &signature);
        Ok((tx, self.utxos))
    }
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
    #[cfg(feature = "native")]
    pub fn start(
        chain: &Esplora,
        key: &Keypair,
        output: &SwapOutput,
        tx: Transaction,
        genesis: BlockHash,
    ) -> Res<Self> {
        let utxos = spent_outputs(chain, &tx)?;
        Self::start_with(key, output, tx, utxos, genesis)
    }

    /// The same, given the spent outputs in input order.
    pub fn start_with(
        key: &Keypair,
        output: &SwapOutput,
        tx: Transaction,
        utxos: Vec<TxOut>,
        genesis: BlockHash,
    ) -> Res<Self> {
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

/// S→L: waiting for the maker's share. Its nonce is used up, so nothing here can sign again, and a client can keep this across a
/// reload. It should: the maker gives its share once, and only this can use it.
pub struct SignedClaim {
    pub tx: Transaction,
    pub utxos: Vec<TxOut>,
    pub message: [u8; 32],
    pub aggregate: liq2sol_swapkey::AggregateNonce,
    pub share: Share,
    pub maker_nonce: Nonce,
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
