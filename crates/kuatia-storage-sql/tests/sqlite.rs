#![allow(missing_docs)]
#![cfg(feature = "sqlite")]

use kuatia_storage_sql::SqlStore;

async fn new_store() -> SqlStore {
    sqlx::any::install_default_drivers();
    let pool = sqlx::any::AnyPoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    let store = SqlStore::new(pool);
    store.migrate().await.unwrap();
    store
}

kuatia_storage::store_tests!(new_store);

/// migrate() is idempotent: running it repeatedly on the same DB is a no-op.
#[tokio::test]
async fn migrate_is_idempotent() {
    sqlx::any::install_default_drivers();
    let pool = sqlx::any::AnyPoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    let store = SqlStore::new(pool);
    store.migrate().await.unwrap();
    store.migrate().await.unwrap();
    store.migrate().await.unwrap();
}
