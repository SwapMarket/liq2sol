//! One swap's state machine. `step` is a pure function: (state, what the chains show) -> (new state, actions).
//!
//! Actions may be repeated if the daemon crashes or a broadcast is lost, so each must be safe to repeat. The one
//! thing that isn't is paying twice to the Liquid swap address, which is why `LockLiquid` is emitted once and retries use
//! `RebroadcastLock`, the identical stored transaction.

use serde::{Deserialize, Serialize};

use liq2sol_swapkey::secret_from_solana;

use crate::LockPoint;

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
    pub lock_point: LockPoint,
    pub liquid_amount: u64,
    pub solana_amount: u64,
    pub h_accept: u32,
    pub lock_height: u32,
}

impl Terms {
    pub fn swap_id(&self) -> [u8; 32] {
        self.lock_point.id()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum State {
    /// Waiting for the taker's lock.
    WatchLock,
    /// L→S: our Solana lock is sent, with its deadline fixed for every retry.
    LockedSolana { deadline_slot: u64 },
    /// S→L: our Liquid lock is sent.
    LockedLiquid,
    /// The secret is known, and our claim on the taker's chain is in progress.
    Claiming { secret: [u8; 32] },
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
    /// It locks to this swap's point.
    pub point_ok: bool,
    /// The recipient is the party that should receive this escrow under the terms.
    pub recipient_ok: bool,
    pub deadline_slot: u64,
    pub close_on_restart: bool,
    pub extended: bool,
    /// Short-leg only: a restart after creation has closed the claim.
    pub restarted: bool,
    /// What the claim revealed, `s = t * Gx mod n`.
    pub revealed: [u8; 32],
}

/// How a closed escrow was settled, as far as the daemon can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Closed {
    /// Gone before the deadline we last saw. A deadline only moves later, and we only lock on escrows that don't close on a
    /// restart, and never release one after locking, so only a claim can have settled it.
    BeforeDeadline,
    /// Its history shows a claim.
    Claimed,
    /// Its history shows a refund.
    Refunded,
    /// Gone after the deadline, and its history doesn't say how, or can't be read.
    Unknown,
}

/// What an escrow's absence alone says, given the last slot it was seen open at and its latest deadline then.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Absence {
    /// Read from a node no further on than the one that saw it, which may just not have it yet. Says nothing.
    Behind,
    /// Closed before the deadline, so claimed: see `Closed::BeforeDeadline`.
    BeforeDeadline,
    /// Only its history can tell. `seen` says whether it was ever seen open here.
    AskHistory { seen: bool },
}

pub fn absence(seen: Option<(u64, u64)>, read_at: u64) -> Absence {
    match seen {
        Some((seen_at, _)) if read_at <= seen_at => Absence::Behind,
        Some((_, deadline)) if read_at < deadline => Absence::BeforeDeadline,
        seen => Absence::AskHistory { seen: seen.is_some() },
    }
}

