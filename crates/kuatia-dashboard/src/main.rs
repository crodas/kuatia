//! Kuatia dashboard server.
//!
//! Connects to a ledger database, then serves two views of it: a server-rendered
//! HTML dashboard (Tera templates, htmx live refresh) and a read-only JSON REST
//! API under `/api` for anyone who wants to build a richer client.
//!
//! ```sh
//! cargo run -p kuatia-dashboard -- --seed          # in-memory demo
//! cargo run -p kuatia-dashboard -- --db sqlite://kuatia.db --port 8080
//! # then open http://127.0.0.1:<port>
//! ```

mod api;
mod assets;
mod data;
mod seed;
mod ui;

use std::sync::Arc;

use axum::Router;
use clap::Parser;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use data::AppState;

/// Command-line options. Each flag falls back to an environment variable, then
/// a default.
#[derive(Debug, Parser)]
#[command(
    name = "kuatia-dashboard",
    about = "Server-rendered dashboard and REST API for a Kuatia ledger"
)]
struct Cli {
    /// Ledger database URL. The scheme selects the backend: `sqlite::memory:`,
    /// `sqlite://path.db`, or `postgres://user:pass@host/db`.
    #[arg(long, env = "KUATIA_DASHBOARD_DB", default_value = "sqlite::memory:")]
    db: String,

    /// Host/interface to bind.
    #[arg(long, env = "KUATIA_DASHBOARD_HOST", default_value = "127.0.0.1")]
    host: String,

    /// TCP port to listen on.
    #[arg(long, env = "KUATIA_DASHBOARD_PORT", default_value_t = 3000)]
    port: u16,

    /// Seed demo accounts and transfers if the ledger is empty. A no-op against
    /// an already-populated database.
    #[arg(long, env = "KUATIA_DASHBOARD_SEED", default_value_t = false)]
    seed: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "kuatia_dashboard=info,tower_http=info".into()),
        )
        .init();

    let cli = Cli::parse();

    let ledger = seed::connect(&cli.db).await?;
    tracing::info!("connected to ledger at {}", cli.db);
    if cli.seed {
        if seed::seed_if_empty(&ledger).await? {
            tracing::info!("seeded demo ledger");
        } else {
            tracing::info!("ledger already populated, skipping seed");
        }
    }

    let state = AppState {
        ledger,
        assets: Arc::new(assets::registry()),
        tera: Arc::new(ui::build_tera()?),
    };

    let app = Router::new()
        .merge(ui::router(state.clone()))
        .nest("/api", api::router(state))
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive());

    let addr = format!("{}:{}", cli.host, cli.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("dashboard listening on http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}
