//! API key authentication (decisions.md D5).
//!
//! Keys look like `sk_<64 hex chars>` (32 random bytes). We store only the SHA-256 hash and a
//! short prefix. A fast hash is fine here: the key is 256 bits of randomness, so there is
//! nothing to brute-force, and an unsalted hash can be looked up through a unique index.

use axum::{extract::FromRequestParts, http::request::Parts};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{app::AppState, error::ApiError};

/// The authenticated caller. Adding this as a handler argument makes the route require a key.
pub struct Business {
    pub id: Uuid,
}

impl FromRequestParts<AppState> for Business {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let key = parts
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(ApiError::unauthorized)?;

        // Revocation is checked on every request, so revoking a key takes effect immediately.
        let business_id: Option<Uuid> = sqlx::query_scalar(
            "SELECT business_id FROM api_keys WHERE key_hash = $1 AND revoked_at IS NULL",
        )
        .bind(hash_key(key))
        .fetch_optional(&state.db)
        .await?;

        business_id.map(|id| Business { id }).ok_or_else(ApiError::unauthorized)
    }
}

pub fn hash_key(key: &str) -> Vec<u8> {
    Sha256::digest(key.as_bytes()).to_vec()
}

pub fn generate_key() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("OS random number generator unavailable");
    format!("sk_{}", hex::encode(bytes))
}

/// First 11 chars (`sk_` + 8 hex), stored so humans can tell keys apart without the secret.
fn key_prefix(key: &str) -> &str {
    &key[..11.min(key.len())]
}

/// Creates a business and its first API key. Used by the CLI and by the demo seed.
pub async fn create_business(db: &PgPool, name: &str, key: &str) -> sqlx::Result<Uuid> {
    let mut tx = db.begin().await?;
    let business_id: Uuid =
        sqlx::query_scalar("INSERT INTO businesses (name) VALUES ($1) RETURNING id")
            .bind(name)
            .fetch_one(&mut *tx)
            .await?;
    sqlx::query("INSERT INTO api_keys (business_id, key_prefix, key_hash) VALUES ($1, $2, $3)")
        .bind(business_id)
        .bind(key_prefix(key))
        .bind(hash_key(key))
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(business_id)
}