/// What is known about the swap's Liquid coin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HtlcView {
    /// No coin that unblinds to exactly the right asset and amount. The chain doesn't check these, only we do.
    Absent,
    Unspent {
        confirmations: u32,
    },
    /// Spent by a key-path claim, whose signature gave this secret, which is checked against the point here. A secret is
    /// usable at once, but a swap isn't finished until the spend is `confirmed`, because an unconfirmed one can still drop.
    SpentByClaim {
        secret: [u8; 32],
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
    /// L→S: the signer holds a checked adapter signature for the claim, so a Solana lock can't lose our funds.
    pub adaptor_held: bool,
    pub htlc: HtlcView,
    pub escrow: Option<EscrowView>,
    /// S→L, while claiming: how an escrow that is no longer there was settled. The taker pays its rent, so may close it at once.
    pub escrow_closed: Option<Closed>,
    /// Open exposure in this direction, excluding this swap.
    pub open_exposure: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    /// L→S: the coin is good, so run a signing session with the taker. Safe to repeat.
    RequestAdaptor,
    /// L→S: lock the maker's Solana funds, with `close_on_restart` set.
    LockSolana {
        deadline_slot: u64,
    },
    /// S→L: pay the swap address and the taker's fee coin. Emitted at most once per swap.
    LockLiquid,
    /// S→L: send the identical stored lock transaction again.
    RebroadcastLock,
    /// L→S: claim the taker's coin on Liquid.
    ClaimLiquid {
        secret: [u8; 32],
    },
    /// S→L: claim the taker's escrow on Solana.
    ClaimSolana {
        secret: [u8; 32],
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
    /// A known secret still isn't claimed, and time is short. In blocks for L→S, in slots for S→L.
    ClaimUrgent {
        left: u64,
    },
    /// Our Liquid lock never appeared by the lock height.
    LockMissing,
    Lost,
    /// An on-chain value that should be impossible, such as a revealed value that doesn't open the point.
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
    l2s_coin_acceptable(cfg, terms, v) && v.adaptor_held
}

/// Everything the L→S gate needs except the adapter signature, which is worth asking for only once this holds.
fn l2s_coin_acceptable(cfg: &Config, terms: &Terms, v: &View) -> bool {
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
            && e.point_ok
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
            } else if l2s_coin_acceptable(cfg, terms, v) {
                (state.clone(), vec![Action::RequestAdaptor])
            } else if v.liquid_tip > terms.h_accept.saturating_add(cfg.l2s_fund_window_blocks) {
                // Nothing of ours moved. A coin the taker sent late is theirs to refund.
                (State::Abandoned, vec![])
            } else {
                (state.clone(), vec![])
            }
        }
        State::LockedSolana { .. } => l2s_locked(cfg, terms, state, v),
        State::LockedLiquid => wrong_direction(state),
        State::Claiming { secret } => match &v.htlc {
            HtlcView::SpentByClaim { confirmed: true, .. } => (State::Done { outcome: Outcome::Completed }, vec![]),
            // Ours is in the mempool. Sending it again is harmless, and it survives a dropped transaction.
            HtlcView::SpentByClaim { confirmed: false, .. } => {
                (state.clone(), vec![Action::ClaimLiquid { secret: *secret }])
            }
            HtlcView::SpentByRefund { confirmed: true } => {
                (State::Done { outcome: Outcome::Lost }, vec![Action::Page(Page::Lost)])
            }
            _ => {
                let mut actions = vec![Action::ClaimLiquid { secret: *secret }];
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
            // The program checked this, so a failure here means the store or the RPC is lying.
            let Some(secret) = secret_from_solana(&escrow.revealed).ok().filter(|t| terms.lock_point.opened_by(t))
            else {
                let page = Page::Impossible("the revealed value does not open the lock point".into());
                return (state.clone(), vec![Action::Page(page)]);
            };
            step_l2s(cfg, terms, &State::Claiming { secret }, v)
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
        State::Claiming { secret } => match v.escrow.as_ref() {
            Some(e) if e.state == EscrowState::Claimed => (State::Done { outcome: Outcome::Completed }, vec![]),
            Some(e) if e.state == EscrowState::Refunded => {
                (State::Done { outcome: Outcome::Lost }, vec![Action::Page(Page::Lost)])
            }
            None if matches!(v.escrow_closed, Some(Closed::BeforeDeadline | Closed::Claimed)) => {
                (State::Done { outcome: Outcome::Completed }, vec![])
            }
            None if v.escrow_closed == Some(Closed::Refunded) => {
                (State::Done { outcome: Outcome::Lost }, vec![Action::Page(Page::Lost)])
            }
            None if v.escrow_closed == Some(Closed::Unknown) => {
                let page = Page::Impossible("the escrow was closed after its deadline, and its history doesn't say how".into());
                (state.clone(), vec![Action::Page(page)])
            }
            escrow => {
                let mut actions = vec![Action::ClaimSolana { secret: *secret }];
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
    let HtlcView::SpentByClaim { secret, .. } = &v.htlc else {
        return (state.clone(), vec![]);
    };
    // Both keys signed this spend, and we only signed the session we kept. Anything else needs a human.
    if !terms.lock_point.opened_by(secret) {
        let page = Page::Impossible("the claim's signature does not give the secret".into());
        return (state.clone(), vec![Action::Page(page)]);
    }
    step_s2l(cfg, terms, &State::Claiming { secret: *secret }, v)
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
