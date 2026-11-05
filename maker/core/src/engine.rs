//! One swap's state machine. `step` is a pure function: (state, what the chains show) -> (new state, actions).
//!
//! Actions may be repeated if the daemon crashes or a broadcast is lost, so each must be safe to repeat. The one
//! thing that isn't is paying twice to the Liquid HTLC, which is why `LockLiquid` is emitted once and retries use
//! `RebroadcastLock`, the identical stored transaction.

use serde::{Deserialize, Serialize};

use crate::hash_preimage;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    /// Taker locks L-USDt, maker locks SOL-USDT.
    L2S,
    /// Taker locks SOL-USDT, maker locks L-USDt.
    S2L,
}

/// Limits from docs/protocol.md section 3 and docs/maker.md sections 3 and 4.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub confirmations: u32,
    /// Swaps above this many Solana base units need `confirmations_large` (decision D7).
    pub large_above: u64,
    pub confirmations_large: u32,
    pub max_per_swap: u64,
    pub max_open_l2s: u64,
    pub max_open_s2l: u64,
    /// L→S: lock only with at least this many blocks before `lock_height`.
    pub l2s_gate_blocks: u32,
    pub l2s_fund_window_blocks: u32,
    /// Length of the short Solana lock the maker creates.
    pub l2s_deadline_slots: u64,
    /// S→L: lock only with at least this many slots before the taker's deadline.
    pub s2l_gate_slots: u64,
    pub s2l_lock_window_blocks: u32,
    pub liquid_stale_secs: u64,
    pub solana_stale_secs: u64,
    /// S→L: extend when Liquid has been stalled this long...
    pub extend_after_stall_secs: u64,
    /// ...and the taker's refund is this close.
    pub extend_within_slots: u64,
    pub extend_by_slots: u64,
    /// L→S: page when a reveal is unclaimed and the lock height is this close.
    pub urgent_blocks: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            confirmations: 2,
            large_above: 1_000_000_000,
            confirmations_large: 10,
            max_per_swap: 1_000_000_000,
            max_open_l2s: 5_000_000_000,
            max_open_s2l: 5_000_000_000,
            l2s_gate_blocks: 210,
            l2s_fund_window_blocks: 30,
            l2s_deadline_slots: 8_400,
            s2l_gate_slots: 84_500,
            s2l_lock_window_blocks: 30,
            liquid_stale_secs: 600,
            solana_stale_secs: 60,
            extend_after_stall_secs: 3_600,
            extend_within_slots: 16_900,
            extend_by_slots: 3_000_000,
            urgent_blocks: 60,
        }
    }
}

/// What both sides agreed in `rfq-terms`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Terms {
    pub direction: Direction,
    pub payment_hash: [u8; 32],
    pub liquid_amount: u64,
    pub solana_amount: u64,
    pub h_accept: u32,
    pub lock_height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum State {
    /// Waiting for the taker's lock.
    WatchLock,
    /// L→S: our Solana lock is sent, with its deadline fixed for every retry.
    LockedSolana { deadline_slot: u64 },
    /// S→L: our Liquid lock is sent.
    LockedLiquid,
    /// The preimage is known, and our claim on the taker's chain is in progress.
    Claiming { preimage: [u8; 32] },
    /// Our refund is in progress.
    Refunding,
    /// Nothing more to do: claimed, refunded or lost, as `outcome` says.
    Done { outcome: Outcome },
    /// The swap never started, so no funds of ours moved.
    Abandoned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    Completed,
    Refunded,
    /// Our funds are gone without payment. Always paged.
    Lost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EscrowState {
    Open,
    Claimed,
    Refunded,
}

/// The escrow PDA for this swap, read at `finalized`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EscrowView {
    pub state: EscrowState,
    pub amount: u64,
    pub payment_hash: [u8; 32],
    /// The recipient is the party that should receive this escrow under the terms.
    pub recipient_ok: bool,
    pub deadline_slot: u64,
    pub close_on_restart: bool,
    pub extended: bool,
    /// Short-leg only: a restart after creation has closed the claim.
    pub restarted: bool,
    pub preimage: [u8; 32],
}

/// What is known about the Liquid HTLC coin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HtlcView {
    /// No exact, explicit coin of the right asset and amount.
    Absent,
    Unspent {
        confirmations: u32,
    },
    /// Spent by a claim that reveals this preimage, which is checked against the hash here. A preimage is usable at
    /// once, but a swap isn't finished until the spend is `confirmed`, because an unconfirmed one can still drop.
    SpentByClaim {
        preimage: [u8; 32],
        confirmed: bool,
    },
    /// Spent by the refund path.
    SpentByRefund {
        confirmed: bool,
    },
}

