//! Webhook endpoint registration. The signing secret is returned once, at creation.

use axum::{Router, extract::State, http::StatusCode, routing::get};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    app::AppState,
    auth::Business,
    error::{ApiError, ApiResult},
    extract::Json,
};

pub fn routes() -> Router<AppState> {
    Router::new().route("/webhook_endpoints", get(list_endpoints).post(create_endpoint))
}

#[derive(Deserialize)]
pub struct CreateEndpoint {
    pub url: String,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct Endpoint {
    pub id: Uuid,
    pub url: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Serialize)]
pub struct CreatedEndpoint {
    #[serde(flatten)]
    pub endpoint: Endpoint,
    /// Shown only in this response. Stored in plaintext because HMAC signing needs it.
    pub secret: String,
}

async fn create_endpoint(
    State(state): State<AppState>,
    business: Business,
    Json(req): Json<CreateEndpoint>,
) -> ApiResult<(StatusCode, Json<CreatedEndpoint>)> {
    let url = reqwest::Url::parse(req.url.trim())
        .ok()
        .filter(|u| matches!(u.scheme(), "http" | "https") && u.host().is_some())
        .ok_or_else(|| ApiError::validation("url must be an absolute http(s) URL"))?;

    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("OS random number generator unavailable");
    let secret = format!("whsec_{}", hex::encode(bytes));

    let endpoint = sqlx::query_as::<_, Endpoint>(
        "INSERT INTO webhook_endpoints (business_id, url, secret) VALUES ($1, $2, $3)
         RETURNING id, url, created_at",
    )
    .bind(business.id)
    .bind(url.as_str())
    .bind(&secret)
    .fetch_one(&state.db)
    .await?;

    Ok((StatusCode::CREATED, Json(CreatedEndpoint { endpoint, secret })))
}

async fn list_endpoints(
    State(state): State<AppState>,
    business: Business,
) -> ApiResult<Json<Vec<Endpoint>>> {
    let endpoints = sqlx::query_as::<_, Endpoint>(
        "SELECT id, url, created_at FROM webhook_endpoints
         WHERE business_id = $1 AND disabled_at IS NULL ORDER BY id",
    )
    .bind(business.id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(endpoints))
}
