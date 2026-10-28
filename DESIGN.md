# solana-escrow — Design Document

An atomic maker/taker token barter on Solana. Alice locks token A in a
program-owned vault and publishes an order; Bob fulfills it — paying token
B to Alice and receiving the vault's contents — **in one transaction**. Any
failing leg rolls the entire exchange back.

## 1. Goals & non-goals

**Goals**
- Counterparty-risk-free OTC swap: no window where one side can receive and
  not pay
- Many concurrent orders per maker without collision or replay
- Maker can always cancel and get 100% of their tokens + rent back
- The taker, who pays the transaction fee, is compensated with the closed
  accounts' rent

**Non-goals**
- Partial fills / order matching (this is a settlement primitive, not an
  order book) — see §7 for the intended composition
- Price oracles, expiration, off-chain indexing

## 2. Account model

Two PDAs per order:

```
escrow = PDA(["escrow", maker, seed.to_le_bytes()])   the order record
vault  = PDA(["vault", escrow])                       SPL TokenAccount holding mint_a
                                                       authority = vault PDA itself
```

**Why the order id (`seed`) is part of the derivation.** A maker can run
unlimited concurrent orders; each derives to a different address, so orders
can never collide, and "replaying" an id simply re-derives an existing
account (which fails `init`). The seed also makes the escrow address
deterministic for clients without reading state first.

**Custody is identical to solana-vault's design**: the vault token
account's authority is the vault PDA, so only program logic (via
`invoke_signed` with the stored `vault_bump`) can move the locked tokens.

### State layout (`Escrow`)

| Field | Type | Purpose |
|---|---|---|
| `seed` | u64 | Order id; part of the PDA seeds |
| `maker` | Pubkey | Order owner; seeds derivation makes only their signature reach this account |
| `mint_a` / `mint_b` | Pubkey | Offered / demanded mints |
| `receive_amount` | u64 | The price, in mint_b, that the taker must pay the maker |
| `bump`, `vault_bump` | u8 | Stored bumps for cheap PDA re-signing |

Note what is **not** stored: the deposit amount. The vault's on-chain
balance is the truth (see §5).

## 3. Instructions

### `make(seed, deposit, receive_amount)`
Creates the escrow and vault PDAs, then CPI-transfers `deposit` of mint_a
from the maker's ATA into the vault — authorized by the **maker's own
transaction signature** (`invoke` semantics). Both `deposit` and
`receive_amount` must be > 0 (`InvalidAmount`).

### `take(seed)` — the atomic core
One transaction, four effects, any failure reverts all of them:

```
leg 1:  taker_ata_b --receive_amount--> maker_ata_b     (taker signs)
leg 2:  vault       --entire amount--> taker_ata_a      (invoke_signed, vault PDA)
close:  vault token account closed via CPI               (rent -> taker)
close:  escrow closed by Anchor (`close = taker`)        (rent -> taker)
```

Account constraints (the load-bearing ones):
- `escrow`: `seeds = ["escrow", maker, seed]`, `bump = escrow.bump`
  (stored bump), `has_one = maker`, `has_one = mint_b`, `close = taker`
- `vault`: `seeds = ["vault", escrow]`, `token::authority = vault`
- `maker_ata_b`: `token::mint = mint_b, token::authority = maker` — pins
  **what** the maker is paid in and **that** it is their account
- `taker_ata_a` / `taker_ata_b`: see §6 — several checks here are delegated
  to the SPL Token Program on purpose

**Why the taker receives the entire vault balance rather than a stored
amount**: there is no stored amount. Whoever can trigger a release gets
everything the vault holds — the same "chain balance is the single source
of truth" rule as the vault project. A maker over-funding their own vault
only hurts themselves.

**Why rent goes to the taker**: the taker pays the transaction fee and does
the work of settling; the two closed accounts' rent (~0.003 SOL) is their
compensation. The maker's compensation is the payment itself.

### `cancel(seed)`
Signer-derived escrow PDA means **only the maker can reach their order** —
a different signer derives a different (non-existent) address and fails
before execution. Refunds the entire vault balance to the maker
(`invoke_signed`), closes the vault (rent → maker), then Anchor closes the
escrow (`close = maker`). Order of closes matters for the same reason as in
solana-vault: no window with assets but no bookkeeping.

## 4. Events & errors

Events: `MakeEvent`, `TakeEvent` (both amounts), `CancelEvent` — an indexer
can reconstruct every order's lifecycle.