/// One tick's view of the world. The caller only sets values that its sources agree on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct View {
    pub liquid_tip: u32,
    pub liquid_tip_age_secs: u64,
    pub solana_slot: u64,
    pub solana_slot_age_secs: u64,
    /// Liquid's tip went down, or a confirmed transaction vanished. Latched until an operator clears it.
    pub rollback_detected: bool,
    /// The two Liquid sources disagree, so nothing they say can start a new lock.
    pub sources_disagree: bool,
    /// The maker's Solana token account exists and is not frozen.
    pub maker_token_ok: bool,
    pub htlc: HtlcView,
    pub escrow: Option<EscrowView>,
    /// Open exposure in this direction, excluding this swap.
    pub open_exposure: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    /// L→S: lock the maker's Solana funds, with `close_on_restart` set.
    LockSolana {
        deadline_slot: u64,
    },
    /// S→L: pay the HTLC and the taker's fee coin. Emitted at most once per swap.
    LockLiquid,
    /// S→L: send the identical stored lock transaction again.
    RebroadcastLock,
    /// L→S: claim the taker's coin on Liquid.
    ClaimLiquid {
        preimage: [u8; 32],
    },
    /// S→L: claim the taker's escrow on Solana.
    ClaimSolana {
        preimage: [u8; 32],
    },
    /// L→S: take back our Solana lock.
    RefundSolana,
    /// S→L: take back our Liquid lock.
    RefundLiquid,
    /// S→L: move the taker's refund later during a Liquid halt.
    Extend {
        new_deadline: u64,
    },
    /// S→L: we won't fill this swap, so give the taker's funds back at once.
    Release,
    Page(Page),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Page {
    Rollback,
    /// A known preimage still isn't claimed, and time is short. In blocks for L→S, in slots for S→L.
    ClaimUrgent {
        left: u64,
    },
    /// Our Liquid lock never appeared by the lock height.
    LockMissing,
    Lost,
    /// An on-chain value that should be impossible, such as a stored preimage that doesn't hash correctly.
    Impossible(String),
}

fn required_confirmations(cfg: &Config, terms: &Terms) -> u32 {
    if terms.solana_amount > cfg.large_above {
        cfg.confirmations_large
    } else {
        cfg.confirmations
    }
}

/// Whether both chains are fresh and agree enough to take on new risk.
fn healthy(cfg: &Config, v: &View) -> bool {
    v.liquid_tip_age_secs <= cfg.liquid_stale_secs
        && v.solana_slot_age_secs <= cfg.solana_stale_secs
        && !v.sources_disagree
}

fn within_limits(cfg: &Config, terms: &Terms, v: &View) -> bool {
    let max_open = match terms.direction {
        Direction::L2S => cfg.max_open_l2s,
        Direction::S2L => cfg.max_open_s2l,
    };
    terms.solana_amount <= cfg.max_per_swap
        && v.open_exposure.checked_add(terms.solana_amount).is_some_and(|total| total <= max_open)
}

/// The L→S gate: maker.md section 3.
pub fn l2s_may_lock(cfg: &Config, terms: &Terms, v: &View) -> bool {
    let coin_ok =
        matches!(v.htlc, HtlcView::Unspent { confirmations } if confirmations >= required_confirmations(cfg, terms));
    coin_ok
        && terms.lock_height.saturating_sub(v.liquid_tip) >= cfg.l2s_gate_blocks
        && v.liquid_tip <= terms.h_accept.saturating_add(cfg.l2s_fund_window_blocks)
        && v.escrow.is_none()
        && healthy(cfg, v)
        && within_limits(cfg, terms, v)
}

/// The taker's S→L escrow must match the terms exactly.
pub fn s2l_escrow_acceptable(cfg: &Config, terms: &Terms, v: &View) -> bool {
    v.escrow.as_ref().is_some_and(|e| {
        e.state == EscrowState::Open
            && e.amount == terms.solana_amount
            && e.payment_hash == terms.payment_hash
            && e.recipient_ok
            && !e.close_on_restart
            && !e.extended
            && e.deadline_slot.saturating_sub(v.solana_slot) >= cfg.s2l_gate_slots
    })
}

/// The S→L gate: maker.md section 3.
pub fn s2l_may_lock(cfg: &Config, terms: &Terms, v: &View) -> bool {
    s2l_escrow_acceptable(cfg, terms, v)
        && v.htlc == HtlcView::Absent
        && v.liquid_tip <= terms.h_accept.saturating_add(cfg.s2l_lock_window_blocks)
        && v.maker_token_ok
        && healthy(cfg, v)
        && within_limits(cfg, terms, v)
}

