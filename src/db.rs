use std::time::Duration;

use sqlx::postgres::{PgPool, PgPoolOptions};

/// Connect to the shared `apex` database and apply this service's migrations.
pub async fn connect_and_migrate(database_url: &str) -> anyhow::Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .acquire_timeout(Duration::from_secs(10))
        .connect(database_url)
        .await?;

    sqlx::migrate!("./migrations").run(&pool).await?;
    tracing::info!("migrations applied");
    Ok(pool)
}
