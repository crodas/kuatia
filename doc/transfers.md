# Transfers

## Overview

A transfer is the atomic unit of value movement in the ledger. It consumes
existing postings and creates new ones, preserving per-asset conservation.

There are two layers:

- **Intent layer**: callers express movements (from, to, asset, amount). The
  ledger resolves these into concrete postings.
- **Envelope layer**: concrete postings to consume and create. Used internally
  after resolution and available for direct callers.

## Movements

A movement is the fundamental building block:

```rust
struct Movement {
    from: AccountId,  // account being debited
    to: AccountId,    // account being credited
    asset: AssetId,   // asset to transfer
    amount: Cent,     // amount (may be negative for offset postings)
}
```

Every operation (pay, deposit, withdraw) is expressed as one or more
movements. The resolve step aggregates net debits per (account, asset) and
selects postings only for accounts with a positive net debit.

## Operations

### Pay

Transfer value between two accounts.

```rust
TransferBuilder::new()
    .pay(from, to, asset, amount)
    .build()
```

Produces one movement:

| from | to | asset | amount |
|------|----|-------|--------|
| A | B | USD | 50 |

Resolve selects postings from A to cover 50, creates a +50 posting on B, and
returns change to A if the selected postings exceed 50.

### Deposit

Fund an account from a system/external source. Creates an offset posting on
the source and a credit on the target.

```rust
TransferBuilder::new()
    .deposit(to, asset, amount, external)
    .build()
```

Produces two movements:

| from | to | asset | amount |
|------|----|-------|--------|
| external | external | USD | -100 |
| external | to | USD | +100 |

The first movement creates a -100 offset posting on the external account. The
second creates a +100 posting on the target account.

Net debit on the external account: -100 + 100 = **0**. No posting selection is
needed: the offset is created directly.

Conservation: created sum = -100 + 100 = 0. Consumed sum = 0. Both sides
balance.

### Withdraw

Move value from an account to an external destination.

```rust
TransferBuilder::new()
    .withdraw(from, asset, amount, external)
    .build()
```

Produces one movement:

| from | to | asset | amount |
|------|----|-------|--------|
| A | external | USD | 50 |

Resolve selects postings from A to cover 50, creates a +50 posting on the
external account, and returns change to A.

### Raw movement

For operations that don't fit the convenience methods:

```rust
TransferBuilder::new()
    .movement(from, to, asset, amount)
    .build()
```

## Resolve Algorithm

The resolve step converts a `Transfer` (intent) into an `Envelope` (concrete
postings) using a two-pass algorithm:

### Pass 1: Create output postings and aggregate debits

For each movement:
1. Create a `NewPosting { owner: to, asset, value: amount }` with
   `payer: Some(from)` when `from != to`
2. Accumulate the movement's amount into a net debit map keyed by
   `(from, asset)`

### Pass 2: Select postings for accounts with positive net debit

For each `(account, asset)` pair where net debit > 0:
1. Query active postings for that account and asset
2. If positive postings cover the net debit: run greedy largest-first
   selection, compute change = selected sum − net debit, and (if change > 0)
   create a change posting returning the remainder to the account.
3. If positive postings are **insufficient**:
   - For `CappedOverdraft` / `UncappedOverdraft` accounts: consume all positive
     postings and create a **negative posting** for the shortfall
     (`net_debit − total_positive`). The `CappedOverdraft` floor is enforced
     later in validation.
   - For any other policy: fail with `InsufficientFunds`.

Pairs with net debit <= 0 (e.g. the external account in a deposit) are skipped.
No posting selection needed.

### Aggregation benefit

Aggregating debits before selection means that multiple movements debiting the
same account share one selection pass. For example, if a transfer contains two
payments from account A (50 + 30), the resolve selects postings once for 80
rather than twice.

## Envelope

After resolution, the result is an `Envelope`:

```rust
struct Envelope {
    consumes: Vec<PostingId>,       // postings to deactivate
    creates: Vec<NewPosting>,       // new postings to create
    account_snapshots: Vec<AccountSnapshotId>,
    book: BookId,
    metadata: Metadata,
}
```

The envelope is content-addressed: its `EnvelopeId` is the double-SHA-256 of
its canonical binary serialization. This provides idempotency (committing the
same envelope twice returns the cached receipt) and tamper evidence.

## Transfer Builder

The `TransferBuilder` provides a fluent API for constructing transfers:

```rust
let transfer = TransferBuilder::new()
    .deposit(alice, usd, Cent::from(1000), bank)
    .pay(alice, bob, usd, Cent::from(200))
    .book(sales_book)
    .metadata(metadata)
    .build();
```

A single transfer can contain multiple movements of different types. All
movements execute atomically. A transfer with multiple movements is a compound
journal entry; see [journaling.md](journaling.md).

## Commit Paths

### Saga commit (default)

```
Transfer → resolve → Envelope → reserve → finalize(validate → write) → Receipt
```

Resolution is read-only; `commit(transfer)` resolves then runs the two-step
envelope saga (reserve → finalize) with automatic retry and LIFO compensation.
Validation runs inside the finalize step, immediately before the writes.

### Committing a pre-built envelope

```
Envelope → reserve → finalize(validate → write) → Receipt
```

`ledger.commit_envelope(envelope)` runs the same saga for an envelope you
already hold (e.g. a hand-built multi-asset/FX envelope, or a reversal).
`reverse()` uses it. There is no separate single-pass "atomic" path.

## Reversal

`reverse(transfer_id)` creates a compensating envelope that:
1. Consumes the original transfer's created postings
2. Recreates the original transfer's consumed postings

This undoes the operation while preserving the full audit trail. No postings
are deleted.

## Validation

Every envelope passes through `validate_and_plan()` before being applied. The
validation steps are:

1. Non-empty (must consume or create at least one posting)
2. No duplicate consumed PostingIds
3. All consumed postings exist
4. All consumed postings are Active or PendingInactive
5. All referenced accounts exist, not frozen, not closed
6. Account snapshot pinning (if provided)
7. Book policy (if a book is loaded): referenced assets/accounts/flags allowed
   by the book
8. Per-asset conservation: `sum(consumed) == sum(created)`
9. Negative postings forbidden only on `NoOverdraft` accounts (allowed on
   overdraft/system/external)
10. Policy enforcement: projected balance satisfies account floor

Validation runs inside the finalize step, immediately before it writes (the
last-step floor / freeze-close re-check). The finalize step then applies the
effects through a sequence of dumb, idempotent store primitives
(`deactivate_postings` → `insert_postings` → `store_transfer` → `append_event`),
verifying every end-state. There is no single transaction; crash-safety comes
from a phase-tracked write-ahead `PendingSaga` record plus `recover()`
roll-forward. The `CappedOverdraft` floor is re-checked as that last step
and is best-effort (not strictly atomic) under concurrency: two transfers
that each pass the floor check against the same pre-transfer balance can
both commit and jointly push the account below its floor. Per-asset
conservation still holds in that case (the negative postings are real
value owed, not minted). The overdraft floor is the only guard with this
property; double-spend prevention is exact (see
`crates/kuatia/tests/concurrency.rs`, which asserts the exact guarantees
and documents the floor race with an ignored test). See
[architecture.md](architecture.md).

See [architecture.md](architecture.md) for details on each check.
