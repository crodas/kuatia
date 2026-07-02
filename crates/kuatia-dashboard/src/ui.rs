//! Server-rendered HTML views (Tera templates) with htmx-driven live refresh.
//!
//! Full-page routes render the whole document; `/ui/*` routes render just the
//! dynamic fragment that htmx polls and swaps in place. Both read from the same
//! [`crate::data`] builders as the JSON API, then format money and timestamps
//! for display. Templates and static assets are embedded in the binary, so the
//! server needs no files on disk at runtime.

use axum::{
    Router,
    extract::{Path, State},
    http::header,
    response::{Html, IntoResponse, Response},
    routing::get,
};
use kuatia_core::{Amount, AssetId, Cent};
use serde::Serialize;
use tera::{Context, Tera};

use crate::assets::AssetMeta;
use crate::data::{
    self, AccountDto, ApiError, AppState, EventDto, OverviewDto, PostingDto, TransferDto,
};

// ---------------------------------------------------------------------------
// Tera setup — templates embedded via include_str!.
// ---------------------------------------------------------------------------

/// Build the Tera instance with all templates registered by name.
pub fn build_tera() -> Result<Tera, tera::Error> {
    let mut tera = Tera::default();
    tera.add_raw_templates(vec![
        ("base.html", include_str!("../templates/base.html")),
        (
            "pages/overview.html",
            include_str!("../templates/pages/overview.html"),
        ),
        (
            "pages/accounts.html",
            include_str!("../templates/pages/accounts.html"),
        ),
        (
            "pages/account.html",
            include_str!("../templates/pages/account.html"),
        ),
        (
            "pages/transfers.html",
            include_str!("../templates/pages/transfers.html"),
        ),
        (
            "pages/events.html",
            include_str!("../templates/pages/events.html"),
        ),
        (
            "partials/overview.html",
            include_str!("../templates/partials/overview.html"),
        ),
        (
            "partials/accounts.html",
            include_str!("../templates/partials/accounts.html"),
        ),
        (
            "partials/account.html",
            include_str!("../templates/partials/account.html"),
        ),
        (
            "partials/transfers.html",
            include_str!("../templates/partials/transfers.html"),
        ),
        (
            "partials/events.html",
            include_str!("../templates/partials/events.html"),
        ),
    ])?;
    Ok(tera)
}

/// Build the UI router: full pages, `/ui/*` htmx partials, and `/static/*`.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(overview_page))
        .route("/accounts", get(accounts_page))
        .route("/accounts/{id}", get(account_page))
        .route("/transfers", get(transfers_page))
        .route("/events", get(events_page))
        .route("/ui/overview", get(overview_partial))
        .route("/ui/accounts", get(accounts_partial))
        .route("/ui/accounts/{id}", get(account_partial))
        .route("/ui/transfers", get(transfers_partial))
        .route("/ui/events", get(events_partial))
        .route("/static/dashboard.css", get(css))
        .route("/static/htmx.min.js", get(htmx_js))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// View models — DTOs with money and timestamps pre-formatted for display.
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct MoneyView {
    text: String,
    negative: bool,
}

#[derive(Serialize)]
struct BalanceView {
    code: String,
    money: MoneyView,
}

#[derive(Serialize)]
struct AccountView {
    id: i64,
    name: String,
    version: u64,
    policy_kind: &'static str,
    floor: Option<MoneyView>,
    frozen: bool,
    closed: bool,
    balances: Vec<BalanceView>,
}

#[derive(Serialize)]
struct PostingView {
    short_id: String,
    status: String,
    money: MoneyView,
}

#[derive(Serialize)]
struct LegView {
    to_name: String,
    from_name: Option<String>,
    is_change: bool,
    money: MoneyView,
}

#[derive(Serialize)]
struct TransferView {
    short_id: String,
    full_id: String,
    time: String,
    consumes: usize,
    legs: Vec<LegView>,
}

#[derive(Serialize)]
struct EventView {
    seq: u64,
    kind: &'static str,
    account: Option<i64>,
    transfer_short: Option<String>,
    time: String,
}

