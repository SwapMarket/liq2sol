//! Every rule in docs/protocol.md sections 4 and 5, and docs/maker.md sections 3 and 4.

use liq2sol_maker_core::engine::*;
use liq2sol_maker_core::hash_preimage;
use proptest::prelude::*;

const P: [u8; 32] = [7; 32];
const SOL: u64 = 100_000_000;
const ACCEPT: u32 = 1_000;

fn cfg() -> Config {
    Config::default()
}

fn terms(direction: Direction) -> Terms {
    let lock_height = match direction {
        Direction::L2S => ACCEPT + 300,
        Direction::S2L => ACCEPT + 90,
    };
    Terms {
        direction,
        payment_hash: hash_preimage(&P),
        liquid_amount: SOL * 100,
        solana_amount: SOL,
        h_accept: ACCEPT,
        lock_height,
    }
}

fn view() -> View {
    View {
        liquid_tip: ACCEPT + 5,
        liquid_tip_age_secs: 30,
        solana_slot: 50_000,
        solana_slot_age_secs: 1,
        rollback_detected: false,
        sources_disagree: false,
        maker_token_ok: true,
        htlc: HtlcView::Absent,
        escrow: None,
        open_exposure: 0,
    }
}

fn taker_escrow() -> EscrowView {
    EscrowView {
        state: EscrowState::Open,
        amount: SOL,
        payment_hash: hash_preimage(&P),
        recipient_ok: true,
        deadline_slot: 50_000 + 101_400,
        close_on_restart: false,
        extended: false,
        restarted: false,
        preimage: [0; 32],
    }
}

fn maker_escrow(deadline_slot: u64) -> EscrowView {
    EscrowView {
        deadline_slot,
        close_on_restart: true,
        ..taker_escrow()
    }
}

fn run(t: &Terms, s: &State, v: &View) -> (State, Vec<Action>) {
    step(&cfg(), t, s, v)
}

// ---- L to S ----

fn coin(confirmations: u32) -> HtlcView {
    HtlcView::Unspent { confirmations }
}

#[test]
fn l2s_locks_once_the_coin_is_confirmed() {
    let t = terms(Direction::L2S);
    let (s, a) = run(
        &t,
        &State::WatchLock,
        &View {
            htlc: coin(2),
            ..view()
        },
    );
    assert_eq!(
        s,
        State::LockedSolana {
            deadline_slot: 58_400
        }
    );
    assert_eq!(
        a,
        vec![Action::LockSolana {
            deadline_slot: 58_400
        }]
    );
}

#[test]
fn l2s_will_not_lock_without_each_condition() {
    let t = terms(Direction::L2S);
    let good = View {
        htlc: coin(2),
        ..view()
    };
    let bad = [
        View {
            htlc: HtlcView::Absent,
            ..good.clone()
        },
        View {
            htlc: coin(1),
            ..good.clone()
        },
        View {
            liquid_tip: ACCEPT + 31,
            ..good.clone()
        }, // Past the fund window.
        View {
            liquid_tip_age_secs: 601,
            ..good.clone()
        },
        View {
            solana_slot_age_secs: 61,
            ..good.clone()
        },
        View {
            sources_disagree: true,
            ..good.clone()
        },
        View {
            open_exposure: cfg().max_open_l2s,
            ..good.clone()
        },
        View {
            escrow: Some(maker_escrow(60_000)),
            ..good.clone()
        },
    ];
    for v in bad {
        let (_, a) = run(&t, &State::WatchLock, &v);
        assert!(a.is_empty(), "{v:?} gave {a:?}");
    }
}

#[test]
fn l2s_gate_margin_is_exact() {
    let mut t = terms(Direction::L2S);
    t.lock_height = ACCEPT + 5 + 210;
    let v = View {
        htlc: coin(2),
        ..view()
    };
    assert!(l2s_may_lock(&cfg(), &t, &v));
    t.lock_height -= 1;
    assert!(
        !l2s_may_lock(&cfg(), &t, &v),
        "209 blocks of margin must refuse"
    );
}

