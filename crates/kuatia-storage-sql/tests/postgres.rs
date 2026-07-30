#![allow(missing_docs)]
#![cfg(feature = "test-postgres")]

//! PostgreSQL conformance run.
//!
//! The same `store_tests!` suite the SQLite backend passes, driven against a
//! real PostgreSQL instance so the Postgres-only code paths (the `FOR UPDATE`
//! row locks behind `lock_clause`, the `ON CONFLICT` upserts) are actually
//! exercised. Point `DATABASE_URL` at a Postgres database to run it; the CI
//! `Test (PostgreSQL)` job supplies one.
//!
//! Unlike `sqlite::memory:`, where every pool is its own fresh database, all
//! tests here share one Postgres database and the conformance tests reuse fixed
//! ids. Each store therefore gets its own uniquely-named schema, and the pool is
//! pinned to a single connection so the session `search_path` set below persists
//! for the store's whole lifetime.

use std::sync::atomic::{AtomicU64, Ordering};

use kuatia_storage_sql::SqlStore;
use sqlx::{Any, Pool};

static SCHEMA_SEQ: AtomicU64 = AtomicU64::new(0);

async fn new_store() -> SqlStore {
    sqlx::any::install_default_drivers();
    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must point at a PostgreSQL instance for this suite");

    // One connection per store so the session-level `search_path` set below
    // survives across every query the store issues.
    let pool: Pool<Any> = sqlx::any::AnyPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();

    // Isolate each store in its own schema: the conformance tests reuse fixed
    // ids, so a shared schema would collide across tests.
    let n = SCHEMA_SEQ.fetch_add(1, Ordering::Relaxed);
    let schema = format!("conformance_{n}");
    for stmt in [
        format!("DROP SCHEMA IF EXISTS {schema} CASCADE"),
        format!("CREATE SCHEMA {schema}"),
        format!("SET search_path TO {schema}"),
    ] {
        sqlx::query(&stmt).execute(&pool).await.unwrap();
    }

    let store = SqlStore::new(pool);
    store.migrate().await.unwrap();
    store
}

kuatia_storage::store_tests!(new_store);
