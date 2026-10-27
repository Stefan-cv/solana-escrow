use anchor_lang::prelude::*;
use anchor_spl::token::{self, CloseAccount, Mint, Token, TokenAccount, Transfer};

use crate::{constants::*, events::CancelEvent, state::Escrow};

/// Cancels an order: the full vault balance is refunded to the maker and both
/// accounts are closed with their rent returned. Because the `escrow` PDA is
/// derived from the signer's pubkey, only the actual maker can reach this
/// order — any other signer derives a different, non-existent PDA.
#[derive(Accounts)]
#[instruction(seed: u64)]
pub struct Cancel<'info> {
    #[account(mut)]
    pub maker: Signer<'info>,

    #[account(
        mut,
        seeds = [ESCROW_SEED, maker.key().as_ref(), &seed.to_le_bytes()],
        bump = escrow.bump,
        has_one = maker,
        close = maker,
    )]
    pub escrow: Account<'info, Escrow>,

    #[account(
        mut,
        seeds = [VAULT_SEED, escrow.key().as_ref()],
        bump = escrow.vault_bump,
        token::mint = mint_a,
        token::authority = vault,
    )]
    pub vault: Account<'info, TokenAccount>,

    #[account(mut, token::mint = mint_a, token::authority = maker)]
    pub maker_ata_a: Account<'info, TokenAccount>,

    pub mint_a: Account<'info, Mint>,
    pub token_program: Program<'info, Token>,
}

pub fn handle_cancel(ctx: Context<Cancel>, seed: u64) -> Result<()> {
    let escrow_key = ctx.accounts.escrow.key();
    let seeds = &[
        VAULT_SEED,
        escrow_key.as_ref(),
        &[ctx.accounts.escrow.vault_bump],
    ];

    // Refund everything the vault holds — its on-chain balance is the source
    // of truth, not any amount stored in the order.
    let refund_amount = ctx.accounts.vault.amount;
    token::transfer(
        CpiContext::new_with_signer(
            ctx.accounts.token_program.key(),
            Transfer {
                from: ctx.accounts.vault.to_account_info(),
                to: ctx.accounts.maker_ata_a.to_account_info(),
                authority: ctx.accounts.vault.to_account_info(),
            },
            &[&seeds[..]],
        ),
        refund_amount,
    )?;

    // Close the vault token account first (PDA-signed), then Anchor's
    // `close = maker` reclaims the escrow account — both rents to the maker,
    // and no intermediate state can strand funds.
    token::close_account(CpiContext::new_with_signer(
        ctx.accounts.token_program.key(),
        CloseAccount {
            account: ctx.accounts.vault.to_account_info(),
            destination: ctx.accounts.maker.to_account_info(),
            authority: ctx.accounts.vault.to_account_info(),
        },
        &[&seeds[..]],
    ))?;

    emit!(CancelEvent {
        maker: ctx.accounts.maker.key(),
        seed,
        refund_amount,
    });
    msg!("Escrow {seed} cancelled, {refund_amount} refunded to maker");
    Ok(())
}
