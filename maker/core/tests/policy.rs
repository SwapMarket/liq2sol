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

fn lock_solana() -> Request {
    Request::LockSolana {
        payment_hash: hash(),
        amount: 100,
        deadline_slot: 8_400,
        solana_slot: 0,
        close_on_restart: true,
        liquid_lock_height: 1_300,
        liquid_tip: 1_000,
    }
}

fn lock_liquid() -> Request {
    Request::LockLiquid {
        payment_hash: hash(),
        amount: 100,
        lock_height: 1_090,
        liquid_tip: 1_010,
    }
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
    let over = Request::LockSolana {
        payment_hash: hash(),
        amount: limits().max_per_swap + 1,
        deadline_slot: 8_400,
        solana_slot: 0,
        close_on_restart: true,
        liquid_lock_height: 1_300,
        liquid_tip: 1_000,
    };
    assert_eq!(
        check(&limits(), &m, NOW, &over),
        Err(Refusal::OverSwapLimit)
    );
    let full = Mem {
        used: limits().max_per_window,
        ..Mem::default()
    };
    assert_eq!(
        check(&limits(), &full, NOW, &lock_solana()),
        Err(Refusal::OverWindowLimit)
    );
    let almost = Mem {
        used: u64::MAX,
        ..Mem::default()
    };
    assert_eq!(
        check(&limits(), &almost, NOW, &lock_solana()),
        Err(Refusal::OverWindowLimit),
        "no overflow"
    );
}

#[test]
fn solana_lock_terms_are_checked() {
    let m = Mem::default();
    let bad = |r: Request| {
        assert!(
            matches!(check(&limits(), &m, NOW, &r), Err(Refusal::BadTimeout(_))),
            "{r:?}"
        )
    };
    let Request::LockSolana {
        payment_hash,
        amount,
        ..
    } = lock_solana()
    else {
        unreachable!()
    };
    let base = |deadline_slot, close_on_restart, liquid_lock_height| Request::LockSolana {
        payment_hash,
        amount,
        deadline_slot,
        solana_slot: 0,
        close_on_restart,
        liquid_lock_height,
        liquid_tip: 1_000,
    };
    bad(base(8_400, false, 1_300));
    bad(base(8_401, true, 1_300));
    bad(base(0, true, 1_300));
    bad(base(8_400, true, 1_209));
    assert!(check(&limits(), &m, NOW, &base(8_400, true, 1_210)).is_ok());
}

#[test]
fn a_second_different_lock_is_refused_but_an_identical_retry_is_signed() {
    let m = Mem::default();
    apply(&m, &lock_solana()).unwrap();
    apply(&m, &lock_solana()).expect("identical retry");
    let different = Request::LockSolana {
        payment_hash: hash(),
        amount: 100,
        deadline_slot: 8_399,
        solana_slot: 0,
        close_on_restart: true,
        liquid_lock_height: 1_300,
        liquid_tip: 1_000,
    };
    assert_eq!(apply(&m, &different), Err(Refusal::AlreadyLocked));
    assert_eq!(apply(&m, &lock_liquid()), Err(Refusal::AlreadyLocked));

    let m = Mem::default();
    apply(&m, &lock_liquid()).unwrap();
    assert_eq!(
        apply(&m, &lock_liquid()),
        Err(Refusal::AlreadyLocked),
        "a Liquid lock is never signed twice"
    );
}

#[test]
fn liquid_lock_height_must_be_in_range() {
    let m = Mem::default();
    let at = |lock_height| Request::LockLiquid {
        payment_hash: hash(),
        amount: 100,
        lock_height,
        liquid_tip: 1_000,
    };
    assert!(matches!(
        check(&limits(), &m, NOW, &at(1_029)),
        Err(Refusal::BadTimeout(_))
    ));
    assert!(matches!(
        check(&limits(), &m, NOW, &at(1_121)),
        Err(Refusal::BadTimeout(_))
    ));
    assert!(check(&limits(), &m, NOW, &at(1_030)).is_ok());
}

#[test]
fn claims_need_a_known_swap_the_right_preimage_and_our_own_script() {
    let m = Mem::default();
    let claim = |preimage, pays_our_script| Request::ClaimLiquid {
        payment_hash: hash(),
        preimage,
        pays_our_script,
    };
    assert_eq!(apply(&m, &claim(P, true)), Err(Refusal::UnknownSwap));
    apply(&m, &lock_solana()).unwrap();
    assert_eq!(
        apply(&m, &claim([1; 32], true)),
        Err(Refusal::WrongPreimage)
    );
    assert_eq!(apply(&m, &claim(P, false)), Err(Refusal::WrongDestination));
    apply(&m, &claim(P, true)).unwrap();

    let m = Mem::default();
    let sol = |preimage| Request::ClaimSolana {
        payment_hash: hash(),
        preimage,
    };
    assert_eq!(apply(&m, &sol(P)), Err(Refusal::UnknownSwap));
    apply(&m, &lock_liquid()).unwrap();
    assert_eq!(apply(&m, &sol([1; 32])), Err(Refusal::WrongPreimage));
    apply(&m, &sol(P)).unwrap();
}

