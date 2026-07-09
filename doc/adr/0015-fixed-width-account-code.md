# Shorten the account code to a fixed 20 characters

* Status: accepted
* Authors: Cesar Rodas
* Date: 2026-07-09
* Targeted modules: `kuatia-types` (`AccountId` string form), `kuatia`
  (`inflight`)
* Associated tickets/PRs: N/A

## Context and Problem Statement

ADR-0012 gave `AccountId` an IBAN-style string form: two leading mod-97 check
digits and a 26-character base-36 body, 28 characters in total. The body encoded
both `i64` legs at full width (a 128-bit Feistel permutation, then 13 base-36
characters per 64-bit half). 28 characters is long to read, transcribe, or speak,
and it does not group evenly, so the presentation form was more awkward than an
IBAN or a card number.

The length is driven by encoding two full 64-bit values. Can the code be
materially shorter while keeping the checksum, the obfuscation, and round-trip
parsing?

## Decision Drivers

* **Shorter, evenly grouped**: the human-facing code should be short and group
  cleanly (an IBAN/card-number feel), not an odd-length 28.
* **Keep the checksum**: a mistyped id must still be rejected before it reaches
  the store.
* **Keep the obfuscation**: sequential ids must not produce visibly related
  codes, and a base account and its subaccount must not share a prefix.
* **No storage or content-hash impact**: the change is presentation-only;
  `ToBytes`, serde, the SQL schema, and every content hash keep the two full
  `i64` legs, so there is no migration.

## Considered Options

#### Option 1: Fixed 20 characters, an 18-char body plus two trailing check digits (chosen)

Pack the base id (63 bits, since a snowflake never sets the sign bit) and the
subaccount (30 bits) into one 93-bit value, permute it, and base-36 encode it in
18 characters, then append two mod-97 check digits. `36^18 > 2^93`, so 18
characters always fit. Total 20 characters, five groups of four.

**Pros:**

* Good, because 20 characters group evenly into five blocks of four, reading like
  an IBAN or a card number.
* Good, because it keeps the mod-97 checksum and the keyed obfuscation, so
  mistyped ids are still rejected and codes still look unrelated.
* Good, because storage and content hashes are untouched (still two full `i64`
  legs), so there is no migration, exactly as in ADR-0012.

**Cons:**

* Bad, because the subaccount now encodes only 30 bits, so a subaccount id must
  fit in that range to round-trip through the string form. Hash-derived inflight
  subaccounts (ADR-0014) must be masked to 30 bits, which raises their collision
  domain (see Negative Consequences).
* Bad, because the string form is now lossy for out-of-range legs: an `id`
  beyond 63 bits or a `sub` beyond 30 bits truncates rather than round-trips. All
  real ids (snowflake `id`, masked inflight `sub`, small explicit buckets) are in
  range.

#### Option 2: Keep 28 characters

Leave the ADR-0012 form unchanged.

**Pros:**

* Good, because it round-trips the entire `(i64, i64)` space and needs no change.

**Cons:**

* Bad, because 28 characters is long and does not group evenly, which was the
  complaint.

#### Option 3: Variable-length (main account short, subaccount long)

Encode only the id leg when `sub == 0` (about 15 characters) and both legs
otherwise (about 21).

**Pros:**

* Good, because a main account, the common case, becomes very short with no range
  loss on either leg.

**Cons:**

* Bad, because codes then have two different lengths, which reads less like a
  uniform account number and complicates any fixed-width UI or validation.
* Bad, because 15 is not a clean multiple of four, so the grouping is uneven.

## Decision Outcome

Chosen option: **Option 1, a fixed 20-character code**, because it is short, groups
into a uniform five-by-four, keeps the checksum and the obfuscation, and stays
presentation-only with no migration. The only real cost is capping the subaccount
at 30 bits, which is ample for explicit buckets and acceptable for the
hash-derived inflight subaccounts.

### The encoding

* Constants `ID_BITS = 63` and `SUB_BITS = 30` (93 bits packed) are public on
  `kuatia-types`. `pack` places the low `ID_BITS` of `id` above the low
  `SUB_BITS` of `sub`; `unpack` inverts it.
* Obfuscation is a keyed format-preserving permutation over the 93-bit domain: a
  Feistel network over two 47-bit halves (a 94-bit block) restricted to the
  domain by cycle-walking (re-encrypt while the result exceeds `2^93 - 1`). Since
  the domain is half of `2^94`, this averages about two iterations and is a
  bijection on exactly the packable values. The seed is still the global
  `set_id_seed` key.
* The body is 18 base-36 characters of the permuted value; the two trailing check
  digits are the same ISO 7064 mod-97 scheme as ADR-0012, moved to the end so the
  code is body-then-check. `FromStr` strips spaces and dashes, upper-cases,
  requires 20 characters, validates the checksum, rejects a decoded body outside
  the 93-bit domain, then inverts the permutation and unpacks.
* Under the default seed, `AccountId { id: 5, sub: 7 }` renders
  `FK9RA6QALU15JZ7DZM81` (grouped `FK9R A6QA LU15 JZ7D ZM81`).

### Inflight subaccounts

The per-destination inflight hold subaccount (ADR-0014) was a 63-bit truncation
of a trade hash. It is now masked to the low `SUB_BITS` so every hold has an
encodable code. It stays deterministic and trade-specific.

### Positive Consequences

* The human-facing code drops from 28 to 20 characters and groups evenly.
* No storage, serde, or content-hash change; no migration. Debug output is
  unchanged (`id` / `id.sub`).

### Negative Consequences

* The subaccount space is 30 bits (~1.07 billion) instead of a full `i64`.
  Explicit buckets are unaffected; hash-derived inflight subaccounts collide
  sooner: at 30 bits the birthday bound is roughly a 1% chance around 4,600
  concurrent inflight trades to a single destination. Deployments with very high
  concurrent inflight fan-in to one destination should account for this.
* The string form is lossy for legs outside the encodable ranges; such values
  still hash, persist, and compare correctly but do not round-trip through the
  code. No real id source produces out-of-range legs.

## Links

* Supersedes the IBAN-style account code section of
  [ADR-0012](0012-subaccounts.md); the rest of ADR-0012 (the identity model,
  reads, balances) stands.
* Constrains the inflight hold subaccount of
  [ADR-0014](0014-inflight-holds-via-holding-accounts.md).