#[test]
fn large_swaps_need_more_confirmations_and_over_limit_swaps_never_lock() {
    let c = Config {
        max_per_swap: u64::MAX,
        max_open_l2s: u64::MAX,
        ..cfg()
    };
    let t = Terms {
        solana_amount: c.large_above + 1,
        ..terms(Direction::L2S)
    };
    assert!(!l2s_may_lock(
        &c,
        &t,
        &View {
            htlc: coin(9),
            ..view()
        }
    ));
    assert!(l2s_may_lock(
        &c,
        &t,
        &View {
            htlc: coin(10),
            ..view()
        }
    ));
    let t = Terms {
        solana_amount: cfg().max_per_swap + 1,
        ..terms(Direction::L2S)
    };
    assert!(!l2s_may_lock(
        &cfg(),
        &t,
        &View {
            htlc: coin(99),
            ..view()
        }
    ));
}

#[test]
fn l2s_abandons_when_no_lock_arrives_in_the_window() {
    let t = terms(Direction::L2S);
    let (s, a) = run(
        &t,
        &State::WatchLock,
        &View {
            liquid_tip: ACCEPT + 31,
            ..view()
        },
    );
    assert_eq!((s, a), (State::Abandoned, vec![]));
}

#[test]
fn l2s_retries_a_lost_lock_with_the_same_deadline_while_the_gate_holds() {
    let t = terms(Direction::L2S);
    let s = State::LockedSolana {
        deadline_slot: 58_400,
    };
    let (s2, a) = run(
        &t,
        &s,
        &View {
            solana_slot: 50_500,
            ..view()
        },
    );
    assert_eq!(s2, s);
    assert_eq!(
        a,
        vec![Action::LockSolana {
            deadline_slot: 58_400
        }]
    );
    // Too late to land safely: stop sending, and give up at the deadline.
    let (s2, a) = run(
        &t,
        &s,
        &View {
            liquid_tip: t.lock_height - 209,
            solana_slot: 50_500,
            ..view()
        },
    );
    assert_eq!((s2, a), (s.clone(), vec![]));
    let (s2, a) = run(
        &t,
        &s,
        &View {
            solana_slot: 58_400,
            ..view()
        },
    );
    assert_eq!((s2, a), (State::Abandoned, vec![]));
}

#[test]
fn l2s_reads_the_preimage_from_the_account_and_claims_until_confirmed() {
    let t = terms(Direction::L2S);
    let s = State::LockedSolana {
        deadline_slot: 58_400,
    };
    let claimed = EscrowView {
        state: EscrowState::Claimed,
        preimage: P,
        ..maker_escrow(58_400)
    };
    let v = View {
        escrow: Some(claimed),
        htlc: coin(5),
        ..view()
    };
    let (s2, a) = run(&t, &s, &v);
    assert_eq!(s2, State::Claiming { preimage: P });
    assert_eq!(a, vec![Action::ClaimLiquid { preimage: P }]);

    // Unconfirmed is not finished: a dropped transaction must still be retried.
    let mempool = View {
        htlc: HtlcView::SpentByClaim {
            preimage: P,
            confirmed: false,
        },
        ..v.clone()
    };
    let (s3, a) = run(&t, &s2, &mempool);
    assert_eq!(
        (s3, a),
        (s2.clone(), vec![Action::ClaimLiquid { preimage: P }])
    );
    let done = View {
        htlc: HtlcView::SpentByClaim {
            preimage: P,
            confirmed: true,
        },
        ..v
    };
    assert_eq!(
        run(&t, &s2, &done),
        (
            State::Done {
                outcome: Outcome::Completed
            },
            vec![]
        )
    );
}

