use anchor_lang::prelude::*;

/// One maker order. The account is a PDA derived from
/// `["escrow", maker, seed_le]`, so a maker can run many independent orders
/// and the derivation itself prevents id collisions and replay.
#[account]
#[derive(InitSpace)]
pub struct Escrow {
    /// Order id; part of the PDA seeds (`to_le_bytes`).
    pub seed: u64,
    /// Maker: locks mint_a, wants receive_amount of mint_b.
    pub maker: Pubkey,
    /// Token the maker offers (custodied in the vault PDA).
    pub mint_a: Pubkey,
    /// Token the maker demands in exchange.
    pub mint_b: Pubkey,
    /// Amount of mint_b the taker must pay the maker.
    pub receive_amount: u64,
    /// Bump of the `escrow` PDA.
    pub bump: u8,
    /// Bump of the `vault` token-account PDA, stored so take/cancel can
    /// re-sign as the vault without re-deriving.
    pub vault_bump: u8,
}
