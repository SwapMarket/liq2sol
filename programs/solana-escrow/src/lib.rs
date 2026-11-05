//! liq2sol HTLC escrow. Specification and invariants: docs/contracts.md section 2.

use anchor_lang::prelude::*;
use anchor_spl::associated_token::get_associated_token_address;
use anchor_spl::token::{self, CloseAccount, Mint, Token, TokenAccount, Transfer};
use solana_sha256_hasher::hash;
use solana_sysvar::last_restart_slot::LastRestartSlot;

pub mod state;
use state::*;

declare_id!("DxVq5iSzKfRMAyis8RyLUeLggidM2Jvk1FZyyEauCRQ1");

/// USDT on Solana mainnet: legacy Token program, 6 decimals.
#[cfg(feature = "mainnet")]
pub const USDT_MINT: Pubkey = pubkey!("Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB");
/// Self-issued stand-in for devnet and tests.
#[cfg(not(feature = "mainnet"))]
pub const USDT_MINT: Pubkey = pubkey!("8BXimW8mCEfhdD1fhv9aoCfQgSj3KxFc52jMAbLvaFwi");

pub const ESCROW_SEED: &[u8] = b"escrow";
pub const ESCROW_VAULT_SEED: &[u8] = b"vault";
pub const MAKER_VAULT_SEED: &[u8] = b"maker_vault";
pub const MAKER_VAULT_TOKEN_SEED: &[u8] = b"maker_vault_token";

#[error_code]
pub enum EscrowError {
    #[msg("Amount must be greater than zero")]
    ZeroAmount,
    #[msg("Deadline must be in the future")]
    DeadlinePassed,
    #[msg("Recipient must differ from initiator")]
    SelfSwap,
    #[msg("Preimage does not match the payment hash")]
    WrongPreimage,
    #[msg("The claim window is closed")]
    ClaimClosed,
    #[msg("The refund is not open yet")]
    RefundNotOpen,
    #[msg("This extension is not allowed")]
    ExtensionNotAllowed,
    #[msg("The escrow is already settled")]
    NotOpen,
    #[msg("Token account is not the recipient's canonical account")]
    WrongRecipientAccount,
    #[msg("Token account is not owned by the initiator")]
    WrongInitiatorAccount,
    #[msg("Cannot close this escrow yet")]
    CloseNotAllowed,
    #[msg("Vault window must be longer than zero")]
    ZeroWindow,
    #[msg("This lock is over the vault's window cap")]
    OverWindowCap,
}

fn last_restart_slot(escrow: &Escrow) -> Result<u64> {
    // Short-leg escrows only: a long-leg escrow must never fail because of this sysvar.
    if escrow.close_on_restart {
        Ok(LastRestartSlot::get()?.last_restart_slot)
    } else {
        Ok(0)
    }
}

