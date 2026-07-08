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
    let row = sqlx::query("SELECT metadata FROM accounts")
        .fetch_one(&pool)
        .await
        .unwrap();
    let metadata: String = row.try_get("metadata").unwrap();
    assert!(
        serde_json::from_str::<serde_json::Value>(&metadata).is_ok(),
        "metadata should be JSON text, got: {metadata}"
    );
}

/// The 002 migration adds the subaccount column and a subaccount account/posting
/// round-trips through the schema, kept distinct from the main account.
#[tokio::test]
async fn subaccount_columns_round_trip() {
    let store = new_store().await;

    let sub = AccountId::with_sub(1, 7);
    store
        .create_account(Account::new(sub, AccountPolicy::NoOverdraft))
        .await
        .unwrap();
    // The main account (1, 0) is a separate record.
    store
        .create_account(Account::new(AccountId::new(1), AccountPolicy::NoOverdraft))
        .await
        .unwrap();

    let got = store.get_account(&sub).await.unwrap();
    assert_eq!(got.id, sub);

    let posting = Posting::new(
        PostingId {
            transfer: EnvelopeId([0xcd; 32]),
            index: 0,
        },
        sub,
        AssetId::new(1),
        Cent::from(500),
    );
    store.insert_postings(&[posting]).await.unwrap();

    // Filtering by the subaccount returns it; the main account holds nothing.
    let by_sub = store
        .get_postings_by_account(1, Some(7), None, None)
        .await
        .unwrap();
    assert_eq!(by_sub.len(), 1);
    assert_eq!(by_sub[0].owner, sub);
    let by_main = store
        .get_postings_by_account(1, Some(0), None, None)
        .await
        .unwrap();
    assert!(by_main.is_empty());
}

/// A database created under 001 (no subaccount column) upgrades in place: the
/// 002 migration rebuilds the tables and existing rows default to subaccount 0,
/// the main account.
#[tokio::test]
async fn migration_upgrades_existing_rows_to_main_account() {
    let pool = new_pool().await;

    // Pre-002 schema: the tables 002 rebuilds, in their old (no-subaccount) shape.
    sqlx::query(
        "CREATE TABLE accounts (id BIGINT NOT NULL, version BIGINT NOT NULL, policy TEXT NOT NULL, flags INTEGER NOT NULL, book BIGINT NOT NULL, user_data TEXT NOT NULL, metadata TEXT NOT NULL, PRIMARY KEY (id, version))",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TABLE postings (transfer_id TEXT NOT NULL, idx SMALLINT NOT NULL, owner BIGINT NOT NULL, asset INTEGER NOT NULL, value TEXT NOT NULL, status SMALLINT NOT NULL, reservation BIGINT, PRIMARY KEY (transfer_id, idx))",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("CREATE INDEX idx_postings_owner ON postings(owner, asset, status)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "CREATE TABLE transfer_accounts (transfer_id TEXT NOT NULL, account_id BIGINT NOT NULL, PRIMARY KEY (transfer_id, account_id))",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("CREATE INDEX idx_xfer_acct ON transfer_accounts(account_id)")
        .execute(&pool)
        .await
        .unwrap();
    // Legacy 001/002 schema carried a user_data JSON column; the 003 migration
    // drops it. Seed it with the value the old UserData type serialized to.
    let user_data = r#"{"d128":0,"d64":0,"d32":0}"#.to_string();
    let metadata =
        serde_json::to_string(&std::collections::BTreeMap::<String, String>::new()).unwrap();
    sqlx::query(
        "INSERT INTO accounts (id, version, policy, flags, book, user_data, metadata) VALUES (5, 1, '\"NoOverdraft\"', 0, 0, $1, $2)",
    )
    .bind(user_data)
    .bind(metadata)
    .execute(&pool)
    .await
    .unwrap();
    // Record 001 as applied so migrate() only runs 002 against this schema.
    sqlx::query("CREATE TABLE _migrations (name TEXT PRIMARY KEY)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO _migrations (name) VALUES ('001_init')")
        .execute(&pool)
        .await
        .unwrap();

    let store = SqlStore::new(pool);
    store.migrate().await.unwrap();

    // The pre-existing account is now the main account (subaccount 0).
    let got = store.get_account(&AccountId::new(5)).await.unwrap();
    assert_eq!(got.id, AccountId::new(5));
    assert!(got.id.is_main());
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
