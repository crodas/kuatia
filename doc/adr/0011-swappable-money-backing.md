# Swappable integer backing for monetary amounts, default i64

* Status: accepted
* Authors: Cesar Rodas
* Date: 2026-06-30
* Targeted modules: new `kuatia-money` crate (`Cent`, `Amount`,
  `CentBacking`), `kuatia-types` (re-export, `ToBytes`),
  `kuatia-storage-sql` (value column)
* Associated tickets/PRs: N/A

## Context and Problem Statement

ADR-0009 fixed monetary amounts as `i64` minor units and noted that
widening to a larger integer "is a contained newtype change if a real
asset ever needs it." That contained change is now wanted: a runtime
that defaults to `i64` for the common case but can be compiled with
`i128` for assets whose precision and supply exceed `i64`'s ~±9.2×10¹⁸
ceiling. At the same time the concrete width should stop leaking:
`Cent::value() -> i64`, the 8-byte hash encoding, and the `BIGINT`
column all hard-code the backing. How do we make the backing a
swappable, hidden detail without threading a type parameter through
every posting, movement and store method?

## Decision Drivers

* **Swappable, single choice per build**: i64 by default, i128 by a
  compile-time switch; one money width per binary (a ledger does not
  need two at once).
* **Hidden width**: no public signature, serialized form, or DB column
  should reveal whether the backing is 64- or 128-bit.
* **Minimal churn**: postings, movements, validation, the store traits
  and the saga must not gain a generic parameter; "clarity over
  cleverness."
* **Stable content addresses**: amounts feed the double-SHA256
  `EnvelopeId`/`PostingId`; swapping the width must not silently rehash
  the ledger.
* **Backend reality**: Postgres and SQLite have no native 128-bit
  integer; sqlx cannot bind/get `i128` for them, so the persisted form
  must not be a native wide integer.
* **Exact, safe math**: keep checked add/sub/neg/sum with overflow as an
  error (ADR-0009).

## Considered Options

#### Option 1: Generic `Cent<B: CentBacking>`

Parameterize the type over its backing and let callers pick.

**Pros:**

* Good, because multiple widths could coexist and the choice is explicit
  at the type level.

**Cons:**

* Bad, because the parameter propagates into `Posting`, `NewPosting`,
  `Movement`, `Envelope`, `Account` (via `CappedOverdraft`), every
  builder, all of `kuatia-core`, the `Store` trait family and the saga.
  That is large churn for no domain benefit, since a build uses exactly
  one width.
* Bad, because `Hash`/`Ord`/serde/content-addressing all become generic,
  the opposite of "clarity over cleverness."

#### Option 2: Non-generic `Cent(Backing)` + `CentBacking` trait + cargo-feature selector

`Cent` stays a concrete newtype over a `Backing` type alias. A
`CentBacking` trait carries the arithmetic, canonical-byte and string
primitives, with impls for `i64` and `i128`. A cargo feature flips `type
Backing` between them. The width never appears in a public signature:
reads go through `to_string()`/parse and through a fixed-width canonical
encoding; `value() -> i64` is removed.

**Pros:**

* Good, because swapping is "write the other impl, flip a feature":
  `impl CentBacking for i128` plus `--features kuatia-money/i128`, with
  no change to any downstream crate.
* Good, because nothing downstream gains a type parameter; `Posting`, the
  store traits and the saga are untouched.
* Good, because the public surface (`to_string`, parse, checked math,
  `Ord`, fixed-width canonical bytes) names no concrete integer type, so
  the width is hidden.

**Cons:**

* Bad, because the backing is a workspace-global compile-time choice
  (cargo feature unification), not a per-value one. That is acceptable,
  and in fact the intent.
* Bad, because the `value` column moving to text loses SQL-side numeric
  ordering on amounts. That is already irrelevant, since all arithmetic
  is in Rust and no query sorts or ranges on the amount.

