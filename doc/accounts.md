# Accounts

## Overview

An account is a versioned entity that owns postings. Balance is never
stored: it is always computed from postings for a given (account, asset)
pair. The ledger balance sums non-`Inactive` postings (`Active +
PendingInactive`); the available balance sums only `Active` postings
(excluding those reserved for an in-flight transfer). `balance()` returns
the ledger balance.

## Structure

| Field | Type | Description |
|-------|------|-------------|
| `id` | `AccountId(i64)` | Stable identity, assigned at creation |
| `version` | `u64` | Starts at 1, increments on every mutation |
| `policy` | `AccountPolicy` | Balance floor rule (see below) |
| `flags` | `AccountFlags` | Lifecycle flags (`FROZEN`, `CLOSED`) + user-defined (`USER_0` to `USER_7`) |
| `book` | `BookId` | Book this account belongs to |
| `user_data` | `UserData` | Fixed 28 bytes: `u128 + u64 + u32` for external refs |
| `metadata` | `Metadata` | `BTreeMap<String, Vec<u8>>` for free-form data |

## Policies

Each account has a policy that controls what balance constraints apply:

| Policy | Balance floor | Negative postings | CAS guard |
|--------|--------------|-------------------|-----------|
| `NoOverdraft` | `>= 0` | No | No |
| `CappedOverdraft { floor }` | `>= floor` | Yes (down to floor) | Yes |
| `UncappedOverdraft` | None | Yes (unbounded) | No |
| `SystemAccount` | None | Yes | No |
| `ExternalAccount` | None | Yes | No |

An overdraft is represented as a negative posting (an offset position)
assigned to the account to cover a shortfall. When an account's positive
postings are insufficient for a debit, the resolve step consumes them all
and creates a negative posting for the remainder. `NoOverdraft` accounts
forbid this; validation rejects any transfer that would create a negative
posting on a `NoOverdraft` account. `CappedOverdraft`'s floor bounds how
negative the balance may go; `UncappedOverdraft`, `SystemAccount`, and
`ExternalAccount` are unbounded.

`CappedOverdraft`'s floor is re-validated as the last step before finalize
writes (the finalize step re-loads balances and account versions and
re-runs validation just before deactivating). This is the tightest
best-effort: the check-to-write window is one step, not the whole saga. It
is not strictly atomic. A concurrent commit in that last gap can still
breach the floor (write-skew). Double-spend safety is unaffected. The
reservation protocol (an atomic conditional `reserve_postings`) guarantees
a posting cannot be consumed twice. See
[accounting-mapping.md](accounting-mapping.md) and the ADR at
[adr/0003-dumb-storage-saga-recovery.md](adr/0003-dumb-storage-saga-recovery.md).

## Lifecycle

Accounts follow a three-state lifecycle controlled by flags:

```
Created (v1) → Frozen (v2) → Unfrozen (v3) → Closed (v4)
                  ↑               │
                  └───────────────┘
```

| Operation | Precondition | Effect |
|-----------|-------------|--------|
| `freeze(id)` | Not closed | Sets `FROZEN` flag, increments version |
| `unfreeze(id)` | Frozen | Clears `FROZEN` flag, increments version |
| `close(id)` | Zero active postings | Sets `CLOSED` flag, increments version |

- **Frozen** accounts reject all transfers (both debits and credits).
- **Closed** accounts reject all transfers and cannot be reopened.
- Closing requires zero active postings for all assets.

## Append-Only Versioning

Accounts are never modified in place. Each mutation appends a new version:

```
Version 1: { policy: NoOverdraft, flags: ∅ }         ← created
Version 2: { policy: NoOverdraft, flags: FROZEN }     ← frozen
Version 3: { policy: NoOverdraft, flags: ∅ }         ← unfrozen
```

The store enforces `version_new == version_current + 1`, preventing gaps or
overwrites. The full history is queryable via `account_history(id)`.

## Snapshot Pinning

Transfers can carry `AccountSnapshotId` values: pairs of `(AccountId,
snapshot_hash)` recording which account version the transfer was validated
against.

During validation, if snapshots are present, the current account state is
hashed and compared. A mismatch produces `AccountVersionMismatch`,
preventing TOCTOU races where an account is mutated between load and apply.

The saga `commit()` path auto-populates snapshots when none are provided.

## Balance Computation

Balance for an (account, asset) pair is computed as:

```
balance(account, asset) = sum(p.value for p in postings
                              where p.owner == account
                              and   p.asset == asset
                              and   p.status != Inactive)
```

There is no stored balance field. This eliminates drift between the balance
and the underlying postings.

## Account Types in Practice

### Regular user accounts (`NoOverdraft`)

Hold positive postings only. Cannot go negative. Used for end-user wallets,
merchant accounts, etc.

### System accounts (`SystemAccount`)

Operational accounts representing issuance, sink, revenue, COGS, fees, or
internal balancing. Can hold negative postings (offset positions, e.g. a
liability when the account is the deposit counterparty). Used as the
counterparty in deposits: the system account takes on a negative balance to
offset the value credited elsewhere.

### External accounts (`ExternalAccount`)

Boundary accounts representing the outside world (banks, payment
processors). They represent value entering and leaving the ledger boundary,
and like system accounts they can hold negative postings (offset positions).

### Credit accounts (`CappedOverdraft`)

Accounts with a negative floor (e.g. credit lines). The floor is the maximum
allowed overdraft. When the account's positive postings are insufficient for
a debit, a negative posting is created to cover the shortfall, down to the
floor. The floor is re-validated as the last step before finalize and is
best-effort under concurrency (see above).
