//! The browser's Solana code against the native builders the maker uses. Any difference fails here, not on a live cluster.

use anchor_lang::AccountSerialize;
use liq2sol_chain::sol;
use liq2sol_client_wasm::solana::*;
use solana_escrow::state::{Escrow, EscrowState};
use solana_escrow::USDT_MINT;
use solana_sdk::instruction::Instruction;
use solana_sdk::message::Message;
use solana_sdk::pubkey::Pubkey;

const POINT: [u8; 64] = [7; 64];

fn to_key(p: &Pubkey) -> Key {
    p.to_bytes()
}

fn native(ix: &Instruction) -> Ix {
    Ix {
        program: to_key(&ix.program_id),
        accounts: ix
            .accounts
            .iter()
            .map(|m| Meta { key: to_key(&m.pubkey), signer: m.is_signer, writable: m.is_writable })
            .collect(),
        data: ix.data.clone(),
    }
}

fn ids() -> (Key, Key, Pubkey, Pubkey, Pubkey) {
    (
        to_key(&solana_escrow::id()),
        to_key(&USDT_MINT),
        Pubkey::new_from_array([1; 32]),
        Pubkey::new_from_array([2; 32]),
        Pubkey::new_from_array([3; 32]),
    )
}

fn escrow(initiator: Pubkey, recipient: Pubkey, rent_payer: Pubkey) -> Escrow {
    Escrow {
        initiator,
        recipient,
        refund_token: sol::ata(&initiator),
        rent_payer,
        amount: 1_234_567_890_123,
        lock_point: POINT,
        created_slot: 11,
        deadline_slot: 22,
        close_on_restart: true,
        extended: false,
        state: EscrowState::Claimed,
        revealed: [9; 32],
        settled_slot: 33,
        bump: 254,
    }
}

#[test]
fn addresses_match() {
    let (program, mint, a, _, _) = ids();
    let e = escrow_pda(&to_key(&a), &POINT, &program);
    assert_eq!(e, to_key(&sol::escrow_pda(&a, &POINT)));
    assert_eq!(vault_pda(&e, &program), to_key(&sol::vault_pda(&Pubkey::new_from_array(e))));
    assert_eq!(ata(&to_key(&a), &mint), to_key(&sol::ata(&a)));
}

#[test]
fn instructions_match() {
    let (program, mint, a, b, c) = ids();
    let lock = lock_ix(&program, &mint, &to_key(&a), &POINT, &to_key(&b), 5, 6, false);
    assert_eq!(lock, native(&sol::lock_ix(&a, POINT, b, 5, 6, false)));

    let address = sol::escrow_pda(&a, &POINT);
    let e = escrow(a, b, c);
    let ixs = sol::claim_ixs(&c, &address, &e, [9; 32]);
    assert_eq!(create_ata_ix(&to_key(&c), &to_key(&b), &mint), native(&ixs[0]));
    assert_eq!(claim_ix(&program, &mint, &to_key(&address), &to_key(&b), &to_key(&c), &[9; 32]), native(&ixs[1]));

    let refund = refund_ix(&program, &to_key(&address), &to_key(&e.refund_token), &to_key(&c));
    assert_eq!(refund, native(&sol::refund_ix(&address, &e)));
}

#[test]
fn a_message_matches_the_sdks_byte_for_byte() {
    let (program, mint, a, b, c) = ids();
    let address = sol::escrow_pda(&a, &POINT);
    let e = escrow(a, b, c);
    let hash = solana_sdk::hash::Hash::new_from_array([5; 32]);
    let ours = |payer: &Key, ixs: Vec<Ix>| message(payer, &ixs, &[5; 32]);
    let theirs =
        |payer: &Pubkey, ixs: &[Instruction]| bincode_message(&Message::new_with_blockhash(ixs, Some(payer), &hash));

    let lock = sol::lock_ix(&a, POINT, b, 5, 6, false);
    let mine = lock_ix(&program, &mint, &to_key(&a), &POINT, &to_key(&b), 5, 6, false);
    assert_eq!(ours(&to_key(&a), vec![mine]), theirs(&a, &[lock]));

    let claim = sol::claim_ixs(&c, &address, &e, [9; 32]);
    let mine = vec![
        create_ata_ix(&to_key(&c), &to_key(&b), &mint),
        claim_ix(&program, &mint, &to_key(&address), &to_key(&b), &to_key(&c), &[9; 32]),
    ];
    assert_eq!(ours(&to_key(&c), mine), theirs(&c, &claim));
}

fn bincode_message(m: &Message) -> Vec<u8> {
    m.serialize()
}

#[test]
fn an_unsigned_transaction_has_one_empty_slot_per_signer() {
    let (_, _, a, _, _) = ids();
    let m = message(&to_key(&a), &[], &[0; 32]);
    let t = unsigned_tx(&m);
    assert_eq!(t[0], 1);
    assert_eq!(&t[1..65], &[0; 64]);
    assert_eq!(&t[65..], m.as_slice());
}

#[test]
fn an_escrow_account_reads_back() {
    let (_, _, a, b, c) = ids();
    let e = escrow(a, b, c);
    let mut data = vec![];
    e.try_serialize(&mut data).unwrap();
    let read = parse_escrow(&data).unwrap();
    assert_eq!(read.initiator, a.to_string());
    assert_eq!(read.recipient, b.to_string());
    assert_eq!(read.refund_token, e.refund_token.to_string());
    assert_eq!(read.rent_payer, c.to_string());
    assert_eq!(read.amount, "1234567890123");
    assert_eq!(read.lock_point, "07".repeat(64));
    assert_eq!((read.created_slot, read.deadline_slot, read.settled_slot), (11, 22, 33));
    assert!(read.close_on_restart && !read.extended);
    assert_eq!(read.state, "Claimed");
    assert_eq!(read.revealed, "09".repeat(32));
}

#[test]
fn anything_else_is_refused_as_an_escrow() {
    assert!(parse_escrow(&[]).is_err());
    assert!(parse_escrow(&[0; 400]).is_err());
    let (_, _, a, b, c) = ids();
    let mut data = vec![];
    escrow(a, b, c).try_serialize(&mut data).unwrap();
    let state_at = data.len() - (1 + 8 + 32 + 1);
    data[state_at] = 9;
    assert!(parse_escrow(&data).is_err(), "an unknown state");
}
