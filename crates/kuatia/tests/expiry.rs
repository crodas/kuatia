//! Integration tests for inflight hold expiry: the `expires_at` deadline, the
//! derived `Expired` state, `expire_due`, the background reaper, and rebuilding
//! the deadline index from durable metadata.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use kuatia::prelude::*;

fn usd() -> AssetId {
    AssetId::new(1)
}
fn payer() -> AccountId {
    AccountId::new(1)
}
fn merchant() -> AccountId {
    AccountId::new(2)
}
fn ext() -> AccountId {
    AccountId::new(99)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn make_account(id: i64, policy: AccountPolicy) -> Account {
    Account {
        id: AccountId::new(id),
        version: 1,
        policy,
        flags: AccountFlags::empty(),
        book: BookId(0),
        metadata: BTreeMap::new(),
    }
}

async fn deposit(ledger: &Arc<Ledger>, to: AccountId, asset: AssetId, amount: i64) {
    let t = TransferBuilder::new()
        .deposit(to, asset, Cent::from(amount), ext())
        .unwrap()
        .build();
    ledger.commit(t).await.unwrap();
}

/// A ledger with a payer holding 100 USD, a merchant, and an external account.
async fn setup() -> Arc<Ledger> {
    let ledger = Arc::new(Ledger::new(InMemoryStore::new()));
    for id in [1, 2] {
        ledger
            .store()
            .create_account(make_account(id, AccountPolicy::NoOverdraft))
            .await
            .unwrap();
    }
    ledger
        .store()
        .create_account(make_account(99, AccountPolicy::ExternalAccount))
        .await
        .unwrap();
    deposit(&ledger, payer(), usd(), 100).await;
    ledger
}

/// A single-leg trade: payer -> merchant for `amount` USD.
fn trade(amount: i64) -> Transfer {
    TransferBuilder::new()
        .pay(payer(), merchant(), usd(), Cent::from(amount))
        .build()
}

async fn bal(ledger: &Arc<Ledger>, account: AccountId, asset: AssetId) -> Cent {
    ledger.balance(&account, &asset).await.unwrap()
}

/// The deadline is surfaced on the authorization and on the derived status.
#[tokio::test]
async fn deadline_is_surfaced() {
    let ledger = setup().await;
    let deadline = now_ms() + 60_000;
    let auth = ledger
        .authorize_with_expiry(trade(40), deadline)
        .await
        .unwrap();
    assert_eq!(auth.expires_at, Some(deadline));

    let status = ledger.inflight_status(&auth.inflight).await.unwrap();
    assert_eq!(status.expires_at, Some(deadline));
    // Still in the future, funds parked: Held, not Expired.
    assert_eq!(status.state, InflightState::Held);

    // Plain authorize carries no deadline and never reports Expired.
    let plain = ledger.authorize(trade(10)).await.unwrap();
    assert_eq!(plain.expires_at, None);
    assert_eq!(
        ledger
            .inflight_status(&plain.inflight)
            .await
            .unwrap()
            .expires_at,
        None
    );
}

/// A past deadline with funds still held is reported as Expired, before any
/// reaper runs.
#[tokio::test]
async fn past_deadline_reads_expired() {
    let ledger = setup().await;
    let auth = ledger
        .authorize_with_expiry(trade(40), now_ms() - 1)
        .await
        .unwrap();
    let status = ledger.inflight_status(&auth.inflight).await.unwrap();
    assert_eq!(status.state, InflightState::Expired);
    // The funds are still held; nothing has moved yet.
    assert_eq!(bal(&ledger, payer(), usd()).await, Cent::from(60));
    assert_eq!(bal(&ledger, merchant(), usd()).await, Cent::ZERO);
}

/// `expire_due` returns every past-deadline hold to its funder, closes the hold,
/// and is idempotent on a second pass.
#[tokio::test]
async fn expire_due_voids_and_returns_funds() {
    let ledger = setup().await;
    let auth = ledger
        .authorize_with_expiry(trade(40), now_ms() - 1)
        .await
        .unwrap();
    assert_eq!(bal(&ledger, payer(), usd()).await, Cent::from(60));

    let reaped = ledger.expire_due(now_ms()).await;
    assert_eq!(reaped, 1);

    // Funds are back with the payer, nothing reached the merchant, hold closed.
    assert_eq!(bal(&ledger, payer(), usd()).await, Cent::from(100));
    assert_eq!(bal(&ledger, merchant(), usd()).await, Cent::ZERO);
    assert!(ledger.list_open_inflights().await.unwrap().is_empty());
    let status = ledger.inflight_status(&auth.inflight).await.unwrap();
    assert_eq!(status.state, InflightState::Voided);

    // Nothing left due: a second sweep is a no-op.
    assert_eq!(ledger.expire_due(now_ms()).await, 0);
}

/// A deadline still in the future is left alone by `expire_due`.
#[tokio::test]
async fn future_deadline_is_not_reaped() {
    let ledger = setup().await;
    let auth = ledger
        .authorize_with_expiry(trade(40), now_ms() + 60_000)
        .await
        .unwrap();
    assert_eq!(ledger.expire_due(now_ms()).await, 0);
    assert_eq!(
        ledger.inflight_status(&auth.inflight).await.unwrap().state,
        InflightState::Held
    );
    assert_eq!(bal(&ledger, payer(), usd()).await, Cent::from(60));
}

/// The background reaper auto-voids a hold shortly after its deadline passes,
/// with no manual sweep.
#[tokio::test]
async fn reaper_auto_voids_after_deadline() {
    let ledger = setup().await;
    let auth = ledger
        .authorize_with_expiry(trade(40), now_ms() + 150)
        .await
        .unwrap();
    let _reaper = ledger.spawn_expiry_reaper();

    // Poll until the reaper returns the funds, up to a generous timeout.
    let mut returned = false;
    for _ in 0..40 {
        if bal(&ledger, payer(), usd()).await == Cent::from(100) {
            returned = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(returned, "reaper did not void the expired hold in time");
    assert_eq!(
        ledger.inflight_status(&auth.inflight).await.unwrap().state,
        InflightState::Voided
    );
    assert!(ledger.list_open_inflights().await.unwrap().is_empty());
}

/// Confirming before the deadline settles to the destination; a later sweep sees
/// nothing to reap (the terminal op deregistered the deadline, and the hold is
/// closed regardless).
#[tokio::test]
async fn confirmed_hold_is_not_reaped() {
    let ledger = setup().await;
    let auth = ledger
        .authorize_with_expiry(trade(40), now_ms() - 1)
        .await
        .unwrap();
    ledger.confirm_all(&auth.inflight).await.unwrap();
    assert_eq!(bal(&ledger, merchant(), usd()).await, Cent::from(40));

    // Even with a past deadline, the settled hold is not touched.
    assert_eq!(ledger.expire_due(now_ms()).await, 0);
    assert_eq!(bal(&ledger, merchant(), usd()).await, Cent::from(40));
    assert_eq!(
        ledger.inflight_status(&auth.inflight).await.unwrap().state,
        InflightState::Confirmed
    );
}

/// `rebuild_expiry_index` reconstructs the index from durable metadata: it picks
/// up open holds with deadlines and ignores ones already settled.
#[tokio::test]
async fn rebuild_index_reflects_open_holds() {
    let ledger = setup().await;
    // One open expiring hold, and one already-voided expiring hold.
    let open = ledger
        .authorize_with_expiry(trade(40), now_ms() - 1)
        .await
        .unwrap();
    let closed = ledger
        .authorize_with_expiry(trade(20), now_ms() - 1)
        .await
        .unwrap();
    ledger.void(&closed.inflight).await.unwrap();

    // Rebuild from scratch: the index now comes only from durable metadata of
    // still-open holds, so exactly the one open hold is due.
    ledger.rebuild_expiry_index().await.unwrap();
    assert_eq!(ledger.expire_due(now_ms()).await, 1);

    // The surviving hold was the open one; funds are fully back with the payer.
    assert_eq!(bal(&ledger, payer(), usd()).await, Cent::from(100));
    assert_eq!(
        ledger.inflight_status(&open.inflight).await.unwrap().state,
        InflightState::Voided
    );
}
