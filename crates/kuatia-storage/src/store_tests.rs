//! Generic conformance test suite for [`Store`] implementations.
//!
//! Use the [`store_tests!`](crate::store_tests!) macro to generate the full suite for any Store impl.
//!
//! ```text
//! async fn new_store() -> MyStore { MyStore::new() }
//! kuatia_storage::store_tests!(new_store);
//! ```

use std::collections::BTreeMap;

use kuatia_types::*;

use crate::error::StoreError;
use crate::events::{LedgerEvent, LedgerEventKind};
use crate::store::*;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_account(id: i64, flags: AccountFlags) -> Account {
    Account {
        id: AccountId::new(id),
        version: 1,
        flags,
        book: BookId(0),
        metadata: BTreeMap::new(),
    }
}

fn make_posting(
    transfer_hash: [u8; 32],
    index: u16,
    owner: i64,
    asset: u32,
    value: i64,
) -> Posting {
    Posting::new(
        PostingId {
            transfer: EnvelopeId(transfer_hash),
            index,
        },
        AccountId::new(owner),
        AssetId::new(asset),
        Cent::from(value),
    )
}

fn make_posting_sub(
    transfer_hash: [u8; 32],
    index: u16,
    owner: i64,
    sub: i64,
    asset: u32,
    value: i64,
) -> Posting {
    Posting::new(
        PostingId {
            transfer: EnvelopeId(transfer_hash),
            index,
        },
        AccountId::with_sub(owner, sub),
        AssetId::new(asset),
        Cent::from(value),
    )
}

fn make_envelope_with_book(book: BookId) -> (Envelope, EnvelopeId) {
    let t = EnvelopeBuilder::new()
        .creates(vec![
            NewPosting {
                owner: AccountId::new(1),
                asset: AssetId::new(1),
                value: Cent::from(100),
                payer: None,
            },
            NewPosting {
                owner: AccountId::new(99),
                asset: AssetId::new(1),
                value: Cent::from(-100),
                payer: None,
            },
        ])
        .book(book)
        .build();
    // Use book id to create distinct EnvelopeIds.
    let mut tid_bytes = [0u8; 32];
    tid_bytes[0] = book.0 as u8;
    tid_bytes[1] = 42;
    (t, EnvelopeId(tid_bytes))
}

fn make_envelope() -> (Envelope, EnvelopeId) {
    let t = EnvelopeBuilder::new()
        .creates(vec![
            NewPosting {
                owner: AccountId::new(1),
                asset: AssetId::new(1),
                value: Cent::from(100),
                payer: None,
            },
            NewPosting {
                owner: AccountId::new(99),
                asset: AssetId::new(1),
                value: Cent::from(-100),
                payer: None,
            },
        ])
        .build();
    // Use a fixed EnvelopeId — store tests don't need content-addressing.
    let tid = EnvelopeId([42; 32]);
    (t, tid)
}

