//! Solana accounts and transactions, with no Solana libraries. `tests/parity.rs` compares each with the native builders.

use curve25519_dalek::edwards::CompressedEdwardsY;
use sha2::{Digest, Sha256};

pub type Key = [u8; 32];

pub const SYSTEM: Key = [0; 32];

pub fn key(text: &str) -> Result<Key, String> {
    let bytes = bs58::decode(text).into_vec().map_err(|e| format!("bad Solana address: {e}"))?;
    bytes.try_into().map_err(|_| "a Solana address is 32 bytes".to_string())
}

pub fn text(key: &Key) -> String {
    bs58::encode(key).into_string()
}

fn known(text: &str) -> Key {
    key(text).expect("a constant address")
}

pub fn token_program() -> Key {
    known("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA")
}

pub fn ata_program() -> Key {
    known("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL")
}

fn compute_budget() -> Key {
    known("ComputeBudget111111111111111111111111111111")
}

/// The first off-curve address for these seeds, as the runtime derives it.
pub fn find_pda(seeds: &[&[u8]], program: &Key) -> Key {
    for bump in (0..=255u8).rev() {
        let mut hash = Sha256::new();
        for seed in seeds {
            hash.update(seed);
        }
        hash.update([bump]);
        hash.update(program);
        hash.update(b"ProgramDerivedAddress");
        let candidate: Key = hash.finalize().into();
        if CompressedEdwardsY(candidate).decompress().is_none() {
            return candidate;
        }
    }
    panic!("no program address");
}

pub fn escrow_pda(initiator: &Key, lock_point: &[u8; 64], program: &Key) -> Key {
    find_pda(&[b"escrow", initiator, &lock_point[..32]], program)
}

pub fn vault_pda(escrow: &Key, program: &Key) -> Key {
    find_pda(&[b"vault", escrow], program)
}

