pub mod constants;
pub mod error;
pub mod events;
pub mod instructions;
pub mod state;

use anchor_lang::prelude::*;

pub use constants::*;
pub use instructions::*;
pub use state::*;

declare_id!("76MxqbNrmaQMhTRUNB6uXeAsMoDGs1eBCUCcRMXmaUuQ");

#[program]
pub mod solana_escrow {
    use super::*;

    pub fn make(ctx: Context<Make>, seed: u64, deposit: u64, receive_amount: u64) -> Result<()> {
        crate::instructions::make::handle_make(ctx, seed, deposit, receive_amount)
    }

    pub fn take(ctx: Context<Take>, seed: u64) -> Result<()> {
        crate::instructions::take::handle_take(ctx, seed)
    }

    pub fn cancel(ctx: Context<Cancel>, seed: u64) -> Result<()> {
        crate::instructions::cancel::handle_cancel(ctx, seed)
    }
}
