# Collapse account policy into a single debit-must-not-exceed-credit flag

* Status: accepted
* Authors: Cesar Rodas
* Date: 2026-07-16
* Targeted modules: `kuatia-types` (`AccountFlags`, `Account`), `kuatia-core`
  (`validate.rs`, `posting_resolution.rs`), `kuatia-storage-sql`
* Associated tickets/PRs: N/A

## Context and Problem Statement

[ADR-0004](0004-account-policies-overdraft-model.md) modeled per-account
balance rules as a closed `AccountPolicy` enum with five variants
(`NoOverdraft`, `CappedOverdraft { floor }`, `UncappedOverdraft`,
`SystemAccount`, `ExternalAccount`). In practice only one distinction earned
its keep: may this account's balance go negative, or not? The capped floor, the
`System`/`External` labels, and the `Uncapped` variant all resolved to the same
runtime behavior (overdraft allowed, no floor), while adding an enum to
serialize, a SQL column, resolve/validate match arms, and a dashboard DTO.

We want a single, legible knob for that one question.

## Decision Drivers

* **One real distinction**: overdraft allowed vs. forbidden is the only balance
  rule the domain actually enforces once the floor is dropped.
* **Fewer moving parts**: a flag rides the existing `AccountFlags` round-trip
  and needs no separate enum, column, or DTO.
* **Legible vocabulary**: `debit_must_not_exceed_credit` names the invariant
  directly.
* **Safe default**: deposits, withdrawals, and system/boundary accounts all need
  to hold the negative side of value, so the default must permit overdraft.

## Considered Options

#### Option 1: Keep the `AccountPolicy` enum (ADR-0004)

**Cons:**

* Bad, because four of five variants are behaviorally identical once the floor
  is gone.
* Bad, because it carries a serialized enum, a SQL column, resolve/validate
  matches, and a dashboard DTO for a single boolean's worth of information.

#### Option 2: Keep a bounded floor (`CappedOverdraft`)

**Cons:**

* Bad, because the floor is only best-effort under concurrency (see
  [ADR-0003](0003-dumb-storage-saga-recovery.md)), so it never was a hard
  guarantee.
* Bad, because a credit-line limit, when actually needed, is better enforced by
  the application above the ledger than by a soft ledger-level bound.

#### Option 3: A single `AccountFlags::DEBIT_MUST_NOT_EXCEED_CREDIT` bit

The account either carries the flag (balance may not go negative, no negative
posting allowed) or does not (overdraft allowed without bound; a shortfall
becomes a negative offset posting; the transfer records as long as it conserves
value per asset). Overdraft is the default.

**Pros:**

* Good, because it keeps the one distinction that matters and drops the rest.
* Good, because it reuses the `AccountFlags` bitfield and its storage round-trip
  (a system-range bit), so there is no new column beyond dropping `policy`.
* Good, because `Account::debit_must_not_exceed_credit(id)` names the invariant.

**Cons:**

* Bad, because the capped credit-line floor is no longer expressible in the
  ledger (moved to the application, if needed).
* Bad, because the `System`/`External` intent labels are gone; a boundary
  account is now just an ordinary overdraft-permitting account.

## Decision Outcome

Chosen option: **Option 3, a single `DEBIT_MUST_NOT_EXCEED_CREDIT` flag**, with
overdraft allowed by default. Validation forbids a negative posting, and rejects
a negative projected balance, only for accounts carrying the flag. Resolution
covers a shortfall with a negative offset posting only for accounts that permit
overdraft. The `policy` column is dropped from the SQL schema (migration
`006_drop_policy.sql`).

### Positive Consequences

* One legible knob per account; the ledger enforces exactly one balance rule.
* Smaller surface: no `AccountPolicy` enum, no policy column, no policy DTO.

### Negative Consequences

* No ledger-enforced capped floor (credit lines are an application concern).
* The mirror constraint (a balance ceiling, where credits may not exceed debits)
  is still not modeled; balance rules remain floor-only, now with a single floor
  of zero.
* Account intent (customer vs. boundary vs. system) is no longer readable from a
  type; it must be inferred from the flag plus context.
* A former `SystemAccount`/`ExternalAccount` debited past its available postings
  now absorbs the shortfall as a negative offset posting instead of failing with
  `InsufficientFunds`. This is moot for deposits (they net to zero on the
  boundary account) but is a behavior change for a direct over-debit.
* Removing `policy` from the `Account` canonical preimage bumps
  `CANONICAL_VERSION` (4 → 5), changing account snapshot hashes. Persisted
  transfers keep their original `EnvelopeId`s and stay self-consistent; a saga
  in flight across the upgrade re-validates on recovery and aborts cleanly (or
  rolls forward) rather than corrupting state, per ADR-0003.

## Links

* Supersedes [ADR-0004](0004-account-policies-overdraft-model.md).
* Builds on [ADR-0001](0001-modified-utxo-signed-postings.md) (signed postings).
* Floor-under-concurrency background: [ADR-0003](0003-dumb-storage-saga-recovery.md).
* Background: [accounts.md](../accounts.md), [accounting-mapping.md](../accounting-mapping.md).