pub fn ata(owner: &Key, mint: &Key) -> Key {
    find_pda(&[owner, &token_program(), mint], &ata_program())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Meta {
    pub key: Key,
    pub signer: bool,
    pub writable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ix {
    pub program: Key,
    pub accounts: Vec<Meta>,
    pub data: Vec<u8>,
}

fn w(key: Key) -> Meta {
    Meta { key, signer: false, writable: true }
}

fn r(key: Key) -> Meta {
    Meta { key, signer: false, writable: false }
}

fn s(key: Key) -> Meta {
    Meta { key, signer: true, writable: true }
}

fn anchor_data(name: &str) -> Vec<u8> {
    Sha256::digest(format!("global:{name}").as_bytes())[..8].to_vec()
}

pub fn lock_ix(
    program: &Key,
    mint: &Key,
    initiator: &Key,
    lock_point: &[u8; 64],
    recipient: &Key,
    amount: u64,
    deadline_slot: u64,
    close_on_restart: bool,
) -> Ix {
    let escrow = escrow_pda(initiator, lock_point, program);
    let mut data = anchor_data("lock");
    data.extend_from_slice(lock_point);
    data.extend_from_slice(recipient);
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(&deadline_slot.to_le_bytes());
    data.push(u8::from(close_on_restart));
    Ix {
        program: *program,
        accounts: vec![
            s(*initiator),
            w(escrow),
            w(vault_pda(&escrow, program)),
            w(ata(initiator, mint)),
            r(*mint),
            r(token_program()),
            r(SYSTEM),
        ],
        data,
    }
}

/// The devnet faucet's account recording that this wallet has claimed.
pub fn faucet_claim(program: &Key, owner: &Key) -> Key {
    find_pda(&[b"claim", owner], program)
}

/// Claim from the devnet faucet. It pays only the claimant's own token account.
pub fn faucet_ix(program: &Key, mint: &Key, claimant: &Key) -> Ix {
    Ix {
        program: *program,
        accounts: vec![
            s(*claimant),
            w(faucet_claim(program, claimant)),
            w(find_pda(&[b"vault"], program)),
            w(ata(claimant, mint)),
            r(token_program()),
            r(SYSTEM),
        ],
        data: anchor_data("drip"),
    }
}

/// Create the recipient's token account if it doesn't exist. Idempotent.
pub fn create_ata_ix(payer: &Key, owner: &Key, mint: &Key) -> Ix {
    Ix {
        program: ata_program(),
        accounts: vec![s(*payer), w(ata(owner, mint)), r(*owner), r(*mint), r(SYSTEM), r(token_program())],
        data: vec![1],
    }
}

pub fn claim_ix(
    program: &Key,
    mint: &Key,
    address: &Key,
    recipient: &Key,
    rent_payer: &Key,
    revealed: &[u8; 32],
) -> Ix {
    let mut data = anchor_data("claim");
    data.extend_from_slice(revealed);
    Ix {
        program: *program,
        accounts: vec![
            w(*address),
            w(vault_pda(address, program)),
            w(ata(recipient, mint)),
            w(*rent_payer),
            r(token_program()),
        ],
        data,
    }
}

pub fn refund_ix(program: &Key, address: &Key, refund_token: &Key, rent_payer: &Key) -> Ix {
    Ix {
        program: *program,
        accounts: vec![
            w(*address),
            w(vault_pda(address, program)),
            w(*refund_token),
            w(*rent_payer),
            r(token_program()),
        ],
        data: anchor_data("refund"),
    }
}

/// A priority fee, so a claim still lands when the cluster is busy.
pub fn budget_ixs(units: u32, micro_lamports: u64) -> Vec<Ix> {
    let mut limit = vec![2];
    limit.extend_from_slice(&units.to_le_bytes());
    let mut price = vec![3];
    price.extend_from_slice(&micro_lamports.to_le_bytes());
    vec![
        Ix { program: compute_budget(), accounts: vec![], data: limit },
        Ix { program: compute_budget(), accounts: vec![], data: price },
    ]
}

fn shortvec(mut n: usize, out: &mut Vec<u8>) {
    loop {
        let byte = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// A legacy message. The wallet signs it, and the app sends it.
pub fn message(payer: &Key, ixs: &[Ix], blockhash: &Key) -> Vec<u8> {
    let mut metas: Vec<Meta> = vec![Meta { key: *payer, signer: true, writable: true }];
    let mut note = |m: Meta| match metas.iter_mut().find(|x| x.key == m.key) {
        Some(x) => {
            x.signer |= m.signer;
            x.writable |= m.writable;
        }
        None => metas.push(m),
    };
    for ix in ixs {
        for a in &ix.accounts {
            note(a.clone());
        }
        note(r(ix.program));
    }
    // The payer first, then signers before others and writable before read-only, sorted within each group.
    let (first, rest) = metas.split_at_mut(1);
    rest.sort_by_key(|m| (!m.signer, !m.writable, m.key));
    let keys: Vec<&Meta> = first.iter().chain(rest.iter()).collect();
    let signed = keys.iter().filter(|m| m.signer).count();
    let ro_signed = keys.iter().filter(|m| m.signer && !m.writable).count();
    let ro_unsigned = keys.iter().filter(|m| !m.signer && !m.writable).count();
    let index = |k: &Key| keys.iter().position(|m| m.key == *k).expect("every key is listed") as u8;

    let mut out = vec![signed as u8, ro_signed as u8, ro_unsigned as u8];
    shortvec(keys.len(), &mut out);
    for m in &keys {
        out.extend_from_slice(&m.key);
    }
    out.extend_from_slice(blockhash);
    shortvec(ixs.len(), &mut out);
    for ix in ixs {
        out.push(index(&ix.program));
        shortvec(ix.accounts.len(), &mut out);
        for a in &ix.accounts {
            out.push(index(&a.key));
        }
        shortvec(ix.data.len(), &mut out);
        out.extend_from_slice(&ix.data);
    }
    out
}

/// A transaction with an empty signature slot for each signer, as wallets expect.
pub fn unsigned_tx(message: &[u8]) -> Vec<u8> {
    let signers = message[0] as usize;
    let mut out = vec![];
    shortvec(signers, &mut out);
    out.resize(out.len() + 64 * signers, 0);
    out.extend_from_slice(message);
    out
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct EscrowAccount {
    pub initiator: String,
    pub recipient: String,
    pub refund_token: String,
    pub rent_payer: String,
    /// A string, because JavaScript numbers lose precision above 2^53.
    pub amount: String,
    pub lock_point: String,
    pub created_slot: u64,
    pub deadline_slot: u64,
    pub close_on_restart: bool,
    pub extended: bool,
    pub state: String,
    pub revealed: String,
    pub settled_slot: u64,
}

/// Read an escrow from its account data, refusing anything that isn't one.
pub fn parse_escrow(data: &[u8]) -> Result<EscrowAccount, String> {
    let disc = &Sha256::digest(b"account:Escrow")[..8];
    if data.len() < 8 + 32 * 4 + 8 + 64 + 8 + 8 + 3 + 32 + 8 + 1 || &data[..8] != disc {
        return Err("this account isn't an escrow".into());
    }
    let mut at = 8;
    let mut take = |n: usize| {
        let part = &data[at..at + n];
        at += n;
        part
    };
    let pubkey = |b: &[u8]| bs58::encode(b).into_string();
    let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    let num = |b: &[u8]| u64::from_le_bytes(b.try_into().expect("8 bytes"));
    let initiator = pubkey(take(32));
    let recipient = pubkey(take(32));
    let refund_token = pubkey(take(32));
    let rent_payer = pubkey(take(32));
    let amount = num(take(8)).to_string();
    let lock_point = hex(take(64));
    let created_slot = num(take(8));
    let deadline_slot = num(take(8));
    let close_on_restart = take(1)[0] != 0;
    let extended = take(1)[0] != 0;
    let state = match take(1)[0] {
        0 => "Open",
        1 => "Claimed",
        2 => "Refunded",
        _ => return Err("an unknown escrow state".into()),
    }
    .to_string();
    let revealed = hex(take(32));
    let settled_slot = num(take(8));
    Ok(EscrowAccount {
        initiator,
        recipient,
        refund_token,
        rent_payer,
        amount,
        lock_point,
        created_slot,
        deadline_slot,
        close_on_restart,
        extended,
        state,
        revealed,
        settled_slot,
    })
}