/// Move the vault's whole balance out and close it, then mark the escrow settled.
fn settle<'info>(
    escrow: &mut Account<'info, Escrow>,
    vault: &Account<'info, TokenAccount>,
    destination: AccountInfo<'info>,
    rent_payer: AccountInfo<'info>,
    token_program: &Program<'info, Token>,
    state: EscrowState,
    slot: u64,
) -> Result<()> {
    let (initiator, hash_seed, bump) = (escrow.initiator, escrow.payment_hash, escrow.bump);
    let seeds: &[&[u8]] = &[ESCROW_SEED, initiator.as_ref(), hash_seed.as_ref(), &[bump]];
    let signer: &[&[&[u8]]] = &[seeds];
    token::transfer(
        CpiContext::new_with_signer(
            token_program.key(),
            Transfer {
                from: vault.to_account_info(),
                to: destination,
                authority: escrow.to_account_info(),
            },
            signer,
        ),
        escrow.amount,
    )?;
    token::close_account(CpiContext::new_with_signer(
        token_program.key(),
        CloseAccount {
            account: vault.to_account_info(),
            destination: rent_payer,
            authority: escrow.to_account_info(),
        },
        signer,
    ))?;
    escrow.state = state;
    escrow.settled_slot = slot;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn init_escrow(
    escrow: &mut Escrow,
    initiator: Pubkey,
    refund_token: Pubkey,
    rent_payer: Pubkey,
    payment_hash: [u8; 32],
    recipient: Pubkey,
    amount: u64,
    deadline_slot: u64,
    close_on_restart: bool,
    slot: u64,
    bump: u8,
) -> Result<()> {
    require!(amount > 0, EscrowError::ZeroAmount);
    require!(deadline_slot > slot, EscrowError::DeadlinePassed);
    require_keys_neq!(recipient, initiator, EscrowError::SelfSwap);
    *escrow = Escrow {
        initiator,
        recipient,
        refund_token,
        rent_payer,
        amount,
        payment_hash,
        created_slot: slot,
        deadline_slot,
        close_on_restart,
        extended: false,
        state: EscrowState::Open,
        preimage: [0; 32],
        settled_slot: 0,
        bump,
    };
    Ok(())
}

#[program]
pub mod solana_escrow {
    use super::*;

    /// Lock funds from a wallet.
    pub fn lock(
        ctx: Context<Lock>,
        payment_hash: [u8; 32],
        recipient: Pubkey,
        amount: u64,
        deadline_slot: u64,
        close_on_restart: bool,
    ) -> Result<()> {
        let initiator = ctx.accounts.initiator.key();
        init_escrow(
            &mut ctx.accounts.escrow,
            initiator,
            get_associated_token_address(&initiator, &USDT_MINT),
            initiator,
            payment_hash,
            recipient,
            amount,
            deadline_slot,
            close_on_restart,
            Clock::get()?.slot,
            ctx.bumps.escrow,
        )?;
        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.key(),
                Transfer {
                    from: ctx.accounts.initiator_token.to_account_info(),
                    to: ctx.accounts.vault.to_account_info(),
                    authority: ctx.accounts.initiator.to_account_info(),
                },
            ),
            amount,
        )
    }

    /// Pay the recipient. Anyone holding the preimage may call this.
    pub fn claim(ctx: Context<Claim>, preimage: [u8; 32]) -> Result<()> {
        let slot = Clock::get()?.slot;
        let escrow = &mut ctx.accounts.escrow;
        require!(escrow.state == EscrowState::Open, EscrowError::NotOpen);
        require!(
            escrow.can_claim(slot, last_restart_slot(escrow)?),
            EscrowError::ClaimClosed
        );
        require!(
            hash(&preimage).to_bytes() == escrow.payment_hash,
            EscrowError::WrongPreimage
        );
        escrow.preimage = preimage;
        settle(
            escrow,
            &ctx.accounts.vault,
            ctx.accounts.recipient_token.to_account_info(),
            ctx.accounts.rent_payer.to_account_info(),
            &ctx.accounts.token_program,
            EscrowState::Claimed,
            slot,
        )
    }

    /// Return funds to the initiator once the claim has closed. Anyone may call this.
    pub fn refund(ctx: Context<Refund>) -> Result<()> {
        let slot = Clock::get()?.slot;
        let escrow = &mut ctx.accounts.escrow;
        require!(escrow.state == EscrowState::Open, EscrowError::NotOpen);
        require!(
            escrow.can_refund(slot, last_restart_slot(escrow)?),
            EscrowError::RefundNotOpen
        );
        settle(
            escrow,
            &ctx.accounts.vault,
            ctx.accounts.initiator_token.to_account_info(),
            ctx.accounts.rent_payer.to_account_info(),
            &ctx.accounts.token_program,
            EscrowState::Refunded,
            slot,
        )
    }

    /// The recipient gives up the swap and returns the funds at once.
    pub fn release(ctx: Context<Release>) -> Result<()> {
        let slot = Clock::get()?.slot;
        let escrow = &mut ctx.accounts.escrow;
        require!(escrow.state == EscrowState::Open, EscrowError::NotOpen);
        settle(
            escrow,
            &ctx.accounts.vault,
            ctx.accounts.initiator_token.to_account_info(),
            ctx.accounts.rent_payer.to_account_info(),
            &ctx.accounts.token_program,
            EscrowState::Refunded,
            slot,
        )
    }

    /// The recipient of a long-leg escrow moves the deadline later, once, for a Liquid halt (decision D1).
    pub fn extend(ctx: Context<Extend>, new_deadline: u64) -> Result<()> {
        let slot = Clock::get()?.slot;
        let escrow = &mut ctx.accounts.escrow;
        require!(
            escrow.can_extend(slot, new_deadline),
            EscrowError::ExtensionNotAllowed
        );
        escrow.deadline_slot = new_deadline;
        escrow.extended = true;
        Ok(())
    }

    /// Reclaim the escrow's rent after settlement. The rent payer may do this at once, anyone after `RETENTION_SLOTS`.
    pub fn close(ctx: Context<Close>) -> Result<()> {
        let is_rent_payer = ctx.accounts.closer.key() == ctx.accounts.escrow.rent_payer;
        require!(
            ctx.accounts
                .escrow
                .can_close(Clock::get()?.slot, is_rent_payer),
            EscrowError::CloseNotAllowed
        );
        Ok(())
    }

    /// Create a maker vault. The signer becomes its cold owner.
    pub fn vault_init(
        ctx: Context<VaultInit>,
        operator: Pubkey,
        window_slots: u64,
        window_cap: u64,
    ) -> Result<()> {
        require!(window_slots > 0, EscrowError::ZeroWindow);
        ctx.accounts.maker_vault.set_inner(MakerVault {
            owner: ctx.accounts.owner.key(),
            operator,
            window_slots,
            window_cap,
            window_start_slot: Clock::get()?.slot,
            window_used: 0,
            bump: ctx.bumps.maker_vault,
        });
        Ok(())
    }

    /// Add funds to a maker vault.
    pub fn vault_deposit(ctx: Context<VaultDeposit>, amount: u64) -> Result<()> {
        require!(amount > 0, EscrowError::ZeroAmount);
        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.key(),
                Transfer {
                    from: ctx.accounts.from.to_account_info(),
                    to: ctx.accounts.vault_token.to_account_info(),
                    authority: ctx.accounts.depositor.to_account_info(),
                },
            ),
            amount,
        )
    }

    /// Withdraw from a maker vault. Cold key only.
    pub fn vault_withdraw(ctx: Context<VaultWithdraw>, amount: u64) -> Result<()> {
        require!(amount > 0, EscrowError::ZeroAmount);
        let owner = ctx.accounts.owner.key();
        let seeds: &[&[u8]] = &[
            MAKER_VAULT_SEED,
            owner.as_ref(),
            &[ctx.accounts.maker_vault.bump],
        ];
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.key(),
                Transfer {
                    from: ctx.accounts.vault_token.to_account_info(),
                    to: ctx.accounts.to.to_account_info(),
                    authority: ctx.accounts.maker_vault.to_account_info(),
                },
                &[seeds],
            ),
            amount,
        )
    }

    /// Rotate the hot key or change the cap. Cold key only.
    pub fn vault_set_operator(
        ctx: Context<VaultSetOperator>,
        operator: Pubkey,
        window_slots: u64,
        window_cap: u64,
    ) -> Result<()> {
        require!(window_slots > 0, EscrowError::ZeroWindow);
        let vault = &mut ctx.accounts.maker_vault;
        vault.operator = operator;
        vault.window_slots = window_slots;
        vault.window_cap = window_cap;
        Ok(())
    }

    /// Lock vault funds, within the cap. The escrow's initiator is the vault, so refunds return to it.
    pub fn lock_from_vault(
        ctx: Context<LockFromVault>,
        payment_hash: [u8; 32],
        recipient: Pubkey,
        amount: u64,
        deadline_slot: u64,
        close_on_restart: bool,
    ) -> Result<()> {
        let slot = Clock::get()?.slot;
        let vault_key = ctx.accounts.maker_vault.key();
        let (start, used) = ctx
            .accounts
            .maker_vault
            .after_lock(slot, amount)
            .ok_or(EscrowError::OverWindowCap)?;
        ctx.accounts.maker_vault.window_start_slot = start;
        ctx.accounts.maker_vault.window_used = used;
        init_escrow(
            &mut ctx.accounts.escrow,
            vault_key,
            ctx.accounts.vault_token.key(),
            ctx.accounts.operator.key(),
            payment_hash,
            recipient,
            amount,
            deadline_slot,
            close_on_restart,
            slot,
            ctx.bumps.escrow,
        )?;
        let owner = ctx.accounts.maker_vault.owner;
        let seeds: &[&[u8]] = &[
            MAKER_VAULT_SEED,
            owner.as_ref(),
            &[ctx.accounts.maker_vault.bump],
        ];
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.key(),
                Transfer {
                    from: ctx.accounts.vault_token.to_account_info(),
                    to: ctx.accounts.vault.to_account_info(),
                    authority: ctx.accounts.maker_vault.to_account_info(),
                },
                &[seeds],
            ),
            amount,
        )
    }
}