#[test]
fn l2s_ignores_a_stored_preimage_that_does_not_hash() {
    let t = terms(Direction::L2S);
    let s = State::LockedSolana {
        deadline_slot: 58_400,
    };
    let bad = EscrowView {
        state: EscrowState::Claimed,
        preimage: [9; 32],
        ..maker_escrow(58_400)
    };
    let (s2, a) = run(
        &t,
        &s,
        &View {
            escrow: Some(bad),
            htlc: coin(5),
            ..view()
        },
    );
    assert_eq!(s2, s);
    assert!(matches!(a.as_slice(), [Action::Page(Page::Impossible(_))]));
}

#[test]
fn l2s_pages_when_the_claim_is_urgent_and_when_it_is_lost() {
    let t = terms(Direction::L2S);
    let s = State::Claiming { preimage: P };
    let (_, a) = run(
        &t,
        &s,
        &View {
            htlc: coin(5),
            liquid_tip: t.lock_height - 60,
            ..view()
        },
    );
    assert!(a.contains(&Action::Page(Page::ClaimUrgent { left: 60 })));
    let (_, a) = run(
        &t,
        &s,
        &View {
            htlc: coin(5),
            liquid_tip: t.lock_height - 61,
            ..view()
        },
    );
    assert_eq!(a, vec![Action::ClaimLiquid { preimage: P }]);
    let (s2, a) = run(
        &t,
        &s,
        &View {
            htlc: HtlcView::SpentByRefund { confirmed: true },
            ..view()
        },
    );
    assert_eq!(
        s2,
        State::Done {
            outcome: Outcome::Lost
        }
    );
    assert_eq!(a, vec![Action::Page(Page::Lost)]);
}

#[test]
fn l2s_refunds_at_the_deadline_or_after_a_restart_and_prefers_a_claim_that_landed() {
    let t = terms(Direction::L2S);
    let s = State::LockedSolana {
        deadline_slot: 58_400,
    };
    let open = maker_escrow(58_400);
    let (s2, a) = run(
        &t,
        &s,
        &View {
            escrow: Some(open.clone()),
            solana_slot: 58_399,
            ..view()
        },
    );
    assert_eq!((s2, a), (s.clone(), vec![]));
    let (s2, a) = run(
        &t,
        &s,
        &View {
            escrow: Some(open.clone()),
            solana_slot: 58_400,
            ..view()
        },
    );
    assert_eq!(
        (s2.clone(), a),
        (State::Refunding, vec![Action::RefundSolana])
    );
    let restarted = EscrowView {
        restarted: true,
        ..open.clone()
    };
    let (s3, a) = run(
        &t,
        &s,
        &View {
            escrow: Some(restarted),
            ..view()
        },
    );
    assert_eq!((s3, a), (State::Refunding, vec![Action::RefundSolana]));

    let done = EscrowView {
        state: EscrowState::Refunded,
        ..open.clone()
    };
    assert_eq!(
        run(
            &t,
            &s2,
            &View {
                escrow: Some(done),
                ..view()
            }
        )
        .0,
        State::Done {
            outcome: Outcome::Refunded
        }
    );
    let claimed = EscrowView {
        state: EscrowState::Claimed,
        preimage: P,
        ..open
    };
    let (s3, a) = run(
        &t,
        &s2,
        &View {
            escrow: Some(claimed),
            htlc: coin(5),
            ..view()
        },
    );
    assert_eq!(
        (s3, a),
        (
            State::Claiming { preimage: P },
            vec![Action::ClaimLiquid { preimage: P }]
        )
    );
}

// ---- S to L ----

#[test]
fn s2l_locks_liquid_once_the_escrow_matches() {
    let t = terms(Direction::S2L);
    let (s, a) = run(
        &t,
        &State::WatchLock,
        &View {
            escrow: Some(taker_escrow()),
            ..view()
        },
    );
    assert_eq!((s, a), (State::LockedLiquid, vec![Action::LockLiquid]));
}

