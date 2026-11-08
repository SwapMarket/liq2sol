//! The signer's own rules. These run in a separate process and trust nothing the engine says, so a compromised RFQ
//! parser or engine still can't get an out-of-policy signature. The signer keeps its own ledger.

use serde::{Deserialize, Serialize};

use crate::engine::{Config, Direction};
use crate::LockPoint;

/// What the signer's own chain connections show about the taker's Liquid coin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoinEvidence {
    pub amount: u64,
    pub confirmations: u32,
}

/// What the signer's own chain connections show about the taker's Solana escrow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EscrowEvidence {
    pub amount: u64,
    /// Open, locked to this swap's point and our recipient key, a long-leg escrow that has not been extended.
    pub matches_terms: bool,
    pub slots_left: u64,
}

/// A signing request, as the signer assembles it. Every field here comes from the signer itself: amounts and
/// destinations from transactions it builds, and heights, slots and evidence from its own chain connections. A
/// compromised engine therefore can't lie its way past these rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    /// L→S: lock Solana funds, always with `close_on_restart`.
    LockSolana {
        swap_id: [u8; 32],
        amount: u64,
        deadline_slot: u64,
        solana_slot: u64,
        liquid_lock_height: u32,
        liquid_tip: u32,
        taker_coin: Option<CoinEvidence>,
    },
    /// L→S: agree to the taker's early refund, which is only safe if we have never signed a lock for this swap.
    EarlyRefund {
        swap_id: [u8; 32],
    },
    /// S→L: lock Liquid funds.
    LockLiquid {
        swap_id: [u8; 32],
        liquid_amount: u64,
        lock_height: u32,
        liquid_tip: u32,
        taker_escrow: Option<EscrowEvidence>,
    },
    /// L→S: keep a checked adapter signature. Nothing leaves the signer, but a Solana lock depends on it.
    HoldAdaptor {
        swap_id: [u8; 32],
    },
    /// S→L: keep the taker's adapter signature, and release our own share, which lets the taker finish the claim. Once
    /// only: a signature from any other session couldn't be read for the secret, and we would lose the Solana side.
    GiveShare {
        swap_id: [u8; 32],
        lock_height: u32,
        liquid_tip: u32,
        /// The taker's escrow is still there to collect. Once it's refunded, a claim here would be theft.
        taker_escrow_open: bool,
    },
    /// The claim pays our own wallet, because the signer builds it that way.
    ClaimLiquid {
        lock_point: LockPoint,
        secret: [u8; 32],
    },
    ClaimSolana {
        lock_point: LockPoint,
        secret: [u8; 32],
    },
    /// The refund pays our own wallet, because the signer builds it that way.
    RefundLiquid {
        swap_id: [u8; 32],
        lock_height: u32,
        liquid_tip: u32,
    },
    RefundSolana {
        swap_id: [u8; 32],
    },
    Extend {
        swap_id: [u8; 32],
        current_deadline: u64,
        new_deadline: u64,
        liquid_tip_age_secs: u64,
    },
    /// Give the taker's S→L escrow back. Only ever before we locked anything for this swap.
    Release {
        swap_id: [u8; 32],
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Refusal {
    OverSwapLimit,
    OverWindowLimit,
    AlreadyLocked,
    UnknownSwap,
    WrongSecret,
    /// L→S: we'd have no way to collect the taker's coin after the taker claims.
    NoAdaptor,
    /// A swap has one signing session. Its result is kept and resent, never replaced.
    AdaptorAlreadyHeld,
    BadTimeout(String),
    /// Refused: we have claimed the taker's escrow, so the Liquid coin is the taker's (protocol section 7.3).
    RefundAfterClaim,
    /// L→S: the taker's coin isn't there, or isn't confirmed enough. A compromised engine could otherwise lock our
    /// funds to a colluding taker who never paid.
    NoTakerCoin,
    /// S→L: the taker's escrow isn't there, doesn't match, or is too close to its deadline.
    NoTakerEscrow,
    /// We would pay out more than we are receiving.
    BadPrice,
    ExtendNotJustified,
    /// Refused: we locked on Liquid, so giving the taker its escrow back would hand it both sides.
    ReleaseAfterLock,
    /// Refused: we agreed to refund the taker, who may already have the coin back, so we must never lock.
    EarlyRefunded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub max_per_swap: u64,
    pub window_secs: u64,
    pub max_per_window: u64,
    pub confirmations: u32,
    pub large_above: u64,
    pub confirmations_large: u32,
    pub s2l_gate_slots: u64,
    pub l2s_gate_blocks: u32,
    pub l2s_max_deadline_slots: u64,
    pub s2l_min_blocks: u32,
    pub s2l_max_blocks: u32,
    pub extend_after_stall_secs: u64,
    pub extend_by_slots: u64,
}

impl From<&Config> for Limits {
    fn from(cfg: &Config) -> Self {
        Self {
            max_per_swap: cfg.max_per_swap,
            window_secs: 3_600,
            max_per_window: cfg.max_open_l2s.max(cfg.max_open_s2l),
            confirmations: cfg.confirmations,
            large_above: cfg.large_above,
            confirmations_large: cfg.confirmations_large,
            s2l_gate_slots: cfg.s2l_gate_slots,
            l2s_gate_blocks: cfg.l2s_gate_blocks,
            l2s_max_deadline_slots: cfg.l2s_deadline_slots,
            s2l_min_blocks: 30,
            s2l_max_blocks: 120,
            extend_after_stall_secs: cfg.extend_after_stall_secs,
            extend_by_slots: cfg.extend_by_slots,
        }
    }
}

/// The signer's memory of one swap. Persisted before any signature is released.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapRecord {
    pub direction: Option<Direction>,
    /// In Solana base units.
    pub locked_amount: u64,
    /// L→S: a retry must have this exact deadline, or it could outlive the margin.
    pub deadline_slot: u64,
    pub solana_claimed: bool,
    pub liquid_refunded: bool,
    /// L→S: a checked adapter signature is stored. S→L: and our share of it has been given out.
    pub adaptor_held: bool,
    /// We gave the taker its money back early, so this swap is never locked: L→S by signing its refund, S→L by releasing its
    /// Solana lock. Absent in records from before this existed.
    #[serde(default)]
    pub early_refunded: bool,
}

