use std::sync::Arc;

use actix_web::{web, App, HttpServer};

mod api;
mod config;
mod db;
mod error;
mod models;
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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,sqlx=warn".into()),
        )
        .init();

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
            .configure(api::configure)
    })
    .bind(&bind_addr)?
    .run()
    .await?;

    Ok(())
}
