# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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

[0.2.0]: https://github.com/crodas/kuatia/releases/tag/v0.2.0
[0.1.0]: https://github.com/crodas/kuatia/releases/tag/v0.1.0
