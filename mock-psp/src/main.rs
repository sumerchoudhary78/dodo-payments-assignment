//! Mock payment processor (PSP).
//!
//! Behaves like a real external PSP in the two ways the invoice service relies on:
//!   1. `POST /charges` is idempotent on the `Idempotency-Key` header — the same key never
//!      charges twice, it returns the existing charge.
//!   2. `GET /charges/{key}` reports what happened to a charge, so a caller that timed out
//!      or crashed can find out the real outcome later.
//!
//! The outcome is decided by the card token (see `Outcome::from_token`).
//! State is in memory only: restarting the mock forgets every charge (a real PSP is durable).

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Charge {
    Processing,
    Succeeded { psp_ref: Uuid },
    Failed { code: String },
}

#[derive(Clone)]
struct AppState {
    charges: Arc<Mutex<HashMap<String, Charge>>>,
    normal_delay: Duration,
    timeout_delay: Duration,
}

#[derive(Deserialize)]
struct ChargeRequest {
    #[allow(dead_code)] // accepted for realism; the mock does not validate amounts
    amount_cents: i64,
    card_token: String,
}

#[derive(Clone, Copy)]
enum Outcome {
    Succeed,
    Decline(&'static str),
    SlowSucceed,
    NetworkError,
}

impl Outcome {
    fn from_token(token: &str) -> Self {
        match token {
            "tok_success" => Outcome::Succeed,
            "tok_insufficient_funds" => Outcome::Decline("insufficient_funds"),
            "tok_card_declined" => Outcome::Decline("card_declined"),
            "tok_timeout" => Outcome::SlowSucceed,
            "tok_network_error" => Outcome::NetworkError,
            _ => Outcome::Decline("invalid_card_token"),
        }
    }
}

async fn create_charge(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ChargeRequest>,
) -> Response {
    let Some(key) = headers.get("idempotency-key").and_then(|v| v.to_str().ok()) else {
        return (StatusCode::BAD_REQUEST, "Idempotency-Key header required").into_response();
    };
    let key = key.to_owned();

    // Record the charge *before* doing any work. This is what makes "GET returns 404"
    // trustworthy for the caller: 404 means we never received the request at all.
    {
        let mut charges = state.charges.lock().unwrap();
        if let Some(existing) = charges.get(&key) {
            tracing::info!(%key, "duplicate charge request, returning existing charge");
            return (StatusCode::OK, Json(existing.clone())).into_response();
        }
        charges.insert(key.clone(), Charge::Processing);
    }

    let outcome = Outcome::from_token(&req.card_token);
    tracing::info!(%key, token = %req.card_token, "charge received");

    let delay = match outcome {
        Outcome::SlowSucceed => state.timeout_delay,
        _ => state.normal_delay,
    };

    // The charge is completed in a spawned task, not in this handler. If the caller gives up
    // and drops the connection, axum drops this handler future — but the charge must still
    // finish, exactly like a real PSP would keep processing a charge it already accepted.
    let task_state = state.clone();
    let task_key = key.clone();
    let final_charge = tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let charge = match outcome {
            Outcome::Succeed | Outcome::SlowSucceed => Charge::Succeeded { psp_ref: Uuid::new_v4() },
            Outcome::Decline(code) => Charge::Failed { code: code.into() },
            Outcome::NetworkError => Charge::Failed { code: "processing_error".into() },
        };
        task_state.charges.lock().unwrap().insert(task_key.clone(), charge.clone());
        tracing::info!(key = %task_key, "charge completed");
        charge
    })
    .await
    .expect("charge task panicked");

    match outcome {
        Outcome::NetworkError => {
            (StatusCode::INTERNAL_SERVER_ERROR, "upstream processor error").into_response()
        }
        _ => (StatusCode::OK, Json(final_charge)).into_response(),
    }
}

async fn get_charge(State(state): State<AppState>, Path(key): Path<String>) -> Response {
    match state.charges.lock().unwrap().get(&key) {
        Some(charge) => (StatusCode::OK, Json(charge.clone())).into_response(),
        None => (StatusCode::NOT_FOUND, Json(serde_json::json!({ "status": "not_found" })))
            .into_response(),
    }
}

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
