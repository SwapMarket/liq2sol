//! The signer must refuse out-of-policy requests by itself, whatever a compromised engine asks for.

use std::cell::RefCell;
use std::collections::HashMap;

use liq2sol_maker_core::engine::{Config, Direction};
use liq2sol_maker_core::hash_preimage;
use liq2sol_maker_core::policy::*;
use liq2sol_maker_core::store::SignerLedger;

const P: [u8; 32] = [7; 32];
const NOW: u64 = 1_000_000;

#[derive(Default)]
struct Mem {
    records: RefCell<HashMap<[u8; 32], SwapRecord>>,
    used: u64,
}

impl Ledger for Mem {
    fn record(&self, hash: &[u8; 32]) -> SwapRecord {
        self.records.borrow().get(hash).cloned().unwrap_or_default()
    }
    fn locked_in_window(&self, _: u64, _: u64) -> u64 {
        self.used
    }
}

fn limits() -> Limits {
    Limits::from(&Config::default())
}

fn hash() -> [u8; 32] {
    hash_preimage(&P)
}

const AMOUNT: u64 = 100;

fn coin(amount: u64, confirmations: u32) -> Option<CoinEvidence> {
    Some(CoinEvidence { amount, confirmations })
}

fn lock_solana_with(
    amount: u64,
    deadline_slot: u64,
    liquid_lock_height: u32,
    taker_coin: Option<CoinEvidence>,
) -> Request {
    Request::LockSolana {
        payment_hash: hash(),
        amount,
        deadline_slot,
        solana_slot: 0,
        liquid_lock_height,
        liquid_tip: 1_000,
        taker_coin,
    }
}

fn lock_solana() -> Request {
    lock_solana_with(AMOUNT, 8_400, 1_300, coin(AMOUNT * 100, 2))
}

fn escrow(amount: u64, matches_terms: bool, slots_left: u64) -> Option<EscrowEvidence> {
    Some(EscrowEvidence { amount, matches_terms, slots_left })
}

fn lock_liquid_with(liquid_amount: u64, lock_height: u32, taker_escrow: Option<EscrowEvidence>) -> Request {
    Request::LockLiquid { payment_hash: hash(), liquid_amount, lock_height, liquid_tip: 1_000, taker_escrow }
}

fn lock_liquid() -> Request {
    lock_liquid_with(AMOUNT * 100, 1_080, escrow(AMOUNT, true, 100_000))
}

/// Apply a request as the signer would: check, then remember.
fn apply(m: &Mem, r: &Request) -> Result<(), Refusal> {
    let rec = check(&limits(), m, NOW, r)?;
    m.records.borrow_mut().insert(hash(), rec);
    Ok(())
}

#[test]
fn a_lock_is_refused_outside_its_limits() {
    let m = Mem::default();
    let big = limits().max_per_swap + 1;
    assert_eq!(
        check(&limits(), &m, NOW, &lock_solana_with(big, 8_400, 1_300, coin(big * 100, 9))),
        Err(Refusal::OverSwapLimit)
    );
    let full = Mem { used: limits().max_per_window, ..Mem::default() };
    assert_eq!(check(&limits(), &full, NOW, &lock_solana()), Err(Refusal::OverWindowLimit));
    let almost = Mem { used: u64::MAX, ..Mem::default() };
    assert_eq!(check(&limits(), &almost, NOW, &lock_solana()), Err(Refusal::OverWindowLimit), "no overflow");
}

/// A compromised engine locking our funds to a colluding taker who never paid.
#[test]
fn a_solana_lock_needs_the_takers_coin_confirmed() {
    let m = Mem::default();
    let got = |c| check(&limits(), &m, NOW, &lock_solana_with(AMOUNT, 8_400, 1_300, c));
    assert_eq!(got(None), Err(Refusal::NoTakerCoin));
    assert_eq!(got(coin(AMOUNT * 100, 1)), Err(Refusal::NoTakerCoin));
    assert!(got(coin(AMOUNT * 100, 2)).is_ok());

    let large = limits().large_above + 1;
    let wide = Limits { max_per_swap: u64::MAX, max_per_window: u64::MAX, ..limits() };
    let at = |confs| check(&wide, &m, NOW, &lock_solana_with(large, 8_400, 1_300, coin(large * 100, confs)));
    assert_eq!(at(9), Err(Refusal::NoTakerCoin));
    assert!(at(10).is_ok());
}

/// A compromised engine paying out more than the coin it receives.
#[test]
fn a_solana_lock_is_refused_at_a_loss() {
    let m = Mem::default();
    let got = |c| check(&limits(), &m, NOW, &lock_solana_with(AMOUNT, 8_400, 1_300, c));
    assert_eq!(got(coin(AMOUNT * 100 - 1, 2)), Err(Refusal::BadPrice));
    assert!(got(coin(AMOUNT * 100, 2)).is_ok());
    let wide = Limits { max_per_swap: u64::MAX, max_per_window: u64::MAX, ..limits() };
    let overflow = lock_solana_with(u64::MAX, 8_400, 1_300, coin(u64::MAX, 99));
    assert_eq!(check(&wide, &m, NOW, &overflow), Err(Refusal::BadPrice), "no overflow");
}