#[test]
fn s2l_will_not_lock_without_each_condition() {
    let t = terms(Direction::S2L);
    let e = taker_escrow();
    let good = View {
        escrow: Some(e.clone()),
        ..view()
    };
    let bad = [
        View {
            escrow: None,
            ..good.clone()
        },
        View {
            escrow: Some(EscrowView {
                amount: SOL - 1,
                ..e.clone()
            }),
            ..good.clone()
        },
        View {
            escrow: Some(EscrowView {
                payment_hash: [1; 32],
                ..e.clone()
            }),
            ..good.clone()
        },
        View {
            escrow: Some(EscrowView {
                recipient_ok: false,
                ..e.clone()
            }),
            ..good.clone()
        },
        View {
            escrow: Some(EscrowView {
                close_on_restart: true,
                ..e.clone()
            }),
            ..good.clone()
        },
        View {
            escrow: Some(EscrowView {
                extended: true,
                ..e.clone()
            }),
            ..good.clone()
        },
        View {
            escrow: Some(EscrowView {
                state: EscrowState::Refunded,
                ..e.clone()
            }),
            ..good.clone()
        },
        View {
            escrow: Some(EscrowView {
                deadline_slot: 50_000 + 84_499,
                ..e.clone()
            }),
            ..good.clone()
        },
        View {
            maker_token_ok: false,
            ..good.clone()
        },
        View {
            htlc: coin(0),
            ..good.clone()
        },
        View {
            liquid_tip_age_secs: 601,
            ..good.clone()
        },
        View {
            solana_slot_age_secs: 61,
            ..good.clone()
        },
        View {
            sources_disagree: true,
            ..good.clone()
        },
        View {
            open_exposure: cfg().max_open_s2l,
            ..good.clone()
        },
    ];
    for v in bad {
        let (_, a) = run(&t, &State::WatchLock, &v);
        assert!(!a.contains(&Action::LockLiquid), "{v:?}");
    }
    assert!(s2l_may_lock(
        &cfg(),
        &t,
        &View {
            escrow: Some(EscrowView {
                deadline_slot: 50_000 + 84_500,
                ..e
            }),
            ..good
        }
    ));
}

#[test]
fn s2l_past_the_window_abandons_and_releases_the_taker() {
    let t = terms(Direction::S2L);
    let v = View {
        liquid_tip: ACCEPT + 31,
        escrow: Some(taker_escrow()),
        ..view()
    };
    assert_eq!(
        run(&t, &State::WatchLock, &v),
        (State::Abandoned, vec![Action::Release])
    );
    // Someone else's escrow gets no release, and no escrow means nothing to release.
    let other = View {
        escrow: Some(EscrowView {
            recipient_ok: false,
            ..taker_escrow()
        }),
        ..v.clone()
    };
    assert_eq!(
        run(&t, &State::WatchLock, &other),
        (State::Abandoned, vec![])
    );
    assert_eq!(
        run(&t, &State::WatchLock, &View { escrow: None, ..v }),
        (State::Abandoned, vec![])
    );
}

#[test]
fn s2l_only_ever_resends_the_same_lock() {
    let t = terms(Direction::S2L);
    let (s, a) = run(
        &t,
        &State::LockedLiquid,
        &View {
            escrow: Some(taker_escrow()),
            ..view()
        },
    );
    assert_eq!((s, a), (State::LockedLiquid, vec![Action::RebroadcastLock]));
    let (_, a) = run(
        &t,
        &State::LockedLiquid,
        &View {
            liquid_tip: t.lock_height,
            escrow: Some(taker_escrow()),
            ..view()
        },
    );
    assert_eq!(a, vec![Action::Page(Page::LockMissing)]);
}

