# Journaling model: a transfer is a journal entry, the transfer log is the journal

* Status: accepted
* Authors: Cesar Rodas
* Date: 2026-07-08
* Targeted modules: kuatia-types, kuatia-core, kuatia (transfer log), doc
* Associated tickets/PRs: n/a

## Context and Problem Statement

Kuatia is presented as a double-entry-style ledger, but it does not have a
"journal" or "journal entry" type, and it has a `Book` type whose name invites
confusion with an accounting book or journal. Users ask whether the ledger
supports journaling, in particular compound entries that touch more than two
accounts. The mechanics exist already (a `Transfer` is a list of `Movement`s
committed atomically, and `TransferStore` is an append-only log), so the open
question is naming and framing: what, in Kuatia's model, is a journal entry,
what is the journal, and what is `Book`?

## Decision Drivers

* Accountants and integrators reason in journal / journal-entry / compound-entry
  terms; the model should map onto those without a new type.
* The mapping must be unambiguous about what is and is not the journal, because
  `Book`, the transfer log, and the event log are all easy to conflate.
* Per-asset conservation is the safety invariant and must be the stated
  equivalent of `Σ debits = Σ credits`.
* Avoid introducing redundant types that duplicate `Transfer` / `Envelope` /
  `TransferStore`.

## Considered Options

#### Option 1: Model journaling with the existing types, and document the mapping

Declare that a committed `Transfer` (resolved into an `Envelope`) is a journal
entry, that a `Transfer` with multiple `Movement`s is a compound journal entry,
and that the transfer log (`TransferStore` of `EnvelopeRecord`s) is the
accounting journal. Enforce balance through the existing per-asset conservation
check. Capture the terminology in `accounting-mapping.md` and `journaling.md`.

**Pros:**

* No new types; the intent API (ADR-0005) and transfer log already provide the
  grain (one entry) and the collection (the journal).
* Compound and multi-asset entries fall out for free: a transfer is already a
  list of movements, and conservation is already per asset.
* Auditability is already structural: replaying the log reconstructs balances,
  and content-addressed ids give tamper evidence.

**Cons:**

* The word "posting" is a noun in Kuatia (a value fragment) but a verb in
  classical accounting (the act of recording), a collision that must be called
  out.
* `Book` keeps a name that suggests "accounting book"; the docs must repeatedly
  disclaim it.

#### Option 2: Add explicit `Journal` and `JournalEntry` types

Introduce dedicated types that wrap the transfer log and a committed transfer.

**Pros:**

* The vocabulary is present in the type system, not only in prose.

**Cons:**

* Pure duplication: `JournalEntry` would be a `Transfer`/`Envelope`, `Journal`
  would be `TransferStore`. Two names for one concept invites drift.
* A dedicated debit/credit line type would fight the UTXO model, where the
  primitive is a signed posting, not a one-sided debit or credit line.

#### Option 3: Rename `Book` to something journal-adjacent

Rename `Book` to reduce the accounting-book confusion.

**Pros:**

* Removes one source of naming conflation.

**Cons:**

* `Book` is a transfer policy scope, not a journal; a journal-adjacent name
  would make the confusion worse, not better. The fix is documentation, not a
  rename.

## Decision Outcome

Chosen option: Option 1. Model journaling with the existing types and document
the mapping. A committed `Transfer` resolved into an `Envelope` is a journal
entry; a `Transfer` with multiple `Movement`s is a compound journal entry; the
transfer log is the accounting journal. Per-asset conservation
(`sum(consumed) == sum(created)`, enforced in `validate_and_plan`) is the
equivalent of `Σ debits = Σ credits`. `Book` is a transfer policy scope, not the
journal, not a journal entry, and not a balance partition. The event log
(ADR-0010) is for lifecycle notifications and is not the journal.

### Positive Consequences

* Journaling, including compound and multi-asset entries, is supported with no
  new types and no schema change.
* The journal is auditable by replay and tamper-evident by construction.
* The naming pitfalls (`Book` vs. journal, posting-noun vs. posting-verb,
  transfer log vs. event log) are documented in one place.

### Negative Consequences

* The vocabulary lives in documentation rather than in the type system, so it
  relies on `journaling.md` and `accounting-mapping.md` staying accurate.
* `Book` retains a name that needs a standing disclaimer.

## Links

* Documented by [journaling.md](../journaling.md) and
  [accounting-mapping.md](../accounting-mapping.md)
* Builds on [ADR-0001](0001-modified-utxo-signed-postings.md) (value as signed
  postings, conservation is structural)
* Builds on [ADR-0005](0005-intent-api-movements-vs-envelopes.md) (a `Transfer`
  is intent resolved into an `Envelope`)
* Relates to [ADR-0010](0010-event-stream-vs-transfer-log.md) (the event stream
  is not the journal)
