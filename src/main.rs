use std::sync::Arc;

use actix_web::{web, App, HttpServer};

mod api;
mod config;
mod db;
mod error;
mod models;
mod observability;
mod notify;
mod petroapp_client;
mod scheduler;
mod sync;

use config::Config;
use petroapp_client::PetroClient;

/// Shared application state.
pub struct AppState {
    pub pool: sqlx::PgPool,
    pub cfg: Config,
    pub petro: PetroClient,
    /// General-purpose client used for notifications (separate from the PetroApp client).
    pub http: reqwest::Client,
}

#[actix_web::main]
async fn main() -> anyhow::Result<()> {
    // Registry rather than fmt(), so the Sentry layer can carry a filter of its
    // own. The console keeps sqlx=warn — query logs would drown it — while
    // Sentry sees sqlx at debug, which is what puts the last queries and their
    // elapsed times on an error event.
    {
        use tracing_subscriber::prelude::*;
        let console = tracing_subscriber::fmt::layer().with_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,sqlx=warn")),
        );
        let sentry_layer = sentry_tracing::layer().with_filter(
            tracing_subscriber::EnvFilter::try_from_env("SENTRY_TRACING_FILTER")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,sqlx=debug")),
        );
        tracing_subscriber::registry()
            .with(sentry_layer)
            .with(console)
            .init();
    }

    // Sentry, if SENTRY_DSN is set. The guard must outlive the server; with no
    // DSN this is None and the service runs exactly as it did before.
    let _sentry = observability::init();

    let cfg = Config::from_env()?;
    let pool = db::connect_and_migrate(&cfg.database_url).await?;
    let petro = PetroClient::new(&cfg)?;
    let http = reqwest::Client::builder()
        .timeout(cfg.http_timeout)
        .build()?;

    let bind_addr = cfg.bind_addr.clone();
    let state = Arc::new(AppState {
        pool,
        cfg,
        petro,
        http,
    });

    // Background sync loop.
    scheduler::spawn(state.clone());

    tracing::info!(%bind_addr, "apex-petroapp listening");
    let data = web::Data::from(state);
    HttpServer::new(move || {
        App::new()
            .app_data(data.clone())
            // 5xx only. A client sending a bad request is not a bug in this
            // service, and 4xx volume would bury the ones that are. Everything
            // attached here passes through `observability`'s scrubber first.
            .wrap(
                sentry_actix::Sentry::builder()
                    .capture_server_errors(true)
                    // Transactions, so upstream latency and throughput are
                    // visible and not just outright failures.
                    .start_transaction(true)
                    .finish(),
            )
            .configure(api::configure)
    })
    .bind(&bind_addr)?
    .run()
    .await?;

    Ok(())
}
