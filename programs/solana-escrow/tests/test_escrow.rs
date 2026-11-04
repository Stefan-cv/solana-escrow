//! Integration tests for the solana-escrow program, executed against an
//! in-process SVM (LiteSVM) with the built-in SPL Token / ATA programs.
//!
//! Coverage:
//! 1. Happy path: make -> take executes both legs atomically, closes accounts
//! 2. Take rolls back completely when the taker cannot pay (atomicity)
//! 3. Cancel refunds the full vault balance and closes both accounts
//! 4. Only the maker can cancel (seeds derivation rejects other signers)
//! 5. One maker can run independent concurrent orders (seed uniqueness)
//! 6. Zero deposit / receive amounts rejected (custom error 6000)
//!
//! The SPL Token / ATA setup instructions are built by hand, mirroring the
//! solana-vault test harness.

use {
    anchor_lang::{
        prelude::Pubkey,
        solana_program::{
            instruction::{AccountMeta, Instruction},
            system_instruction, system_program,
        },
        AccountDeserialize, InstructionData, ToAccountMetas,
    },
    anchor_spl::{associated_token::ID as ATA_PROGRAM_ID, token::ID as TOKEN_PROGRAM_ID},
    litesvm::LiteSVM,
    solana_keypair::Keypair,
    solana_message::{Message, VersionedMessage},
    solana_signer::Signer,
    solana_transaction::versioned::VersionedTransaction,
};

const INVALID_AMOUNT_ERR: u32 = 6000; // 0x1770

/// spl_token::state::Mint::LEN
const TOKEN_MINT_SPACE: u64 = 82;

/// Lamports for newly created accounts — far above the rent-exempt minimum.
const ACCOUNT_LAMPORTS: u64 = 1_000_000_000;

// ---------------------------------------------------------------------------
// Hand-rolled SPL Token / ATA setup instructions
// ---------------------------------------------------------------------------

fn spl_initialize_mint2(mint: &Pubkey, mint_authority: &Pubkey, decimals: u8) -> Instruction {
    let mut data = Vec::with_capacity(35);
    data.push(20u8); // TokenInstruction::InitializeMint2
    data.push(decimals);
    data.extend_from_slice(mint_authority.as_ref());
    data.push(0u8); // COption::None (freeze authority)
    Instruction::new_with_bytes(
        TOKEN_PROGRAM_ID,
        &data,
        vec![AccountMeta::new(*mint, false)],
    )
}

fn spl_mint_to(
    mint: &Pubkey,
    destination: &Pubkey,
    authority: &Pubkey,
    amount: u64,
) -> Instruction {
    let mut data = Vec::with_capacity(9);
    data.push(7u8); // TokenInstruction::MintTo
    data.extend_from_slice(&amount.to_le_bytes());
    Instruction::new_with_bytes(
        TOKEN_PROGRAM_ID,
        &data,
        vec![
            AccountMeta::new(*mint, false),
            AccountMeta::new(*destination, false),
            AccountMeta::new(*authority, true),
        ],
    )
}

