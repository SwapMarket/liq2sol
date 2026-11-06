//! The signer must refuse out-of-policy requests by itself, whatever a compromised engine asks for.

use std::cell::RefCell;
use std::collections::HashMap;

use liq2sol_maker_core::engine::{Config, Direction};
use liq2sol_maker_core::policy::*;
use liq2sol_maker_core::store::SignerLedger;
use liq2sol_maker_core::LockPoint;
use liq2sol_swapkey::point_of;

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

fn point() -> LockPoint {
    LockPoint(point_of(&P).unwrap())
}

fn hash() -> [u8; 32] {
    point().id()
}

/// A swap with its adapter signature already stored, the only state a Solana lock can start from.
fn held() -> Mem {
    let m = Mem::default();
    m.records.borrow_mut().insert(hash(), SwapRecord { adaptor_held: true, ..SwapRecord::default() });
    m
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
        swap_id: hash(),
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
    Request::LockLiquid { swap_id: hash(), liquid_amount, lock_height, liquid_tip: 1_000, taker_escrow }
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
    let m = held();
    let big = limits().max_per_swap + 1;
    assert_eq!(
        check(&limits(), &m, NOW, &lock_solana_with(big, 8_400, 1_300, coin(big * 100, 9))),
        Err(Refusal::OverSwapLimit)
    );
    let full = Mem { used: limits().max_per_window, ..held() };
    assert_eq!(check(&limits(), &full, NOW, &lock_solana()), Err(Refusal::OverWindowLimit));
    let almost = Mem { used: u64::MAX, ..held() };
    assert_eq!(check(&limits(), &almost, NOW, &lock_solana()), Err(Refusal::OverWindowLimit), "no overflow");
}

/// A compromised engine locking our funds to a colluding taker who never paid.
#[test]
fn a_solana_lock_needs_the_takers_coin_confirmed() {
    let m = held();
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
    let m = held();
    let got = |c| check(&limits(), &m, NOW, &lock_solana_with(AMOUNT, 8_400, 1_300, c));
    assert_eq!(got(coin(AMOUNT * 100 - 1, 2)), Err(Refusal::BadPrice));
    assert!(got(coin(AMOUNT * 100, 2)).is_ok());
    let wide = Limits { max_per_swap: u64::MAX, max_per_window: u64::MAX, ..limits() };
    let overflow = lock_solana_with(u64::MAX, 8_400, 1_300, coin(u64::MAX, 99));
    assert_eq!(check(&wide, &m, NOW, &overflow), Err(Refusal::BadPrice), "no overflow");
}

#[test]
fn solana_lock_timing_is_checked() {
    let m = held();
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
    let m = held();
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
    let m = held();
    apply(&m, &lock_solana()).unwrap();
    apply(&m, &lock_solana()).expect("identical retry");
    let different = lock_solana_with(AMOUNT, 8_399, 1_300, coin(AMOUNT * 100, 2));
    assert_eq!(apply(&m, &different), Err(Refusal::AlreadyLocked));
    assert_eq!(apply(&m, &lock_liquid()), Err(Refusal::AlreadyLocked));
    // A "retry" for a bigger amount is a second lock, and would slip past every lock limit.
    let bigger = lock_solana_with(AMOUNT + 1, 8_400, 1_300, coin((AMOUNT + 1) * 100, 2));
    assert_eq!(apply(&m, &bigger), Err(Refusal::AlreadyLocked));

    let m = held();
    apply(&m, &lock_liquid()).unwrap();
    assert_eq!(apply(&m, &lock_liquid()), Err(Refusal::AlreadyLocked), "a Liquid lock is never signed twice");
}

#[test]
fn liquid_lock_height_must_be_in_range() {
    let m = held();
    let at = |h| check(&limits(), &m, NOW, &lock_liquid_with(AMOUNT * 100, h, escrow(AMOUNT, true, 100_000)));
    assert!(matches!(at(1_029), Err(Refusal::BadTimeout(_))));
    assert!(matches!(at(1_121), Err(Refusal::BadTimeout(_))));
    assert!(at(1_030).is_ok());
    assert!(at(1_120).is_ok());
}

#[test]
fn claims_need_a_known_swap_and_the_right_secret() {
    let m = held();
    let claim = |secret| Request::ClaimLiquid { lock_point: point(), secret };
    assert_eq!(apply(&m, &claim(P)), Err(Refusal::UnknownSwap));
    apply(&m, &lock_solana()).unwrap();
    assert_eq!(apply(&m, &claim([1; 32])), Err(Refusal::WrongSecret));
    apply(&m, &claim(P)).unwrap();

    let m = held();
    let sol = |secret| Request::ClaimSolana { lock_point: point(), secret };
    assert_eq!(apply(&m, &sol(P)), Err(Refusal::UnknownSwap));
    apply(&m, &lock_liquid()).unwrap();
    assert_eq!(apply(&m, &sol([1; 32])), Err(Refusal::WrongSecret));
    apply(&m, &sol(P)).unwrap();
}