#### Option 3: Keep `i64`, just widen the column to `NUMERIC`

Leave the Rust type as `i64` and only make storage wider.

**Pros:**

* Good, because it is the smallest change.

**Cons:**

* Bad, because it does not actually let the runtime compute in `i128`;
  the in-memory ceiling is still `i64`. It solves none of the request.

## Decision Outcome

Chosen option: **Option 2, a non-generic `Cent(Backing)` newtype with a
`CentBacking` trait and a cargo-feature selector**, in a new
`kuatia-money` crate. It delivers the i64↔i128 swap as a second trait
impl behind a feature flag, keeps the default at `i64`, and hides the
width from every public and stored form, all without threading a generic
through the ledger.

Concretely:

* **`kuatia-money` crate** (leaf, `serde` only) holds `Cent`,
  `OverflowError`, `Amount`, `ParseAmountError`, the `CentBacking` trait,
  `impl CentBacking for i64`/`i128`, and the `Backing` alias.
  `kuatia-types` depends on it and re-exports (`pub use
  kuatia_money::{Cent, Amount, …}`), so every existing
  `kuatia_types::Cent` import keeps compiling.
* **Selector:** `#[cfg(not(feature = "i128"))] pub type Backing = i64;` /
  `#[cfg(feature = "i128")] pub type Backing = i128;`. Default backing is
  `i64`.
* **Hidden width:** `value() -> i64` is removed. The public surface is
  `to_string()` (minor-unit string), `FromStr`, the
  `From<i32/u32/u8/i8/i64>` literal constructors, checked math,
  predicates and `Ord`. Serde for `Cent` is hand-written to
  (de)serialize the **string** form, so no serialized form reveals the
  width.
* **Canonical bytes for hashing are fixed at 16 bytes** (sign-extended
  big-endian), independent of the backing, so an i64 amount and the same
  i128 amount hash identically and swapping the backing does not change
  any `EnvelopeId`/`PostingId`. `CANONICAL_VERSION` is bumped 1→2 to mark
  the new encoding.
* **Storage serializes to string:** the `value` column becomes `TEXT`;
  the codec binds `cent.to_string()` and reads it back with `FromStr`.
  This both hides the width and avoids native 128-bit integers, which
  Postgres/SQLite lack.

### Positive Consequences

* The default build is behaviorally identical to ADR-0009's `i64`
  ledger; switching to `i128` is one cargo feature and needs no source
  edits beyond the (already-written) second trait impl.
* No public signature, serialized blob, or column type names the backing
  integer; the width is an internal detail.
* Content addresses are stable across the swap, because the hash preimage
  width is fixed.
* All arithmetic stays exact and checked; overflow remains an
  `OverflowError`.

### Negative Consequences

* The backing is chosen workspace-wide at compile time, not per value
  (cargo feature unification).
* Amounts are stored as text, so the database cannot order or
  range-filter on the amount. This is a non-issue here, since amount
  math is Rust-only and no query depends on SQL ordering of the value.
* This supersedes ADR-0009's "persisted amount is a plain `BIGINT`"
  consequence; the persisted form is now text. Per project convention
  there are no migrations pre-release: `001_init.sql` is edited in place
  and the database recreated.

## Links

* Refines [ADR-0009](0009-monetary-representation-integer-minor-units.md)
  (keeps integer minor units; makes the width swappable and hidden, and
  changes the persisted form from `BIGINT` to text).
* Feeds the content-addressed id of
  [ADR-0001](0001-modified-utxo-signed-postings.md) /
  [ADR-0005](0005-intent-api-movements-vs-envelopes.md); fixed-width
  canonical bytes keep ids stable.
* Floor checks on exact integers:
  [ADR-0004](0004-account-policies-overdraft-model.md).
* Background: `crates/kuatia-money/src/lib.rs` (new),
  `crates/kuatia-types/src/lib.rs`,
  `crates/kuatia-storage-sql/src/lib.rs`.
