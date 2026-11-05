//! The signer's own rules. These run in a separate process and trust nothing the engine says, so a compromised RFQ
//! parser or engine still can't get an out-of-policy signature. The signer keeps its own ledger.

use serde::{Deserialize, Serialize};

use crate::engine::{Config, Direction};

/// What the engine wants signed, in terms the signer can check for itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    /// L→S: lock Solana funds. `liquid_lock_height` and `liquid_tip` come from the signer's own node.
    LockSolana {
        payment_hash: [u8; 32],
        amount: u64,
        deadline_slot: u64,
        solana_slot: u64,
        close_on_restart: bool,
        liquid_lock_height: u32,
        liquid_tip: u32,
    },
    /// S→L: lock Liquid funds.
    LockLiquid {
        payment_hash: [u8; 32],
        amount: u64,
        lock_height: u32,
        liquid_tip: u32,
    },
    ClaimLiquid {
        payment_hash: [u8; 32],
        preimage: [u8; 32],
        pays_our_script: bool,
    },
    ClaimSolana {
        payment_hash: [u8; 32],
        preimage: [u8; 32],
    },
    RefundLiquid {
        payment_hash: [u8; 32],
        lock_height: u32,
        liquid_tip: u32,
        pays_our_script: bool,
    },
    RefundSolana {
        payment_hash: [u8; 32],
    },
    Extend {
        payment_hash: [u8; 32],
        current_deadline: u64,
        new_deadline: u64,
        liquid_tip_age_secs: u64,
    },
    /// Give the taker's S→L escrow back. Only ever before we locked anything for this swap.
    Release {
        payment_hash: [u8; 32],
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Refusal {
    OverSwapLimit,
    OverWindowLimit,
    AlreadyLocked,
    UnknownSwap,
    WrongPreimage,
    BadTimeout(String),
    /// Refused: we have claimed the taker's escrow, so the Liquid coin is the taker's (protocol section 7.3).
    RefundAfterClaim,
    /// A claim or refund pays somewhere other than our own wallet.
    WrongDestination,
    ExtendNotJustified,
    /// Refused: we locked on Liquid, so giving the taker its escrow back would hand it both sides.
    ReleaseAfterLock,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub max_per_swap: u64,
    pub window_secs: u64,
    pub max_per_window: u64,
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
    pub locked_amount: u64,
    /// L→S: a retry must have this exact deadline, or it could outlive the margin.
    pub deadline_slot: u64,
    pub solana_claimed: bool,
    pub liquid_refunded: bool,
}

/// What the check needs from the signer's ledger.
pub trait Ledger {
    fn record(&self, payment_hash: &[u8; 32]) -> SwapRecord;
    /// Total locked in the current window, in Solana base units.
    fn locked_in_window(&self, now_secs: u64, window_secs: u64) -> u64;
}

/// Check one request. On `Ok`, the caller must persist the returned record, and only then sign.
pub fn check(
    limits: &Limits,
    ledger: &dyn Ledger,
    now_secs: u64,
    request: &Request,
) -> Result<SwapRecord, Refusal> {
    use Request::*;
    let hash = match request {
        LockSolana { payment_hash, .. }
        | LockLiquid { payment_hash, .. }
        | ClaimLiquid { payment_hash, .. }
        | ClaimSolana { payment_hash, .. }
        | RefundLiquid { payment_hash, .. }
        | RefundSolana { payment_hash }
        | Extend { payment_hash, .. }
        | Release { payment_hash } => payment_hash,
    };
    let mut record = ledger.record(hash);
    let check_lock = |amount: u64| -> Result<(), Refusal> {
        if record.direction.is_some() {
            return Err(Refusal::AlreadyLocked);
        }
        if amount > limits.max_per_swap {
            return Err(Refusal::OverSwapLimit);
        }
        let used = ledger.locked_in_window(now_secs, limits.window_secs);
        if used
            .checked_add(amount)
            .is_none_or(|total| total > limits.max_per_window)
        {
            return Err(Refusal::OverWindowLimit);
        }
        Ok(())
    };
    match request {
        LockSolana {
            amount,
            deadline_slot,
            solana_slot,
            close_on_restart,
            liquid_lock_height,
            liquid_tip,
            ..
        } => {
            // A lost transaction may be signed again with a new blockhash, if every term is identical.
            let retry = record.direction == Some(Direction::L2S)
                && record.locked_amount == *amount
                && record.deadline_slot == *deadline_slot;
            if !retry {
                check_lock(*amount)?;
            }
            if !close_on_restart {
                return Err(Refusal::BadTimeout(
                    "a short leg must close on restart".into(),
                ));
            }
            if liquid_lock_height.saturating_sub(*liquid_tip) < limits.l2s_gate_blocks {
                return Err(Refusal::BadTimeout(
                    "too close to the taker's refund".into(),
                ));
            }
            let len = deadline_slot.saturating_sub(*solana_slot);
            if len == 0 || len > limits.l2s_max_deadline_slots {
                return Err(Refusal::BadTimeout(format!("deadline {len} slots away")));
            }
            record.direction = Some(Direction::L2S);
            record.locked_amount = *amount;
            record.deadline_slot = *deadline_slot;
        }
        LockLiquid {
            amount,
            lock_height,
            liquid_tip,
            ..
        } => {
            check_lock(*amount)?;
            let blocks = lock_height.saturating_sub(*liquid_tip);
            if blocks < limits.s2l_min_blocks || blocks > limits.s2l_max_blocks {
                return Err(Refusal::BadTimeout(format!(
                    "lock height {blocks} blocks away"
                )));
            }
            record.direction = Some(Direction::S2L);
            record.locked_amount = *amount;
        }
        ClaimLiquid {
            payment_hash,
            preimage,
            pays_our_script,
        } => {
            if record.direction != Some(Direction::L2S) {
                return Err(Refusal::UnknownSwap);
            }
            if crate::hash_preimage(preimage) != *payment_hash {
                return Err(Refusal::WrongPreimage);
            }
            if !pays_our_script {
                return Err(Refusal::WrongDestination);
            }
        }
        ClaimSolana {
            payment_hash,
            preimage,
        } => {
            if record.direction != Some(Direction::S2L) {
                return Err(Refusal::UnknownSwap);
            }
            if crate::hash_preimage(preimage) != *payment_hash {
                return Err(Refusal::WrongPreimage);
            }
            record.solana_claimed = true;
        }
        RefundLiquid {
            lock_height,
            liquid_tip,
            pays_our_script,
            ..
        } => {
            if record.direction != Some(Direction::S2L) {
                return Err(Refusal::UnknownSwap);
            }
            if record.solana_claimed {
                return Err(Refusal::RefundAfterClaim);
            }
            if liquid_tip < lock_height {
                return Err(Refusal::BadTimeout("before the lock height".into()));
            }
            if !pays_our_script {
                return Err(Refusal::WrongDestination);
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
        }
        Extend {
            current_deadline,
            new_deadline,
            liquid_tip_age_secs,
            ..
        } => {
            if record.direction != Some(Direction::S2L) {
                return Err(Refusal::UnknownSwap);
            }
            if *liquid_tip_age_secs < limits.extend_after_stall_secs {
                return Err(Refusal::ExtendNotJustified);
            }
            if *new_deadline <= *current_deadline
                || new_deadline - current_deadline > limits.extend_by_slots
            {
                return Err(Refusal::BadTimeout("extension out of range".into()));
            }
        }
    }
    Ok(record)
}

/// The amount a request newly locks, for the window count. A retry of an existing lock adds nothing.
pub fn newly_locked(before: &SwapRecord, request: &Request) -> Option<u64> {
    match request {
        Request::LockSolana { amount, .. } | Request::LockLiquid { amount, .. }
            if before.direction.is_none() =>
        {
            Some(*amount)
        }
        _ => None,
    }
}