fn refund(liquid_tip: u32) -> Request {
    Request::RefundLiquid { swap_id: hash(), lock_height: 1_090, liquid_tip }
}

/// A compromised engine trying to take both sides after a rollback.
#[test]
fn liquid_refund_is_refused_after_collecting_on_solana() {
    let m = held();
    apply(&m, &lock_liquid()).unwrap();
    apply(&m, &Request::ClaimSolana { lock_point: point(), secret: P }).unwrap();
    assert_eq!(apply(&m, &refund(2_000)), Err(Refusal::RefundAfterClaim));
}

#[test]
fn liquid_refund_rules() {
    let m = held();
    assert_eq!(apply(&m, &refund(1_090)), Err(Refusal::UnknownSwap));
    apply(&m, &lock_liquid()).unwrap();
    assert!(matches!(apply(&m, &refund(1_089)), Err(Refusal::BadTimeout(_))));
    apply(&m, &refund(1_090)).unwrap();
}

/// A compromised engine handing a colluding taker both sides.
#[test]
fn release_is_refused_once_we_have_locked() {
    let m = held();
    let release = Request::Release { swap_id: hash() };
    assert!(check(&limits(), &m, NOW, &release).is_ok());
    apply(&m, &lock_liquid()).unwrap();
    assert_eq!(apply(&m, &release), Err(Refusal::ReleaseAfterLock));
}

