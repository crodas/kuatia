//! Concurrency tests for `InMemoryStore` primitives.
//!
//! The generated conformance suite drives the store through a single `&store`,
//! so it never exercises two callers racing on the same rows. `reserve_postings`
//! is the primitive the saga relies on to make double-spends impossible: it must
//! flip each `Active` posting to `PendingInactive` for exactly one caller, even
//! when many callers target the same postings at once.

#![allow(missing_docs)]

use std::sync::Arc;

use kuatia_storage::mem_store::InMemoryStore;
use kuatia_storage::store::PostingStore;
use kuatia_types::*;

fn posting(index: u16) -> Posting {
    Posting::new(
        PostingId {
            transfer: EnvelopeId([1; 32]),
            index,
        },
        AccountId::new(1),
        AssetId::new(1),
        Cent::from(100),
    )
}

/// Many tasks concurrently reserve the same set of postings, each with its own
/// reservation id. Reservation is a claim, so each posting may be reserved by
/// exactly one task: the per-task counts sum to the number of postings, and
/// every posting ends `PendingInactive`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_reserve_claims_each_posting_once() {
    const POSTINGS: u16 = 32;
    const TASKS: i64 = 8;

    let store = Arc::new(InMemoryStore::new());
    let all: Vec<Posting> = (0..POSTINGS).map(posting).collect();
    store.insert_postings(&all).await.unwrap();

    let ids: Vec<PostingId> = all.iter().map(|p| p.id).collect();

    let mut handles = Vec::new();
    for t in 0..TASKS {
        let store = Arc::clone(&store);
        let ids = ids.clone();
        handles.push(tokio::spawn(async move {
            store
                .reserve_postings(&ids, ReservationId::new(t + 1))
                .await
                .unwrap()
        }));
    }

    let mut total_reserved: u64 = 0;
    for h in handles {
        total_reserved += h.await.unwrap();
    }

    assert_eq!(
        total_reserved, POSTINGS as u64,
        "each posting is reserved by exactly one task"
    );

    let final_postings = store.get_postings(&ids).await.unwrap();
    assert!(
        final_postings
            .iter()
            .all(|p| p.status == PostingStatus::PendingInactive),
        "every posting ends reserved"
    );
}