fn spl_create_ata(funding: &Pubkey, owner: &Pubkey, mint: &Pubkey) -> (Pubkey, Instruction) {
    let ata = Pubkey::find_program_address(
        &[owner.as_ref(), TOKEN_PROGRAM_ID.as_ref(), mint.as_ref()],
        &ATA_PROGRAM_ID,
    )
    .0;
    let ix = Instruction::new_with_bytes(
        ATA_PROGRAM_ID,
        &[0u8], // AssociatedTokenAccountInstruction::Create
        vec![
            AccountMeta::new(*funding, true),
            AccountMeta::new(ata, false),
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(system_program::ID, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
        ],
    );
    (ata, ix)
}

// ---------------------------------------------------------------------------
// Test environment
// ---------------------------------------------------------------------------

struct TestEnv {
    svm: LiteSVM,
    program_id: Pubkey,
    payer: Keypair,
    mint_a: Pubkey,
    mint_b: Pubkey,
    /// Wallets already funded with lamports. LiteSVM's airdrop signs with a
    /// fixed keypair against a static blockhash, so a second airdrop to the
    /// same address reuses the identical signature and is rejected as
    /// AlreadyProcessed — each wallet is airdropped exactly once instead.
    funded: std::collections::HashSet<Pubkey>,
}

impl TestEnv {
    fn new() -> Self {
        let program_id = solana_escrow::id();
        let mut svm = LiteSVM::new().with_default_programs();
        let bytes = include_bytes!(concat!(
            env!("CARGO_TARGET_TMPDIR"),
            "/../deploy/solana_escrow.so"
        ));
        svm.add_program(program_id, bytes).unwrap();

        let payer = Keypair::new();
        svm.airdrop(&payer.pubkey(), 10_000_000_000_000).unwrap();

        let mint_a = Self::create_mint(&mut svm, &payer);
        let mint_b = Self::create_mint(&mut svm, &payer);

        Self {
            svm,
            program_id,
            payer,
            mint_a,
            mint_b,
            funded: std::collections::HashSet::new(),
        }
    }

    fn create_mint(svm: &mut LiteSVM, payer: &Keypair) -> Pubkey {
        let mint_keypair = Keypair::new();
        let mint = mint_keypair.pubkey();
        let create_ix = system_instruction::create_account(
            &payer.pubkey(),
            &mint,
            ACCOUNT_LAMPORTS,
            TOKEN_MINT_SPACE,
            &TOKEN_PROGRAM_ID,
        );
        let init_ix = spl_initialize_mint2(&mint, &payer.pubkey(), 9);
        send_ok(svm, &[create_ix, init_ix], &[payer, &mint_keypair]);
        mint
    }

    /// Funds `owner` with an ATA holding `amount` of `mint`; returns the ATA.
    /// `mint` is taken by value (Pubkey: Copy) so call sites can pass
    /// `env.mint_a` directly without borrowing `env` twice.
    fn fund(&mut self, owner: &Keypair, mint: Pubkey, amount: u64) -> Pubkey {
        if self.funded.insert(owner.pubkey()) {
            self.svm
                .airdrop(&owner.pubkey(), 1_000_000_000_000)
                .unwrap();
        }
        let (ata, create_ix) = spl_create_ata(&self.payer.pubkey(), &owner.pubkey(), &mint);
        if amount > 0 {
            let mint_ix = spl_mint_to(&mint, &ata, &self.payer.pubkey(), amount);
            send_ok(&mut self.svm, &[create_ix, mint_ix], &[&self.payer]);
        } else {
            send_ok(&mut self.svm, &[create_ix], &[&self.payer]);
        }
        ata
    }

    fn escrow_pdas(&self, maker: &Pubkey, seed: u64) -> (Pubkey, Pubkey) {
        let escrow = Pubkey::find_program_address(
            &[
                solana_escrow::constants::ESCROW_SEED,
                maker.as_ref(),
                &seed.to_le_bytes(),
            ],
            &self.program_id,
        )
        .0;
        let vault = Pubkey::find_program_address(
            &[solana_escrow::constants::VAULT_SEED, escrow.as_ref()],
            &self.program_id,
        )
        .0;
        (escrow, vault)
    }

    fn make_ix(
        &self,
        maker: &Pubkey,
        maker_ata_a: &Pubkey,
        seed: u64,
        deposit: u64,
        receive_amount: u64,
    ) -> Instruction {
        let (escrow, vault) = self.escrow_pdas(maker, seed);
        Instruction::new_with_bytes(
            self.program_id,
            &solana_escrow::instruction::Make {
                seed,
                deposit,
                receive_amount,
            }
            .data(),
            solana_escrow::accounts::Make {
                maker: *maker,
                escrow,
                vault,
                maker_ata_a: *maker_ata_a,
                mint_a: self.mint_a,
                mint_b: self.mint_b,
                token_program: TOKEN_PROGRAM_ID,
                system_program: system_program::ID,
                associated_token_program: ATA_PROGRAM_ID,
            }
            .to_account_metas(None),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn take_ix(
        &self,
        taker: &Pubkey,
        maker: &Pubkey,
        seed: u64,
        taker_ata_a: &Pubkey,
        taker_ata_b: &Pubkey,
        maker_ata_b: &Pubkey,
    ) -> Instruction {
        let (escrow, vault) = self.escrow_pdas(maker, seed);
        Instruction::new_with_bytes(
            self.program_id,
            &solana_escrow::instruction::Take { seed }.data(),
            solana_escrow::accounts::Take {
                taker: *taker,
                maker: *maker,
                escrow,
                vault,
                taker_ata_a: *taker_ata_a,
                maker_ata_b: *maker_ata_b,
                taker_ata_b: *taker_ata_b,
                mint_b: self.mint_b,
                token_program: TOKEN_PROGRAM_ID,
            }
            .to_account_metas(None),
        )
    }

    fn cancel_ix(&self, maker: &Pubkey, seed: u64, maker_ata_a: &Pubkey) -> Instruction {
        let (escrow, vault) = self.escrow_pdas(maker, seed);
        Instruction::new_with_bytes(
            self.program_id,
            &solana_escrow::instruction::Cancel { seed }.data(),
            solana_escrow::accounts::Cancel {
                maker: *maker,
                escrow,
                vault,
                maker_ata_a: *maker_ata_a,
                mint_a: self.mint_a,
                token_program: TOKEN_PROGRAM_ID,
            }
            .to_account_metas(None),
        )
    }

    /// Token amount of a token account (offset 64 in the SPL Token layout).
    fn token_balance(&mut self, account: &Pubkey) -> u64 {
        self.svm
            .get_account(account)
            .expect("token account exists")
            .data[64..72]
            .try_into()
            .map(u64::from_le_bytes)
            .unwrap()
    }

    fn escrow_state(&mut self, account: &Pubkey) -> solana_escrow::state::Escrow {
        let acct = self.svm.get_account(account).expect("escrow exists");
        let mut data: &[u8] = &acct.data;
        solana_escrow::state::Escrow::try_deserialize(&mut data).unwrap()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn send(svm: &mut LiteSVM, ixs: &[Instruction], signers: &[&Keypair]) -> Result<(), String> {
    let blockhash = svm.latest_blockhash();
    let msg = Message::new_with_blockhash(ixs, Some(&signers[0].pubkey()), &blockhash);
    let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), signers).unwrap();
    svm.send_transaction(tx)
        .map(|_| ())
        .map_err(|e| format!("{}; logs:\n{}", e.err, e.meta.pretty_logs()))
}

fn send_ok(svm: &mut LiteSVM, ixs: &[Instruction], signers: &[&Keypair]) {
    send(svm, ixs, signers).unwrap_or_else(|e| panic!("transaction failed: {e}"));
}

fn send_expect_custom_err(svm: &mut LiteSVM, ixs: &[Instruction], signers: &[&Keypair], code: u32) {
    match send(svm, ixs, signers) {
        Ok(()) => panic!("expected transaction to fail with error {code}"),
        Err(e) => {
            let expected = format!("0x{:x}", code);
            assert!(
                e.contains(&expected),
                "expected custom error {expected}, got: {e}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn happy_path_make_take_atomic_swap() {
    let mut env = TestEnv::new();
    let alice = Keypair::new();
    let bob = Keypair::new();
    let alice_ata_a = env.fund(&alice, env.mint_a, 1_000);
    let alice_ata_b = env.fund(&alice, env.mint_b, 0);
    let bob_ata_a = env.fund(&bob, env.mint_a, 0);
    let bob_ata_b = env.fund(&bob, env.mint_b, 500);

    // Alice offers 100 A for 50 B.
    let (escrow_pda, vault_pda) = env.escrow_pdas(&alice.pubkey(), 1);
    let ix = env.make_ix(&alice.pubkey(), &alice_ata_a, 1, 100, 50);
    send_ok(&mut env.svm, &[ix], &[&alice]);

    assert_eq!(env.token_balance(&vault_pda), 100);
    assert_eq!(env.token_balance(&alice_ata_a), 900);
    let state = env.escrow_state(&escrow_pda);
    assert_eq!(state.maker, alice.pubkey());
    assert_eq!(state.receive_amount, 50);
    assert_eq!(state.mint_a, env.mint_a);
    assert_eq!(state.mint_b, env.mint_b);

    // Bob takes: pays 50 B to Alice, receives 100 A; accounts closed and the
    // taker's rent balance goes up (he received both closed accounts' rent).
    let lamports_before = env
        .svm
        .get_account(&bob.pubkey())
        .expect("taker wallet exists")
        .lamports;
    let ix = env.take_ix(
        &bob.pubkey(),
        &alice.pubkey(),
        1,
        &bob_ata_a,
        &bob_ata_b,
        &alice_ata_b,
    );
    send_ok(&mut env.svm, &[ix], &[&bob]);

    assert_eq!(env.token_balance(&bob_ata_a), 100);
    assert_eq!(env.token_balance(&bob_ata_b), 450);
    assert_eq!(env.token_balance(&alice_ata_b), 50);
    assert!(env.svm.get_account(&escrow_pda).is_none());
    assert!(env.svm.get_account(&vault_pda).is_none());
    let lamports_after = env
        .svm
        .get_account(&bob.pubkey())
        .expect("taker wallet exists")
        .lamports;
    assert!(
        lamports_after > lamports_before,
        "closed-account rent should be refunded to the taker"
    );
}

#[test]
fn take_rolls_back_completely_when_taker_cannot_pay() {
    let mut env = TestEnv::new();
    let alice = Keypair::new();
    let bob = Keypair::new();
    let alice_ata_a = env.fund(&alice, env.mint_a, 1_000);
    let alice_ata_b = env.fund(&alice, env.mint_b, 0);
    let bob_ata_a = env.fund(&bob, env.mint_a, 0);
    // Bob can only pay 10 of the 50 B demanded.
    let bob_ata_b = env.fund(&bob, env.mint_b, 10);

    let (escrow_pda, vault_pda) = env.escrow_pdas(&alice.pubkey(), 7);
    let ix = env.make_ix(&alice.pubkey(), &alice_ata_a, 7, 100, 50);
    send_ok(&mut env.svm, &[ix], &[&alice]);

    // The take fails on leg 1 (insufficient mint_b) — the whole transaction
    // must roll back: no partial swap, no closed accounts.
    let ix = env.take_ix(
        &bob.pubkey(),
        &alice.pubkey(),
        7,
        &bob_ata_a,
        &bob_ata_b,
        &alice_ata_b,
    );
    let err = send(&mut env.svm, &[ix], &[&bob]).unwrap_err();
    assert!(
        err.contains("custom program error"),
        "expected the exchange to fail on-chain, got: {err}"
    );

    // Atomicity: everything is exactly as before.
    assert!(env.svm.get_account(&escrow_pda).is_some());
    assert_eq!(env.token_balance(&vault_pda), 100);
    assert_eq!(env.token_balance(&alice_ata_b), 0);
    assert_eq!(env.token_balance(&bob_ata_a), 0);
    assert_eq!(env.token_balance(&bob_ata_b), 10);
}

#[test]
fn cancel_refunds_maker_and_closes_accounts() {
    let mut env = TestEnv::new();
    let alice = Keypair::new();
    let alice_ata_a = env.fund(&alice, env.mint_a, 500);
    let (escrow_pda, vault_pda) = env.escrow_pdas(&alice.pubkey(), 3);

    let ix = env.make_ix(&alice.pubkey(), &alice_ata_a, 3, 200, 80);
    send_ok(&mut env.svm, &[ix], &[&alice]);
    assert_eq!(env.token_balance(&vault_pda), 200);
    assert_eq!(env.token_balance(&alice_ata_a), 300);

    let lamports_before = env
        .svm
        .get_account(&alice.pubkey())
        .expect("maker wallet exists")
        .lamports;
    let ix = env.cancel_ix(&alice.pubkey(), 3, &alice_ata_a);
    send_ok(&mut env.svm, &[ix], &[&alice]);

    assert_eq!(env.token_balance(&alice_ata_a), 500);
    assert!(env.svm.get_account(&escrow_pda).is_none());
    assert!(env.svm.get_account(&vault_pda).is_none());
    let lamports_after = env
        .svm
        .get_account(&alice.pubkey())
        .expect("maker wallet exists")
        .lamports;
    assert!(
        lamports_after > lamports_before,
        "rent should be refunded to the maker"
    );
}

#[test]
fn only_maker_can_cancel() {
    let mut env = TestEnv::new();
    let alice = Keypair::new();
    let attacker = Keypair::new();
    let alice_ata_a = env.fund(&alice, env.mint_a, 500);
    let (alice_escrow_pda, alice_vault_pda) = env.escrow_pdas(&alice.pubkey(), 5);

    let ix = env.make_ix(&alice.pubkey(), &alice_ata_a, 5, 100, 40);
    send_ok(&mut env.svm, &[ix], &[&alice]);

    // The attacker signs a cancel; the escrow PDA derived from the
    // attacker's key does not exist, so the order is never reachable.
    env.svm.airdrop(&attacker.pubkey(), 1_000_000_000).unwrap();
    let (attacker_escrow_pda, _) = env.escrow_pdas(&attacker.pubkey(), 5);
    let ix = env.cancel_ix(&attacker.pubkey(), 5, &alice_ata_a);
    let err = send(&mut env.svm, &[ix], &[&attacker]).unwrap_err();
    assert!(
        err.contains(&attacker_escrow_pda.to_string()) || err.contains("custom program error"),
        "expected rejection, got: {err}"
    );

    // Alice's order is untouched and she can still cancel it herself.
    assert!(env.svm.get_account(&alice_escrow_pda).is_some());
    assert_eq!(env.token_balance(&alice_vault_pda), 100);
    let ix = env.cancel_ix(&alice.pubkey(), 5, &alice_ata_a);
    send_ok(&mut env.svm, &[ix], &[&alice]);
    assert!(env.svm.get_account(&alice_escrow_pda).is_none());
}

#[test]
fn one_maker_runs_independent_concurrent_orders() {
    let mut env = TestEnv::new();
    let alice = Keypair::new();
    let bob = Keypair::new();
    let alice_ata_a = env.fund(&alice, env.mint_a, 1_000);
    let alice_ata_b = env.fund(&alice, env.mint_b, 0);
    let bob_ata_a = env.fund(&bob, env.mint_a, 0);
    let bob_ata_b = env.fund(&bob, env.mint_b, 1_000);

    let (escrow1, vault1) = env.escrow_pdas(&alice.pubkey(), 1);
    let (escrow2, vault2) = env.escrow_pdas(&alice.pubkey(), 2);
    assert_ne!(escrow1, escrow2);
    assert_ne!(vault1, vault2);

    let ix = env.make_ix(&alice.pubkey(), &alice_ata_a, 1, 100, 50);
    send_ok(&mut env.svm, &[ix], &[&alice]);
    let ix = env.make_ix(&alice.pubkey(), &alice_ata_a, 2, 300, 200);
    send_ok(&mut env.svm, &[ix], &[&alice]);
    assert_eq!(env.token_balance(&vault1), 100);
    assert_eq!(env.token_balance(&vault2), 300);

    // Taking order 1 leaves order 2 fully intact.
    let ix = env.take_ix(
        &bob.pubkey(),
        &alice.pubkey(),
        1,
        &bob_ata_a,
        &bob_ata_b,
        &alice_ata_b,
    );
    send_ok(&mut env.svm, &[ix], &[&bob]);
    assert!(env.svm.get_account(&escrow1).is_none());
    assert!(env.svm.get_account(&escrow2).is_some());
    assert_eq!(env.token_balance(&vault2), 300);
    assert_eq!(env.token_balance(&alice_ata_b), 50);
}

#[test]
fn zero_amounts_are_rejected() {
    let mut env = TestEnv::new();
    let alice = Keypair::new();
    let alice_ata_a = env.fund(&alice, env.mint_a, 100);

    let ix = env.make_ix(&alice.pubkey(), &alice_ata_a, 1, 0, 50);
    send_expect_custom_err(&mut env.svm, &[ix], &[&alice], INVALID_AMOUNT_ERR);

    let ix = env.make_ix(&alice.pubkey(), &alice_ata_a, 1, 10, 0);
    send_expect_custom_err(&mut env.svm, &[ix], &[&alice], INVALID_AMOUNT_ERR);
}