#[test]
fn s2l_reads_the_preimage_from_a_mempool_claim_and_collects() {
    let t = terms(Direction::S2L);
    let v = View {
        htlc: HtlcView::SpentByClaim {
            preimage: P,
            confirmed: false,
        },
        escrow: Some(taker_escrow()),
        ..view()
    };
    let (s, a) = run(&t, &State::LockedLiquid, &v);
    assert_eq!(s, State::Claiming { preimage: P });
    assert_eq!(a, vec![Action::ClaimSolana { preimage: P }]);
    let claimed = EscrowView {
        state: EscrowState::Claimed,
        ..taker_escrow()
    };
    assert_eq!(
        run(
            &t,
            &s,
            &View {
                escrow: Some(claimed),
                ..v.clone()
            }
        )
        .0,
        State::Done {
            outcome: Outcome::Completed
        }
    );
    let refunded = EscrowView {
        state: EscrowState::Refunded,
        ..taker_escrow()
    };
    let (s2, a) = run(
        &t,
        &s,
        &View {
            escrow: Some(refunded),
            ..v
        },
    );
    assert_eq!(
        (s2, a),
        (
            State::Done {
                outcome: Outcome::Lost
            },
            vec![Action::Page(Page::Lost)]
        )
    );
}

#[test]
fn s2l_rejects_a_claim_witness_that_does_not_hash() {
    let t = terms(Direction::S2L);
    let v = View {
        htlc: HtlcView::SpentByClaim {
            preimage: [9; 32],
            confirmed: true,
        },
        escrow: Some(taker_escrow()),
        ..view()
    };
    let (s, a) = run(&t, &State::LockedLiquid, &v);
    assert_eq!(s, State::LockedLiquid);
    assert!(matches!(a.as_slice(), [Action::Page(Page::Impossible(_))]));
}

#[test]
fn s2l_pages_when_an_unclaimed_preimage_nears_the_deadline() {
    let t = terms(Direction::S2L);
    let s = State::Claiming { preimage: P };
    let e = EscrowView {
        deadline_slot: 50_000 + 16_900,
        ..taker_escrow()
    };
    let (_, a) = run(
        &t,
        &s,
        &View {
            escrow: Some(e),
            ..view()
        },
    );
    assert!(a.contains(&Action::Page(Page::ClaimUrgent { left: 16_900 })));
}

#[test]
fn s2l_refund_timing_and_a_late_claim_racing_it() {
    let t = terms(Direction::S2L);
    let e = Some(taker_escrow());
    let (s, a) = run(
        &t,
        &State::LockedLiquid,
        &View {
            liquid_tip: t.lock_height - 1,
            htlc: coin(5),
            escrow: e.clone(),
            ..view()
        },
    );
    assert_eq!((s, a), (State::LockedLiquid, vec![]));
    let (s, a) = run(
        &t,
        &State::LockedLiquid,
        &View {
            liquid_tip: t.lock_height,
            htlc: coin(5),
            escrow: e.clone(),
            ..view()
        },
    );
    assert_eq!(
        (s.clone(), a),
        (State::Refunding, vec![Action::RefundLiquid])
    );
    let (s2, a) = run(
        &t,
        &s,
        &View {
            htlc: HtlcView::SpentByClaim {
                preimage: P,
                confirmed: false,
            },
            escrow: e.clone(),
            ..view()
        },
    );
    assert_eq!(
        (s2, a),
        (
            State::Claiming { preimage: P },
            vec![Action::ClaimSolana { preimage: P }]
        )
    );
    let (s2, a) = run(
        &t,
        &s,
        &View {
            htlc: HtlcView::SpentByRefund { confirmed: false },
            escrow: e.clone(),
            ..view()
        },
    );
    assert_eq!((s2, a), (State::Refunding, vec![Action::RefundLiquid]));
    assert_eq!(
        run(
            &t,
            &s,
            &View {
                htlc: HtlcView::SpentByRefund { confirmed: true },
                escrow: e,
                ..view()
            }
        )
        .0,
        State::Done {
            outcome: Outcome::Refunded
        }
    );
}

