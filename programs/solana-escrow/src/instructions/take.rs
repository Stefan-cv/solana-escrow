use anchor_lang::prelude::*;
use anchor_spl::token::{self, CloseAccount, Mint, Token, TokenAccount, Transfer};

use crate::{constants::*, events::TakeEvent, state::Escrow};

/// Fulfills an order in a single transaction: the taker pays
/// `receive_amount` of mint_b to the maker, receives the entire vault
/// balance of mint_a in return, and both the vault token account and the
/// escrow account are closed with their rent refunded to the taker (a small
/// reward for the party that pays the transaction fee). All four effects are
/// atomic — a failure in any leg rolls the whole exchange back.
///
/// `mint_a` is intentionally NOT an account here: the vault PDA is pinned by
/// its seeds and the SPL Token Program itself rejects transfers between
/// accounts of different mints, so the redundant `token::mint` checks would
/// only bloat the deserializer stack frame past the SBF 4 KiB limit.
/// `mint_b` IS validated (`has_one` + destination ATA) — without it a taker
/// could pay the maker with tokens of a worthless mint.
#[derive(Accounts)]
#[instruction(seed: u64)]
pub struct Take<'info> {
    pub taker: Signer<'info>,

    /// CHECK: Safe as a PDA seed source — the `escrow` account below is
    /// derived from this key (seeds = ["escrow", maker, seed]) and checked
    /// with `has_one = maker`, so a wrong key yields a different PDA and the
    /// seeds constraint fails before any funds move.
    pub maker: UncheckedAccount<'info>,

    #[account(
        mut,
        seeds = [ESCROW_SEED, maker.key().as_ref(), &seed.to_le_bytes()],
        bump = escrow.bump,
        has_one = maker,
        has_one = mint_b,
        close = taker,
    )]
    pub escrow: Account<'info, Escrow>,

    #[account(
        mut,
        seeds = [VAULT_SEED, escrow.key().as_ref()],
        bump = escrow.vault_bump,
        token::authority = vault,
    )]
    pub vault: Account<'info, TokenAccount>,

    #[account(mut, token::authority = taker)]
    pub taker_ata_a: Account<'info, TokenAccount>,

    #[account(mut, token::mint = mint_b, token::authority = maker)]
    pub maker_ata_b: Account<'info, TokenAccount>,

    /// Source of the taker's payment. No `token::authority` constraint: the
    /// SPL Token Program itself rejects a transfer whose source account is
    /// not owned by the signing authority (the taker), so the check is
    /// enforced at runtime and kept off the deserializer's stack frame.
    #[account(mut)]
    pub taker_ata_b: Account<'info, TokenAccount>,

    pub mint_b: Account<'info, Mint>,
    pub token_program: Program<'info, Token>,
}

pub fn handle_take(ctx: Context<Take>, seed: u64) -> Result<()> {
    let receive_amount = ctx.accounts.escrow.receive_amount;

    // Leg 1: taker pays mint_b to the maker, authorized by the taker's own
    // transaction signature.
    token::transfer(
        CpiContext::new(
            ctx.accounts.token_program.key(),
            Transfer {
                from: ctx.accounts.taker_ata_b.to_account_info(),
                to: ctx.accounts.maker_ata_b.to_account_info(),
                authority: ctx.accounts.taker.to_account_info(),
            },
        ),
        receive_amount,
    )?;

    // Leg 2: the entire vault balance of mint_a goes to the taker, signed by
    // the vault PDA (`invoke_signed`) — no private keys can move it.
    let escrow_key = ctx.accounts.escrow.key();
    let seeds = &[
        VAULT_SEED,
        escrow_key.as_ref(),
        &[ctx.accounts.escrow.vault_bump],
    ];
    let received_amount = ctx.accounts.vault.amount;
    token::transfer(
        CpiContext::new_with_signer(
            ctx.accounts.token_program.key(),
            Transfer {
                from: ctx.accounts.vault.to_account_info(),
                to: ctx.accounts.taker_ata_a.to_account_info(),
                authority: ctx.accounts.vault.to_account_info(),
            },
            &[&seeds[..]],
        ),
        received_amount,
    )?;

    // Close the vault token account (PDA-signed); rent goes to the taker.
    // Anchor's `close = taker` reclaims the escrow account afterwards.
    token::close_account(CpiContext::new_with_signer(
        ctx.accounts.token_program.key(),
        CloseAccount {
            account: ctx.accounts.vault.to_account_info(),
            destination: ctx.accounts.taker.to_account_info(),
            authority: ctx.accounts.vault.to_account_info(),
        },
        &[&seeds[..]],
    ))?;

    emit!(TakeEvent {
        taker: ctx.accounts.taker.key(),
        maker: ctx.accounts.maker.key(),
        seed,
        paid_amount: receive_amount,
        received_amount,
    });
    msg!("Escrow {seed} taken: swapped {receive_amount} mint_b for {received_amount} mint_a");
    Ok(())
}
