//! Demo webhook receiver: what a business's endpoint would do with our webhooks.
//!
//!   1. Verify `Dodo-Signature` (HMAC-SHA256 over "<t>.<raw body>") in constant time.
//!   2. Reject timestamps more than 5 minutes from now (replay protection).
//!   3. Drop duplicate event ids (delivery is at-least-once).
//!
//! Not part of the invoice service; it exists so the demo shows verified deliveries arriving.

use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{Router, body::Bytes, extract::State, http::{HeaderMap, StatusCode}, routing::post};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

const TOLERANCE_SECS: i64 = 300;

#[derive(Clone)]
struct AppState {
    secret: Arc<String>,
    seen_event_ids: Arc<Mutex<HashSet<String>>>,
}

fn verify(secret: &str, header: &str, body: &[u8]) -> Result<(), &'static str> {
    let mut timestamp = None;
    let mut signature = None;
    for part in header.split(',') {
        match part.split_once('=') {
            Some(("t", v)) => timestamp = v.parse::<i64>().ok(),
            Some(("v1", v)) => signature = hex::decode(v).ok(),
            _ => {}
        }
    }
    let (timestamp, signature) = timestamp.zip(signature).ok_or("malformed signature header")?;

    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
    if (now - timestamp).abs() > TOLERANCE_SECS {
        return Err("timestamp outside tolerance");
    }

    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(format!("{timestamp}.").as_bytes());
    mac.update(body);
    mac.verify_slice(&signature).map_err(|_| "signature mismatch") // constant-time comparison
}

async fn receive(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> StatusCode {
    let header = headers.get("dodo-signature").and_then(|v| v.to_str().ok()).unwrap_or("");
    if let Err(reason) = verify(&state.secret, header, &body) {
        tracing::warn!(reason, "REJECTED webhook");
        return StatusCode::BAD_REQUEST;
    }

    let event: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(event) => event,
        Err(_) => return StatusCode::BAD_REQUEST,
    };
    let id = event["id"].as_str().unwrap_or_default().to_string();
    if !state.seen_event_ids.lock().unwrap().insert(id.clone()) {
        tracing::info!(event_id = %id, "duplicate delivery, already processed");
        return StatusCode::OK;
    }

    let invoice = &event["data"]["invoice"];
    let attempt = &event["data"]["payment_attempt"];
    tracing::info!(
        "✔ verified {:<24} invoice={} state={} total_cents={} attempt_status={} failure_code={}",
        event["type"].as_str().unwrap_or("?"),
        invoice["id"].as_str().unwrap_or("-"),
        invoice["state"].as_str().unwrap_or("-"),
        invoice["total_cents"],
        attempt["status"].as_str().unwrap_or("-"),
        attempt["failure_code"].as_str().unwrap_or("-"),
    );
    StatusCode::OK
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_target(false).init();
    let secret = std::env::var("WEBHOOK_SECRET").expect("WEBHOOK_SECRET is required");
    let state = AppState { secret: Arc::new(secret), seen_event_ids: Arc::default() };

    let app = Router::new().route("/webhooks", post(receive)).with_state(state);
    let addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:9000".into());
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind");
    tracing::info!(%addr, "webhook receiver listening");
    axum::serve(listener, app).await.expect("server");
}