#[test]
fn s2l_extends_only_for_a_long_stall_near_the_deadline() {
    let t = terms(Direction::S2L);
    let near = EscrowView {
        deadline_slot: 50_000 + 16_900,
        ..taker_escrow()
    };
    let v = View {
        liquid_tip_age_secs: 3_600,
        htlc: coin(5),
        escrow: Some(near.clone()),
        ..view()
    };
    let want = Action::Extend {
        new_deadline: 50_000 + 16_900 + 3_000_000,
    };
    assert_eq!(run(&t, &State::LockedLiquid, &v).1, vec![want]);
    let no = |v: View| assert_eq!(run(&t, &State::LockedLiquid, &v).1, vec![], "{v:?}");
    no(View {
        liquid_tip_age_secs: 3_599,
        ..v.clone()
    });
    no(View {
        escrow: Some(EscrowView {
            deadline_slot: 50_000 + 16_901,
            ..near.clone()
        }),
        ..v.clone()
    });
    no(View {
        escrow: Some(EscrowView {
            extended: true,
            ..near.clone()
        }),
        ..v.clone()
    });
    no(View {
        solana_slot: near.deadline_slot,
        ..v
    });
}

// ---- Both ----

#[test]
fn rollback_stops_everything_and_pages() {
    for d in [Direction::L2S, Direction::S2L] {
        let t = terms(d);
        for s in [
            State::WatchLock,
            State::LockedLiquid,
            State::LockedSolana { deadline_slot: 1 },
            State::Claiming { preimage: P },
            State::Refunding,
        ] {
            let v = View {
                rollback_detected: true,
                htlc: coin(9),
                escrow: Some(taker_escrow()),
                liquid_tip: t.lock_height,
                ..view()
            };
            assert_eq!(
                run(&t, &s, &v),
                (s.clone(), vec![Action::Page(Page::Rollback)])
            );
        }
    }
}

/// Protocol section 7.3: after a rollback, an honest maker that collected on Solana must leave the coin to the taker.
#[test]
fn a_finished_swap_never_refunds_even_if_the_coin_comes_back() {
    let t = terms(Direction::S2L);
    let done = State::Done {
        outcome: Outcome::Completed,
    };
    let claimed = EscrowView {
        state: EscrowState::Claimed,
        ..taker_escrow()
    };
    let v = View {
        liquid_tip: t.lock_height + 100,
        htlc: coin(9),
        escrow: Some(claimed.clone()),
        ..view()
    };
    assert_eq!(run(&t, &done, &v), (done.clone(), vec![]));
    // Even a swap somehow still in LockedLiquid sees the escrow as claimed, and leaves it alone.
    assert_eq!(
        run(&t, &State::LockedLiquid, &v),
        (State::LockedLiquid, vec![])
    );
    assert_eq!(run(&t, &State::Refunding, &v), (State::Refunding, vec![]));
}

// ---- Invariants under arbitrary inputs ----

fn any_escrow() -> impl Strategy<Value = Option<EscrowView>> {
    proptest::option::of(
        (
            0u8..3,
            0u64..3,
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
            40_000u64..200_000,
            any::<bool>(),
        )
            .prop_map(
                |(
                    state,
                    amount,
                    hash_ok,
                    recipient_ok,
                    close,
                    extended,
                    deadline_slot,
                    good_preimage,
                )| EscrowView {
                    state: [
                        EscrowState::Open,
                        EscrowState::Claimed,
                        EscrowState::Refunded,
                    ][state as usize],
                    amount: SOL - amount,
                    payment_hash: if hash_ok { hash_preimage(&P) } else { [1; 32] },
                    recipient_ok,
                    deadline_slot,
                    close_on_restart: close,
                    extended,
                    restarted: extended && close,
                    preimage: if good_preimage { P } else { [3; 32] },
                },
            ),
    )
}

