//! Shared application state and the HTTP router.

use axum::{Router, routing::get};
use sqlx::PgPool;

use crate::{config::Config, customers, error::ApiError};

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub config: Config,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .merge(customers::routes())
        .fallback(|| async { ApiError::not_found("route") })
        .with_state(state)
}
