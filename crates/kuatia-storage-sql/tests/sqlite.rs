#![allow(missing_docs)]
#![cfg(feature = "sqlite")]

use kuatia_storage::store::{AccountStore, PostingStore};
use kuatia_storage_sql::SqlStore;
use kuatia_types::*;
use sqlx::{Any, Pool, Row};

async fn new_pool() -> Pool<Any> {
    sqlx::any::install_default_drivers();
    sqlx::any::AnyPoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap()
}

async fn new_store() -> SqlStore {
    let store = SqlStore::new(new_pool().await);
    store.migrate().await.unwrap();
    store
}

kuatia_storage::store_tests!(new_store);

/// The point of the schema: no column holds opaque binary. A content-addressed
/// id is stored as lower-case hex text, and a structured payload as readable
/// JSON text, so a row can be audited in a plain SQL client.
#[tokio::test]
async fn columns_store_hex_ids_and_json_text() {
    let pool = new_pool().await;
    let store = SqlStore::new(pool.clone());
    store.migrate().await.unwrap();

    let account = Account {
        id: AccountId::new(1),
        version: 1,
        policy: AccountPolicy::NoOverdraft,
        flags: AccountFlags::empty(),
        book: BookId(0),
        user_data: UserData::default(),
        metadata: std::collections::BTreeMap::new(),
    };
    store.create_account(account).await.unwrap();

    let tid = EnvelopeId([0xab; 32]);
    let posting = Posting::new(
        PostingId {
            transfer: tid,
            index: 0,
        },
        AccountId::new(1),
        AssetId::new(1),
        Cent::from(100),
    );
    store.insert_postings(&[posting]).await.unwrap();

    // The 32-byte transfer id is stored as its 64-char lower-case hex form.
    let row = sqlx::query("SELECT transfer_id FROM postings")
        .fetch_one(&pool)
        .await
        .unwrap();
    let transfer_id: String = row.try_get("transfer_id").unwrap();
    assert_eq!(transfer_id, "ab".repeat(32));

    // The account payload is readable JSON text, not a blob.
    let row = sqlx::query("SELECT user_data FROM accounts")
        .fetch_one(&pool)
        .await
        .unwrap();
    let user_data: String = row.try_get("user_data").unwrap();
    assert!(
        serde_json::from_str::<serde_json::Value>(&user_data).is_ok(),
        "user_data should be JSON text, got: {user_data}"
    );
}

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