fn any_view() -> impl Strategy<Value = View> {
    (
        (
            900u32..1_400,
            0u64..5_000,
            40_000u64..200_000,
            0u64..200,
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
        ),
        0u8..5,
        any::<bool>(),
        any_escrow(),
        0u64..6_000_000_000,
    )
        .prop_map(
            |(
                (
                    liquid_tip,
                    liquid_tip_age_secs,
                    solana_slot,
                    solana_slot_age_secs,
                    rollback_detected,
                    sources_disagree,
                    maker_token_ok,
                ),
                htlc,
                flag,
                escrow,
                open_exposure,
            )| View {
                liquid_tip,
                liquid_tip_age_secs,
                solana_slot,
                solana_slot_age_secs,
                rollback_detected,
                sources_disagree,
                maker_token_ok,
                htlc: match htlc {
                    0 => HtlcView::Absent,
                    1 => HtlcView::Unspent {
                        confirmations: u32::from(flag) * 5,
                    },
                    2 => HtlcView::SpentByClaim {
                        preimage: P,
                        confirmed: flag,
                    },
                    3 => HtlcView::SpentByClaim {
                        preimage: [3; 32],
                        confirmed: flag,
                    },
                    _ => HtlcView::SpentByRefund { confirmed: flag },
                },
                escrow,
                open_exposure,
            },
        )
}

fn is_safe_preimage(a: &Action) -> bool {
    match a {
        Action::ClaimLiquid { preimage } | Action::ClaimSolana { preimage } => {
            hash_preimage(preimage) == hash_preimage(&P)
        }
        _ => true,
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 2_000, ..ProptestConfig::default() })]

    /// Run whole random histories, and check the rules that must never break.
    #[test]
    fn invariants_hold_over_random_histories(s2l in any::<bool>(), views in proptest::collection::vec(any_view(), 1..25)) {
        let t = terms(if s2l { Direction::S2L } else { Direction::L2S });
        let c = cfg();
        let mut state = State::WatchLock;
        let mut lock_liquid_count = 0;
        let mut collected = false;
        for v in &views {
            let before = state.clone();
            let (next, actions) = step(&c, &t, &state, v);

            for a in &actions {
                prop_assert!(is_safe_preimage(a), "only a correct preimage is ever used: {a:?}");
                if v.rollback_detected {
                    prop_assert!(matches!(a, Action::Page(_)), "a rollback stops everything but pages: {a:?}");
                }
                match a {
                    Action::LockLiquid => {
                        lock_liquid_count += 1;
                        prop_assert!(s2l && s2l_may_lock(&c, &t, v) && before == State::WatchLock);
                    }
                    Action::LockSolana { .. } => {
                        prop_assert!(!s2l);
                        if before == State::WatchLock {
                            prop_assert!(l2s_may_lock(&c, &t, v));
                        }
                        prop_assert!(t.lock_height.saturating_sub(v.liquid_tip) >= c.l2s_gate_blocks, "never lock inside the margin");
                    }
                    Action::RefundLiquid => {
                        prop_assert!(s2l && v.liquid_tip >= t.lock_height || before == State::Refunding);
                        prop_assert!(!collected, "never refund a coin after collecting its escrow");
                        prop_assert!(!v.escrow.as_ref().is_some_and(|e| e.state == EscrowState::Claimed));
                    }
                    Action::Release => {
                        prop_assert!(before == State::WatchLock && lock_liquid_count == 0, "never release after locking");
                    }
                    Action::Extend { new_deadline } => {
                        let e = v.escrow.as_ref().expect("extend needs an escrow");
                        prop_assert!(s2l && !e.extended && e.state == EscrowState::Open);
                        prop_assert!(*new_deadline > e.deadline_slot && *new_deadline - e.deadline_slot <= 3_000_000);
                        prop_assert!(v.liquid_tip_age_secs >= c.extend_after_stall_secs);
                    }
                    _ => {}
                }
            }
            prop_assert!(lock_liquid_count <= 1, "the Liquid lock can't be paid twice");

            if matches!(before, State::Done { .. } | State::Abandoned) {
                prop_assert_eq!(&next, &before, "finished is final");
                prop_assert!(actions.is_empty(), "a finished swap does nothing");
            }
            if v.rollback_detected {
                prop_assert_eq!(&next, &before, "a rollback changes no state");
            }
            if v.escrow.as_ref().is_some_and(|e| e.state == EscrowState::Claimed) && s2l {
                collected = true;
            }
            state = next;
        }
    }
}