/// What the check needs from the signer's ledger.
pub trait Ledger {
    fn record(&self, swap_id: &[u8; 32]) -> SwapRecord;
    /// Total locked in the current window, in Solana base units.
    fn locked_in_window(&self, now_secs: u64, window_secs: u64) -> u64;
}

fn id_of(request: &Request) -> [u8; 32] {
    use Request::*;
    match request {
        LockSolana { swap_id, .. }
        | LockLiquid { swap_id, .. }
        | HoldAdaptor { swap_id }
        | EarlyRefund { swap_id }
        | GiveShare { swap_id, .. }
        | RefundLiquid { swap_id, .. }
        | RefundSolana { swap_id }
        | Extend { swap_id, .. }
        | Release { swap_id } => *swap_id,
        ClaimLiquid { lock_point, .. } | ClaimSolana { lock_point, .. } => lock_point.id(),
    }
}

/// Check one request. On `Ok`, the caller must persist the returned record, and only then release a signature.
pub fn check(limits: &Limits, ledger: &dyn Ledger, now_secs: u64, request: &Request) -> Result<SwapRecord, Refusal> {
    use Request::*;
    let mut record = ledger.record(&id_of(request));
    let check_lock = |amount: u64| -> Result<(), Refusal> {
        if record.direction.is_some() {
            return Err(Refusal::AlreadyLocked);
        }
        if amount > limits.max_per_swap {
            return Err(Refusal::OverSwapLimit);
        }
        let used = ledger.locked_in_window(now_secs, limits.window_secs);
        if used.checked_add(amount).is_none_or(|total| total > limits.max_per_window) {
            return Err(Refusal::OverWindowLimit);
        }
        Ok(())
    };
    match request {
        EarlyRefund { .. } => {
            // Any signed lock could still land, even one that seems lost, so one ever signed rules this out.
            if record.direction.is_some() {
                return Err(Refusal::AlreadyLocked);
            }
            record.early_refunded = true;
        }
        LockSolana { amount, deadline_slot, solana_slot, liquid_lock_height, liquid_tip, taker_coin, .. } => {
            if record.early_refunded {
                return Err(Refusal::EarlyRefunded);
            }
            if !record.adaptor_held {
                return Err(Refusal::NoAdaptor);
            }
            // A lost transaction may be signed again with a new blockhash, if every term is identical.
            let retry = record.direction == Some(Direction::L2S)
                && record.locked_amount == *amount
                && record.deadline_slot == *deadline_slot;
            if !retry {
                check_lock(*amount)?;
            }
            let coin = taker_coin.ok_or(Refusal::NoTakerCoin)?;
            let needed = if *amount > limits.large_above { limits.confirmations_large } else { limits.confirmations };
            if coin.confirmations < needed {
                return Err(Refusal::NoTakerCoin);
            }
            if amount.checked_mul(100).is_none_or(|owed| coin.amount < owed) {
                return Err(Refusal::BadPrice);
            }
            if liquid_lock_height.saturating_sub(*liquid_tip) < limits.l2s_gate_blocks {
                return Err(Refusal::BadTimeout("too close to the taker's refund".into()));
            }
            let len = deadline_slot.saturating_sub(*solana_slot);
            if len == 0 || len > limits.l2s_max_deadline_slots {
                return Err(Refusal::BadTimeout(format!("deadline {len} slots away")));
            }
            record.direction = Some(Direction::L2S);
            record.locked_amount = *amount;
            record.deadline_slot = *deadline_slot;
        }
        LockLiquid { liquid_amount, lock_height, liquid_tip, taker_escrow, .. } => {
            // We gave the taker their Solana lock back, so a Liquid lock now could only be lost.
            if record.early_refunded {
                return Err(Refusal::EarlyRefunded);
            }
            let amount = liquid_amount.div_ceil(100);
            check_lock(amount)?;
            let escrow = taker_escrow.ok_or(Refusal::NoTakerEscrow)?;
            if !escrow.matches_terms || escrow.slots_left < limits.s2l_gate_slots {
                return Err(Refusal::NoTakerEscrow);
            }
            if amount > escrow.amount || liquid_amount % 100 != 0 {
                return Err(Refusal::BadPrice);
            }
            let blocks = lock_height.saturating_sub(*liquid_tip);
            if blocks < limits.s2l_min_blocks || blocks > limits.s2l_max_blocks {
                return Err(Refusal::BadTimeout(format!("lock height {blocks} blocks away")));
            }
            record.direction = Some(Direction::S2L);
            record.locked_amount = amount;
        }
        HoldAdaptor { .. } => {
            if record.early_refunded {
                return Err(Refusal::EarlyRefunded);
            }
            if record.direction.is_some() {
                return Err(Refusal::AlreadyLocked);
            }
            if record.adaptor_held {
                return Err(Refusal::AdaptorAlreadyHeld);
            }
            record.adaptor_held = true;
        }
        GiveShare { lock_height, liquid_tip, taker_escrow_open, .. } => {
            if record.direction != Some(Direction::S2L) {
                return Err(Refusal::UnknownSwap);
            }
            if record.adaptor_held {
                return Err(Refusal::AdaptorAlreadyHeld);
            }
            if record.liquid_refunded || liquid_tip >= lock_height {
                return Err(Refusal::BadTimeout("past the lock height".into()));
            }
            if !taker_escrow_open {
                return Err(Refusal::NoTakerEscrow);
            }
            record.adaptor_held = true;
        }
        ClaimLiquid { lock_point, secret } => {
            if record.direction != Some(Direction::L2S) {
                return Err(Refusal::UnknownSwap);
            }
            if !lock_point.opened_by(secret) {
                return Err(Refusal::WrongSecret);
            }
        }
        ClaimSolana { lock_point, secret } => {
            if record.direction != Some(Direction::S2L) {
                return Err(Refusal::UnknownSwap);
            }
            if !lock_point.opened_by(secret) {
                return Err(Refusal::WrongSecret);
            }
            record.solana_claimed = true;
        }
        RefundLiquid { lock_height, liquid_tip, .. } => {
            if record.direction != Some(Direction::S2L) {
                return Err(Refusal::UnknownSwap);
            }
            if record.solana_claimed {
                return Err(Refusal::RefundAfterClaim);
            }
            if liquid_tip < lock_height {
                return Err(Refusal::BadTimeout("before the lock height".into()));
            }
            record.liquid_refunded = true;
        }
        RefundSolana { .. } => {
            if record.direction != Some(Direction::L2S) {
                return Err(Refusal::UnknownSwap);
            }
        }
        Release { .. } => {
            if record.direction.is_some() {
                return Err(Refusal::ReleaseAfterLock);
            }
            // And never lock on Liquid afterwards.
            record.early_refunded = true;
        }
        Extend { current_deadline, new_deadline, liquid_tip_age_secs, .. } => {
            if record.direction != Some(Direction::S2L) {
                return Err(Refusal::UnknownSwap);
            }
            if *liquid_tip_age_secs < limits.extend_after_stall_secs {
                return Err(Refusal::ExtendNotJustified);
            }
            if *new_deadline <= *current_deadline || new_deadline - current_deadline > limits.extend_by_slots {
                return Err(Refusal::BadTimeout("extension out of range".into()));
            }
        }
    }
    Ok(record)
}

/// The amount a request newly locks, for the window count. A retry of an existing lock adds nothing.
pub fn newly_locked(before: &SwapRecord, request: &Request) -> Option<u64> {
    if before.direction.is_some() {
        return None;
    }
    match request {
        Request::LockSolana { amount, .. } => Some(*amount),
        Request::LockLiquid { liquid_amount, .. } => Some(liquid_amount.div_ceil(100)),
        _ => None,
    }
}