#[derive(Accounts)]
#[instruction(payment_hash: [u8; 32])]
pub struct Lock<'info> {
    #[account(mut)]
    pub initiator: Signer<'info>,
    #[account(
        init,
        payer = initiator,
        space = 8 + Escrow::INIT_SPACE,
        seeds = [ESCROW_SEED, initiator.key().as_ref(), payment_hash.as_ref()],
        bump
    )]
    pub escrow: Account<'info, Escrow>,
    #[account(
        init,
        payer = initiator,
        seeds = [ESCROW_VAULT_SEED, escrow.key().as_ref()],
        bump,
        token::mint = mint,
        token::authority = escrow
    )]
    pub vault: Account<'info, TokenAccount>,
    #[account(mut, token::mint = mint, token::authority = initiator)]
    pub initiator_token: Account<'info, TokenAccount>,
    #[account(address = USDT_MINT)]
    pub mint: Account<'info, Mint>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Claim<'info> {
    #[account(mut)]
    pub escrow: Account<'info, Escrow>,
    #[account(mut, seeds = [ESCROW_VAULT_SEED, escrow.key().as_ref()], bump)]
    pub vault: Account<'info, TokenAccount>,
    #[account(
        mut,
        constraint = recipient_token.key() == get_associated_token_address(&escrow.recipient, &USDT_MINT)
            && recipient_token.owner == escrow.recipient @ EscrowError::WrongRecipientAccount
    )]
    pub recipient_token: Account<'info, TokenAccount>,
    /// CHECK: Only receives the vault's rent; its address is checked against the escrow.
    #[account(mut, address = escrow.rent_payer)]
    pub rent_payer: UncheckedAccount<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct Refund<'info> {
    #[account(mut)]
    pub escrow: Account<'info, Escrow>,
    #[account(mut, seeds = [ESCROW_VAULT_SEED, escrow.key().as_ref()], bump)]
    pub vault: Account<'info, TokenAccount>,
    #[account(
        mut,
        constraint = initiator_token.key() == escrow.refund_token && initiator_token.owner == escrow.initiator
            @ EscrowError::WrongInitiatorAccount
    )]
    pub initiator_token: Account<'info, TokenAccount>,
    /// CHECK: Only receives the vault's rent; its address is checked against the escrow.
    #[account(mut, address = escrow.rent_payer)]
    pub rent_payer: UncheckedAccount<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct Release<'info> {
    pub recipient: Signer<'info>,
    #[account(mut, has_one = recipient)]
    pub escrow: Account<'info, Escrow>,
    #[account(mut, seeds = [ESCROW_VAULT_SEED, escrow.key().as_ref()], bump)]
    pub vault: Account<'info, TokenAccount>,
    #[account(
        mut,
        constraint = initiator_token.key() == escrow.refund_token && initiator_token.owner == escrow.initiator
            @ EscrowError::WrongInitiatorAccount
    )]
    pub initiator_token: Account<'info, TokenAccount>,
    /// CHECK: Only receives the vault's rent; its address is checked against the escrow.
    #[account(mut, address = escrow.rent_payer)]
    pub rent_payer: UncheckedAccount<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct Extend<'info> {
    pub recipient: Signer<'info>,
    #[account(mut, has_one = recipient)]
    pub escrow: Account<'info, Escrow>,
}