/// Seed `create` as Active postings via the dumb `insert_postings` primitive.
/// `tag` is unused now (kept so existing call sites read unchanged).
async fn seed_active(store: &(impl Store + 'static), _tag: u8, create: &[Posting]) {
    store.insert_postings(create).await.unwrap();
}

/// Fetch the derived [`PostingState`] of a single posting.
async fn state_of(store: &(impl Store + 'static), id: PostingId) -> PostingState {
    store.get_posting_states(&[id]).await.unwrap()[0]
}

/// Persist `envelope` as a committed transfer, deriving its created postings the
/// way the ledger does (`PostingId { transfer: tid, index }`) and indexing the
/// created owners — the same shape the saga produces.
async fn commit_envelope(
    store: &(impl Store + 'static),
    envelope: Envelope,
    tid: EnvelopeId,
    created_at: i64,
) {
    let create: Vec<Posting> = envelope
        .creates()
        .iter()
        .enumerate()
        .map(|(i, np)| {
            Posting::new(
                PostingId {
                    transfer: tid,
                    index: i as u16,
                },
                np.owner,
                np.asset,
                np.value,
            )
        })
        .collect();
    let mut involved: Vec<AccountId> = create.iter().map(|p| p.owner).collect();
    involved.sort();
    involved.dedup();
    store.insert_postings(&create).await.unwrap();
    store
        .store_transfer(
            EnvelopeRecord {
                envelope,
                receipt: Receipt { transfer_id: tid },
                created_at,
            },
            &involved,
        )
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// AccountStore tests
// ---------------------------------------------------------------------------

/// Create an account and retrieve it.
pub async fn create_and_get_account(store: &(impl Store + 'static)) {
    let acc = make_account(1, AccountFlags::DEBIT_MUST_NOT_EXCEED_CREDIT);
    store.create_account(acc.clone()).await.unwrap();
    let got = store.get_account(&AccountId::new(1)).await.unwrap();
    assert_eq!(got.id, acc.id);
    assert_eq!(got.version, 1);
    // The balance constraint lives in `flags`; it must survive the round-trip.
    assert_eq!(got.flags, acc.flags);
}

/// Duplicate account creation fails.
pub async fn create_duplicate_account_fails(store: &(impl Store + 'static)) {
    let acc = make_account(1, AccountFlags::DEBIT_MUST_NOT_EXCEED_CREDIT);
    store.create_account(acc.clone()).await.unwrap();
    let err = store.create_account(acc).await.unwrap_err();
    assert!(matches!(err, StoreError::AlreadyExists(_)));
}

/// Get non-existent account returns NotFound.
pub async fn get_missing_account_fails(store: &(impl Store + 'static)) {
    let err = store.get_account(&AccountId::new(999)).await.unwrap_err();
    assert!(matches!(err, StoreError::NotFound(_)));
}

/// Fetch multiple accounts in one call.
pub async fn get_accounts_batch(store: &(impl Store + 'static)) {
    store
        .create_account(make_account(1, AccountFlags::DEBIT_MUST_NOT_EXCEED_CREDIT))
        .await
        .unwrap();
    store
        .create_account(make_account(2, AccountFlags::DEBIT_MUST_NOT_EXCEED_CREDIT))
        .await
        .unwrap();
    let accs = store
        .get_accounts(&[AccountId::new(1), AccountId::new(2)])
        .await
        .unwrap();
    assert_eq!(accs.len(), 2);
}

/// Append a new version and verify get returns the latest.
pub async fn append_account_version(store: &(impl Store + 'static)) {
    let acc = make_account(1, AccountFlags::DEBIT_MUST_NOT_EXCEED_CREDIT);
    store.create_account(acc.clone()).await.unwrap();

    let mut v2 = acc.clone();
    v2.version = 2;
    v2.flags = AccountFlags::FROZEN;
    store.append_account_version(v2).await.unwrap();

    let got = store.get_account(&AccountId::new(1)).await.unwrap();
    assert_eq!(got.version, 2);
    assert!(got.is_frozen());
}

/// Appending with wrong version number fails.
pub async fn append_version_conflict(store: &(impl Store + 'static)) {
    let acc = make_account(1, AccountFlags::DEBIT_MUST_NOT_EXCEED_CREDIT);
    store.create_account(acc.clone()).await.unwrap();

    let mut bad = acc.clone();
    bad.version = 5;
    let err = store.append_account_version(bad).await.unwrap_err();
    assert!(matches!(err, StoreError::VersionConflict { .. }));
}

/// Re-appending an already-taken version is rejected and leaves the history
/// intact: no gap, no duplicate. Exercises the version guard and the insert
/// backstop of the locking append.
pub async fn append_duplicate_version_rejected(store: &(impl Store + 'static)) {
    let acc = make_account(1, AccountFlags::DEBIT_MUST_NOT_EXCEED_CREDIT);
    store.create_account(acc.clone()).await.unwrap();

    let mut v2 = acc.clone();
    v2.version = 2;
    v2.flags = AccountFlags::FROZEN;
    store.append_account_version(v2).await.unwrap();

    // A second append that also targets version 2 (now the current max) must be
    // rejected rather than duplicating or overwriting it.
    let mut v2_again = acc.clone();
    v2_again.version = 2;
    v2_again.flags = AccountFlags::CLOSED;
    let err = store.append_account_version(v2_again).await.unwrap_err();
    assert!(matches!(err, StoreError::VersionConflict { .. }));

    // Exactly one row at version 2, and it is the first (frozen) write.
    let history = store.get_account_history(&AccountId::new(1)).await.unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[1].version, 2);
    assert!(history[1].is_frozen());
    assert!(!history[1].is_closed());
}

/// Account history returns all versions.
pub async fn get_account_history(store: &(impl Store + 'static)) {
    let acc = make_account(1, AccountFlags::DEBIT_MUST_NOT_EXCEED_CREDIT);
    store.create_account(acc.clone()).await.unwrap();

    let mut v2 = acc.clone();
    v2.version = 2;
    store.append_account_version(v2).await.unwrap();

    let history = store.get_account_history(&AccountId::new(1)).await.unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].version, 1);
    assert_eq!(history[1].version, 2);
}

/// List accounts returns latest version of each.
pub async fn list_accounts(store: &(impl Store + 'static)) {
    store
        .create_account(make_account(1, AccountFlags::DEBIT_MUST_NOT_EXCEED_CREDIT))
        .await
        .unwrap();
    store
        .create_account(make_account(2, AccountFlags::empty()))
        .await
        .unwrap();
    let list = store.list_accounts().await.unwrap();
    assert_eq!(list.len(), 2);
}

// ---------------------------------------------------------------------------
// PostingStore tests
// ---------------------------------------------------------------------------

/// Committing with empty deactivate creates new postings.
pub async fn commit_creates_postings(store: &(impl Store + 'static)) {
    let p = make_posting([1; 32], 0, 1, 1, 100);
    seed_active(store, 200, std::slice::from_ref(&p)).await;

    let got = store.get_postings(&[p.id]).await.unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].value, Cent::from(100));
}

/// Get non-existent posting returns NotFound.
pub async fn get_postings_missing_fails(store: &(impl Store + 'static)) {
    let missing = PostingId {
        transfer: EnvelopeId([0; 32]),
        index: 0,
    };
    let err = store.get_postings(&[missing]).await.unwrap_err();
    assert!(matches!(err, StoreError::NotFound(_)));
}

/// Filter postings by account, asset, and status.
pub async fn get_postings_by_account_filters(store: &(impl Store + 'static)) {
    let p1 = make_posting([1; 32], 0, 1, 1, 100);
    let p2 = make_posting([1; 32], 1, 1, 2, 200);
    let p3 = make_posting([1; 32], 2, 2, 1, 300);
    seed_active(store, 200, &[p1, p2, p3]).await;

    let all = store
        .get_postings_by_account(1, None, None, PostingFilter::All)
        .await
        .unwrap();
    assert_eq!(all.len(), 2);

    let filtered = store
        .get_postings_by_account(1, None, Some(&AssetId::new(1)), PostingFilter::All)
        .await
        .unwrap();
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].value, Cent::from(100));

    let active = store
        .get_postings_by_account(1, None, None, PostingFilter::Active)
        .await
        .unwrap();
    assert_eq!(active.len(), 2);
}

/// Postings are segregated by subaccount: reading a base id spans every
/// subaccount, a subaccount filter restricts to one, and no read ever sums
/// across subaccounts.
pub async fn get_postings_by_subaccount(store: &(impl Store + 'static)) {
    // Base account 1 holds three postings across two subaccounts of asset 1.
    let main = make_posting_sub([7; 32], 0, 1, 0, 1, 100);
    let sub7a = make_posting_sub([7; 32], 1, 1, 7, 1, 200);
    let sub7b = make_posting_sub([7; 32], 2, 1, 7, 1, 50);
    seed_active(store, 0, &[main, sub7a, sub7b]).await;

    // sub = None spans every subaccount of base id 1.
    let all = store
        .get_postings_by_account(1, None, Some(&AssetId::new(1)), PostingFilter::All)
        .await
        .unwrap();
    assert_eq!(all.len(), 3);

    // sub = Some(0) is the main account only.
    let main_only = store
        .get_postings_by_account(1, Some(0), Some(&AssetId::new(1)), PostingFilter::All)
        .await
        .unwrap();
    assert_eq!(main_only.len(), 1);
    assert_eq!(main_only[0].value, Cent::from(100));
    assert_eq!(main_only[0].owner, AccountId::new(1));

    // sub = Some(7) is that subaccount only; its two postings are never folded
    // into the main account's figure.
    let sub_only = store
        .get_postings_by_account(1, Some(7), Some(&AssetId::new(1)), PostingFilter::All)
        .await
        .unwrap();
    assert_eq!(sub_only.len(), 2);
    assert!(
        sub_only
            .iter()
            .all(|p| p.owner == AccountId::with_sub(1, 7))
    );

    // A subaccount that was never used returns nothing.
    let empty = store
        .get_postings_by_account(1, Some(9), None, PostingFilter::All)
        .await
        .unwrap();
    assert!(empty.is_empty());
}

/// Query postings with pagination.
pub async fn query_postings_pagination(store: &(impl Store + 'static)) {
    // Create 5 postings for account 1, asset 1
    let postings: Vec<Posting> = (0..5)
        .map(|i| make_posting([1; 32], i, 1, 1, (i as i64 + 1) * 100))
        .collect();
    seed_active(store, 200, &postings).await;

    // Page 1: first 2
    let page1 = store
        .query_postings(&PostingQuery {
            account: 1,
            sub: None,
            asset: None,
            filter: PostingFilter::All,
            limit: Some(2),
            offset: Some(0),
        })
        .await
        .unwrap();
    assert_eq!(page1.items.len(), 2);
    assert_eq!(page1.total, 5);

    // Page 2: next 2
    let page2 = store
        .query_postings(&PostingQuery {
            account: 1,
            sub: None,
            asset: None,
            filter: PostingFilter::All,
            limit: Some(2),
            offset: Some(2),
        })
        .await
        .unwrap();
    assert_eq!(page2.items.len(), 2);
    assert_eq!(page2.total, 5);

    // Page 3: last 1
    let page3 = store
        .query_postings(&PostingQuery {
            account: 1,
            sub: None,
            asset: None,
            filter: PostingFilter::All,
            limit: Some(2),
            offset: Some(4),
        })
        .await
        .unwrap();
    assert_eq!(page3.items.len(), 1);
    assert_eq!(page3.total, 5);

    // With asset filter
    let filtered = store
        .query_postings(&PostingQuery {
            account: 1,
            sub: None,
            asset: Some(AssetId::new(1)),
            filter: PostingFilter::All,
            limit: Some(10),
            offset: None,
        })
        .await
        .unwrap();
    assert_eq!(filtered.total, 5);
    assert_eq!(filtered.items.len(), 5);
}

/// Reserve a batch of postings: active index → reserved index.
pub async fn reserve_postings_batch(store: &(impl Store + 'static)) {
    let p1 = make_posting([1; 32], 0, 1, 1, 100);
    let p2 = make_posting([1; 32], 1, 1, 1, 200);
    seed_active(store, 200, &[p1.clone(), p2.clone()]).await;

    let rid = ReservationId::new(1);
    store.reserve_postings(&[p1.id, p2.id], rid).await.unwrap();

    let states = store.get_posting_states(&[p1.id, p2.id]).await.unwrap();
    assert!(states.iter().all(|s| *s == PostingState::Reserved(rid)));
}

/// Reserve only flips the still-Active postings and reports that count; an
/// already-reserved posting in the batch is skipped (the saga interprets the
/// short count).
pub async fn reserve_skips_non_active(store: &(impl Store + 'static)) {
    let p1 = make_posting([1; 32], 0, 1, 1, 100);
    let p2 = make_posting([1; 32], 1, 1, 1, 200);
    seed_active(store, 200, &[p1.clone(), p2.clone()]).await;

    assert_eq!(
        store
            .reserve_postings(&[p1.id], ReservationId::new(1))
            .await
            .unwrap(),
        1
    );

    // p1 already reserved → only p2 (still active) reserves.
    assert_eq!(
        store
            .reserve_postings(&[p1.id, p2.id], ReservationId::new(1))
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        state_of(store, p2.id).await,
        PostingState::Reserved(ReservationId::new(1))
    );
}

/// Release reserved postings back to Active.
pub async fn release_postings_batch(store: &(impl Store + 'static)) {
    let p1 = make_posting([1; 32], 0, 1, 1, 100);
    seed_active(store, 200, std::slice::from_ref(&p1)).await;
    store
        .reserve_postings(&[p1.id], ReservationId::new(1))
        .await
        .unwrap();

    store
        .release_postings(&[p1.id], ReservationId::new(1))
        .await
        .unwrap();

    assert_eq!(state_of(store, p1.id).await, PostingState::Active);
}

/// Releasing an Active posting is a no-op (succeeds silently).
pub async fn release_active_is_noop(store: &(impl Store + 'static)) {
    let p1 = make_posting([1; 32], 0, 1, 1, 100);
    seed_active(store, 200, std::slice::from_ref(&p1)).await;

    store
        .release_postings(&[p1.id], ReservationId::new(1))
        .await
        .unwrap();

    assert_eq!(state_of(store, p1.id).await, PostingState::Active);
}

/// Releasing a spent posting is a no-op: zero rows released.
pub async fn release_inactive_zero(store: &(impl Store + 'static)) {
    let p1 = make_posting([1; 32], 0, 1, 1, 100);
    seed_active(store, 200, std::slice::from_ref(&p1)).await;

    // Deactivate p1 (raw path: still active) so the release sees a spent posting.
    assert_eq!(store.deactivate_postings(&[p1.id], None).await.unwrap(), 1);

    assert_eq!(
        store
            .release_postings(&[p1.id], ReservationId::new(1))
            .await
            .unwrap(),
        0
    );
    assert_eq!(state_of(store, p1.id).await, PostingState::Spent);
}

/// Deactivating a reserved posting (saga path) removes it from the reserved
/// index (→ spent) while a separate insert adds and activates the created
/// posting.
pub async fn commit_deactivates_postings(store: &(impl Store + 'static)) {
    let p1 = make_posting([1; 32], 0, 1, 1, 100);
    seed_active(store, 200, std::slice::from_ref(&p1)).await;
    store
        .reserve_postings(&[p1.id], ReservationId::new(1))
        .await
        .unwrap();

    let p2 = make_posting([2; 32], 0, 1, 1, 100);
    // Saga path: p1 is reserved by reservation 1.
    assert_eq!(
        store
            .deactivate_postings(&[p1.id], Some(ReservationId::new(1)))
            .await
            .unwrap(),
        1
    );
    store
        .insert_postings(std::slice::from_ref(&p2))
        .await
        .unwrap();

    assert_eq!(state_of(store, p1.id).await, PostingState::Spent);
    assert_eq!(state_of(store, p2.id).await, PostingState::Active);
}

// ---------------------------------------------------------------------------
// Dumb count-returning primitives (storage reports counts, never interprets)
// ---------------------------------------------------------------------------

/// `insert_postings` reports how many rows were newly inserted; already-present
/// postings contribute zero (idempotent).
pub async fn insert_postings_counts(store: &(impl Store + 'static)) {
    let p1 = make_posting([3; 32], 0, 1, 1, 100);
    let p2 = make_posting([3; 32], 1, 1, 1, 200);
    assert_eq!(
        store
            .insert_postings(std::slice::from_ref(&p1))
            .await
            .unwrap(),
        1
    );
    // p1 already present, p2 new → 1
    assert_eq!(
        store
            .insert_postings(&[p1.clone(), p2.clone()])
            .await
            .unwrap(),
        1
    );
    // both present → 0
    assert_eq!(store.insert_postings(&[p1, p2]).await.unwrap(), 0);
}

/// `deactivate_postings` (raw path) removes active ids (→ spent) and reports the
/// count; a replay over already-spent postings reports zero.
pub async fn deactivate_postings_counts(store: &(impl Store + 'static)) {
    let p1 = make_posting([4; 32], 0, 1, 1, 100);
    let p2 = make_posting([4; 32], 1, 1, 1, 200);
    store
        .insert_postings(&[p1.clone(), p2.clone()])
        .await
        .unwrap();

    assert_eq!(
        store
            .deactivate_postings(&[p1.id, p2.id], None)
            .await
            .unwrap(),
        2
    );
    // replay: already spent → 0
    assert_eq!(
        store
            .deactivate_postings(&[p1.id, p2.id], None)
            .await
            .unwrap(),
        0
    );
    assert_eq!(state_of(store, p1.id).await, PostingState::Spent);
}

/// `deactivate_postings` (saga path) only flips postings reserved by the given
/// reservation; a non-matching reservation reports zero.
pub async fn deactivate_postings_saga_path(store: &(impl Store + 'static)) {
    let p1 = make_posting([5; 32], 0, 1, 1, 100);
    store
        .insert_postings(std::slice::from_ref(&p1))
        .await
        .unwrap();
    store
        .reserve_postings(&[p1.id], ReservationId::new(7))
        .await
        .unwrap();

    // wrong reservation → 0 (storage doesn't error; the saga decides)
    assert_eq!(
        store
            .deactivate_postings(&[p1.id], Some(ReservationId::new(8)))
            .await
            .unwrap(),
        0
    );
    // right reservation → 1
    assert_eq!(
        store
            .deactivate_postings(&[p1.id], Some(ReservationId::new(7)))
            .await
            .unwrap(),
        1
    );
}

/// `get_posting_states` reflects index membership across every derived state:
/// active, reserved (with its owner), spent, and missing.
pub async fn get_posting_states_reflect_membership(store: &(impl Store + 'static)) {
    let active = make_posting([9; 32], 0, 1, 1, 100);
    let reserved = make_posting([9; 32], 1, 1, 1, 200);
    let spent = make_posting([9; 32], 2, 1, 1, 300);
    let missing = PostingId {
        transfer: EnvelopeId([0xEE; 32]),
        index: 0,
    };
    store
        .insert_postings(&[active.clone(), reserved.clone(), spent.clone()])
        .await
        .unwrap();

    let rid = ReservationId::new(3);
    store.reserve_postings(&[reserved.id], rid).await.unwrap();
    store.deactivate_postings(&[spent.id], None).await.unwrap();

    let states = store
        .get_posting_states(&[active.id, reserved.id, spent.id, missing])
        .await
        .unwrap();
    assert_eq!(
        states,
        vec![
            PostingState::Active,
            PostingState::Reserved(rid),
            PostingState::Spent,
            PostingState::Missing,
        ]
    );
}

/// `get_posting_states` stays aligned to input order for a larger batch, with
/// ids interleaved out of insertion order and repeated. This guards the batched
/// SQL path, which fetches each state table as a set and must reconstruct the
/// per-id state positionally (duplicates resolve to the same state).
pub async fn get_posting_states_batch_preserves_order(store: &(impl Store + 'static)) {
    let active = make_posting([7; 32], 0, 1, 1, 100);
    let reserved = make_posting([7; 32], 1, 1, 1, 200);
    let spent = make_posting([7; 32], 2, 1, 1, 300);
    let missing = PostingId {
        transfer: EnvelopeId([0xAB; 32]),
        index: 5,
    };
    store
        .insert_postings(&[active.clone(), reserved.clone(), spent.clone()])
        .await
        .unwrap();

    let rid = ReservationId::new(7);
    store.reserve_postings(&[reserved.id], rid).await.unwrap();
    store.deactivate_postings(&[spent.id], None).await.unwrap();

    // Interleaved, out of insertion order, with `active` and `missing` repeated.
    let query = [
        missing,
        spent.id,
        active.id,
        reserved.id,
        active.id,
        missing,
    ];
    let states = store.get_posting_states(&query).await.unwrap();
    assert_eq!(
        states,
        vec![
            PostingState::Missing,
            PostingState::Spent,
            PostingState::Active,
            PostingState::Reserved(rid),
            PostingState::Active,
            PostingState::Missing,
        ]
    );

    // Empty input returns an empty vec (the batched path guards this).
    assert!(store.get_posting_states(&[]).await.unwrap().is_empty());
}

/// A consumed posting is removed from the indexes but stays in the immutable
/// table forever: `get_postings` still returns it while its state is `Spent`.
pub async fn spent_posting_remains_in_immutable_table(store: &(impl Store + 'static)) {
    let p = make_posting([0xA1; 32], 0, 1, 1, 100);
    seed_active(store, 0, std::slice::from_ref(&p)).await;
    store.deactivate_postings(&[p.id], None).await.unwrap();

    let got = store.get_postings(&[p.id]).await.unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].value, Cent::from(100));
    assert_eq!(state_of(store, p.id).await, PostingState::Spent);
}

/// The `Live` filter returns active and reserved postings but excludes spent
/// ones (the replacement for the old "not Inactive" balance-bearing set).
pub async fn get_postings_by_account_live_filter(store: &(impl Store + 'static)) {
    let active = make_posting([0xB2; 32], 0, 1, 1, 100);
    let reserved = make_posting([0xB2; 32], 1, 1, 1, 200);
    let spent = make_posting([0xB2; 32], 2, 1, 1, 300);
    store
        .insert_postings(&[active.clone(), reserved.clone(), spent.clone()])
        .await
        .unwrap();
    store
        .reserve_postings(&[reserved.id], ReservationId::new(1))
        .await
        .unwrap();
    store.deactivate_postings(&[spent.id], None).await.unwrap();

    let live = store
        .get_postings_by_account(1, None, None, PostingFilter::Live)
        .await
        .unwrap();
    let mut ids: Vec<PostingId> = live.iter().map(|p| p.id).collect();
    ids.sort_by_key(|id| id.index);
    assert_eq!(ids, vec![active.id, reserved.id]);
}

/// `store_transfer` returns 1 when the record is newly inserted, 0 on replay,
/// and indexes the involved accounts.
pub async fn store_transfer_counts(store: &(impl Store + 'static)) {
    let (envelope, tid) = make_envelope(); // creates owners 1 and 99
    let record = EnvelopeRecord {
        envelope,
        receipt: Receipt { transfer_id: tid },
        created_at: 1000,
    };
    let involved = [AccountId::new(1), AccountId::new(99)];

    assert_eq!(
        store
            .store_transfer(record.clone(), &involved)
            .await
            .unwrap(),
        1
    );
    // replay → 0
    assert_eq!(store.store_transfer(record, &involved).await.unwrap(), 0);
    assert!(store.get_transfer(&tid).await.unwrap().is_some());
    assert_eq!(
        store
            .get_transfers_for_account(1, None)
            .await
            .unwrap()
            .len(),
        1
    );
}

/// `store_transfer` indexes the transfer under exactly the `involved` set the
/// caller supplies and derives participation from nowhere else. A consumed
/// posting's owner (account 7) is distinct from every created owner (account 8),
/// and that consumed posting is deliberately never seeded into the store, so the
/// transfer can be found for account 7 only if the backend trusts `involved`.
/// This is the case where a derive-from-postings backend and a trust-`involved`
/// backend would disagree; both must return the transfer for both accounts.
pub async fn store_transfer_indexes_involved(store: &(impl Store + 'static)) {
    let consumed = PostingId {
        transfer: EnvelopeId([9; 32]),
        index: 0,
    };
    let tid = EnvelopeId([7; 32]);
    let envelope = EnvelopeBuilder::new()
        .consumes(vec![consumed])
        .creates(vec![NewPosting {
            owner: AccountId::new(8),
            asset: AssetId::new(1),
            value: Cent::from(100),
            payer: None,
        }])
        .build();
    let record = EnvelopeRecord {
        envelope,
        receipt: Receipt { transfer_id: tid },
        created_at: 2000,
    };
    // Full participation set: created owner 8 plus consumed owner 7. Account 7's
    // posting is never inserted, so only `involved` can surface the transfer.
    let involved = [AccountId::new(8), AccountId::new(7)];

    assert_eq!(store.store_transfer(record, &involved).await.unwrap(), 1);

    // Consumed-owner branch: found purely because `involved` said so.
    let for_consumed = store.get_transfers_for_account(7, None).await.unwrap();
    assert_eq!(for_consumed.len(), 1);
    assert_eq!(for_consumed[0].receipt.transfer_id, tid);

    // The created owner is indexed identically.
    let for_created = store.get_transfers_for_account(8, None).await.unwrap();
    assert_eq!(for_created.len(), 1);
    assert_eq!(for_created[0].receipt.transfer_id, tid);
}

// ---------------------------------------------------------------------------
// Reservation / double-spend regressions (sequential — the conformance harness
// holds a single `&store`; the second attempt is what must report zero).
// ---------------------------------------------------------------------------

/// A posting reserved by one reservation cannot be reserved by another: the
/// second reserve flips zero rows (the saga reads the count to know it lost).
pub async fn reserve_twice_second_zero(store: &(impl Store + 'static)) {
    let p1 = make_posting([1; 32], 0, 1, 1, 100);
    seed_active(store, 200, std::slice::from_ref(&p1)).await;

    assert_eq!(
        store
            .reserve_postings(&[p1.id], ReservationId::new(1))
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .reserve_postings(&[p1.id], ReservationId::new(2))
            .await
            .unwrap(),
        0
    );
}

/// A posting cannot be deactivated twice: once Inactive, a second raw deactivate
/// reports zero — the double-spend guard at the storage layer.
pub async fn deactivate_twice_second_zero(store: &(impl Store + 'static)) {
    let consumed = make_posting([7; 32], 0, 1, 1, 100);
    seed_active(store, 200, std::slice::from_ref(&consumed)).await;

    assert_eq!(
        store
            .deactivate_postings(&[consumed.id], None)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .deactivate_postings(&[consumed.id], None)
            .await
            .unwrap(),
        0
    );
}

/// `append_event` is idempotent on a transfer's dedup key: re-appending the same
/// `TransferCommitted` returns the existing seq and does not duplicate the row.
pub async fn append_event_idempotent(store: &(impl Store + 'static)) {
    let event = LedgerEvent {
        seq: 0,
        timestamp: 1000,
        kind: LedgerEventKind::TransferCommitted {
            transfer_id: EnvelopeId([8; 32]),
        },
    };
    let seq1 = store.append_event(&event).await.unwrap();
    let seq2 = store.append_event(&event).await.unwrap();
    assert_eq!(seq1, seq2);
    assert_eq!(store.get_events_since(0, 10).await.unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// TransferStore tests
// ---------------------------------------------------------------------------

/// Commit a transfer and retrieve it by id.
pub async fn commit_and_get_transfer(store: &(impl Store + 'static)) {
    let (envelope, tid) = make_envelope();
    commit_envelope(store, envelope, tid, 1000).await;

    let got = store.get_transfer(&tid).await.unwrap();
    assert!(got.is_some());
    assert_eq!(got.unwrap().receipt.transfer_id, tid);
}

/// Get non-existent transfer returns None.
pub async fn get_missing_transfer(store: &(impl Store + 'static)) {
    let got = store.get_transfer(&EnvelopeId([0; 32])).await.unwrap();
    assert!(got.is_none());
}

/// Query transfers by account.
pub async fn get_transfers_for_account(store: &(impl Store + 'static)) {
    let (envelope, tid) = make_envelope();
    commit_envelope(store, envelope, tid, 1000).await;

    let records = store.get_transfers_for_account(1, None).await.unwrap();
    assert_eq!(records.len(), 1);

    let empty = store.get_transfers_for_account(999, None).await.unwrap();
    assert!(empty.is_empty());
}

/// Verify that created_at roundtrips through commit/retrieve.
pub async fn commit_preserves_created_at(store: &(impl Store + 'static)) {
    let (envelope, tid) = make_envelope();
    commit_envelope(store, envelope, tid, 1718000000000).await;

    let got = store.get_transfer(&tid).await.unwrap().unwrap();
    assert_eq!(got.created_at, 1718000000000);
}

// ---------------------------------------------------------------------------
// TransferQuery tests
// ---------------------------------------------------------------------------

/// Query transfers by date range.
pub async fn query_transfers_by_date_range(store: &(impl Store + 'static)) {
    let (e1, t1) = make_envelope();
    commit_envelope(store, e1, t1, 1000).await;

    let (e2, t2) = make_envelope_with_book(BookId(1));
    commit_envelope(store, e2, t2, 2000).await;

    let page = store
        .query_transfers(&TransferQuery {
            account: Some(1),
            sub: None,
            from_ts: Some(1500),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(page.items[0].created_at, 2000);
}

/// Query transfers with pagination.
pub async fn query_transfers_pagination(store: &(impl Store + 'static)) {
    // Store 3 transfers with different timestamps.
    for i in 0..3u8 {
        let mut tid_bytes = [0u8; 32];
        tid_bytes[0] = i + 10;
        let (envelope, _) = make_envelope();
        let tid = EnvelopeId(tid_bytes);
        commit_envelope(store, envelope, tid, (i as i64 + 1) * 1000).await;
    }

    let page = store
        .query_transfers(&TransferQuery {
            account: Some(1),
            sub: None,
            limit: Some(2),
            offset: Some(0),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(page.items.len(), 2);
    assert_eq!(page.total, 3);

    let page2 = store
        .query_transfers(&TransferQuery {
            account: Some(1),
            sub: None,
            limit: Some(2),
            offset: Some(2),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(page2.items.len(), 1);
    assert_eq!(page2.total, 3);
}

/// Query transfers by book.
pub async fn query_transfers_by_book(store: &(impl Store + 'static)) {
    let (e1, t1) = make_envelope(); // book = 0
    commit_envelope(store, e1, t1, 1000).await;

    let (e2, t2) = make_envelope_with_book(BookId(5));
    commit_envelope(store, e2, t2, 2000).await;

    let page = store
        .query_transfers(&TransferQuery {
            account: Some(1),
            sub: None,
            book: Some(BookId(5)),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(page.items[0].envelope.book(), BookId(5));
}

/// An `account == None` query is a store-wide scan on every backend: it returns
/// all transfers regardless of participation, and the time-window/book filters
/// still apply. This pins the account-optional contract that used to diverge
/// (in-memory errored, SQL scanned).
pub async fn query_transfers_store_wide(store: &(impl Store + 'static)) {
    let (e1, t1) = make_envelope_with_book(BookId(1));
    commit_envelope(store, e1, t1, 1000).await;

    let (e2, t2) = make_envelope_with_book(BookId(2));
    commit_envelope(store, e2, t2, 2000).await;

    // No account filter: both transfers come back, newest bound honored.
    let page = store
        .query_transfers(&TransferQuery::default())
        .await
        .unwrap();
    assert_eq!(page.total, 2);

    // The shared book filter still narrows a store-wide scan.
    let page = store
        .query_transfers(&TransferQuery {
            book: Some(BookId(2)),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(page.items[0].created_at, 2000);
}

// ---------------------------------------------------------------------------
// SagaStore tests
// ---------------------------------------------------------------------------

/// Save saga state and list it.
pub async fn save_and_list_sagas(store: &(impl Store + 'static)) {
    let id: i64 = 42;
    let data = vec![1, 2, 3];
    store.save_saga(&id, data.clone()).await.unwrap();

    let pending = store.list_pending_sagas().await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].0, id);
    assert_eq!(pending[0].1, data);
}

/// Delete a saga state.
pub async fn delete_saga(store: &(impl Store + 'static)) {
    let id: i64 = 42;
    store.save_saga(&id, vec![1, 2, 3]).await.unwrap();
    store.delete_saga(&id).await.unwrap();

    let pending = store.list_pending_sagas().await.unwrap();
    assert!(pending.is_empty());
}

// ---------------------------------------------------------------------------
// EventStore tests
// ---------------------------------------------------------------------------

/// Append events and query them back.
pub async fn append_and_query_events(store: &(impl Store + 'static)) {
    let e1 = LedgerEvent {
        seq: 0,
        timestamp: 1000,
        kind: LedgerEventKind::AccountCreated {
            account_id: AccountId::new(1),
        },
    };
    let e2 = LedgerEvent {
        seq: 0,
        timestamp: 2000,
        kind: LedgerEventKind::TransferCommitted {
            transfer_id: EnvelopeId([42; 32]),
        },
    };

    let seq1 = store.append_event(&e1).await.unwrap();
    let seq2 = store.append_event(&e2).await.unwrap();
    assert!(seq2 > seq1);

    let events = store.get_events_since(0, 100).await.unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].seq, seq1);
    assert_eq!(events[1].seq, seq2);
}

/// Events are ordered by sequence number and support cursor-based pagination.
pub async fn events_sequence_ordering(store: &(impl Store + 'static)) {
    for i in 0..5u64 {
        store
            .append_event(&LedgerEvent {
                seq: 0,
                timestamp: (i as i64 + 1) * 1000,
                kind: LedgerEventKind::AccountCreated {
                    account_id: AccountId::new(i as i64 + 1),
                },
            })
            .await
            .unwrap();
    }

    let page1 = store.get_events_since(0, 3).await.unwrap();
    assert_eq!(page1.len(), 3);

    let page2 = store.get_events_since(page1[2].seq, 10).await.unwrap();
    assert_eq!(page2.len(), 2);
}

// ---------------------------------------------------------------------------
// BookStore
// ---------------------------------------------------------------------------

fn make_book(id: i64, name: &str) -> Book {
    BookBuilder::new(name)
        .id(BookId::new(id))
        .allow_asset(AssetId::new(1))
        .build()
}

/// Create a book and read it back.
pub async fn create_and_get_book(store: &(impl Store + 'static)) {
    let book = make_book(1, "sales");
    store.create_book(book.clone()).await.unwrap();
    let got = store.get_book(&BookId::new(1)).await.unwrap();
    assert_eq!(got, book);
}

/// Duplicate book creation fails.
pub async fn create_duplicate_book_fails(store: &(impl Store + 'static)) {
    let book = make_book(1, "sales");
    store.create_book(book.clone()).await.unwrap();
    let err = store.create_book(book).await.unwrap_err();
    assert!(matches!(err, StoreError::AlreadyExists(_)));
}

/// Get a non-existent book returns NotFound.
pub async fn get_missing_book_fails(store: &(impl Store + 'static)) {
    let err = store.get_book(&BookId::new(999)).await.unwrap_err();
    assert!(matches!(err, StoreError::NotFound(_)));
}

/// List all books.
pub async fn list_books(store: &(impl Store + 'static)) {
    store.create_book(make_book(1, "sales")).await.unwrap();
    store.create_book(make_book(2, "inventory")).await.unwrap();
    let mut books = store.list_books().await.unwrap();
    books.sort_by_key(|b| b.id.0);
    assert_eq!(books.len(), 2);
    assert_eq!(books[0].name, "sales");
    assert_eq!(books[1].name, "inventory");
}

// ---------------------------------------------------------------------------
// BalanceProjectionStore tests
// ---------------------------------------------------------------------------

/// No cache point yet reads back as `None`.
pub async fn balance_projection_absent_is_none(store: &(impl Store + 'static)) {
    let got = store
        .get_closest_balance_projection(&AccountId::new(1), &AssetId::new(1), i64::MAX)
        .await
        .unwrap();
    assert!(got.is_none());
}

/// An appended cache point round-trips: value and watermark come back intact and
/// carry a store-minted id.
pub async fn balance_projection_append_and_get_closest(store: &(impl Store + 'static)) {
    store
        .append_balance_projection(&AccountId::new(1), &AssetId::new(1), Cent::from(1234), 500)
        .await
        .unwrap();
    let got = store
        .get_closest_balance_projection(&AccountId::new(1), &AssetId::new(1), i64::MAX)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.account, AccountId::new(1));
    assert_eq!(got.asset, AssetId::new(1));
    assert_eq!(got.balance, Cent::from(1234));
    assert_eq!(got.watermark, 500);
}

/// Append-only cache points: `get_closest` returns the freshest one at or before
/// `as_of` (largest watermark), never a cache point covering past `as_of`. Ids
/// increase across appends.
pub async fn balance_projection_closest_at_or_before_as_of(store: &(impl Store + 'static)) {
    let acc = AccountId::new(1);
    let asset = AssetId::new(1);
    store
        .append_balance_projection(&acc, &asset, Cent::from(100), 200)
        .await
        .unwrap();
    let first = store
        .get_closest_balance_projection(&acc, &asset, i64::MAX)
        .await
        .unwrap()
        .unwrap();

    store
        .append_balance_projection(&acc, &asset, Cent::from(300), 400)
        .await
        .unwrap();

    // As of a time at/after both, the freshest (watermark 400) is returned.
    let latest = store
        .get_closest_balance_projection(&acc, &asset, 1000)
        .await
        .unwrap()
        .unwrap();
    assert!(
        latest.id > first.id,
        "cache-point id must increase on append"
    );
    assert_eq!(latest.balance, Cent::from(300));
    assert_eq!(latest.watermark, 400);

    // As of a time between them, the watermark-400 point is excluded (it would
    // cover transfers after as_of), so the watermark-200 point is returned.
    let earlier = store
        .get_closest_balance_projection(&acc, &asset, 300)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(earlier.balance, Cent::from(100));
    assert_eq!(earlier.watermark, 200);

    // As of before any cache point, none is returned.
    let none = store
        .get_closest_balance_projection(&acc, &asset, 100)
        .await
        .unwrap();
    assert!(none.is_none());
}

// ---------------------------------------------------------------------------
// Macro
// ---------------------------------------------------------------------------

/// Generate the full Store conformance test suite.
///
/// `$factory` must be an async fn returning a value that implements [`Store`].
///
/// ```text
/// async fn new_store() -> InMemoryStore { InMemoryStore::new() }
/// kuatia_storage::store_tests!(new_store);
/// ```
#[macro_export]
macro_rules! store_tests {
    ($factory:path) => {
        $crate::store_tests!(@tests $factory,
            // AccountStore
            create_and_get_account,
            create_duplicate_account_fails,
            get_missing_account_fails,
            get_accounts_batch,
            append_account_version,
            append_version_conflict,
            append_duplicate_version_rejected,
            get_account_history,
            list_accounts,
            // PostingStore
            commit_creates_postings,
            get_postings_missing_fails,
            get_postings_by_account_filters,
            get_postings_by_subaccount,
            query_postings_pagination,
            reserve_postings_batch,
            reserve_skips_non_active,
            release_postings_batch,
            release_active_is_noop,
            release_inactive_zero,
            commit_deactivates_postings,
            insert_postings_counts,
            deactivate_postings_counts,
            deactivate_postings_saga_path,
            get_posting_states_reflect_membership,
            get_posting_states_batch_preserves_order,
            spent_posting_remains_in_immutable_table,
            get_postings_by_account_live_filter,
            store_transfer_counts,
            store_transfer_indexes_involved,
            // Reservation / double-spend regressions
            reserve_twice_second_zero,
            deactivate_twice_second_zero,
            append_event_idempotent,
            // TransferStore
            commit_and_get_transfer,
            get_missing_transfer,
            get_transfers_for_account,
            commit_preserves_created_at,
            // TransferQuery
            query_transfers_by_date_range,
            query_transfers_pagination,
            query_transfers_by_book,
            query_transfers_store_wide,
            // SagaStore
            save_and_list_sagas,
            delete_saga,
            // EventStore
            append_and_query_events,
            events_sequence_ordering,
            // BookStore
            create_and_get_book,
            create_duplicate_book_fails,
            get_missing_book_fails,
            list_books,
            // BalanceProjectionStore
            balance_projection_absent_is_none,
            balance_projection_append_and_get_closest,
            balance_projection_closest_at_or_before_as_of,
        );
    };

    (@tests $factory:path, $($test:ident),+ $(,)?) => {
        ::paste::paste! {
            $(
                #[tokio::test]
                async fn [< $test >]() {
                    $crate::store_tests::$test(&$factory().await).await;
                }
            )+
        }
    };
}
