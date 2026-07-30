# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.0] - 2026-07-30

### Added

- Subaccounts: `AccountId` gains a subaccount leg (`{ id, sub }`), so several
  independent balances live under one owner, each individually addressable,
  drained, and closed. Aggregate reads take a base id plus an optional
  subaccount filter; exact operations take the full `AccountId`; balances are
  never summed across subaccounts. (ADR-0012)
- Inflight holds: authorize, confirm, and void value through per-destination
  holding subaccounts, so an authorization reserves funds without committing
  them and a void returns them. (ADR-0014)
- IBAN-style account identifiers: `AccountId` has a fixed 20-character
  `Display`/`FromStr` form (a base-36 body plus two mod-97 check digits) for
  presentation and routing, while storage keeps the two integer legs. (ADR-0015)
- Cached balance projection: an append-only balance snapshot, refreshed lazily
  off the read path once enough activity accrues, shortens the everyday balance
  read; the authoritative live-posting sum stays the source of truth. (ADR-0019)
- `SagaStore::get_saga`, a keyed read for a single write-ahead record, so
  recovery no longer scans every pending record.
- Continuous integration now runs the storage conformance suite against
  PostgreSQL (exercising the Postgres-only `FOR UPDATE` and `ON CONFLICT` paths)
  and runs the test suite with the i128 Cent backing.

### Changed

- Storage write contract: every `Store` write method returns the number of
  affected rows and makes no domain decision. The saga interprets counts and
  owns idempotency and compensation, with a phase-tracked write-ahead record and
  roll-forward crash recovery in place of a monolithic commit transaction.
  (ADR-0003) *(breaking for `Store` implementors)*
- Account balance constraint collapsed to the single
  `DEBIT_MUST_NOT_EXCEED_CREDIT` flag; overdraft is allowed by default. (ADR-0018)
- The SQL schema separates append-only value tables from disposable
  active/reserved index tables. (ADR-0016, ADR-0017)
- Intent resolution is pure and preserves typed errors across the saga.
- Internal structure: the `Ledger` was split into concern-named submodules; the
  commit-safety invariant is documented and its double-spend guard named
  (ADR-0021); the write-ahead recovery record was concentrated into one
  `pending` module; balance computation was consolidated into one module.

### Removed

- The vestigial `UserData` type and the orphaned withdraw saga step. *(breaking)*

### Documentation

- Document that the ledger supports journaling: a committed transfer is a
  journal entry, a transfer with multiple movements is a compound journal
  entry, and the transfer log is the accounting journal. Adds
  `doc/journaling.md` and ADR-0013, and notes it in the README overview.

## [0.2.0] - 2026-07-01

### Added

- `kuatia-dashboard`: a server-rendered dashboard and REST API for observing a
  Kuatia ledger over HTTP. Browse accounts, postings, transfers, and the event
  log; inspect per-account balances computed in Rust.

### Changed

- The SQL backend stores binary identifiers as hex text and structured columns
  as JSON text instead of opaque blobs, so a ledger can be audited directly
  with SQL tooling. The SQLite and PostgreSQL schemas were unified into a
  single `001_init.sql`.

## [0.1.0] - 2026-06-30

Initial release.

### Added

- Append-only, multi-asset, UTXO-style ledger. Value is tracked as signed
  postings with no mutable balance fields. Transfers atomically consume and
  create postings, enforcing per-asset conservation (`sum(consumed) ==
  sum(created)`).
- Intent API: movements (`pay`, `deposit`, `withdraw`) resolved into concrete
  postings by the core, committed through a single `reserve → finalize` saga
  with automatic retry and LIFO compensation.
- Content-addressed transfers (double-SHA-256 of canonical bytes) for
  idempotency and tamper evidence.
- Account policies: `NoOverdraft`, `CappedOverdraft`, `UncappedOverdraft`,
  `SystemAccount`, `ExternalAccount`, with append-only versioned accounts and
  snapshot pinning to guard against TOCTOU races.
- Durable crash recovery via a phase-tracked write-ahead saga record and
  `Ledger::recover()` (roll-forward, not rollback).
- Dumb-storage `Store` trait split into focused sub-traits, with an in-memory
  backend and a SQLite/PostgreSQL backend (`kuatia-storage-sql`).
- A conformance test suite (`store_tests!`) applied to every storage backend.
- Snowflake-style `i64` IDs generated in Rust; the database never assigns IDs.
- Compile-time swappable monetary backing (`i64` default, `i128` via the
  `i128` feature).

### Crates

- `kuatia-money` — monetary `Cent` type with swappable integer backing.
- `kuatia-types` — domain types: accounts, postings, transfers, books.
- `kuatia-core` — pure, sans-IO logic: validation, hashing, posting selection.
- `kuatia-storage` — storage abstraction and conformance suite.
- `kuatia-storage-sql` — SQLite/PostgreSQL backend.
- `kuatia` — async `Ledger` resource and saga commit pipeline.

[0.3.0]: https://github.com/crodas/kuatia/releases/tag/v0.3.0
[0.2.0]: https://github.com/crodas/kuatia/releases/tag/v0.2.0
[0.1.0]: https://github.com/crodas/kuatia/releases/tag/v0.1.0
