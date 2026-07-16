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
        flags: AccountFlags::DEBIT_MUST_NOT_EXCEED_CREDIT,
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
        .create_account(Account::debit_must_not_exceed_credit(sub))
        .await
        .unwrap();
    // The main account (1, 0) is a separate record.
    store
        .create_account(Account::debit_must_not_exceed_credit(AccountId::new(1)))
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
        .get_postings_by_account(1, Some(7), None, PostingFilter::All)
        .await
        .unwrap();
    assert_eq!(by_sub.len(), 1);
    assert_eq!(by_sub[0].owner, sub);
    let by_main = store
        .get_postings_by_account(1, Some(0), None, PostingFilter::All)
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

/// The 004 migration splits lifecycle state out of `postings` into the two
/// id-only index tables: an active (status 0) posting is backfilled into
/// `active_postings`, a reserved (status 1) posting into `reserved_postings`
/// with its token, and the `status` column is dropped from `postings`.
#[tokio::test]
async fn migration_004_backfills_index_tables() {
    let pool = new_pool().await;

    // Pre-004 postings schema (the shape 001->003 leave it): status + reservation.
    sqlx::query(
        "CREATE TABLE postings (transfer_id TEXT NOT NULL, idx SMALLINT NOT NULL, owner BIGINT NOT NULL, subaccount BIGINT NOT NULL DEFAULT 0, asset INTEGER NOT NULL, value TEXT NOT NULL, status SMALLINT NOT NULL, reservation BIGINT, PRIMARY KEY (transfer_id, idx))",
    )
    .execute(&pool)
    .await
    .unwrap();
    // One active (status 0) and one reserved (status 1, reservation 77) posting.
    sqlx::query("INSERT INTO postings (transfer_id, idx, owner, subaccount, asset, value, status, reservation) VALUES ('aa', 0, 1, 0, 1, '100', 0, NULL)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO postings (transfer_id, idx, owner, subaccount, asset, value, status, reservation) VALUES ('bb', 0, 1, 0, 1, '200', 1, 77)")
        .execute(&pool)
        .await
        .unwrap();

    // A post-003 DB also has the accounts table (empty here); migrate() will run
    // 005 after 004, which backfills the account head from it.
    sqlx::query(
        "CREATE TABLE accounts (id BIGINT NOT NULL, subaccount BIGINT NOT NULL DEFAULT 0, version BIGINT NOT NULL, policy TEXT NOT NULL, flags INTEGER NOT NULL, book BIGINT NOT NULL, metadata TEXT NOT NULL, PRIMARY KEY (id, subaccount, version))",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Record 001-003 as applied so migrate() only runs 004 and 005.
    sqlx::query("CREATE TABLE _migrations (name TEXT PRIMARY KEY)")
        .execute(&pool)
        .await
        .unwrap();
    for m in ["001_init", "002_subaccounts", "003_drop_user_data"] {
        sqlx::query("INSERT INTO _migrations (name) VALUES ($1)")
            .bind(m)
            .execute(&pool)
            .await
            .unwrap();
    }

    let store = SqlStore::new(pool.clone());
    store.migrate().await.unwrap();

    // The active posting is now in active_postings, carrying a full row copy.
    let active = sqlx::query("SELECT transfer_id, owner, asset, value FROM active_postings")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(active.len(), 1);
    let a_tid: String = active[0].try_get("transfer_id").unwrap();
    let a_owner: i64 = active[0].try_get("owner").unwrap();
    let a_value: String = active[0].try_get("value").unwrap();
    assert_eq!(a_tid, "aa");
    assert_eq!(a_owner, 1);
    assert_eq!(a_value, "100");

    // The reserved posting is in reserved_postings with its data copy and token.
    let reserved = sqlx::query("SELECT transfer_id, value, reservation FROM reserved_postings")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(reserved.len(), 1);
    let r_tid: String = reserved[0].try_get("transfer_id").unwrap();
    let r_value: String = reserved[0].try_get("value").unwrap();
    let r_res: i64 = reserved[0].try_get("reservation").unwrap();
    assert_eq!(r_tid, "bb");
    assert_eq!(r_value, "200");
    assert_eq!(r_res, 77);

    // Both immutable rows survive; the status column is gone.
    let all = sqlx::query("SELECT transfer_id FROM postings")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(all.len(), 2);
    assert!(
        sqlx::query("SELECT status FROM postings")
            .fetch_all(&pool)
            .await
            .is_err()
    );
}

/// The 005 migration backfills `account_head` with the current (highest)
/// version of each account, so a subsequent read hits one row per account
/// without scanning the version history.
#[tokio::test]
async fn migration_005_backfills_account_head() {
    let pool = new_pool().await;

    // Pre-005 accounts schema (post-004 shape) with a versioned history:
    // account 1 has three versions, account 2 has one.
    sqlx::query(
        "CREATE TABLE accounts (id BIGINT NOT NULL, subaccount BIGINT NOT NULL DEFAULT 0, version BIGINT NOT NULL, policy TEXT NOT NULL, flags INTEGER NOT NULL, book BIGINT NOT NULL, metadata TEXT NOT NULL, PRIMARY KEY (id, subaccount, version))",
    )
    .execute(&pool)
    .await
    .unwrap();
    for (id, version) in [(1, 1), (1, 2), (1, 3), (2, 1)] {
        sqlx::query("INSERT INTO accounts (id, subaccount, version, policy, flags, book, metadata) VALUES ($1, 0, $2, '\"NoOverdraft\"', 0, 0, '{}')")
            .bind(id as i64)
            .bind(version as i64)
            .execute(&pool)
            .await
            .unwrap();
    }

    // Record 001-004 as applied so migrate() only runs 005.
    sqlx::query("CREATE TABLE _migrations (name TEXT PRIMARY KEY)")
        .execute(&pool)
        .await
        .unwrap();
    for m in [
        "001_init",
        "002_subaccounts",
        "003_drop_user_data",
        "004_index_tables",
    ] {
        sqlx::query("INSERT INTO _migrations (name) VALUES ($1)")
            .bind(m)
            .execute(&pool)
            .await
            .unwrap();
    }

    let store = SqlStore::new(pool.clone());
    store.migrate().await.unwrap();

    // One head per account, each pointing at the highest version.
    let mut heads: Vec<(i64, i64)> = sqlx::query("SELECT id, version FROM account_head")
        .fetch_all(&pool)
        .await
        .unwrap()
        .iter()
        .map(|r| (r.try_get("id").unwrap(), r.try_get("version").unwrap()))
        .collect();
    heads.sort();
    assert_eq!(heads, vec![(1, 3), (2, 1)]);

    // The store reads the current version through the head.
    let acct = store.get_account(&AccountId::new(1)).await.unwrap();
    assert_eq!(acct.version, 3);
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

/// The id-batch primitives chunk a large id set so it never exceeds the
/// backend's bind-parameter limit (SQLite caps a statement at 32766 variables).
/// A batch larger than one chunk must reserve, read, and deactivate every id
/// exactly as a small one does.
#[tokio::test]
async fn id_batch_primitives_chunk_large_batches() {
    // Comfortably larger than the internal chunk size so the batch spans
    // multiple statements.
    const N: u16 = 9000;
    let store = new_store().await;

    let tid = EnvelopeId([0xcd; 32]);
    let postings: Vec<Posting> = (0..N)
        .map(|i| {
            Posting::new(
                PostingId {
                    transfer: tid,
                    index: i,
                },
                AccountId::new(1),
                AssetId::new(1),
                Cent::from(1),
            )
        })
        .collect();
    let ids: Vec<PostingId> = postings.iter().map(|p| p.id).collect();

    assert_eq!(
        store.insert_postings(&postings).await.unwrap(),
        u64::from(N)
    );

    // Reserve the whole batch in one call: every id is claimed across chunks.
    let rid = ReservationId::new(1);
    assert_eq!(
        store.reserve_postings(&ids, rid).await.unwrap(),
        u64::from(N)
    );

    // Reads span chunks and return every id.
    let states = store.get_posting_states(&ids).await.unwrap();
    assert_eq!(states.len(), N as usize);
    assert!(states.iter().all(|s| *s == PostingState::Reserved(rid)));
    assert_eq!(store.get_postings(&ids).await.unwrap().len(), N as usize);

    // Deactivating the whole batch spends every id.
    assert_eq!(
        store.deactivate_postings(&ids, Some(rid)).await.unwrap(),
        u64::from(N)
    );
    let after = store.get_posting_states(&ids).await.unwrap();
    assert!(after.iter().all(|s| *s == PostingState::Spent));
}
