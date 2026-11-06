//! Hands out the stand-in USDT, once per wallet, from a balance minted to this program. There is no key to steal, and the
//! tokens have no value. A person with many wallets can empty it, and minting to its vault fills it again. Devnet only.

use anchor_lang::prelude::*;
use anchor_spl::associated_token::get_associated_token_address;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};

declare_id!("GAcP2avhVggTqeHSEoKGmGXw4wpJCTasRBjbUfrC1Xpu");

/// The stand-in USDT.
pub const MINT: Pubkey = pubkey!("8BXimW8mCEfhdD1fhv9aoCfQgSj3KxFc52jMAbLvaFwi");
pub const VAULT_SEED: &[u8] = b"vault";
pub const CLAIM_SEED: &[u8] = b"claim";
/// 100 tokens, of 6 decimals: enough for a few swaps at the maker's limits.
pub const DRIP_UNITS: u64 = 100_000_000;

#[error_code]
pub enum FaucetError {
    #[msg("The faucet is empty")]
    Empty,
}

#[program]
pub mod devnet_faucet {
    use super::*;

    /// Anyone may create the vault, once. It belongs to itself, so only this program moves its tokens.
    pub fn init(_ctx: Context<Init>) -> Result<()> {
        Ok(())
    }

    /// The claim account is created here, so a wallet's second claim fails.
    pub fn drip(ctx: Context<Drip>) -> Result<()> {
        require!(ctx.accounts.vault.amount >= DRIP_UNITS, FaucetError::Empty);
        let seeds: &[&[u8]] = &[VAULT_SEED, &[ctx.bumps.vault]];
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.key(),
                Transfer {
                    from: ctx.accounts.vault.to_account_info(),
                    to: ctx.accounts.to.to_account_info(),
                    authority: ctx.accounts.vault.to_account_info(),
                },
                &[seeds],
            ),
            DRIP_UNITS,
        )
    }
}

#[derive(Accounts)]
pub struct Init<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(address = MINT)]
    pub mint: Account<'info, Mint>,
    #[account(init, payer = payer, seeds = [VAULT_SEED], bump, token::mint = mint, token::authority = vault)]
    pub vault: Account<'info, TokenAccount>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[account]
pub struct Claim {}

#[derive(Accounts)]
pub struct Drip<'info> {
    #[account(mut)]
    pub claimant: Signer<'info>,
    #[account(init, payer = claimant, space = 8, seeds = [CLAIM_SEED, claimant.key().as_ref()], bump)]
    pub claim: Account<'info, Claim>,
    #[account(mut, seeds = [VAULT_SEED], bump)]
    pub vault: Account<'info, TokenAccount>,
    #[account(mut, address = get_associated_token_address(&claimant.key(), &MINT))]
    pub to: Account<'info, TokenAccount>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}
