//! The maker's Solana fees: priced like the page's, limited near what a transaction uses, and countable afterwards.
use liq2sol_chain::sol::{budget_ixs, compute_limit, priority_lamports, priority_price, COMPUTE_BUDGET};
use solana_sdk::{hash::Hash, signature::Keypair, signer::Signer, transaction::Transaction};

#[test]
fn the_price_is_the_75th_percentile_within_floor_and_cap() {
    assert_eq!(priority_price(&[], 1_000, 2_000_000), 1_000, "nothing recent: the floor");
    assert_eq!(priority_price(&[0, 0, 0, 0], 1_000, 2_000_000), 1_000);
    assert_eq!(priority_price(&[10, 50_000, 20, 30], 1_000, 2_000_000), 50_000, "the 75th percentile, in any order");
    assert_eq!(priority_price(&[9_000_000; 4], 1_000, 2_000_000), 2_000_000, "never above the cap, however busy");
    assert_eq!(priority_price(&[5], 1_000, 10), 1_000, "a cap below the floor can't win");
}

#[test]
fn the_limit_leaves_room_but_not_much() {
    assert_eq!(compute_limit(Some(10_000)), 15_000);
    assert_eq!(compute_limit(None), 200_000, "Solana's default");
    assert_eq!(compute_limit(Some(5_000_000)), 1_400_000, "Solana's maximum");
}

#[test]
fn what_a_transaction_can_cost_is_read_back_from_it() {
    let payer = Keypair::new();
    let ixs = budget_ixs(150_000, 2_000);
    let tx = Transaction::new_signed_with_payer(&ixs, Some(&payer.pubkey()), &[&payer], Hash::default());
    assert_eq!(priority_lamports(&tx), 300, "150,000 units at 2,000 micro-lamports");
    let plain = Transaction::new_signed_with_payer(&[], Some(&payer.pubkey()), &[&payer], Hash::default());
    assert_eq!(priority_lamports(&plain), 0, "no price, no priority fee");
    assert_eq!(tx.message.instructions.iter().filter(|i| tx.message.account_keys[i.program_id_index as usize] == COMPUTE_BUDGET).count(), 2);
}