/// Decide the next state and actions for one swap.
pub fn step(cfg: &Config, terms: &Terms, state: &State, v: &View) -> (State, Vec<Action>) {
    // A rollback means nothing we believed is reliable. Stop everything and wake a human.
    if v.rollback_detected {
        let actions = match state {
            State::Done { .. } | State::Abandoned => vec![],
            _ => vec![Action::Page(Page::Rollback)],
        };
        return (state.clone(), actions);
    }
    match terms.direction {
        Direction::L2S => step_l2s(cfg, terms, state, v),
        Direction::S2L => step_s2l(cfg, terms, state, v),
    }
}

fn step_l2s(cfg: &Config, terms: &Terms, state: &State, v: &View) -> (State, Vec<Action>) {
    match state {
        State::WatchLock => {
            if l2s_may_lock(cfg, terms, v) {
                let deadline_slot = v.solana_slot.saturating_add(cfg.l2s_deadline_slots);
                (State::LockedSolana { deadline_slot }, vec![Action::LockSolana { deadline_slot }])
            } else if v.liquid_tip > terms.h_accept.saturating_add(cfg.l2s_fund_window_blocks) {
                // Nothing of ours moved. A coin the taker sent late is theirs to refund.
                (State::Abandoned, vec![])
            } else {
                (state.clone(), vec![])
            }
        }
        State::LockedSolana { .. } => l2s_locked(cfg, terms, state, v),
        State::LockedLiquid => wrong_direction(state),
        State::Claiming { preimage } => match &v.htlc {
            HtlcView::SpentByClaim { confirmed: true, .. } => (State::Done { outcome: Outcome::Completed }, vec![]),
            // Ours is in the mempool. Sending it again is harmless, and it survives a dropped transaction.
            HtlcView::SpentByClaim { confirmed: false, .. } => {
                (state.clone(), vec![Action::ClaimLiquid { preimage: *preimage }])
            }
            HtlcView::SpentByRefund { confirmed: true } => {
                (State::Done { outcome: Outcome::Lost }, vec![Action::Page(Page::Lost)])
            }
            _ => {
                let mut actions = vec![Action::ClaimLiquid { preimage: *preimage }];
                let left = terms.lock_height.saturating_sub(v.liquid_tip);
                if left <= cfg.urgent_blocks {
                    actions.push(Action::Page(Page::ClaimUrgent { left: u64::from(left) }));
                }
                (state.clone(), actions)
            }
        },
        State::Refunding => match v.escrow.as_ref().map(|e| e.state) {
            Some(EscrowState::Refunded) => (State::Done { outcome: Outcome::Refunded }, vec![]),
            // A claim that landed first still pays us on Liquid, so go back and take it.
            Some(EscrowState::Claimed) => l2s_locked(cfg, terms, state, v),
            _ => (state.clone(), vec![Action::RefundSolana]),
        },
        State::Done { .. } | State::Abandoned => (state.clone(), vec![]),
    }
}

/// A stored state from the other direction means the store is corrupt, so do nothing and wake a human.
fn wrong_direction(state: &State) -> (State, Vec<Action>) {
    (state.clone(), vec![Action::Page(Page::Impossible("state does not match the swap's direction".into()))])
}

fn l2s_locked(cfg: &Config, terms: &Terms, state: &State, v: &View) -> (State, Vec<Action>) {
    let Some(escrow) = &v.escrow else {
        let State::LockedSolana { deadline_slot } = state else {
            return (state.clone(), vec![]);
        };
        if v.solana_slot >= *deadline_slot {
            // It never landed, and now it can't. No funds of ours moved.
            return (State::Abandoned, vec![]);
        }
        // Repeating is safe, because the PDA can only be created once. It keeps the same deadline, and stops once a
        // late landing would no longer leave the margin the gate promises.
        let safe = terms.lock_height.saturating_sub(v.liquid_tip) >= cfg.l2s_gate_blocks;
        let actions = if safe { vec![Action::LockSolana { deadline_slot: *deadline_slot }] } else { vec![] };
        return (state.clone(), actions);
    };
    match escrow.state {
        EscrowState::Claimed => {
            if hash_preimage(&escrow.preimage) != terms.payment_hash {
                let page = Page::Impossible("stored preimage does not match the hash".into());
                return (state.clone(), vec![Action::Page(page)]);
            }
            let next = State::Claiming { preimage: escrow.preimage };
            step_l2s(cfg, terms, &next, v)
        }
        EscrowState::Refunded => (State::Done { outcome: Outcome::Refunded }, vec![]),
        EscrowState::Open => {
            if v.solana_slot >= escrow.deadline_slot || escrow.restarted {
                (State::Refunding, vec![Action::RefundSolana])
            } else {
                (state.clone(), vec![])
            }
        }
    }
}

