//! Accounts and the pure rules that govern them. No Solana calls here, so the rules can be model-checked.

use anchor_lang::prelude::*;

/// Largest single `extend`: about 7 days at 213 ms per slot.
pub const MAX_EXTENSION_SLOTS: u64 = 3_000_000;
/// After settlement plus this, anyone may close an escrow. Until then the revealed value stays readable.
pub const RETENTION_SLOTS: u64 = 3_000_000;

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, Debug, InitSpace)]
pub enum EscrowState {
    Open,
    Claimed,
    Refunded,
}

#[account]
#[derive(InitSpace, Debug)]
pub struct Escrow {
    /// Funded the lock and receives a refund: a wallet, or a maker vault PDA.
    pub initiator: Pubkey,
    /// Receives the claim.
    pub recipient: Pubkey,
    /// The only token account a refund may pay: the wallet's canonical account, or the maker vault's own.
    /// An owner check alone is not enough, because anyone can create an account owned by a PDA.
    pub refund_token: Pubkey,
    /// Receives the rent when accounts close, and may close early.
    pub rent_payer: Pubkey,
    pub amount: u64,
    /// The point `T` this escrow locks to, as x then y. The first half also seeds the PDA.
    pub lock_point: [u8; 64],
    pub created_slot: u64,
    /// Claim closes and refund opens at this slot.
    pub deadline_slot: u64,
    /// Set on short-leg locks: a cluster restart after creation closes the claim.
    pub close_on_restart: bool,
    pub extended: bool,
    pub state: EscrowState,
    /// `s = t * Gx mod n`, set on claim. A counterparty reads it from the account, not from transaction history, and
    /// computes `t` from it.
    pub revealed: [u8; 32],
    pub settled_slot: u64,
    pub bump: u8,
}

impl Escrow {
    /// Only asked for when it can matter, so long-leg escrows never depend on the sysvar.
    pub fn restarted(&self, last_restart_slot: u64) -> bool {
        self.close_on_restart && last_restart_slot > self.created_slot
    }

    pub fn can_claim(&self, slot: u64, last_restart_slot: u64) -> bool {
        self.state == EscrowState::Open
            && slot < self.deadline_slot
            && !self.restarted(last_restart_slot)
    }

    /// The exact opposite of `can_claim` for an open escrow.
    pub fn can_refund(&self, slot: u64, last_restart_slot: u64) -> bool {
        self.state == EscrowState::Open && !self.can_claim(slot, last_restart_slot)
    }

    pub fn can_extend(&self, slot: u64, new_deadline: u64) -> bool {
        self.state == EscrowState::Open
            && !self.close_on_restart
            && !self.extended
            && slot < self.deadline_slot
            && new_deadline > self.deadline_slot
            && new_deadline <= self.deadline_slot.saturating_add(MAX_EXTENSION_SLOTS)
    }

    pub fn can_close(&self, slot: u64, closer_is_rent_payer: bool) -> bool {
        self.state != EscrowState::Open
            && (closer_is_rent_payer || slot >= self.settled_slot.saturating_add(RETENTION_SLOTS))
    }
}

#[account]
#[derive(InitSpace, Debug)]
pub struct MakerVault {
    /// Cold key: withdraws and changes the operator.
    pub owner: Pubkey,
    /// Hot key: locks up to the cap per window.
    pub operator: Pubkey,
    pub window_slots: u64,
    pub window_cap: u64,
    pub window_start_slot: u64,
    pub window_used: u64,
    pub bump: u8,
}

impl MakerVault {
    /// The (window start, amount used) after locking `amount` at `slot`, or `None` if over the cap.
    pub fn after_lock(&self, slot: u64, amount: u64) -> Option<(u64, u64)> {
        let expired = slot >= self.window_start_slot.saturating_add(self.window_slots);
        let (start, used) = if expired {
            (slot, 0)
        } else {
            (self.window_start_slot, self.window_used)
        };
        let used = used.checked_add(amount)?;
        (used <= self.window_cap).then_some((start, used))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn escrow(state: EscrowState, close_on_restart: bool) -> Escrow {
        Escrow {
            initiator: Pubkey::default(),
            recipient: Pubkey::default(),
            refund_token: Pubkey::default(),
            rent_payer: Pubkey::default(),
            amount: 1,
            lock_point: [0; 64],
            created_slot: 100,
            deadline_slot: 200,
            close_on_restart,
            extended: false,
            state,
            revealed: [0; 32],
            settled_slot: 150,
            bump: 0,
        }
    }

    /// Invariant S4, exhaustively over the interesting boundaries.
    #[test]
    fn exactly_one_of_claim_or_refund_while_open() {
        for close_on_restart in [false, true] {
            let e = escrow(EscrowState::Open, close_on_restart);
            for slot in [0, 99, 100, 199, 200, 201, u64::MAX] {
                for restart in [0, 99, 100, 101, 199, 200, u64::MAX] {
                    assert_ne!(e.can_claim(slot, restart), e.can_refund(slot, restart));
                }
            }
        }
    }

    #[test]
    fn neither_claim_nor_refund_once_settled() {
        for state in [EscrowState::Claimed, EscrowState::Refunded] {
            let e = escrow(state, true);
            assert!(!e.can_claim(150, 0));
            assert!(!e.can_refund(500, 0));
        }
    }

    #[test]
    fn restart_only_closes_short_legs_and_only_if_after_creation() {
        let short = escrow(EscrowState::Open, true);
        assert!(
            short.can_claim(150, 100),
            "restart at the creation slot came first"
        );
        assert!(!short.can_claim(150, 101));
        let long = escrow(EscrowState::Open, false);
        assert!(long.can_claim(150, 101));
    }

    #[test]
    fn window_cap_and_reset() {
        let v = MakerVault {
            owner: Pubkey::default(),
            operator: Pubkey::default(),
            window_slots: 100,
            window_cap: 10,
            window_start_slot: 1000,
            window_used: 8,
            bump: 0,
        };
        assert_eq!(v.after_lock(1050, 2), Some((1000, 10)));
        assert_eq!(v.after_lock(1050, 3), None);
        assert_eq!(v.after_lock(1099, 3), None);
        assert_eq!(v.after_lock(1100, 10), Some((1100, 10)));
        assert_eq!(v.after_lock(1100, 11), None);
        assert_eq!(v.after_lock(1050, u64::MAX), None);
    }
}
