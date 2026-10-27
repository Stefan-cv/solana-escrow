use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};

use crate::{constants::*, error::EscrowError, events::MakeEvent, state::Escrow};

/// Creates an order: an `escrow` PDA (seeds = ["escrow", maker, seed]) plus a
/// `vault` token-account PDA (seeds = ["vault", escrow]) whose authority is
/// the vault PDA itself, then locks `deposit` of mint_a inside it. The seed
/// in the PDA derivation lets one maker run many concurrent orders that can
/// never collide or be replayed against each other.
#[derive(Accounts)]
#[instruction(seed: u64, deposit: u64, receive_amount: u64)]
pub struct Make<'info> {
    #[account(mut)]
    pub maker: Signer<'info>,

    #[account(
        init,
        payer = maker,
        space = 8 + Escrow::INIT_SPACE,
        seeds = [ESCROW_SEED, maker.key().as_ref(), &seed.to_le_bytes()],
        bump,
    )]
    pub escrow: Account<'info, Escrow>,

    #[account(
        init,
        payer = maker,
        token::mint = mint_a,
        token::authority = vault,
        seeds = [VAULT_SEED, escrow.key().as_ref()],
        bump,
    )]
    pub vault: Account<'info, TokenAccount>,

    #[account(mut, token::mint = mint_a, token::authority = maker)]
    pub maker_ata_a: Account<'info, TokenAccount>,

    pub mint_a: Account<'info, Mint>,
    pub mint_b: Account<'info, Mint>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
    pub associated_token_program: Program<'info, AssociatedToken>,
}

pub fn handle_make(ctx: Context<Make>, seed: u64, deposit: u64, receive_amount: u64) -> Result<()> {
    require!(deposit > 0, EscrowError::InvalidAmount);
    require!(receive_amount > 0, EscrowError::InvalidAmount);

    ctx.accounts.escrow.seed = seed;
    ctx.accounts.escrow.maker = ctx.accounts.maker.key();
    ctx.accounts.escrow.mint_a = ctx.accounts.mint_a.key();
    ctx.accounts.escrow.mint_b = ctx.accounts.mint_b.key();
    ctx.accounts.escrow.receive_amount = receive_amount;
    ctx.accounts.escrow.bump = ctx.bumps.escrow;
    ctx.accounts.escrow.vault_bump = ctx.bumps.vault;

    // CPI into the SPL Token Program at the transaction level: the maker's
    // own signature authorizes moving their tokens into the vault PDA.
    token::transfer(
        CpiContext::new(
            ctx.accounts.token_program.key(),
            Transfer {
                from: ctx.accounts.maker_ata_a.to_account_info(),
                to: ctx.accounts.vault.to_account_info(),
                authority: ctx.accounts.maker.to_account_info(),
            },
        ),
        deposit,
    )?;

    emit!(MakeEvent {
        maker: ctx.accounts.maker.key(),
        seed,
        deposit_amount: deposit,
        receive_amount,
    });
    msg!("Escrow {seed} created: locked {deposit} of mint_a");
    Ok(())
}