#[derive(Serialize)]
struct IssuedView {
    code: String,
    money: MoneyView,
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

/// Look up asset metadata by id.
fn asset_of(assets: &[AssetMeta], id: AssetId) -> Option<&AssetMeta> {
    assets.iter().find(|a| a.id == id)
}

/// Format a signed [`Cent`] with the given decimals and symbol, grouping the
/// integer part with thousands separators.
fn fmt(value: Cent, decimals: u8, symbol: &str) -> MoneyView {
    let raw = Amount::new(decimals).format(value); // e.g. "-1234567.89" or "1000"
    let negative = raw.starts_with('-');
    let unsigned = raw.trim_start_matches('-');
    let (whole, frac) = match unsigned.split_once('.') {
        Some((w, f)) => (w, Some(f)),
        None => (unsigned, None),
    };
    let grouped = group_thousands(whole);
    let body = match frac {
        Some(f) => format!("{grouped}.{f}"),
        None => grouped,
    };
    let sign = if negative { "-" } else { "" };
    MoneyView {
        text: format!("{sign}{symbol}{body}"),
        negative,
    }
}

/// Format using an asset's decimals/symbol, or fall back to the raw value.
fn fmt_asset(value: Cent, asset: Option<&AssetMeta>) -> MoneyView {
    match asset {
        Some(a) => fmt(value, a.decimals, a.symbol),
        None => MoneyView {
            text: value.to_string(),
            negative: value.to_string().starts_with('-'),
        },
    }
}

/// Insert commas every three digits from the right.
fn group_thousands(digits: &str) -> String {
    let bytes = digits.as_bytes();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    let len = bytes.len();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// A short hex form of an id for compact display.
fn short_hex(hex: &str) -> String {
    if hex.len() <= 14 {
        return hex.to_string();
    }
    format!("{}…{}", &hex[..8], &hex[hex.len() - 6..])
}

/// Format unix milliseconds as `YYYY-MM-DD HH:MM:SS UTC` without a date crate.
fn fmt_millis(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (h, m, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    let (y, mo, d) = civil_from_days(days);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}:{s:02} UTC")
}

/// Days since the Unix epoch to a civil (year, month, day). Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The symbol/decimals to render an overdraft floor, which carries no asset.
/// Use the first two-decimal asset (fiat) if present.
fn floor_style(assets: &[AssetMeta]) -> (u8, &str) {
    assets
        .iter()
        .find(|a| a.decimals == 2)
        .map(|a| (a.decimals, a.symbol))
        .unwrap_or((2, ""))
}

// ---------------------------------------------------------------------------
// View builders — DTO -> view model.
// ---------------------------------------------------------------------------

fn account_view(dto: &AccountDto, assets: &[AssetMeta]) -> AccountView {
    let (floor_dec, floor_sym) = floor_style(assets);
    AccountView {
        id: dto.id.0,
        name: dto
            .label
            .map(String::from)
            .unwrap_or_else(|| format!("#{}", dto.id.0)),
        version: dto.version,
        policy_kind: dto.policy.kind,
        floor: dto.policy.floor.map(|f| fmt(f, floor_dec, floor_sym)),
        frozen: dto.frozen,
        closed: dto.closed,
        balances: dto
            .balances
            .iter()
            .map(|b| BalanceView {
                code: asset_of(assets, b.asset)
                    .map(|a| a.code.to_string())
                    .unwrap_or_default(),
                money: fmt_asset(b.value, asset_of(assets, b.asset)),
            })
            .collect(),
    }
}

fn transfer_view(dto: &TransferDto, assets: &[AssetMeta]) -> TransferView {
    TransferView {
        short_id: short_hex(&dto.id),
        full_id: dto.id.clone(),
        time: fmt_millis(dto.created_at),
        consumes: dto.consumes,
        legs: dto
            .legs
            .iter()
            .map(|leg| LegView {
                to_name: leg
                    .label
                    .map(String::from)
                    .unwrap_or_else(|| format!("#{}", leg.owner.0)),
                from_name: leg.payer.map(|p| {
                    leg.payer_label
                        .map(String::from)
                        .unwrap_or_else(|| format!("#{}", p.0))
                }),
                is_change: leg.payer.is_none(),
                money: fmt_asset(leg.value, asset_of(assets, leg.asset)),
            })
            .collect(),
    }
}

fn posting_view(dto: &PostingDto, assets: &[AssetMeta]) -> PostingView {
    PostingView {
        short_id: short_hex(&dto.id),
        status: dto.status.clone(),
        money: fmt_asset(dto.value, asset_of(assets, dto.asset)),
    }
}

fn event_view(dto: &EventDto) -> EventView {
    EventView {
        seq: dto.seq,
        kind: dto.kind,
        account: dto.account.map(|a| a.0),
        transfer_short: dto.transfer.as_deref().map(short_hex),
        time: fmt_millis(dto.timestamp),
    }
}

fn issued_view(dto: &OverviewDto, assets: &[AssetMeta]) -> Vec<IssuedView> {
    dto.issued
        .iter()
        .map(|i| IssuedView {
            code: asset_of(assets, i.asset)
                .map(|a| a.code.to_string())
                .unwrap_or_default(),
            money: fmt_asset(i.issued, asset_of(assets, i.asset)),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Context builders
// ---------------------------------------------------------------------------

async fn overview_ctx(state: &AppState) -> Result<Context, ApiError> {
    let dto = data::overview(state).await?;
    let mut ctx = Context::new();
    ctx.insert("nav", "overview");
    ctx.insert("accounts_count", &dto.accounts);
    ctx.insert("transfers_count", &dto.transfers);
    ctx.insert("assets_count", &dto.assets);
    ctx.insert("issued", &issued_view(&dto, &state.assets));
    Ok(ctx)
}

async fn accounts_ctx(state: &AppState) -> Result<Context, ApiError> {
    let dtos = data::accounts(state).await?;
    let views: Vec<AccountView> = dtos
        .iter()
        .map(|a| account_view(a, &state.assets))
        .collect();
    let mut ctx = Context::new();
    ctx.insert("nav", "accounts");
    ctx.insert("accounts", &views);
    Ok(ctx)
}

async fn account_ctx(state: &AppState, id: i64) -> Result<Context, ApiError> {
    let dto = data::account_detail(state, kuatia_core::AccountId::new(id)).await?;
    let mut ctx = Context::new();
    ctx.insert("nav", "accounts");
    ctx.insert("account", &account_view(&dto.account, &state.assets));
    ctx.insert(
        "postings",
        &dto.postings
            .iter()
            .map(|p| posting_view(p, &state.assets))
            .collect::<Vec<_>>(),
    );
    ctx.insert(
        "transfers",
        &dto.transfers
            .iter()
            .map(|t| transfer_view(t, &state.assets))
            .collect::<Vec<_>>(),
    );
    Ok(ctx)
}

async fn transfers_ctx(state: &AppState) -> Result<Context, ApiError> {
    let dtos = data::transfers(state, None).await?;
    let views: Vec<TransferView> = dtos
        .iter()
        .map(|t| transfer_view(t, &state.assets))
        .collect();
    let mut ctx = Context::new();
    ctx.insert("nav", "transfers");
    ctx.insert("transfers", &views);
    Ok(ctx)
}

async fn events_ctx(state: &AppState) -> Result<Context, ApiError> {
    let dtos = data::events(state, 0, 200).await?;
    // Newest first for display.
    let views: Vec<EventView> = dtos.iter().rev().map(event_view).collect();
    let mut ctx = Context::new();
    ctx.insert("nav", "events");
    ctx.insert("events", &views);
    Ok(ctx)
}

// ---------------------------------------------------------------------------
// Rendering + handlers
// ---------------------------------------------------------------------------

fn render(state: &AppState, template: &str, ctx: &Context) -> Result<Html<String>, ApiError> {
    state
        .tera
        .render(template, ctx)
        .map(Html)
        .map_err(ApiError::from_display)
}

async fn overview_page(State(state): State<AppState>) -> Result<Html<String>, ApiError> {
    render(&state, "pages/overview.html", &overview_ctx(&state).await?)
}
async fn overview_partial(State(state): State<AppState>) -> Result<Html<String>, ApiError> {
    render(
        &state,
        "partials/overview.html",
        &overview_ctx(&state).await?,
    )
}

async fn accounts_page(State(state): State<AppState>) -> Result<Html<String>, ApiError> {
    render(&state, "pages/accounts.html", &accounts_ctx(&state).await?)
}
async fn accounts_partial(State(state): State<AppState>) -> Result<Html<String>, ApiError> {
    render(
        &state,
        "partials/accounts.html",
        &accounts_ctx(&state).await?,
    )
}

async fn account_page(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Html<String>, ApiError> {
    render(
        &state,
        "pages/account.html",
        &account_ctx(&state, id).await?,
    )
}
async fn account_partial(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Html<String>, ApiError> {
    render(
        &state,
        "partials/account.html",
        &account_ctx(&state, id).await?,
    )
}

async fn transfers_page(State(state): State<AppState>) -> Result<Html<String>, ApiError> {
    render(
        &state,
        "pages/transfers.html",
        &transfers_ctx(&state).await?,
    )
}
async fn transfers_partial(State(state): State<AppState>) -> Result<Html<String>, ApiError> {
    render(
        &state,
        "partials/transfers.html",
        &transfers_ctx(&state).await?,
    )
}

async fn events_page(State(state): State<AppState>) -> Result<Html<String>, ApiError> {
    render(&state, "pages/events.html", &events_ctx(&state).await?)
}
async fn events_partial(State(state): State<AppState>) -> Result<Html<String>, ApiError> {
    render(&state, "partials/events.html", &events_ctx(&state).await?)
}

async fn css() -> Response {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../static/dashboard.css"),
    )
        .into_response()
}

async fn htmx_js() -> Response {
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        include_str!("../static/htmx.min.js"),
    )
        .into_response()
}
