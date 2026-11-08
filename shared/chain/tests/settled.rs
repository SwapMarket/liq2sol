//! Reading how a closed escrow was settled from its history: only the escrow program's own settling calls, on this escrow.
use liq2sol_chain::sol::{claim_ixs, close_ix, escrow_pda, refund_ix, release_ix, settled_in, Settled};
use solana_escrow::state::{Escrow, EscrowState};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    transaction::{Transaction, VersionedTransaction},
};

fn escrow() -> Escrow {
    Escrow {
        initiator: Pubkey::new_unique(),
        recipient: Pubkey::new_unique(),
        refund_token: Pubkey::new_unique(),
        rent_payer: Pubkey::new_unique(),
        amount: 1_000_000,
        lock_point: [7; 64],
        created_slot: 100,
        deadline_slot: 1_000,
        close_on_restart: false,
        extended: false,
        state: EscrowState::Open,
        revealed: [0; 32],
        settled_slot: 0,
        bump: 255,
    }
}

fn tx(ixs: &[Instruction]) -> VersionedTransaction {
    // Unsigned: only the message is read.
    Transaction::new_with_payer(ixs, Some(&Pubkey::new_unique())).into()
}

#[test]
fn each_settling_call_is_told_apart() {
    let address = escrow_pda(&Pubkey::new_unique(), &[7; 64]);
    let e = escrow();
    let payer = Pubkey::new_unique();
    assert_eq!(settled_in(&tx(&claim_ixs(&payer, &address, &e, [1; 32])), &address), Some(Settled::Claimed));
    assert_eq!(settled_in(&tx(&[refund_ix(&address, &e)]), &address), Some(Settled::Refunded));
    assert_eq!(settled_in(&tx(&[release_ix(&e.recipient, &address, &e)]), &address), Some(Settled::Released));
    // Closing settles nothing: it follows a settlement, and says nothing about which.
    assert_eq!(settled_in(&tx(&[close_ix(&payer, &address, &e)]), &address), None);
}

#[test]
fn another_escrow_or_another_program_does_not_count() {
    let address = escrow_pda(&Pubkey::new_unique(), &[7; 64]);
    let other = escrow_pda(&Pubkey::new_unique(), &[8; 64]);
    let e = escrow();
    // A claim of a different escrow, in a transaction that only mentions ours elsewhere.
    let mut claim_other = claim_ixs(&Pubkey::new_unique(), &other, &e, [1; 32]);
    claim_other.push(Instruction::new_with_bytes(Pubkey::new_unique(), &[], vec![AccountMeta::new(address, false)]));
    assert_eq!(settled_in(&tx(&claim_other), &address), None);
    // The claim's exact bytes and accounts, sent to a program that isn't ours.
    let mut fake = claim_ixs(&Pubkey::new_unique(), &address, &e, [1; 32]).pop().unwrap();
    fake.program_id = Pubkey::new_unique();
    assert_eq!(settled_in(&tx(&[fake]), &address), None);
    // Our program, our escrow, but data too short to carry a tag.
    let mut short = refund_ix(&address, &e);
    short.data.truncate(7);
    assert_eq!(settled_in(&tx(&[short]), &address), None);
}
