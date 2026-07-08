# Accounting Mapping: Classical Double-Entry ↔ Kuatia

Kuatia provides double-entry-style safety using a UTXO-style model. Value is
held as signed postings, and every committed transfer must satisfy per-asset
conservation. The accounting goal is the same as classical bookkeeping; the
mechanical model is different:

| Classical double-entry | Kuatia |
|---|---|
| `Σ debits = Σ credits` | `sum(consumed) == sum(created)` per asset |

This page maps classical accounting vocabulary onto Kuatia's types and clears
up the terms that are easy to conflate. For a focused, affirmative walkthrough
of journaling (single, compound, and multi-asset entries, and the transfer log
as the journal) see [journaling.md](journaling.md).

The most important correction: in classical accounting a journal is the
append-only book of original entry, while a journal entry is one committed
accounting event. In Kuatia, the closest equivalent to the classical journal
is the transfer log; the closest equivalent to a journal entry is a committed
`Transfer`/`Envelope`. Kuatia's `Book` is neither. It is a transfer policy
scope, not the accounting journal.

## Core mapping

| Classical accounting | Kuatia | Notes |
|---|---|---|
| **Journal** (book of original entry) | **Transfer log**, `TransferStore` of `EnvelopeRecord`s | Append-only, ordered source of truth for committed transfers. |
| **Journal entry** (one balanced event) | Committed **`Transfer`** (intent) → **`Envelope`** (resolved) | One atomic accounting event. |
| **Compound journal entry** | `Transfer` with multiple `Movement`s | One event touching many accounts/assets. |
| **Entry line / leg** | **`Posting`** effect (often derived from a `Movement`) | A concrete account-level value fragment. A `Movement` is two-sided intent `{from, to, asset, amount}` that resolves into consumed/created postings, not a 1:1 debit/credit line. |
| **Σ debits = Σ credits** | **Per-asset conservation** `sum(consumed) == sum(created)` | Enforced in `validate_and_plan`; `ConservationViolation` otherwise. |
| **Ledger** (accounts + running balances) | **Accounts + active postings** | Balances are projections over `Active` postings, never stored. |
| **Posting a transaction** (the verb) | **resolve + commit** (`Transfer` → `Envelope` → apply) | Confusing collision: in Kuatia a *posting* is a noun (a value fragment), not the act of recording. |
| **Accounting book** | *no direct equivalent unless modeled separately* | Kuatia `Book` is **not** this. |
| **Transfer policy scope** | **`Book`** | Gates which accounts/assets may participate. See [below](#where-book-fits-and-doesnt). |

> A journal entry is one committed accounting event. In many accounting
> texts this event is also called a transaction, a word that is overloaded
> in a ledger library (database transaction, business transaction, atomic
> transfer), so this doc prefers "committed accounting event."

## A journal entry is multi-account

Double-entry entries are inherently multi-account; that is the entire point.
A minimal entry has two legs; a compound entry has more. So a Kuatia transfer
touching many accounts is not a mismatch. It is a (compound) journal entry.

Classical compound journal entry:

```
2026-06-26  Cash sale of goods
  Dr  Cash ................. 115
      Cr  Revenue ..........     100
      Cr  Sales tax payable .      15
```

The equivalent business effects as a Kuatia transfer, multiple movements
committed atomically:

```rust
let transfer = TransferBuilder::new()
    .book(sales_book)
    .pay(customer, revenue, usd, Cent::from(100))
    .pay(customer, tax_payable, usd, Cent::from(15))
    .build();
// One Transfer → one Envelope → one EnvelopeRecord in the transfer log.
```

> Note: this is not a literal debit/credit translation. It shows the business
> effects as movements. In a production POS model, cash, revenue, and tax
> might be separate effects routed through system/offset accounts. (A literal
> multi-hop `customer → cash → revenue` chain inside one transfer would
> require spending a posting created earlier in the same envelope, which the
> resolver does not do; it selects from already-committed postings.)

Both are a single balanced event. In the classical entry, `Σ Dr (115) = Σ Cr
(115)`. In Kuatia, the resolved `Envelope` satisfies per-asset conservation:
`sum(consumed) == sum(created)` for USD.

## One entry vs the journal: `Transfer`/`Envelope` vs the transfer log

These differ in grain: one record vs. the collection of all records.

- `Transfer` / `Envelope` = one record (one journal entry).
  - `Transfer` is the intent: `{ movements: Vec<Movement>, book, metadata }`.
    Callers express what should happen, not which postings.
  - `Envelope` is the resolved form produced by `resolve()`: `{ consumes:
    Vec<PostingId>, creates: Vec<NewPosting>, account_snapshots, book, … }`.
    It names the concrete postings to spend and create.
  - Committing one (`commit` / `commit_envelope`) returns a `Receipt {
    transfer_id }` identifying the committed envelope, the `EnvelopeId`, which
    is content-addressed (the double-SHA-256 of the canonical envelope bytes).
- Transfer log = the accounting journal. The append-only, ordered sequence of
  every committed envelope, persisted by `TransferStore` as `EnvelopeRecord {
  envelope, receipt, created_at }`. Each transfer is one entry in it.

> Transfer/Envelope : transfer log :: one journal entry : the journal.

"The system is trivially auditable by replaying the transfer log" means:
re-apply every `EnvelopeRecord` in order and you reconstruct all balances.
There is no stored balance that can drift.

## Two append-only logs: don't conflate "log"

Kuatia keeps two distinct append-only sequences. Only the first is the
accounting journal.

| Log | Type | Records | Role |
|---|---|---|---|
| **Transfer log** | `TransferStore` → `EnvelopeRecord` | Full posting-level detail of each committed transfer | The accounting **journal**, the source of truth for balances. |
| **Event log** | `EventStore` → `LedgerEvent` | High-level lifecycle notifications | Projections / subscribers; *not* the journal. |

`LedgerEvent { seq, timestamp, kind }` carries a monotonic `seq` and a `kind`
of `TransferCommitted | AccountCreated | AccountFrozen | AccountUnfrozen |
AccountClosed`. It tells you that something happened; the transfer log tells
you exactly which postings moved.

## The UTXO wrinkle

Classical ledgers post an entry by mutating each account's running balance.
Kuatia is UTXO-style and posting-based, so the mechanism differs while the
event grain is identical. Because postings are signed, debit/credit is not the
native primitive; resolution works in terms of consuming and creating
postings:

- In a simple movement, the source side is resolved by consuming `Active`
  postings from the source account, creating a change posting if the selected
  postings exceed the amount.
- The destination side is represented by newly created postings on the
  destination account.
- An account's balance is the sum of its `Active` postings, computed on
  demand, never stored.

So an entry line maps to a `Posting` effect, usually derived from a `Movement`
(two-sided intent) that resolves into one or more postings (a created posting,
consumed postings, and possibly change). The balancing rule is unchanged: per
asset, `sum(consumed) == sum(created)`.

## Where `Book` fits (and doesn't)

`Book` is the one Kuatia concept with no classical counterpart. It is a
transfer policy scope: it gates which accounts and assets may participate in a
transfer (`BookPolicy { allowed_assets, allowed_flags, allowed_accounts }`).

It is explicitly not:
- the journal (that is the transfer log),
- a journal entry (that is a `Transfer`/`Envelope`),
- a balance partition (balances are global; a Book only gates participation).

> Despite the name, a Kuatia `Book` must not be confused with an accounting
> book. The accounting journal is the transfer log; `Book` is purely a policy
> scope.

See the [glossary](glossary.md#book) for the Book model and worked examples.

## Quick reference

| Classical accounting | Kuatia | Notes |
|---|---|---|
| Journal | Transfer log / `TransferStore<EnvelopeRecord>` | Append-only source of truth for committed transfers. |
| Journal entry | Committed `Transfer` / `Envelope` | One atomic accounting event. |
| Entry line / leg | `Posting` effect | A concrete account-level value fragment; movements are intent that resolve into postings. |
| Compound journal entry | `Transfer` with multiple movements | One event touching many accounts/assets. |
| Σ debits = Σ credits | Per-asset conservation | `sum(consumed) == sum(created)` per asset. |
| Ledger | Accounts + active postings | Balances are projections over active postings. |
| Posting a transaction (verb) | Resolve + commit | Avoid confusion: Kuatia `Posting` is a noun. |
| Accounting book | No direct equivalent unless modeled separately | Kuatia `Book` is not this. |
| Transfer policy scope | `Book` | Gates allowed accounts/assets. |
| Proof a txn was recorded | `Receipt { transfer_id }` | Content-addressed `EnvelopeId`. |
| Lifecycle notifications | Event log (`LedgerEvent`) | Separate from the transfer log. |

In one line: Kuatia's transfer log is the accounting journal; each committed
envelope is a journal entry; Kuatia's `Book` is a policy scope, not a journal
or a balance partition.