fn refund(liquid_tip: u32, pays_our_script: bool) -> Request {
    Request::RefundLiquid {
        payment_hash: hash(),
        lock_height: 1_090,
        liquid_tip,
        pays_our_script,
    }
}

/// A compromised engine trying to take both sides after a rollback.
#[test]
fn liquid_refund_is_refused_after_collecting_on_solana() {
    let m = Mem::default();
    apply(&m, &lock_liquid()).unwrap();
    apply(
        &m,
        &Request::ClaimSolana {
            payment_hash: hash(),
            preimage: P,
        },
    )
    .unwrap();
    assert_eq!(
        apply(&m, &refund(2_000, true)),
        Err(Refusal::RefundAfterClaim)
    );
}

#[test]
fn liquid_refund_rules() {
    let m = Mem::default();
    assert_eq!(apply(&m, &refund(1_090, true)), Err(Refusal::UnknownSwap));
    apply(&m, &lock_liquid()).unwrap();
    assert!(matches!(
        apply(&m, &refund(1_089, true)),
        Err(Refusal::BadTimeout(_))
    ));
    assert_eq!(
        apply(&m, &refund(1_090, false)),
        Err(Refusal::WrongDestination)
    );
    apply(&m, &refund(1_090, true)).unwrap();
}

/// A compromised engine handing a colluding taker both sides.
#[test]
fn release_is_refused_once_we_have_locked() {
    let m = Mem::default();
    let release = Request::Release {
        payment_hash: hash(),
    };
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
    assert_eq!(
        apply(&m, &ext(200, 3_599)),
        Err(Refusal::ExtendNotJustified)
    );
    assert!(matches!(
        apply(&m, &ext(100, 3_600)),
        Err(Refusal::BadTimeout(_))
    ));
    assert!(matches!(
        apply(&m, &ext(3_000_101, 3_600)),
        Err(Refusal::BadTimeout(_))
    ));
    apply(&m, &ext(3_000_100, 3_600)).unwrap();
}

#[test]
fn wrong_direction_requests_are_refused() {
    let m = Mem::default();
    apply(&m, &lock_solana()).unwrap();
    assert_eq!(apply(&m, &refund(2_000, true)), Err(Refusal::UnknownSwap));
    assert_eq!(
        apply(
            &m,
            &Request::ClaimSolana {
                payment_hash: hash(),
                preimage: P
            }
        ),
        Err(Refusal::UnknownSwap)
    );

    let m = Mem::default();
    apply(&m, &lock_liquid()).unwrap();
    assert_eq!(
        apply(
            &m,
            &Request::RefundSolana {
                payment_hash: hash()
            }
        ),
        Err(Refusal::UnknownSwap)
    );
}

#[test]
fn only_a_first_lock_counts_against_the_window() {
    let before = SwapRecord::default();
    assert_eq!(newly_locked(&before, &lock_solana()), Some(100));
    let locked = SwapRecord {
        direction: Some(Direction::L2S),
        locked_amount: 100,
        deadline_slot: 8_400,
        ..SwapRecord::default()
    };
    assert_eq!(newly_locked(&locked, &lock_solana()), None);
    assert_eq!(
        newly_locked(
            &before,
            &Request::RefundSolana {
                payment_hash: hash()
            }
        ),
        None
    );
}

#[test]
fn the_real_ledger_survives_a_restart_and_fails_closed() {
    let dir = std::env::temp_dir().join(format!("liq2sol-ledger-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("signer.db");
    let _ = std::fs::remove_file(&path);
    {
        let ledger = SignerLedger::open(&path).unwrap();
        let rec = check(&limits(), &ledger, NOW, &lock_liquid()).unwrap();
        ledger.commit(&hash(), &rec, Some((NOW, 100))).unwrap();
        let rec = check(
            &limits(),
            &ledger,
            NOW,
            &Request::ClaimSolana {
                payment_hash: hash(),
                preimage: P,
            },
        )
        .unwrap();
        ledger.commit(&hash(), &rec, None).unwrap();
    }
    let ledger = SignerLedger::open(&path).unwrap();
    assert_eq!(
        check(&limits(), &ledger, NOW, &refund(2_000, true)),
        Err(Refusal::RefundAfterClaim),
        "remembered across a restart"
    );
    assert_eq!(
        check(&limits(), &ledger, NOW, &lock_liquid()),
        Err(Refusal::AlreadyLocked)
    );
    assert_eq!(ledger.locked_in_window(NOW + 1, 3_600), 100);
    assert_eq!(
        ledger.locked_in_window(NOW + 3_601, 3_600),
        0,
        "an old lock leaves the window"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