| Code | Error |
|---|---|
| 6000 | `InvalidAmount` (zero deposit or receive amount) |
| 6001 | `Overflow` |

## 5. Atomicity argument

All four effects of `take` are instructions in one Solana transaction.
Solana commits transactions atomically: if any instruction fails — most
realistically, leg 1 failing because the taker's mint_b balance is
insufficient — the whole transaction reverts, including any state mutated
before the failure. This is not an assertion; it is
`take_rolls_back_completely_when_taker_cannot_pay`, which funds a taker
with 10 of the 50 demanded, lets `take` fail, and asserts **every** balance
and account is byte-for-byte as before.

## 6. The SBF stack-frame constraint (post-mortem)

The first compiled version of `Take` crashed on-chain with
`Access violation in stack frame 5` at 3038 CU. The build log explained it:
Anchor's generated `try_accounts` deserializer for the 10-account `Take`
struct measured a **4424-byte stack frame against the SBF's 4096-byte
limit**.

What was tried and what it proved:
1. Removing `lto = "fat"` from the release profile — frame stayed 4424.
   LTO inlining was *not* the driver.
2. Deleting the `mint_a` account and its `token::mint` constraints — frame
   dropped to 4104 (8 bytes over).
3. Dropping the source-side `token::authority = taker` on `taker_ata_b` —
   under the limit; warning gone; all tests green.

Each removal is a **security-neutral** delegation, documented in code:

- **mint_a checks**: the vault PDA is pinned by its seeds (a substituted
  account cannot occupy that address), and the SPL Token Program rejects
  transfers between accounts of different mints — so `token::mint = mint_a`
  was redundant belt-and-braces.
- **taker source authority**: SPL transfer enforces that the signing
  authority owns the source account at execution time.
- **mint_b checks are kept** (`has_one` + destination ATA constraint):
  without them a taker could pay the maker in a worthless mint — that
  constraint is *not* redundant and must never be "optimized" away.

The general lesson, now encoded in this repo: on SBF, account-validation
breadth is a real budget. Security-redundant checks compete with the frame
limit; security-load-bearing checks do not get removed.

## 7. Testing strategy

Same LiteSVM harness as solana-vault: in-process validator, real SPL Token
and ATA programs, hand-rolled setup instructions, `cargo test` only.

| Test | What it proves |
|---|---|
| `happy_path_make_take_atomic_swap` | Both legs settle; both accounts closed; **taker's rent balance increased** |
| `take_rolls_back_completely_when_taker_cannot_pay` | Failed leg 1 ⇒ every balance unchanged, order alive (§5) |
| `cancel_refunds_maker_and_closes_accounts` | Full refund; accounts gone; rent back to maker |
| `only_maker_can_cancel` | Attacker-signed cancel derives a different PDA and fails; maker can still cancel afterwards |
| `one_maker_runs_independent_concurrent_orders` | seeds 1 & 2 ⇒ independent PDA pairs; taking one leaves the other intact |
| `zero_amounts_are_rejected` | 6000 on zero deposit and zero receive |

A harness quirk worth documenting: **LiteSVM's `airdrop` builds its funding
transaction from a fixed keypair and the current blockhash**, so a second
airdrop to the same address re-derives the identical signature and is
rejected as `AlreadyProcessed`. The test env therefore airdrops each wallet
exactly once (tracked in a `HashSet`) — the multi-mint test setup (each
user funded in mint_a *and* mint_b) is what surfaced it.

## 8. Design decisions & alternatives

- **No stored deposit amount** — vault balance is truth; `take` releases
  everything. Alternative (stored amount + refund of the remainder to the
  maker inside `take`) adds a failure mode for zero benefit.
- **Rent to taker, not maker** — see §3. The alternative (maker gets their
  rent back) removes the taker's incentive and invites griefing-by-not-
  taking.
- **No expiration** — a maker who wants out cancels; on-chain time (Clock
  sysvar) was considered and rejected as scope creep for a settlement
  primitive.
- **Intended composition**: this is the base layer for routed swaps — quote
  off-chain via Jupiter v6, compose the swap into the same transaction
  flow. That integration is deliberately out of this repo's scope (see
  README roadmap).

## 9. Stack

Anchor 1.1 · Rust 1.89 · Agave 3.1 · LiteSVM 0.10 · SPL Token + ATA.
Program id: `76MxqbNrmaQMhTRUNB6uXeAsMoDGs1eBCUCcRMXmaUuQ`.
