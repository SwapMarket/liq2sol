//! Restart behaviour: nothing committed is forgotten, and a payment hash is never accepted twice.

use liq2sol_maker_core::engine::*;
use liq2sol_maker_core::store::Store;

fn terms(n: u8) -> Terms {
    Terms {
        direction: Direction::S2L,
        payment_hash: [n; 32],
        liquid_amount: 100,
        solana_amount: 1,
        h_accept: 10,
        lock_height: 100,
    }
}

fn path(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("liq2sol-store-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("maker.db")
}

#[test]
fn a_payment_hash_is_never_accepted_twice_even_after_finishing() {
    let p = path("once");
    let s = Store::open(&p).unwrap();
    assert!(s.accept(&terms(1), &State::WatchLock).unwrap());
    assert!(!s.accept(&terms(1), &State::WatchLock).unwrap());
    s.commit_step(&[1; 32], &State::Done { outcome: Outcome::Completed }, &[]).unwrap();
    drop(s);
    let s = Store::open(&p).unwrap();
    assert!(!s.accept(&terms(1), &State::WatchLock).unwrap(), "remembered forever");
}

#[test]
fn state_and_intents_survive_a_restart_together() {
    let p = path("restart");
    let s = Store::open(&p).unwrap();
    s.accept(&terms(1), &State::WatchLock).unwrap();
    s.accept(&terms(2), &State::WatchLock).unwrap();
    let ids = s.commit_step(&[1; 32], &State::LockedLiquid, &[Action::LockLiquid]).unwrap();
    s.commit_step(&[2; 32], &State::Abandoned, &[]).unwrap();
    drop(s);

    let s = Store::open(&p).unwrap();
    assert_eq!(s.state(&[1; 32]).unwrap().unwrap().1, State::LockedLiquid);
    let open = s.open_swaps().unwrap();
    assert_eq!(open.len(), 1, "finished swaps are not reloaded");
    assert_eq!(open[0].0.payment_hash, [1; 32]);
    let pending = s.unfinished_intents().unwrap();
    assert_eq!(pending, vec![(ids[0], [1; 32], Action::LockLiquid)]);
    s.finish_intent(ids[0]).unwrap();
    assert!(s.unfinished_intents().unwrap().is_empty());
}

/// The double-payment guard: a crash between storing and broadcasting can only resend the same bytes.
#[test]
fn only_one_lock_transaction_can_ever_be_stored() {
    let p = path("locktx");
    let s = Store::open(&p).unwrap();
    s.accept(&terms(1), &State::WatchLock).unwrap();
    assert!(s.store_lock_tx(&[1; 32], b"tx").is_err(), "no intent yet");
    s.commit_step(&[1; 32], &State::LockedLiquid, &[Action::LockLiquid]).unwrap();
    s.store_lock_tx(&[1; 32], b"tx").unwrap();
    s.store_lock_tx(&[1; 32], b"tx").unwrap();
    assert!(s.store_lock_tx(&[1; 32], b"different").is_err());
    drop(s);
    let s = Store::open(&p).unwrap();
    assert_eq!(s.stored_lock_tx(&[1; 32]).unwrap().unwrap(), b"tx");
    assert!(s.store_lock_tx(&[1; 32], b"different").is_err(), "still refused after a restart");
}

#[test]
fn the_rollback_alarm_stays_until_cleared() {
    let p = path("alarm");
    let s = Store::open(&p).unwrap();
    assert!(!s.rollback().unwrap());
    s.set_rollback(true).unwrap();
    drop(s);
    let s = Store::open(&p).unwrap();
    assert!(s.rollback().unwrap());
    s.set_rollback(false).unwrap();
    assert!(!s.rollback().unwrap());
}
