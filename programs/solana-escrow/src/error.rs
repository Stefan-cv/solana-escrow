use anchor_lang::prelude::*;

#[error_code]
pub enum EscrowError {
    #[msg("Deposit and receive amounts must be greater than zero")]
    InvalidAmount,
    #[msg("Arithmetic overflow")]
    Overflow,
}
