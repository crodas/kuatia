# Journaling

Kuatia supports journaling. A committed transfer is a journal entry, and the
transfer log is the accounting journal: the append-only, ordered book of every
committed entry. This page states what that means in practice, shows single,
compound, and multi-asset entries, and explains why the log is auditable by
replay.

For the vocabulary mapping (journal vs. journal entry vs. `Book`, and the terms
that are easy to conflate) see [accounting-mapping.md](accounting-mapping.md).
For the resolve algorithm and the builder API see [transfers.md](transfers.md).
For the decision record see
[ADR-0013](adr/0013-journaling-model.md).

## What "journaling" means here

In classical bookkeeping:

- A **journal** is the book of original entry: a chronological, append-only
  record of every accounting event.
- A **journal entry** is one balanced event. Its debits equal its credits.
- A **compound journal entry** is one event that touches more than two
  accounts.

Kuatia provides the same guarantees over a UTXO-style, posting-based model:

| Classical accounting | Kuatia |
|---|---|
| Journal (book of original entry) | Transfer log (`TransferStore` of `EnvelopeRecord`s) |
| Journal entry (one balanced event) | Committed `Transfer` resolved into an `Envelope` |
| Compound journal entry | `Transfer` with multiple `Movement`s |
| `Σ debits = Σ credits` | Per-asset conservation `sum(consumed) == sum(created)` |

The mechanism differs (signed postings instead of mutable balances), the
accounting grain is identical: one committed transfer is one journal entry.

## A journal entry: one balanced event

A `Transfer` is a `Vec<Movement>` committed atomically. Each `Movement` is
`{ from, to, asset, amount }`. On commit, `resolve()` turns the movements into
an `Envelope` of concrete postings to consume and create, and validation
enforces per-asset conservation before anything is written. The whole entry
commits or none of it does.

A minimal two-account entry:

```rust
let transfer = TransferBuilder::new()
    .pay(alice, bob, usd, Cent::from(50))
    .build();
```

This is one journal entry: 50 USD leaves Alice, 50 USD arrives at Bob, and the
resolved envelope balances (`sum(consumed) == sum(created)` for USD).

## Compound entries: more than two accounts

A compound journal entry is native, not a special case. Because a `Transfer`
holds a list of movements, chaining builder calls accumulates legs into one
atomic entry.

Classical compound entry (a cash sale with tax):

```
2026-06-26  Cash sale of goods
  Dr  Cash ................. 115
      Cr  Revenue ..........     100
      Cr  Sales tax payable .      15
```

The equivalent business effects as one Kuatia transfer:

```rust
let transfer = TransferBuilder::new()
    .book(sales_book)
    .pay(customer, revenue, usd, Cent::from(100))
    .pay(customer, tax_payable, usd, Cent::from(15))
    .build();
// One Transfer -> one Envelope -> one EnvelopeRecord in the transfer log.
```

Both are a single balanced event. In the classical entry `Σ Dr (115) = Σ Cr
(115)`. In Kuatia the resolved envelope satisfies per-asset conservation for
USD. The resolve step aggregates net debit per `(account, asset)` before
selecting postings, so several legs debiting the same account share one
selection pass. See
[transfers.md](transfers.md#resolve-algorithm) for the aggregation detail, and
the note in [accounting-mapping.md](accounting-mapping.md#a-journal-entry-is-multi-account)
on why this shows business effects rather than a literal debit/credit
translation.

## Multi-asset entries

Conservation is enforced per asset, and each asset is an independent
conservation boundary. A single entry can therefore move more than one asset as
long as every asset balances on its own. This is how a currency exchange or FX
trade is recorded as one journal entry: the USD legs balance against USD, and
the EUR legs balance against EUR, within the same envelope.

A hand-built multi-asset envelope is committed through the same path with
`ledger.commit_envelope(envelope)`. Validation returns
`ConservationViolation { asset, consumed_sum, created_sum }` if any single asset
fails to balance.

## The transfer log is the journal

One entry and the journal differ only in grain:

- A `Transfer` / `Envelope` is one record: one journal entry.
- The transfer log is the collection of all records: the accounting journal.
  `TransferStore` persists each committed envelope as `EnvelopeRecord {
  envelope, receipt, created_at }` in append-only order.

Committing an entry returns a `Receipt { transfer_id }` naming the committed
envelope. The `EnvelopeId` is content-addressed (the double-SHA-256 of the
canonical envelope bytes), which gives idempotency (re-committing the same
envelope returns the cached receipt) and tamper evidence (any edit changes the
id).

Kuatia keeps a second append-only sequence, the event log (`EventStore` of
`LedgerEvent`), for lifecycle notifications and projections. It is not the
journal. It records that something happened; the transfer log records exactly
which postings moved. See
[ADR-0010](adr/0010-event-stream-vs-transfer-log.md).

## Auditability by replay

There are no stored balances to drift. An account's balance is the sum of its
`Active` postings, computed on demand. Re-applying every `EnvelopeRecord` in
order reconstructs all balances exactly. Because each entry is balanced per
asset and the log is append-only and content-addressed, the journal is both
verifiable (recompute any balance) and tamper-evident (any change to a past
entry breaks its id).

## Not a journal

Two Kuatia concepts are easy to mistake for the journal and are not:

- `Book` is a transfer policy scope. It gates which accounts and assets may
  participate in a transfer. It is not the journal, not a journal entry, and not
  a balance partition. See
  [accounting-mapping.md](accounting-mapping.md#where-book-fits-and-doesnt).
- The event log records lifecycle notifications for subscribers. The transfer
  log is the source of truth for balances.

## See also

- [accounting-mapping.md](accounting-mapping.md): full classical double-entry
  to Kuatia vocabulary mapping.
- [transfers.md](transfers.md): `Movement`, the resolve algorithm, and the
  `TransferBuilder` API.
- [ADR-0013](adr/0013-journaling-model.md): the decision to model a transfer
  as a (compound) journal entry and the transfer log as the journal.