#[test]
fn solana_lock_timing_is_checked() {
    let m = Mem::default();
    let c = coin(AMOUNT * 100, 2);
    let got = |deadline, height| check(&limits(), &m, NOW, &lock_solana_with(AMOUNT, deadline, height, c));
    assert!(matches!(got(8_401, 1_300), Err(Refusal::BadTimeout(_))));
    assert!(matches!(got(0, 1_300), Err(Refusal::BadTimeout(_))));
    assert!(matches!(got(8_400, 1_209), Err(Refusal::BadTimeout(_))));
    assert!(got(8_400, 1_210).is_ok());
}

/// A compromised engine locking on Liquid against an escrow that isn't there or doesn't pay us.
#[test]
fn a_liquid_lock_needs_a_matching_escrow() {
    let m = Mem::default();
    let liq = AMOUNT * 100;
    let got = |e| check(&limits(), &m, NOW, &lock_liquid_with(liq, 1_080, e));
    assert_eq!(got(None), Err(Refusal::NoTakerEscrow));
    assert_eq!(got(escrow(AMOUNT, false, 100_000)), Err(Refusal::NoTakerEscrow));
    assert_eq!(got(escrow(AMOUNT, true, limits().s2l_gate_slots - 1)), Err(Refusal::NoTakerEscrow));
    assert_eq!(got(escrow(AMOUNT - 1, true, 100_000)), Err(Refusal::BadPrice));
    assert!(got(escrow(AMOUNT, true, limits().s2l_gate_slots)).is_ok());
    let odd = lock_liquid_with(liq - 1, 1_080, escrow(AMOUNT, true, 100_000));
    assert_eq!(check(&limits(), &m, NOW, &odd), Err(Refusal::BadPrice), "must be an exact multiple of 100");
}

#[test]
fn a_second_different_lock_is_refused_but_an_identical_retry_is_signed() {
    let m = Mem::default();
    apply(&m, &lock_solana()).unwrap();
    apply(&m, &lock_solana()).expect("identical retry");
    let different = lock_solana_with(AMOUNT, 8_399, 1_300, coin(AMOUNT * 100, 2));
    assert_eq!(apply(&m, &different), Err(Refusal::AlreadyLocked));
    assert_eq!(apply(&m, &lock_liquid()), Err(Refusal::AlreadyLocked));
    // A "retry" for a bigger amount is a second lock, and would slip past every lock limit.
    let bigger = lock_solana_with(AMOUNT + 1, 8_400, 1_300, coin((AMOUNT + 1) * 100, 2));
    assert_eq!(apply(&m, &bigger), Err(Refusal::AlreadyLocked));

    let m = Mem::default();
    apply(&m, &lock_liquid()).unwrap();
    assert_eq!(apply(&m, &lock_liquid()), Err(Refusal::AlreadyLocked), "a Liquid lock is never signed twice");
}

#[test]
fn liquid_lock_height_must_be_in_range() {
    let m = Mem::default();
    let at = |h| check(&limits(), &m, NOW, &lock_liquid_with(AMOUNT * 100, h, escrow(AMOUNT, true, 100_000)));
    assert!(matches!(at(1_029), Err(Refusal::BadTimeout(_))));
    assert!(matches!(at(1_121), Err(Refusal::BadTimeout(_))));
    assert!(at(1_030).is_ok());
    assert!(at(1_120).is_ok());
}

#[test]
fn claims_need_a_known_swap_and_the_right_preimage() {
    let m = Mem::default();
    let claim = |preimage| Request::ClaimLiquid { payment_hash: hash(), preimage };
    assert_eq!(apply(&m, &claim(P)), Err(Refusal::UnknownSwap));
    apply(&m, &lock_solana()).unwrap();
    assert_eq!(apply(&m, &claim([1; 32])), Err(Refusal::WrongPreimage));
    apply(&m, &claim(P)).unwrap();

    let m = Mem::default();
    let sol = |preimage| Request::ClaimSolana { payment_hash: hash(), preimage };
    assert_eq!(apply(&m, &sol(P)), Err(Refusal::UnknownSwap));
    apply(&m, &lock_liquid()).unwrap();
    assert_eq!(apply(&m, &sol([1; 32])), Err(Refusal::WrongPreimage));
    apply(&m, &sol(P)).unwrap();
}

fn refund(liquid_tip: u32) -> Request {
    Request::RefundLiquid { payment_hash: hash(), lock_height: 1_090, liquid_tip }
}