#[test]
fn extend_needs_a_real_stall_and_a_bounded_move() {
    let m = held();
    let ext = |new_deadline, liquid_tip_age_secs| Request::Extend {
        swap_id: hash(),
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
    let m = held();
    apply(&m, &lock_solana()).unwrap();
    assert_eq!(apply(&m, &refund(2_000)), Err(Refusal::UnknownSwap));
    assert_eq!(apply(&m, &Request::ClaimSolana { lock_point: point(), secret: P }), Err(Refusal::UnknownSwap));

    let m = held();
    apply(&m, &lock_liquid()).unwrap();
    assert_eq!(apply(&m, &Request::RefundSolana { swap_id: hash() }), Err(Refusal::UnknownSwap));
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
    assert_eq!(newly_locked(&before, &Request::RefundSolana { swap_id: hash() }), None);
}

/// A compromised engine locking our funds with no way to collect the taker's coin afterwards.
#[test]
fn a_solana_lock_needs_a_stored_adapter_signature() {
    let m = Mem::default();
    assert_eq!(apply(&m, &lock_solana()), Err(Refusal::NoAdaptor));
    apply(&m, &Request::HoldAdaptor { swap_id: hash() }).unwrap();
    apply(&m, &lock_solana()).unwrap();
}

#[test]
fn an_adapter_signature_is_stored_once_and_only_before_locking() {
    let hold = Request::HoldAdaptor { swap_id: hash() };
    let m = Mem::default();
    apply(&m, &hold).unwrap();
    assert_eq!(apply(&m, &hold), Err(Refusal::AdaptorAlreadyHeld), "a stored signature is never replaced");
    apply(&m, &lock_solana()).unwrap();
    assert_eq!(apply(&m, &hold), Err(Refusal::AlreadyLocked));
}

fn share(liquid_tip: u32) -> Request {
    Request::GiveShare { swap_id: hash(), lock_height: 1_090, liquid_tip, taker_escrow_open: true }
}

/// A second share would let a taker publish a signature we can't read the secret from, and keep the Solana side too.
#[test]
fn our_share_is_given_once_and_only_while_the_swap_is_live() {
    let m = Mem::default();
    assert_eq!(apply(&m, &share(1_000)), Err(Refusal::UnknownSwap), "nothing is locked");
    apply(&m, &lock_liquid()).unwrap();
    let refunded =
        Request::GiveShare { swap_id: hash(), lock_height: 1_090, liquid_tip: 1_000, taker_escrow_open: false };
    assert_eq!(apply(&m, &refunded), Err(Refusal::NoTakerEscrow), "the taker took its refund, so a claim would steal");
    assert!(matches!(apply(&m, &share(1_090)), Err(Refusal::BadTimeout(_))), "the refund path is open");
    apply(&m, &share(1_089)).unwrap();
    assert_eq!(apply(&m, &share(1_089)), Err(Refusal::AdaptorAlreadyHeld));

    let m = Mem::default();
    apply(&m, &lock_liquid()).unwrap();
    apply(&m, &refund(1_090)).unwrap();
    assert!(matches!(apply(&m, &share(1_000)), Err(Refusal::BadTimeout(_))), "refunded: the coin is gone");

    let m = held();
    apply(&m, &lock_solana()).unwrap();
    assert_eq!(apply(&m, &share(1_000)), Err(Refusal::UnknownSwap), "wrong direction");
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
        ledger.commit(&hash(), &rec, Some((NOW, AMOUNT)), Some(b"tx"), &["cc:1".to_string()]).unwrap();
        let rec = check(&limits(), &ledger, NOW, &Request::ClaimSolana { lock_point: point(), secret: P }).unwrap();
        ledger.commit(&hash(), &rec, None, None, &[]).unwrap();
    }
    let ledger = SignerLedger::open(&path).unwrap();
    assert_eq!(
        check(&limits(), &ledger, NOW, &refund(2_000)),
        Err(Refusal::RefundAfterClaim),
        "remembered across a restart"
    );
    assert_eq!(check(&limits(), &ledger, NOW, &lock_liquid()), Err(Refusal::AlreadyLocked));
    assert_eq!(ledger.lock_tx(&hash()).unwrap().unwrap(), b"tx");
    assert!(
        ledger.is_used("cc:1").unwrap() && !ledger.is_used("cc:2").unwrap(),
        "a signed payment's coins stay marked"
    );
    assert_eq!(ledger.pending_top_up().unwrap(), None);
    ledger.set_pending_top_up(b"top", &["dd:0".to_string()]).unwrap();
    assert_eq!(ledger.pending_top_up().unwrap().unwrap(), b"top");
    assert!(ledger.is_used("dd:0").unwrap());
    ledger.clear_pending_top_up().unwrap();
    assert_eq!(ledger.pending_top_up().unwrap(), None);
    assert!(ledger.reserve_fee_coin(&hash(), "aa:0").unwrap());
    assert!(!ledger.reserve_fee_coin(&[9; 32], "aa:0").unwrap(), "a coin belongs to one swap");
    assert!(ledger.is_reserved("aa:0").unwrap() && !ledger.is_reserved("bb:0").unwrap());
    assert_eq!(ledger.fee_coin(&hash()).unwrap().unwrap(), "aa:0");
    let rec = SwapRecord { adaptor_held: true, ..SwapRecord::default() };
    ledger.commit_adaptor(&[8; 32], &rec, "body").unwrap();
    assert!(ledger.commit_adaptor(&[8; 32], &rec, "other").is_err(), "a stored signature is never replaced");
    assert_eq!(ledger.adaptor(&[8; 32]).unwrap().unwrap(), "body");
    assert_eq!(ledger.locked_in_window(NOW + 1, 3_600), AMOUNT);
    assert_eq!(ledger.locked_in_window(NOW + 3_601, 3_600), 0, "an old lock leaves the window");
    std::fs::remove_dir_all(&dir).unwrap();
}

// ---- An early refund and a Solana lock can never both happen ----

#[test]
fn after_an_early_refund_the_swap_can_never_lock() {
    let m = held();
    apply(&m, &Request::EarlyRefund { swap_id: hash() }).unwrap();
    assert_eq!(apply(&m, &lock_solana()), Err(Refusal::EarlyRefunded));
    let fresh = Mem::default();
    apply(&fresh, &Request::EarlyRefund { swap_id: hash() }).unwrap();
    assert_eq!(apply(&fresh, &Request::HoldAdaptor { swap_id: hash() }), Err(Refusal::EarlyRefunded));
}

#[test]
fn after_signing_a_lock_there_is_no_early_refund() {
    let m = held();
    apply(&m, &lock_solana()).unwrap();
    assert_eq!(apply(&m, &Request::EarlyRefund { swap_id: hash() }), Err(Refusal::AlreadyLocked));
}

#[test]
fn an_early_refund_can_be_asked_for_again() {
    let m = held();
    apply(&m, &Request::EarlyRefund { swap_id: hash() }).unwrap();
    apply(&m, &Request::EarlyRefund { swap_id: hash() }).unwrap();
}

#[test]
fn an_unreadable_record_refuses_an_early_refund() {
    let m = Mem::default();
    m.records.borrow_mut().insert(hash(), SwapRecord { direction: Some(Direction::S2L), ..SwapRecord::default() });
    assert_eq!(apply(&m, &Request::EarlyRefund { swap_id: hash() }), Err(Refusal::AlreadyLocked));
}
