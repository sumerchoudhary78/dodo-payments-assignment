//! Mock payment processor (PSP).
//!
//! Behaves like a real external PSP in the two ways the invoice service relies on:
//!   1. `POST /charges` is idempotent on the `Idempotency-Key` header — the same key never
//!      charges twice, it returns the existing charge.
//!   2. `GET /charges/{key}` reports what happened to a charge, so a caller that timed out
//!      or crashed can find out the real outcome later.
//!
//! The outcome is decided by the card token (see `charge::Outcome::from_token`).
//! State is in memory only: restarting the mock forgets every charge (a real PSP is durable).

mod charge;
mod handlers;

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Router,
    routing::{get, post},
};

use handlers::{AppState, create_charge, get_charge};

fn env_ms(name: &str, default: u64) -> Duration {
    let ms = std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default);
    Duration::from_millis(ms)
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_target(false).init();

    let state = AppState {
        charges: Arc::new(Mutex::new(HashMap::new())),
        normal_delay: env_ms("PSP_NORMAL_DELAY_MS", 100),
        timeout_delay: env_ms("PSP_TIMEOUT_DELAY_MS", 30_000),
    };

    let app = Router::new()
        .route("/charges", post(create_charge))
        .route("/charges/{key}", get(get_charge))
        .with_state(state);

    let addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8081".into());
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind");
    tracing::info!(%addr, "mock PSP listening");
    axum::serve(listener, app).await.expect("server");
}
