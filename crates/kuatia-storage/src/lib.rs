//! Storage abstraction for the ledger.
//!
//! Provides the [`Store`](store::Store) trait (composed of seven sub-traits),
//! an in-memory implementation, and a conformance test suite macro.

pub mod error;
pub mod events;
pub mod mem_store;
pub mod query;
pub mod store;
pub mod store_tests;
