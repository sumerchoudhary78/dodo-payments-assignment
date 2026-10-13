//! Demo data for `docker compose up` (decisions.md D5): a business with a known API key and,
//! optionally, a webhook endpoint with a known secret. Only runs when the env vars are set,
//! and is safe to run on every startup.

use sqlx::PgPool;

use crate::{auth, config::Config};

pub async fn seed_demo(db: &PgPool, config: &Config) -> sqlx::Result<()> {
    let Some(key) = &config.seed_api_key else { return Ok(()) };

    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM api_keys WHERE key_hash = $1)")
        .bind(auth::hash_key(key))
        .fetch_one(db)
        .await?;
    if exists {
        return Ok(());
    }

    let business_id = auth::create_business(db, "Demo Business", key).await?;
    tracing::info!(%business_id, "seeded demo business from SEED_API_KEY");

    if let (Some(url), Some(secret)) = (&config.seed_webhook_url, &config.seed_webhook_secret) {
        sqlx::query("INSERT INTO webhook_endpoints (business_id, url, secret) VALUES ($1, $2, $3)")
            .bind(business_id)
            .bind(url)
            .bind(secret)
            .execute(db)
            .await?;
        tracing::info!(%url, "seeded demo webhook endpoint");
    }
    Ok(())
}
