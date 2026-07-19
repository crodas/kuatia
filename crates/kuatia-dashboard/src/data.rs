//! Shared data layer. Reads the ledger and builds the DTOs consumed by both the
//! JSON API ([`crate::api`]) and the server-rendered HTML views ([`crate::ui`]).
//! Everything here is read-only. Monetary values stay as raw [`Cent`] (minor
//! units); presentation formats them.

use std::sync::Arc;

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use kuatia::ledger::Ledger;
use kuatia_core::{Account, AccountId, AssetId, Cent, PostingId, PostingState};
use kuatia_storage::events::{LedgerEvent, LedgerEventKind};
use kuatia_storage::store::{EnvelopeRecord, TransferQuery};
use serde::Serialize;
use tera::Tera;

use crate::assets::AssetMeta;
use crate::seed::account_label;

/// Shared handler state.
#[derive(Clone)]
pub struct AppState {
    pub ledger: Arc<Ledger>,
    pub assets: Arc<Vec<AssetMeta>>,
    pub tera: Arc<Tera>,
}

// ---------------------------------------------------------------------------
// DTOs
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct BalanceDto {
    pub asset: AssetId,
    pub value: Cent,
}

#[derive(Serialize)]
pub struct AccountDto {
    pub id: AccountId,
    /// IBAN-style account code (machine format, checksum-valid) for the full id.
    pub code: String,
    /// Subaccount id (`0` is the main account). Mirrors `id.sub` for templates.
    pub sub: i64,
    pub label: Option<&'static str>,
    pub version: u64,
    /// Whether the account carries the `DEBIT_MUST_NOT_EXCEED_CREDIT` flag
    /// (balance may not go negative). When `false` the account may overdraw.
    pub debit_must_not_exceed_credit: bool,
    pub frozen: bool,
    pub closed: bool,
    pub balances: Vec<BalanceDto>,
}

#[derive(Serialize)]
pub struct PostingDto {
    pub id: String,
    pub owner: AccountId,
    pub asset: AssetId,
    pub value: Cent,
    pub status: String,
}

#[derive(Serialize)]
pub struct TransferLegDto {
    pub owner: AccountId,
    pub label: Option<&'static str>,
    pub asset: AssetId,
    pub value: Cent,
    pub payer: Option<AccountId>,
    pub payer_label: Option<&'static str>,
}

#[derive(Serialize)]
pub struct TransferDto {
    pub id: String,
    pub created_at: i64,
    pub consumes: usize,
    pub legs: Vec<TransferLegDto>,
}

#[derive(Serialize)]
pub struct EventDto {
    pub seq: u64,
    pub timestamp: i64,
    pub kind: &'static str,
    pub account: Option<AccountId>,
    pub transfer: Option<String>,
}

#[derive(Serialize)]
pub struct IssuedDto {
    pub asset: AssetId,
    pub issued: Cent,
}

#[derive(Serialize)]
pub struct OverviewDto {
    pub accounts: usize,
    pub transfers: u64,
    pub assets: usize,
    pub issued: Vec<IssuedDto>,
}

#[derive(Serialize)]
pub struct AccountDetailDto {
    pub account: AccountDto,
    /// The non-closed subaccounts sharing this account's base id (the viewed
    /// account included), so a base account and its subaccounts are navigable
    /// together while never summed.
    pub subaccounts: Vec<AccountDto>,
    pub postings: Vec<PostingDto>,
    pub transfers: Vec<TransferDto>,
}

// ---------------------------------------------------------------------------
// Conversions
// ---------------------------------------------------------------------------

