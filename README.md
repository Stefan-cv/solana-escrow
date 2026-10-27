# solana-escrow

Atomic maker/taker token barter on Solana, built with Anchor.

Alice locks token A in a PDA vault and publishes an order; Bob fulfills it in a
**single transaction** — paying token B to Alice and receiving the entire vault
balance of A, with both accounts closed and rent refunded. A failure in any leg
rolls the whole exchange back: no counterparty risk, no stuck intermediate
states.

## Architecture

```
Alice (maker)                    Bob (taker)
   │ make(seed, deposit,            │ take(seed)
   ▼                                ▼
┌─────────────────────────┐   ┌──────────────────────────────┐
│ escrow PDA              │   │ single transaction:          │
│ seeds = ["escrow",      │   │  1. Bob  ──B──▶ Alice        │
│          maker, seed]   │   │  2. vault ──A──▶ Bob         │
│ order: mint_b wanted,   │──▶│  3. close vault + escrow     │
│ receive_amount, bumps   │   │     (rent → taker)           │
└─────────────────────────┘   └──────────────────────────────┘
   │                ▲
   ▼                │ seeds = ["vault", escrow]
┌─────────────────────────┐
│ vault PDA (SPL TA)      │  authority = vault PDA itself:
│ custodies deposit of A  │  only program rules move it out
└─────────────────────────┘

  cancel (maker only): vault ──A──▶ maker, close both, rent → maker
```

The `seed` order id is part of the escrow PDA derivation, so one maker can run
many concurrent orders that can never collide, and replaying an order id just
derives an account that already exists.

## Instructions

| Instruction | What it does |
|---|---|
| `make(seed, deposit, receive_amount)` | Creates the `escrow` PDA + `vault` PDA, locks `deposit` of mint_a (user-signed CPI), stores both bumps |
| `take(seed)` | One transaction: taker pays `receive_amount` of mint_b to the maker, the full vault balance of mint_a goes to the taker (`invoke_signed`, PDA authority), vault + escrow closed, rent refunded to the taker |
| `cancel(seed)` | Maker-only: full vault balance refunded, both accounts closed, rent back to the maker |

## Security design

- **PDA custody, no private keys.** Vault outflows only via `invoke_signed`
  with the stored bump — the vault token account's authority is the vault PDA.
- **Seeds as ownership & replay protection.** `escrow = PDA(["escrow",
  maker, seed])`: only the maker's own signer can reach their order in
  `cancel`; a different signer derives a different (non-existent) PDA.
- **Atomic exchange.** Both legs + closes execute in one transaction; the
  `take_rolls_back_completely_when_taker_cannot_pay` test proves a failed leg
  leaves every balance untouched and the order alive.
- **Payment-mint pinning.** `escrow.mint_b` is checked with `has_one`, and the
  maker's receiving ATA is constrained to that mint — a taker cannot pay with
  tokens of a worthless mint. (Source-side authority and mint matching are
  enforced by the SPL Token Program itself at transfer time.)
- **Deliberate trust boundary, documented in code:** `mint_a` and the taker's
  payment-source authority are NOT re-validated in Anchor constraints. They
  are enforced at runtime by the SPL Token Program (mints must match between
  source and destination; the signing authority must own the source). This
  keeps `Take`'s account deserializer under the SBF **4 KiB stack-frame
  limit** — with the redundant constraints the compiled `try_accounts` frame
  measured 4424 bytes and crashed on-chain with
  `Access violation in stack frame`. See the note in `Cargo.toml` and
  `instructions/take.rs`.
- **Checked arithmetic** and explicit error codes (`InvalidAmount`, `Overflow`).

## Tests

Six integration tests run against an in-process validator (LiteSVM) with the
real SPL Token and ATA programs loaded — no local cluster required:

```bash
anchor build   # produces target/deploy/solana_escrow.so used by the tests
anchor test    # or: cargo test (from programs/solana-escrow)
```

| Test | Verifies |
|---|---|
| `happy_path_make_take_atomic_swap` | make → take swaps both legs, closes both accounts, taker receives the refunded rent |
| `take_rolls_back_completely_when_taker_cannot_pay` | Failed leg 1 leaves every balance and the order exactly as before |
| `cancel_refunds_maker_and_closes_accounts` | Full refund of the locked amount + rent to the maker |
| `only_maker_can_cancel` | Attacker-signed cancel derives a different PDA and is rejected; the real maker can still cancel |
| `one_maker_runs_independent_concurrent_orders` | Two seeds → two independent escrow/vault PDA pairs; taking one leaves the other intact |
| `zero_amounts_are_rejected` | Zero deposit / zero receive rejected with `InvalidAmount` (6000) |

## Project layout

```
programs/solana-escrow/src
├── lib.rs                  # instruction entrypoints
├── constants.rs            # PDA seeds ("escrow", "vault")
├── state.rs                # Escrow order account
├── events.rs               # MakeEvent / TakeEvent / CancelEvent
├── error.rs                # EscrowError codes
└── instructions/           # make / take / cancel
programs/solana-escrow/tests/test_escrow.rs   # LiteSVM integration tests
```

## Stack

- Anchor 1.x / Rust 1.89 / solana-cli (Agave) 3.1
- SPL Token + Associated Token Account programs
- LiteSVM for in-process integration tests

## Deploying to devnet

The program id is held in `target/deploy/solana_escrow-keypair.json`
(`76MxqbNrmaQMhTRUNB6uXeAsMoDGs1eBCUCcRMXmaUuQ`).

```bash
solana config set --url devnet
# [provider] cluster = "devnet" in Anchor.toml
solana airdrop 2          # deploy needs ~3 SOL
anchor build && anchor deploy
```

## Roadmap

- Jupiter v6 routing on top of the released side (quote off-chain, swap
  composed into the same transaction flow) — trading infrastructure the
  vault can route through.
