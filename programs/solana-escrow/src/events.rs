use anchor_lang::prelude::*;

#[event]
pub struct MakeEvent {
    pub maker: Pubkey,
    pub seed: u64,
    pub deposit_amount: u64,
    pub receive_amount: u64,
}

#[event]
pub struct TakeEvent {
    pub taker: Pubkey,
    pub maker: Pubkey,
    pub seed: u64,
    pub paid_amount: u64,
    pub received_amount: u64,
}

#[event]
pub struct CancelEvent {
    pub maker: Pubkey,
    pub seed: u64,
    pub refund_amount: u64,
}
