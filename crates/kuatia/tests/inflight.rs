//! Integration tests for inflight holds (authorize / confirm / void).
//!
//! The running example is the ADR's confirmed trade between A and B with a fee
//! account, spanning two assets:
//!
//! ```text
//! A -> B   -> 100 EUR
//! B -> A   ->  10 BTC
//! A -> fee ->   1 BTC
//! B -> fee ->   1 EUR
//! ```
//!
//! Authorized, the funds park in per-destination holding accounts; `fee`'s hold
//! collects EUR from B and BTC from A.

use std::collections::BTreeMap;
use std::sync::Arc;

use kuatia::prelude::*;

fn eur() -> AssetId {
    AssetId::new(1)
}
fn btc() -> AssetId {
    AssetId::new(2)
}
fn a() -> AccountId {
    AccountId::new(1)
}
fn b() -> AccountId {
    AccountId::new(2)
}
fn fee() -> AccountId {
    AccountId::new(3)
}
fn ext() -> AccountId {
    AccountId::new(99)
}

fn make_account(id: i64, flags: AccountFlags) -> Account {
    Account {
        id: AccountId::new(id),
        version: 1,
        flags,
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

/// A ledger with accounts A, B, fee, external; A holds 100 EUR + 1 BTC, B holds
/// 10 BTC + 1 EUR.
async fn setup() -> Arc<Ledger> {
    let ledger = Arc::new(Ledger::new(InMemoryStore::new()));
    for id in [1, 2, 3] {
        ledger
            .store()
            .create_account(make_account(id, AccountFlags::DEBIT_MUST_NOT_EXCEED_CREDIT))
            .await
            .unwrap();
    }
    ledger
        .store()
        .create_account(make_account(99, AccountFlags::empty()))
        .await
        .unwrap();
    deposit(&ledger, a(), eur(), 100).await;
    deposit(&ledger, a(), btc(), 1).await;
    deposit(&ledger, b(), btc(), 10).await;
    deposit(&ledger, b(), eur(), 1).await;
    ledger
}

fn trade() -> Transfer {
    TransferBuilder::new()
        .pay(a(), b(), eur(), Cent::from(100))
        .pay(b(), a(), btc(), Cent::from(10))
        .pay(a(), fee(), btc(), Cent::from(1))
        .pay(b(), fee(), eur(), Cent::from(1))
        .build()
}

async fn bal(ledger: &Arc<Ledger>, account: AccountId, asset: AssetId) -> Cent {
    ledger.balance(&account, &asset).await.unwrap()
}

/// A one-movement confirm set, built with the same `.pay()` interface as a
/// transfer: `from` is the leg's funder, `to` its destination.
fn confirm_one(from: AccountId, to: AccountId, asset: AssetId, amount: i64) -> Transfer {
    TransferBuilder::new()
        .pay(from, to, asset, Cent::from(amount))
        .build()
}

/// After authorize, funds leave the payers and sit in the holds; the payers'
/// balances drop to zero and nothing has reached the destinations yet.
#[tokio::test]
async fn authorize_parks_funds_in_holds() {
    let ledger = setup().await;
    let auth = ledger.authorize(trade()).await.unwrap();

    // Payers emptied.
    assert_eq!(bal(&ledger, a(), eur()).await, Cent::ZERO);
    assert_eq!(bal(&ledger, a(), btc()).await, Cent::ZERO);
    assert_eq!(bal(&ledger, b(), eur()).await, Cent::ZERO);
    assert_eq!(bal(&ledger, b(), btc()).await, Cent::ZERO);

    // Destinations untouched.
    assert_eq!(bal(&ledger, b(), eur()).await, Cent::ZERO);
    assert_eq!(bal(&ledger, fee(), eur()).await, Cent::ZERO);

    // Three holds are open, and status reports everything Held.
    assert_eq!(ledger.list_open_inflights().await.unwrap().len(), 3);
    let status = ledger.inflight_status(&auth.inflight).await.unwrap();
    assert_eq!(status.state, InflightState::Held);
    let total_held: Cent = Cent::checked_sum(status.legs.iter().map(|l| l.held)).unwrap();
    let total_auth: Cent = Cent::checked_sum(status.legs.iter().map(|l| l.authorized)).unwrap();
    assert_eq!(total_held, total_auth);
}

/// Confirming the whole transaction settles every leg to its destination and
/// closes the holds. The net result equals the original trade.
#[tokio::test]
async fn confirm_all_settles_to_destinations() {
    let ledger = setup().await;
    let auth = ledger.authorize(trade()).await.unwrap();

    ledger.confirm_all(&auth.inflight).await.unwrap();

    assert_eq!(bal(&ledger, b(), eur()).await, Cent::from(100));
    assert_eq!(bal(&ledger, a(), btc()).await, Cent::from(10));
    assert_eq!(bal(&ledger, fee(), eur()).await, Cent::from(1));
    assert_eq!(bal(&ledger, fee(), btc()).await, Cent::from(1));

    // Holds drained and closed.
    assert!(ledger.list_open_inflights().await.unwrap().is_empty());
    let status = ledger.inflight_status(&auth.inflight).await.unwrap();
    assert_eq!(status.state, InflightState::Confirmed);
}

/// Voiding returns every held posting to the funder recorded in the leg table,
/// including the multi-asset fee hold funded by two different accounts.
#[tokio::test]
async fn void_returns_funds_to_funders() {
    let ledger = setup().await;
    let auth = ledger.authorize(trade()).await.unwrap();

    ledger.void(&auth.inflight).await.unwrap();

    // Everyone is back where they started.
    assert_eq!(bal(&ledger, a(), eur()).await, Cent::from(100));
    assert_eq!(bal(&ledger, a(), btc()).await, Cent::from(1));
    assert_eq!(bal(&ledger, b(), btc()).await, Cent::from(10));
    assert_eq!(bal(&ledger, b(), eur()).await, Cent::from(1));
    assert_eq!(bal(&ledger, fee(), eur()).await, Cent::ZERO);
    assert_eq!(bal(&ledger, fee(), btc()).await, Cent::ZERO);

    assert!(ledger.list_open_inflights().await.unwrap().is_empty());
    let status = ledger.inflight_status(&auth.inflight).await.unwrap();
    assert_eq!(status.state, InflightState::Voided);
}

/// A partial confirm delivers a slice and leaves the remainder held. Confirming
/// the rest drains and closes the hold.
#[tokio::test]
async fn partial_confirm_then_confirm_remainder() {
    let ledger = setup().await;
    let auth = ledger.authorize(trade()).await.unwrap();

    ledger
        .confirm(&auth.inflight, confirm_one(a(), b(), eur(), 40))
        .await
        .unwrap();
    assert_eq!(bal(&ledger, b(), eur()).await, Cent::from(40));

    // The B/EUR leg is partially confirmed.
    let status = ledger.inflight_status(&auth.inflight).await.unwrap();
    let leg = status
        .legs
        .iter()
        .find(|l| l.destination.id == b().id && l.asset == eur())
        .unwrap();
    assert_eq!(leg.authorized, Cent::from(100));
    assert_eq!(leg.confirmed, Cent::from(40));
    assert_eq!(leg.held, Cent::from(60));
    assert_eq!(status.state, InflightState::PartiallyConfirmed);

    // Confirm the rest.
    ledger
        .confirm(&auth.inflight, confirm_one(a(), b(), eur(), 60))
        .await
        .unwrap();
    assert_eq!(bal(&ledger, b(), eur()).await, Cent::from(100));
    // The B hold is now closed (its only asset drained).
    assert!(
        !ledger
            .list_open_inflights()
            .await
            .unwrap()
            .contains(&leg.hold)
    );
}

/// A partial confirm followed by a void: the slice reaches the destination and
/// the remainder returns to the funder.
#[tokio::test]
async fn partial_confirm_then_void_remainder() {
    let ledger = setup().await;
    let auth = ledger.authorize(trade()).await.unwrap();

    ledger
        .confirm(&auth.inflight, confirm_one(a(), b(), eur(), 40))
        .await
        .unwrap();
    ledger.void(&auth.inflight).await.unwrap();

    // B kept the confirmed 40 EUR from its own hold, and got its 1 EUR fee
    // contribution back from the (now voided) fee hold: 41 total. A got the
    // remaining 60 EUR of B's hold back.
    assert_eq!(bal(&ledger, b(), eur()).await, Cent::from(41));
    assert_eq!(bal(&ledger, a(), eur()).await, Cent::from(60));

    let status = ledger.inflight_status(&auth.inflight).await.unwrap();
    let leg = status
        .legs
        .iter()
        .find(|l| l.destination.id == b().id && l.asset == eur())
        .unwrap();
    assert_eq!(leg.confirmed, Cent::from(40));
    assert_eq!(leg.voided, Cent::from(60));
    assert_eq!(leg.held, Cent::ZERO);
    assert_eq!(status.state, InflightState::Mixed);
}

/// Confirming more than is held is rejected. The `NoOverdraft` hold makes
/// over-confirmation impossible.
#[tokio::test]
async fn over_confirm_is_rejected() {
    let ledger = setup().await;
    let auth = ledger.authorize(trade()).await.unwrap();

    let err = ledger
        .confirm(&auth.inflight, confirm_one(a(), b(), eur(), 101))
        .await
        .unwrap_err();
    assert!(matches!(err, LedgerError::Selection(_)));
    // Nothing moved.
    assert_eq!(bal(&ledger, b(), eur()).await, Cent::ZERO);
}

/// A single confirm call settles several legs at once, built with the same
/// `.pay()` interface as a transfer.
#[tokio::test]
async fn batch_confirm_multiple_legs() {
    let ledger = setup().await;
    let auth = ledger.authorize(trade()).await.unwrap();

    // Confirm B's EUR leg and A's BTC leg in one call.
    let confirms = TransferBuilder::new()
        .pay(a(), b(), eur(), Cent::from(100))
        .pay(b(), a(), btc(), Cent::from(10))
        .build();
    let receipts = ledger.confirm(&auth.inflight, confirms).await.unwrap();
    assert_eq!(receipts.len(), 2);

    assert_eq!(bal(&ledger, b(), eur()).await, Cent::from(100));
    assert_eq!(bal(&ledger, a(), btc()).await, Cent::from(10));
    // The fee hold is untouched, so it is still open.
    assert_eq!(bal(&ledger, fee(), eur()).await, Cent::ZERO);
    assert_eq!(bal(&ledger, fee(), btc()).await, Cent::ZERO);
    assert_eq!(ledger.list_open_inflights().await.unwrap().len(), 1);

    let status = ledger.inflight_status(&auth.inflight).await.unwrap();
    assert_eq!(status.state, InflightState::PartiallyConfirmed);
}

/// Confirming a movement whose `(from, to, asset)` matches no leg is rejected.
#[tokio::test]
async fn confirm_unknown_leg_is_rejected() {
    let ledger = setup().await;
    let auth = ledger.authorize(trade()).await.unwrap();

    // fee never funded a BTC leg to B.
    let err = ledger
        .confirm(&auth.inflight, confirm_one(fee(), b(), btc(), 1))
        .await
        .unwrap_err();
    assert!(matches!(err, LedgerError::InflightLegNotFound { .. }));
}

/// A destination can hold several concurrent inflights (one per distinct trade,
/// each under its own subaccount), but the *same* trade cannot be authorized
/// twice while open (its holds already exist).
#[tokio::test]
async fn concurrent_inflights_per_account() {
    let ledger = setup().await;
    let auth = ledger.authorize(trade()).await.unwrap();

    // Re-authorizing the identical trade collides on the derived hold subaccount.
    let err = ledger.authorize(trade()).await.unwrap_err();
    assert!(matches!(err, LedgerError::InflightAlreadyOpen(_)));

    // A different trade to the same destination B opens a second, independent
    // inflight under a different subaccount.
    deposit(&ledger, a(), eur(), 10).await;
    let other = TransferBuilder::new()
        .pay(a(), b(), eur(), Cent::from(10))
        .build();
    let auth2 = ledger.authorize(other).await.unwrap();
    assert_ne!(auth.inflight, auth2.inflight);

    // Both are open at once: B has two inflight holds under distinct subaccounts.
    let b_holds = ledger
        .list_subaccounts(&b())
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.sub != 0)
        .count();
    assert_eq!(b_holds, 2);
}

/// After a full confirm closes the holds, a fresh inflight to the same
/// destinations is allowed again.
#[tokio::test]
async fn reauthorize_after_settlement() {
    let ledger = setup().await;
    let auth = ledger.authorize(trade()).await.unwrap();
    ledger.confirm_all(&auth.inflight).await.unwrap();

    // B now holds 100 EUR; authorize a new hold of 30 of it to fee.
    let again = TransferBuilder::new()
        .pay(b(), fee(), eur(), Cent::from(30))
        .build();
    let auth2 = ledger.authorize(again).await.unwrap();
    assert_eq!(bal(&ledger, b(), eur()).await, Cent::from(70));
    ledger.confirm_all(&auth2.inflight).await.unwrap();
    assert_eq!(bal(&ledger, fee(), eur()).await, Cent::from(31));
}

/// Operating on a non-inflight or unknown transfer id is a clean error.
#[tokio::test]
async fn unknown_inflight_is_an_error() {
    let ledger = setup().await;
    let bogus = EnvelopeId([7u8; 32]);
    assert!(matches!(
        ledger.confirm_all(&bogus).await.unwrap_err(),
        LedgerError::InflightNotFound(_)
    ));
}

/// Balances are always segregated by subaccount: the account query lists the
/// main subaccount and each open hold separately, never summed.
#[tokio::test]
async fn balances_are_segregated_by_subaccount() {
    let ledger = setup().await;
    let _auth = ledger.authorize(trade()).await.unwrap();

    // B's EUR across subaccounts: the main (0, now empty) and its inflight hold.
    let all = ledger.balances(&b(), &eur(), None).await.unwrap();
    let main = all.iter().find(|e| e.account.sub == 0).unwrap();
    assert_eq!(main.value, Cent::ZERO); // B's own 1 EUR went into the fee hold
    let hold = all.iter().find(|e| e.account.sub != 0).unwrap();
    assert_eq!(hold.value, Cent::from(100)); // A's 100 EUR parked for B
    assert_eq!(all.len(), 2);

    // Filtering to the main subaccount returns only it (still segregated form).
    let only_main = ledger.balances(&b(), &eur(), Some(0)).await.unwrap();
    assert_eq!(only_main.len(), 1);
    assert_eq!(only_main[0].account.sub, 0);
}