fn hex32(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn posting_id(id: &PostingId) -> String {
    format!("{}:{}", hex32(&id.transfer.0), id.index)
}

async fn account_dto(state: &AppState, account: &Account) -> Result<AccountDto, ApiError> {
    let mut balances = Vec::new();
    for asset in state.assets.iter() {
        let value = state.ledger.balance(&account.id, &asset.id).await?;
        // Emit only non-zero balances; a zero renders as the string "0".
        if value.to_string() != "0" {
            balances.push(BalanceDto {
                asset: asset.id,
                value,
            });
        }
    }
    Ok(AccountDto {
        id: account.id,
        code: account.id.to_string(),
        sub: account.id.sub,
        label: account_label(account.id),
        version: account.version,
        debit_must_not_exceed_credit: account.forbids_overdraft(),
        frozen: account.is_frozen(),
        closed: account.is_closed(),
        balances,
    })
}

fn transfer_dto(record: &EnvelopeRecord) -> TransferDto {
    let legs = record
        .envelope
        .creates
        .iter()
        .map(|p| TransferLegDto {
            owner: p.owner,
            label: account_label(p.owner),
            asset: p.asset,
            value: p.value,
            payer: p.payer,
            payer_label: p.payer.and_then(account_label),
        })
        .collect();
    TransferDto {
        id: hex32(&record.receipt.transfer_id.0),
        created_at: record.created_at,
        consumes: record.envelope.consumes.len(),
        legs,
    }
}

fn event_dto(event: &LedgerEvent) -> EventDto {
    let (kind, account, transfer) = match &event.kind {
        LedgerEventKind::TransferCommitted { transfer_id } => {
            ("TransferCommitted", None, Some(hex32(&transfer_id.0)))
        }
        LedgerEventKind::AccountCreated { account_id } => {
            ("AccountCreated", Some(*account_id), None)
        }
        LedgerEventKind::AccountFrozen { account_id, .. } => {
            ("AccountFrozen", Some(*account_id), None)
        }
        LedgerEventKind::AccountUnfrozen { account_id, .. } => {
            ("AccountUnfrozen", Some(*account_id), None)
        }
        LedgerEventKind::AccountClosed { account_id, .. } => {
            ("AccountClosed", Some(*account_id), None)
        }
    };
    EventDto {
        seq: event.seq,
        timestamp: event.timestamp,
        kind,
        account,
        transfer,
    }
}

// ---------------------------------------------------------------------------
// Builders — read the ledger and assemble DTOs.
// ---------------------------------------------------------------------------

/// Ledger-wide summary: counts and total issued per asset.
pub async fn overview(state: &AppState) -> Result<OverviewDto, ApiError> {
    let accounts = state.ledger.list_accounts().await?;
    let page = state
        .ledger
        .query_transfers(&TransferQuery::default())
        .await?;

    // Total issued per asset = the negative of the external account's balance;
    // deposits push the offset (negative) side onto External, so its balance
    // mirrors everything in circulation.
    let mut issued = Vec::new();
    for asset in state.assets.iter() {
        let external = state
            .ledger
            .balance(&crate::seed::EXTERNAL, &asset.id)
            .await?;
        let issued_value = external
            .checked_neg()
            .map_err(|_| ApiError::internal("overflow"))?;
        if issued_value.to_string() != "0" {
            issued.push(IssuedDto {
                asset: asset.id,
                issued: issued_value,
            });
        }
    }

    Ok(OverviewDto {
        accounts: accounts.len(),
        transfers: page.total,
        assets: state.assets.len(),
        issued,
    })
}

/// Every account (sorted by id) with its balances.
pub async fn accounts(state: &AppState) -> Result<Vec<AccountDto>, ApiError> {
    let mut accounts = state.ledger.list_accounts().await?;
    accounts.sort_by_key(|a| (a.id.id, a.id.sub));
    let mut out = Vec::with_capacity(accounts.len());
    for account in &accounts {
        out.push(account_dto(state, account).await?);
    }
    Ok(out)
}

/// Human-readable label for a posting's derived lifecycle state.
fn posting_state_label(state: &PostingState) -> &'static str {
    match state {
        PostingState::Active => "Active",
        PostingState::Reserved(_) => "Reserved",
        PostingState::Spent => "Spent",
        PostingState::Missing => "Missing",
    }
}

/// One account with its postings (largest first) and the transfers it took part
/// in.
pub async fn account_detail(state: &AppState, id: AccountId) -> Result<AccountDetailDto, ApiError> {
    let account = state.ledger.get_account(&id).await?;

    let mut postings: Vec<PostingDto> = state
        .ledger
        .postings_with_state(&id)
        .await?
        .iter()
        .map(|(p, state)| PostingDto {
            id: posting_id(&p.id),
            owner: p.owner,
            asset: p.asset,
            value: p.value,
            status: posting_state_label(state).to_string(),
        })
        .collect();
    postings.sort_by_key(|p| std::cmp::Reverse(p.value));

    let transfers = state
        .ledger
        .history(&id)
        .await?
        .iter()
        .map(transfer_dto)
        .collect();

    // The base account and its non-closed subaccounts, so the detail page can
    // list every partition with its own (segregated) balances.
    let mut subaccounts = Vec::new();
    for sub_id in state.ledger.list_subaccounts(&id).await? {
        let sub_account = state.ledger.get_account(&sub_id).await?;
        subaccounts.push(account_dto(state, &sub_account).await?);
    }

    Ok(AccountDetailDto {
        account: account_dto(state, &account).await?,
        subaccounts,
        postings,
        transfers,
    })
}

/// Recent transfers, newest first.
pub async fn transfers(state: &AppState, limit: Option<u32>) -> Result<Vec<TransferDto>, ApiError> {
    let query = TransferQuery {
        limit: limit.or(Some(100)),
        ..Default::default()
    };
    let page = state.ledger.query_transfers(&query).await?;
    let mut out: Vec<TransferDto> = page.items.iter().map(transfer_dto).collect();
    out.sort_by_key(|t| std::cmp::Reverse(t.created_at));
    Ok(out)
}

/// Ledger events after `after`, oldest first (as stored).
pub async fn events(state: &AppState, after: u64, limit: u32) -> Result<Vec<EventDto>, ApiError> {
    let events = state.ledger.get_events_since(after, limit).await?;
    Ok(events.iter().map(event_dto).collect())
}

// ---------------------------------------------------------------------------
// Error handling
// ---------------------------------------------------------------------------

/// Any handler failure. Rendered as a JSON error body with a 500 (or 404 for a
/// missing account). Shared by the JSON and HTML handlers.
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }

    /// Build a 500 from any displayable error (used by the HTML render path).
    pub fn from_display(err: impl std::fmt::Display) -> Self {
        Self::internal(err.to_string())
    }

    /// Build a 400 from a displayable error (used for a malformed account id in
    /// the URL).
    pub fn bad_request(err: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: err.to_string(),
        }
    }
}

impl From<kuatia::error::LedgerError> for ApiError {
    fn from(err: kuatia::error::LedgerError) -> Self {
        use kuatia::error::LedgerError;
        let status = match err {
            LedgerError::AccountNotFound(_) => StatusCode::NOT_FOUND,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self {
            status,
            message: err.to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({ "error": self.message })),
        )
            .into_response()
    }
}
