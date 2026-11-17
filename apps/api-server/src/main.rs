use anyhow::Context;
use core_kernel::Db;
use rostwerk_api::{AuthConfig, app, registry};
use sqlx::postgres::PgPoolOptions;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,sqlx=warn")),
        )
        .init();

    let database_url = env("DATABASE_URL")?;
    let admin_url = env("DATABASE_ADMIN_URL")?;
    let bind = std::env::var("BIND").unwrap_or_else(|_| "127.0.0.1:8080".into());

    let registry = registry();

    // Migrations run as the owner role; the app itself runs as a role subject to RLS.
    let admin_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .context("connect DATABASE_ADMIN_URL")?;
    registry.migrate(&admin_pool).await?;
    admin_pool.close().await;

    let pool = PgPoolOptions::new()
        .max_connections(20)
        .connect(&database_url)
        .await
        .context("connect DATABASE_URL")?;

    let auth = AuthConfig::from_env();
    if auth.dev_tokens {
        tracing::warn!("dev tokens enabled (ROSTWERK_DEV_AUTH=1), never use in production");
    }

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(%bind, "rostwerk listening");
    axum::serve(listener, app(Db::new(pool), registry, auth)).await?;
    Ok(())
}

fn env(name: &str) -> anyhow::Result<String> {
    std::env::var(name).with_context(|| format!("{name} is not set (see .env.example)"))
}
