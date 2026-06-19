# Monetary amounts as integer minor units, scale outside the value

* Status: accepted (refined by [ADR-0011](0011-swappable-money-backing.md))
* Authors: Cesar Rodas
* Date: 2026-06-29
* Targeted modules: `kuatia-types` (`Cent`, `Amount`, `AssetId`), `kuatia-core`
* Associated tickets/PRs: N/A

## Context and Problem Statement

Every posting carries an amount of one asset (ADR-0001), and the core invariant
is per-asset conservation: `sum(consumed) == sum(created)` checked on every
commit. That sum must be exact. A ledger that rounds is not a ledger. So the
monetary type has to be exact under addition, subtraction and negation, deny
silent overflow, hash deterministically for content-addressing, and still
represent assets with different decimal precision (USD has 2, a token might
have 8, JPY has 0). What type represents a stored monetary amount, and where
does an asset's decimal scale live?

## Decision Drivers

* **Exactness**: addition/subtraction/negation must be exact; conservation and
  floor checks cannot tolerate rounding error.
* **No silent overflow**: an amount that overflows must surface as an error,
  not wrap, since wrapping would forge or destroy value.
* **Deterministic bytes**: amounts are hashed into the content-addressed
  `EnvelopeId`, so the representation must serialize identically everywhere
  (no locale, no float bit-pattern ambiguity).
* **Multi-asset precision**: different assets have different decimal places,
  but the stored value should stay one uniform type.
* **No DB arithmetic**: all sums happen in Rust with checked operations; the
  store never computes on amounts (CLAUDE.md, ADR-0003).

## Considered Options

#### Option 1: Floating point (`f64`)

Store amounts as binary floating point.

**Pros:**

* Good, because it is built-in and handles fractional values without scaling.

**Cons:**

* Bad, because `f64` cannot represent most decimal fractions exactly
  (`0.1 + 0.2`), so conservation sums drift and the `Σ consumed == Σ created`
  check becomes approximate, which disqualifies it for a ledger.
* Bad, because float bit-patterns and rounding modes make hashing and
  cross-platform determinism fragile.

#### Option 2: A decimal / big-integer library (`rust_decimal`, `i128`, rationals)

Use a wider or decimal-aware numeric type that carries its own scale.

**Pros:**

* Good, because it offers larger range (`i128`) or scale-aware decimal math,
  and can embed precision in the value itself.

**Cons:**

* Bad, because it pulls a non-trivial dependency (or wider columns) into the
  most pervasive type, complicating storage layout and serialization for a
  need the domain does not yet have.
* Bad, because a value that carries its own scale invites mixing scales
  silently and still must be pinned down for deterministic hashing.
* Bad, because `i64` minor units already cover ~±9.2×10¹⁸ of the smallest
  unit, ample for realistic balances, so the extra range is mostly unused
  weight.

#### Option 3: `Cent`, an `i64` newtype of minor units, scale held outside

`Cent(i64)` is a private-field newtype holding an amount in the asset's
**smallest unit** (cents, satoshis, …). It exposes only checked arithmetic
(`checked_add`/`checked_sub`/`checked_neg`/`checked_sum` → `OverflowError`),
serializes as big-endian bytes (`ToBytes`) for hashing, and is `Ord`/`Hash`.
Decimal **scale is not stored on the value or the asset**: `AssetId(u32)` is an
opaque identifier, and `Amount { decimals: u8 }` is a presentation-only
parser/formatter (string ⇄ `Cent`) that is *never persisted*.

**Pros:**

* Good, because integer minor units are exact under +, −, negation, so
  conservation and the overdraft floor are checked on exact integers.
* Good, because the private field forbids confusing a monetary amount with a
  plain `i64`, and the only arithmetic offered is checked, so overflow is a
  `Result`, never a wrap.
* Good, because big-endian bytes give one deterministic, locale-free
  representation for content-addressing across backends and platforms.
* Good, because keeping scale out of the stored value means the persisted
  ledger is pure integers, with no per-row precision field to migrate, and
  presentation concerns never touch the conservation math.
* Good, because `i64` minor units are compact (fixed 8 bytes) and index/sum
  cheaply in Rust.

**Cons:**

* Bad, because scale is a convention the *application* must apply
  consistently. A `Cent` is meaningless without knowing its asset's decimals,
  and nothing in the type stops formatting a satoshi amount with 2 decimals.
* Bad, because `i64` caps a single amount/sum at ~±9.2×10¹⁸ minor units; an
  asset with very high precision and very large supply could in principle
  exceed it (surfaced as `OverflowError`, not a wrap, but a hard ceiling
  nonetheless).
* Bad, because fractional or proportional operations (interest, fees, FX rates)
  are not closed over `Cent` and must be defined explicitly with an agreed
  rounding policy when they are introduced.

## Decision Outcome

Chosen option: **Option 3, `Cent`, an `i64` newtype of minor units with scale
held outside the value**, because it is the only option that makes the
conservation sum *exact* and *deterministic* while keeping the stored ledger
pure integers and overflow an explicit error. Scale lives in `Amount`
(presentation) rather than on `Cent` or `AssetId` (storage), so precision is an
edge concern at the application boundary and never leaks into the invariant
math or the database schema. `i64` is chosen over `i128`/decimal because its
range is more than adequate and its fixed width keeps the most pervasive type
small and trivially serializable; widening later is a contained newtype change
if a real asset ever needs it.

### Positive Consequences

* All monetary arithmetic is checked and exact; `validate_and_plan`'s
  conservation and floor checks operate on integers that cannot silently round
  or wrap.
* `Cent`'s big-endian `ToBytes` feeds the content-addressed `EnvelopeId`
  deterministically; the same transfer hashes identically on every backend.
* The persisted amount is a plain `BIGINT`/`i64` with no precision metadata,
  consistent with "no DB arithmetic" and with Rust-owned identity (ADR-0003).
* `Amount` cleanly separates human input/output (with per-asset decimals)
  from the stored, scale-free value.

### Negative Consequences

* Asset scale is an application-level convention; the type system does not
  bind a `Cent` to its asset's decimal places, so callers must format/parse
  with the right `Amount` for the asset.
* `i64` is a hard magnitude ceiling per amount and per sum (overflow →
  `Result`, never a wrap); a future high-precision/high-supply asset may force
  widening the newtype.
* Multiplicative/fractional operations (fees, interest, FX) need an explicit
  rounding policy when added; they are deliberately not part of `Cent` today.

## Links

* Makes the conservation invariant of
  [ADR-0001](0001-modified-utxo-signed-postings.md) exact, and feeds the
  content-addressed id used for idempotency
  (ADR-0005 / future "content-addressed transfer ids").
* Floor checks that rely on exact integers:
  [ADR-0004](0004-account-policies-overdraft-model.md).
* Background: `crates/kuatia-types/src/lib.rs` (`Cent`, `Amount`, `AssetId`),
  [glossary.md](../glossary.md).