fn step_s2l(cfg: &Config, terms: &Terms, state: &State, v: &View) -> (State, Vec<Action>) {
    match state {
        State::WatchLock => {
            if s2l_may_lock(cfg, terms, v) {
                (State::LockedLiquid, vec![Action::LockLiquid])
            } else if v.liquid_tip > terms.h_accept.saturating_add(cfg.s2l_lock_window_blocks) {
                // We won't fill this. If the taker locked, hand its funds back rather than make it wait.
                let open = v.escrow.as_ref().is_some_and(|e| e.state == EscrowState::Open && e.recipient_ok);
                (State::Abandoned, if open { vec![Action::Release] } else { vec![] })
            } else {
                (state.clone(), vec![])
            }
        }
        State::LockedLiquid => s2l_locked(cfg, terms, state, v),
        State::LockedSolana { .. } => wrong_direction(state),
        State::Claiming { preimage } => match v.escrow.as_ref() {
            Some(e) if e.state == EscrowState::Claimed => (State::Done { outcome: Outcome::Completed }, vec![]),
            Some(e) if e.state == EscrowState::Refunded => {
                (State::Done { outcome: Outcome::Lost }, vec![Action::Page(Page::Lost)])
            }
            escrow => {
                let mut actions = vec![Action::ClaimSolana { preimage: *preimage }];
                if let Some(e) = escrow {
                    let left = e.deadline_slot.saturating_sub(v.solana_slot);
                    if left <= cfg.extend_within_slots {
                        actions.push(Action::Page(Page::ClaimUrgent { left }));
                    }
                }
                (state.clone(), actions)
            }
        },
        State::Refunding => match &v.htlc {
            HtlcView::SpentByRefund { confirmed: true } => (State::Done { outcome: Outcome::Refunded }, vec![]),
            HtlcView::SpentByRefund { confirmed: false } => (state.clone(), s2l_refund_actions(v)),
            // The taker's late claim won the race. It pays us on Solana.
            HtlcView::SpentByClaim { .. } => s2l_spent_by_claim(cfg, terms, state, v),
            _ => (state.clone(), s2l_refund_actions(v)),
        },
        State::Done { .. } | State::Abandoned => (state.clone(), vec![]),
    }
}

/// A coin whose Solana escrow we have already claimed belongs to the taker, even past the lock height.
fn s2l_refund_actions(v: &View) -> Vec<Action> {
    if v.escrow.as_ref().is_some_and(|e| e.state == EscrowState::Claimed) {
        vec![]
    } else {
        vec![Action::RefundLiquid]
    }
}

fn s2l_spent_by_claim(cfg: &Config, terms: &Terms, state: &State, v: &View) -> (State, Vec<Action>) {
    let HtlcView::SpentByClaim { preimage, .. } = &v.htlc else {
        return (state.clone(), vec![]);
    };
    if hash_preimage(preimage) != terms.payment_hash {
        let page = Page::Impossible("claim witness preimage does not match the hash".into());
        return (state.clone(), vec![Action::Page(page)]);
    }
    step_s2l(cfg, terms, &State::Claiming { preimage: *preimage }, v)
}

fn s2l_locked(cfg: &Config, terms: &Terms, state: &State, v: &View) -> (State, Vec<Action>) {
    match &v.htlc {
        HtlcView::SpentByClaim { .. } => s2l_spent_by_claim(cfg, terms, state, v),
        HtlcView::SpentByRefund { confirmed: true } => (State::Done { outcome: Outcome::Refunded }, vec![]),
        HtlcView::SpentByRefund { confirmed: false } => (State::Refunding, vec![]),
        // Our lock isn't visible. Only ever resend the same transaction, so we can't pay twice.
        HtlcView::Absent if v.liquid_tip < terms.lock_height => (state.clone(), vec![Action::RebroadcastLock]),
        HtlcView::Absent => (state.clone(), vec![Action::Page(Page::LockMissing)]),
        HtlcView::Unspent { .. } => {
            if v.liquid_tip >= terms.lock_height {
                let actions = s2l_refund_actions(v);
                if actions.is_empty() {
                    return (state.clone(), actions);
                }
                return (State::Refunding, actions);
            }
            (state.clone(), s2l_extend_if_stalled(cfg, v))
        }
    }
}

/// Decision D1: during a long Liquid stall, keep the taker's Solana refund closed, or a claim after the stall steals.
fn s2l_extend_if_stalled(cfg: &Config, v: &View) -> Vec<Action> {
    let Some(e) = &v.escrow else { return vec![] };
    let stalled = v.liquid_tip_age_secs >= cfg.extend_after_stall_secs;
    let near = e.deadline_slot.saturating_sub(v.solana_slot) <= cfg.extend_within_slots;
    if e.state == EscrowState::Open && stalled && near && !e.extended && v.solana_slot < e.deadline_slot {
        vec![Action::Extend { new_deadline: e.deadline_slot.saturating_add(cfg.extend_by_slots) }]
    } else {
        vec![]
    }
}
