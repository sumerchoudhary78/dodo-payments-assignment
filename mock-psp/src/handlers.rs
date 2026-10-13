//! HTTP handlers: `POST /charges` and `GET /charges/{key}`.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;

use crate::charge::{Charge, Outcome};

#[derive(Clone)]
pub struct AppState {
    /// std Mutex (not tokio's): the lock is never held across an `.await`.
    pub charges: Arc<Mutex<HashMap<String, Charge>>>,
    pub normal_delay: Duration,
    pub timeout_delay: Duration,
    /// Number of POST /charges requests received, including duplicates. Lets tests assert
    /// "no second PSP call".
    pub charge_requests: Arc<AtomicUsize>,
}

impl AppState {
    pub fn new(normal_delay: Duration, timeout_delay: Duration) -> Self {
        AppState {
            charges: Arc::new(Mutex::new(HashMap::new())),
            normal_delay,
            timeout_delay,
            charge_requests: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[derive(Deserialize)]
pub struct ChargeRequest {
    #[allow(dead_code)] // accepted for realism; the mock does not validate amounts
    amount_cents: i64,
    card_token: String,
}

pub async fn create_charge(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ChargeRequest>,
) -> Response {
    state.charge_requests.fetch_add(1, Ordering::SeqCst);
    let Some(key) = headers.get("idempotency-key").and_then(|v| v.to_str().ok()) else {
        return (StatusCode::BAD_REQUEST, "Idempotency-Key header required").into_response();
    };
    let key = key.to_owned();

    // Record the charge *before* doing any work. This is what makes "GET returns 404"
    // trustworthy for the caller: 404 means we never received the request at all.
    // Check and insert happen under one lock so two identical requests can't both pass the check.
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
        let charge = outcome.final_charge();
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

pub async fn get_charge(State(state): State<AppState>, Path(key): Path<String>) -> Response {
    match state.charges.lock().unwrap().get(&key) {
        Some(charge) => (StatusCode::OK, Json(charge.clone())).into_response(),
        None => (StatusCode::NOT_FOUND, Json(serde_json::json!({ "status": "not_found" })))
            .into_response(),
    }
}