#[derive(Accounts)]
pub struct Close<'info> {
    pub closer: Signer<'info>,
    #[account(mut, close = rent_payer)]
    pub escrow: Account<'info, Escrow>,
    /// CHECK: Only receives the escrow's rent; its address is checked against the escrow.
    #[account(mut, address = escrow.rent_payer)]
    pub rent_payer: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct VaultInit<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(
        init,
        payer = owner,
        space = 8 + MakerVault::INIT_SPACE,
        seeds = [MAKER_VAULT_SEED, owner.key().as_ref()],
        bump
    )]
    pub maker_vault: Account<'info, MakerVault>,
    #[account(
        init,
        payer = owner,
        seeds = [MAKER_VAULT_TOKEN_SEED, maker_vault.key().as_ref()],
        bump,
        token::mint = mint,
        token::authority = maker_vault
    )]
    pub vault_token: Account<'info, TokenAccount>,
    #[account(address = USDT_MINT)]
    pub mint: Account<'info, Mint>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct VaultDeposit<'info> {
    pub depositor: Signer<'info>,
    pub maker_vault: Account<'info, MakerVault>,
    #[account(mut, seeds = [MAKER_VAULT_TOKEN_SEED, maker_vault.key().as_ref()], bump)]
    pub vault_token: Account<'info, TokenAccount>,
    #[account(mut, token::mint = USDT_MINT, token::authority = depositor)]
    pub from: Account<'info, TokenAccount>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct VaultWithdraw<'info> {
    pub owner: Signer<'info>,
    #[account(has_one = owner, seeds = [MAKER_VAULT_SEED, owner.key().as_ref()], bump = maker_vault.bump)]
    pub maker_vault: Account<'info, MakerVault>,
    #[account(mut, seeds = [MAKER_VAULT_TOKEN_SEED, maker_vault.key().as_ref()], bump)]
    pub vault_token: Account<'info, TokenAccount>,
    #[account(mut, token::mint = USDT_MINT)]
    pub to: Account<'info, TokenAccount>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct VaultSetOperator<'info> {
    pub owner: Signer<'info>,
    #[account(mut, has_one = owner, seeds = [MAKER_VAULT_SEED, owner.key().as_ref()], bump = maker_vault.bump)]
    pub maker_vault: Account<'info, MakerVault>,
}

#[derive(Accounts)]
#[instruction(payment_hash: [u8; 32])]
pub struct LockFromVault<'info> {
    #[account(mut)]
    pub operator: Signer<'info>,
    #[account(
        mut,
        has_one = operator,
        seeds = [MAKER_VAULT_SEED, maker_vault.owner.as_ref()],
        bump = maker_vault.bump
    )]
    pub maker_vault: Account<'info, MakerVault>,
    #[account(mut, seeds = [MAKER_VAULT_TOKEN_SEED, maker_vault.key().as_ref()], bump)]
    pub vault_token: Account<'info, TokenAccount>,
    #[account(
        init,
        payer = operator,
        space = 8 + Escrow::INIT_SPACE,
        seeds = [ESCROW_SEED, maker_vault.key().as_ref(), payment_hash.as_ref()],
        bump
    )]
    pub escrow: Account<'info, Escrow>,
    #[account(
        init,
        payer = operator,
        seeds = [ESCROW_VAULT_SEED, escrow.key().as_ref()],
        bump,
        token::mint = mint,
        token::authority = escrow
    )]
    pub vault: Account<'info, TokenAccount>,
    #[account(address = USDT_MINT)]
    pub mint: Account<'info, Mint>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}