/// A compromised engine trying to take both sides after a rollback.
#[test]
fn liquid_refund_is_refused_after_collecting_on_solana() {
    let m = Mem::default();
    apply(&m, &lock_liquid()).unwrap();
    apply(&m, &Request::ClaimSolana { payment_hash: hash(), preimage: P }).unwrap();
    assert_eq!(apply(&m, &refund(2_000)), Err(Refusal::RefundAfterClaim));
}

#[test]
fn liquid_refund_rules() {
    let m = Mem::default();
    assert_eq!(apply(&m, &refund(1_090)), Err(Refusal::UnknownSwap));
    apply(&m, &lock_liquid()).unwrap();
    assert!(matches!(apply(&m, &refund(1_089)), Err(Refusal::BadTimeout(_))));
    apply(&m, &refund(1_090)).unwrap();
}

/// A compromised engine handing a colluding taker both sides.
#[test]
fn release_is_refused_once_we_have_locked() {
    let m = Mem::default();
    let release = Request::Release { payment_hash: hash() };
    assert!(check(&limits(), &m, NOW, &release).is_ok());
    apply(&m, &lock_liquid()).unwrap();
    assert_eq!(apply(&m, &release), Err(Refusal::ReleaseAfterLock));
}

#[test]
fn extend_needs_a_real_stall_and_a_bounded_move() {
    let m = Mem::default();
    let ext = |new_deadline, liquid_tip_age_secs| Request::Extend {
        payment_hash: hash(),
        current_deadline: 100,
        new_deadline,
        liquid_tip_age_secs,
    };
    assert_eq!(apply(&m, &ext(200, 3_600)), Err(Refusal::UnknownSwap));
    apply(&m, &lock_liquid()).unwrap();
    assert_eq!(apply(&m, &ext(200, 3_599)), Err(Refusal::ExtendNotJustified));
    assert!(matches!(apply(&m, &ext(100, 3_600)), Err(Refusal::BadTimeout(_))));
    assert!(matches!(apply(&m, &ext(3_000_101, 3_600)), Err(Refusal::BadTimeout(_))));
    apply(&m, &ext(3_000_100, 3_600)).unwrap();
}

#[test]
fn wrong_direction_requests_are_refused() {
    let m = Mem::default();
    apply(&m, &lock_solana()).unwrap();
    assert_eq!(apply(&m, &refund(2_000)), Err(Refusal::UnknownSwap));
    assert_eq!(apply(&m, &Request::ClaimSolana { payment_hash: hash(), preimage: P }), Err(Refusal::UnknownSwap));

    let m = Mem::default();
    apply(&m, &lock_liquid()).unwrap();
    assert_eq!(apply(&m, &Request::RefundSolana { payment_hash: hash() }), Err(Refusal::UnknownSwap));
}

#[test]
fn only_a_first_lock_counts_against_the_window() {
    let before = SwapRecord::default();
    assert_eq!(newly_locked(&before, &lock_solana()), Some(AMOUNT));
    assert_eq!(newly_locked(&before, &lock_liquid()), Some(AMOUNT));
    let locked = SwapRecord {
        direction: Some(Direction::L2S),
        locked_amount: AMOUNT,
        deadline_slot: 8_400,
        ..SwapRecord::default()
    };
    assert_eq!(newly_locked(&locked, &lock_solana()), None);
    assert_eq!(newly_locked(&before, &Request::RefundSolana { payment_hash: hash() }), None);
}

#[test]
fn the_real_ledger_survives_a_restart_and_keeps_the_lock_transaction() {
    let dir = std::env::temp_dir().join(format!("liq2sol-ledger-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("signer.db");
    let _ = std::fs::remove_file(&path);
    {
        let ledger = SignerLedger::open(&path).unwrap();
        let rec = check(&limits(), &ledger, NOW, &lock_liquid()).unwrap();
        ledger.commit(&hash(), &rec, Some((NOW, AMOUNT)), Some(b"tx")).unwrap();
        let rec = check(&limits(), &ledger, NOW, &Request::ClaimSolana { payment_hash: hash(), preimage: P }).unwrap();
        ledger.commit(&hash(), &rec, None, None).unwrap();
    }
    let ledger = SignerLedger::open(&path).unwrap();
    assert_eq!(
        check(&limits(), &ledger, NOW, &refund(2_000)),
        Err(Refusal::RefundAfterClaim),
        "remembered across a restart"
    );
    assert_eq!(check(&limits(), &ledger, NOW, &lock_liquid()), Err(Refusal::AlreadyLocked));
    assert_eq!(ledger.lock_tx(&hash()).unwrap().unwrap(), b"tx");
    assert_eq!(ledger.locked_in_window(NOW + 1, 3_600), AMOUNT);
    assert_eq!(ledger.locked_in_window(NOW + 3_601, 3_600), 0, "an old lock leaves the window");
    std::fs::remove_dir_all(&dir).unwrap();
}
