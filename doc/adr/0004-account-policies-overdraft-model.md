# Account policies as the negative-posting and floor gate

* Status: accepted
* Authors: Cesar Rodas
* Date: 2026-06-29
* Targeted modules: `kuatia-types` (`AccountPolicy`), `kuatia-core` (`validate.rs`)
* Associated tickets/PRs: N/A

## Context and Problem Statement

ADR-0001 makes value *signable*: a posting may be negative ("offset
position"). But "may be negative" is not a per-account truth. A customer
wallet must never go negative, a credit line may go negative down to a
limit, and a system/boundary account is unbounded by design. Something
must decide, per account, whether a negative posting is allowed and how
far. Where does that rule live, and what shape does it take?

## Decision Drivers

* **Per-account semantics**: overdraft permission and floor differ by
  account kind, not globally.
* **Validation, not storage**: the rule belongs in the pure validator,
  checked on every transfer.
* **Closed, legible taxonomy**: a small set of named intents is easier to
  reason about and audit than free-form flags.
* **Boundary/system accounts**: deposits and withdrawals need an account
  that may be arbitrarily negative (value entering or leaving the ledger)
  without being a "bug."

## Considered Options

#### Option 1: A single `allow_negative: bool` + `floor: Option<Cent>`

Two fields on the account control negativity and bound.

**Pros:**

* Good, because it is minimal and flexible.

**Cons:**

* Bad, because illegal combinations are representable (`allow_negative = false`
  with a non-zero floor) and must be guarded.
* Bad, because intent is implicit. A reader cannot tell a "customer
  wallet" from a "boundary account" from the fields alone.
* Bad, because future kinds (e.g. distinct system vs. external semantics)
  have no natural home.

#### Option 2: Per-asset policy on each account

Policy varies by `(account, asset)`.

**Pros:**

* Good, because it allows an account to be NoOverdraft in one asset and a
  credit line in another.

**Cons:**

* Bad, because it multiplies configuration and validation surface for a
  need the domain rarely has.
* Bad, because it complicates the account model (policy is no longer an
  account property but an account×asset matrix).

#### Option 3: A closed `AccountPolicy` enum per account

`NoOverdraft`, `CappedOverdraft { floor }`, `UncappedOverdraft`,
`SystemAccount`, `ExternalAccount`. Only `NoOverdraft` forbids negative
postings; the other four permit them; `CappedOverdraft` bounds them at
`floor`; the rest are unbounded.

**Pros:**

* Good, because each variant names an intent (customer wallet, credit line, fee
  pool, value boundary), making accounts self-documenting and auditable.
* Good, because illegal states are unrepresentable (a floor only exists on
  `CappedOverdraft`).
* Good, because validation maps cleanly: reject a negative posting on
  `NoOverdraft`; enforce `floor` on `CappedOverdraft`; allow the rest.

**Cons:**

* Bad, because adding a new policy is an enum change (a deliberate,
  reviewed event rather than a config tweak).
* Bad, because per-asset variation is not expressible without modeling it
  separately.

## Decision Outcome

Chosen option: **Option 3, a closed `AccountPolicy` enum per account**,
because it makes account intent explicit and auditable, keeps illegal
states unrepresentable, and maps directly onto the two validation rules
(negative-posting permission and floor). `SystemAccount` and
`ExternalAccount` give deposits and withdrawals a principled home (value
boundaries that may run arbitrarily negative), rather than treating an
unbounded negative as an exception.

### Positive Consequences

* Validation is a small match on the policy: `validate_and_plan` rejects a
  negative posting on `NoOverdraft` and enforces the `CappedOverdraft` floor;
  other policies skip the floor check.
* Accounts document their own risk posture; an audit can read intent from
  the type.

### Negative Consequences

* New account kinds require an enum (and validation) change. This is
  intentional, but not a runtime or config change.
* Per-asset overdraft, if ever needed, must be modeled on top of this
  rather than for free.
* The `CappedOverdraft` floor is only *best-effort* under concurrency, see
  [ADR-0003](0003-dumb-storage-saga-recovery.md).

## Links

* Refines [ADR-0001](0001-modified-utxo-signed-postings.md) (signed postings).
* Floor-under-concurrency tradeoff: [ADR-0003](0003-dumb-storage-saga-recovery.md).
* Background: [accounts.md](../accounts.md).
