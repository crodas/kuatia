//! REST API over a `Ledger`. Everything is read-only: the dashboard observes
//! the ledger, it does not mutate it. All monetary values are emitted as
//! minor-unit strings (the native `Cent` serialization); clients format them
//! using the asset registry from `/api/assets`.
//!
//! These handlers are thin wrappers over the shared builders in [`crate::data`];
//! the server-rendered HTML views in [`crate::ui`] read from the same builders.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    routing::get,
};
use kuatia_core::AccountId;
use serde::Deserialize;

use crate::assets::AssetMeta;
use crate::data::{
    AccountDetailDto, AccountDto, ApiError, AppState, EventDto, OverviewDto, TransferDto,
};

/// Build the `/api` router.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/assets", get(assets))
        .route("/overview", get(overview))
        .route("/accounts", get(accounts))
        .route("/accounts/{uuid}", get(account_detail))
        .route("/transfers", get(transfers))
        .route("/events", get(events))
        .with_state(state)
}

async fn assets(State(state): State<AppState>) -> Json<Vec<AssetMeta>> {
    Json((*state.assets).clone())
}

async fn overview(State(state): State<AppState>) -> Result<Json<OverviewDto>, ApiError> {
    Ok(Json(crate::data::overview(&state).await?))
}

async fn accounts(State(state): State<AppState>) -> Result<Json<Vec<AccountDto>>, ApiError> {
    Ok(Json(crate::data::accounts(&state).await?))
}

async fn account_detail(
    State(state): State<AppState>,
    Path(uuid): Path<String>,
) -> Result<Json<AccountDetailDto>, ApiError> {
    let id: AccountId = uuid.parse().map_err(ApiError::bad_request)?;
    Ok(Json(crate::data::account_detail(&state, id).await?))
}

#[derive(Deserialize)]
struct TransfersParams {
    limit: Option<u32>,
}

async fn transfers(
    State(state): State<AppState>,
    Query(params): Query<TransfersParams>,
) -> Result<Json<Vec<TransferDto>>, ApiError> {
    Ok(Json(crate::data::transfers(&state, params.limit).await?))
}

#[derive(Deserialize)]
struct EventsParams {
    after: Option<u64>,
    limit: Option<u32>,
}

async fn events(
    State(state): State<AppState>,
    Query(params): Query<EventsParams>,
) -> Result<Json<Vec<EventDto>>, ApiError> {
    Ok(Json(
        crate::data::events(
            &state,
            params.after.unwrap_or(0),
            params.limit.unwrap_or(200),
        )
        .await?,
    ))
}
