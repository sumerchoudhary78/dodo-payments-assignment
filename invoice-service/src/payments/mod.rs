//! Payments: `POST /invoices/{id}/pay` and `GET /payment_attempts/{id}` (decisions.md D2, D3).
//!
//! The pay flow is three steps, and no database transaction is open during the PSP call:
//!
//!   TX1   lock invoice → replay check → payable? → insert pending attempt + idempotency key
//!   PSP   charge with attempt_id as the PSP's idempotency key, 5s timeout
//!   TX2   record_outcome(): lock invoice → resolve attempt → mark invoice paid → outbox event
//!
//! If the PSP outcome is unknown (timeout, 5xx, dropped connection), TX2 is skipped: the
//! attempt stays `pending`, the caller gets 202, and the reconciler finishes the job later.

pub mod idempotency;
pub mod psp_client;

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::{
    app::AppState,
    auth::Business,
    domain::invoice_state::InvoiceState,
    error::{ApiError, ApiResult},
    extract::{Json, Path},
    invoices::{self, lock_invoice, set_state},
    webhooks::events,
};
use psp_client::PspOutcome;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/invoices/{id}/pay", post(pay))
        .route("/payment_attempts/{id}", get(get_attempt))
}

#[derive(Serialize, sqlx::FromRow)]
pub struct PaymentAttempt {
    pub id: Uuid,
    pub invoice_id: Uuid,
    #[serde(skip)]
    pub business_id: Uuid,
    pub amount_cents: i64,
    pub status: String,
    pub failure_code: Option<String>,
    pub psp_ref: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// pending → 202 (outcome not known yet, poll GET /payment_attempts/{id})
/// succeeded → 200
/// failed → 402, in the standard error envelope plus the attempt
fn render(attempt: PaymentAttempt) -> Response {
    match attempt.status.as_str() {
        "succeeded" => (StatusCode::OK, axum::Json(attempt)).into_response(),
        "pending" => (StatusCode::ACCEPTED, axum::Json(attempt)).into_response(),
        _ => {
            let code = attempt.failure_code.clone().unwrap_or_default();
            let body = serde_json::json!({
                "error": { "code": code, "message": "the payment was declined" },
                "payment_attempt": attempt,
            });
            (StatusCode::PAYMENT_REQUIRED, axum::Json(body)).into_response()
        }
    }
}

async fn load_attempt(conn: &mut PgConnection, id: Uuid) -> sqlx::Result<PaymentAttempt> {
    sqlx::query_as::<_, PaymentAttempt>(
        "SELECT id, invoice_id, business_id, amount_cents, status, failure_code, psp_ref, created_at, updated_at
         FROM payment_attempts WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&mut *conn)
    .await
}

fn violates(err: &sqlx::Error, constraint: &str) -> bool {
    matches!(err, sqlx::Error::Database(e) if e.constraint() == Some(constraint))
}

#[derive(Deserialize)]
pub struct PayRequest {
    pub card_token: String,
}

/// What TX1 decided.
enum Started {
    /// The Idempotency-Key was seen before: answer from the existing attempt, no PSP call.
    Replay(PaymentAttempt),
    /// A new pending attempt exists; go charge the card.
    New { attempt_id: Uuid, amount_cents: i64 },
}

async fn pay(
    State(state): State<AppState>,
    business: Business,
    Path(invoice_id): Path<Uuid>,
    headers: HeaderMap,
    Json(req): Json<PayRequest>,
) -> ApiResult<Response> {
    let key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .filter(|k| !k.is_empty() && k.len() <= idempotency::MAX_KEY_LEN)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "idempotency_key_missing",
                "an Idempotency-Key header (1-255 chars) is required",
            )
        })?;
    let request_hash = idempotency::request_hash(invoice_id, &req.card_token);

    let (attempt_id, amount_cents) =
        match start_attempt(&state.db, business.id, invoice_id, key, &request_hash, &req.card_token).await? {
            Started::Replay(attempt) => {
                tracing::info!(attempt_id = %attempt.id, "idempotent replay, no PSP call");
                return Ok(render(attempt));
            }
            Started::New { attempt_id, amount_cents } => (attempt_id, amount_cents),
        };

    // No transaction and no row lock are held from here until TX2.
    let outcome = state.psp.charge(attempt_id, amount_cents, &req.card_token).await;
    tracing::info!(%attempt_id, ?outcome, "PSP charge returned");

    if !matches!(outcome, PspOutcome::Unknown) {
        record_outcome(&state.db, attempt_id, outcome).await?;
    }

    let mut conn = state.db.acquire().await?;
    Ok(render(load_attempt(&mut conn, attempt_id).await?))
}

/// TX1. The invoice row lock serializes every /pay (and void) on the same invoice.
async fn start_attempt(
    db: &PgPool,
    business_id: Uuid,
    invoice_id: Uuid,
    key: &str,
    request_hash: &[u8],
    card_token: &str,
) -> ApiResult<Started> {
    let mut tx = db.begin().await?;
    let invoice = lock_invoice(&mut tx, invoice_id, business_id).await?;

    // Replay check happens after the lock, so a concurrent request with the same key that
    // committed while we waited is visible here. It also happens before the state check, so
    // replaying a successful payment returns that success, not "invoice already paid".
    if let Some(stored) = idempotency::find(&mut tx, business_id, key).await? {
        if stored.request_hash != request_hash {
            return Err(key_reused());
        }
        return Ok(Started::Replay(load_attempt(&mut tx, stored.attempt_id).await?));
    }

    if !invoice.state.is_payable() {
        return Err(ApiError::conflict(
            "invoice_not_payable",
            format!("invoice is {}; only open or uncollectible invoices can be paid", invoice.state.as_str()),
        ));
    }

    let attempt_id: Uuid = sqlx::query_scalar(
        "INSERT INTO payment_attempts (business_id, invoice_id, amount_cents, payment_method)
         VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(business_id)
    .bind(invoice_id)
    .bind(invoice.total_cents)
    .bind(card_token)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| {
        if violates(&e, "payment_attempts_one_pending") {
            ApiError::conflict(
                "payment_in_progress",
                "another payment attempt for this invoice is still pending; poll it or retry later",
            )
        } else {
            e.into()
        }
    })?;

    // Same key used concurrently on a DIFFERENT invoice: the two requests lock different
    // invoice rows, so only the primary key catches it.
    idempotency::insert(&mut tx, business_id, key, request_hash, attempt_id)
        .await
        .map_err(|e| if violates(&e, "idempotency_keys_pkey") { key_reused() } else { e.into() })?;

    tx.commit().await?;
    tracing::info!(%attempt_id, %invoice_id, "payment attempt started");
    Ok(Started::New { attempt_id, amount_cents: invoice.total_cents })
}

fn key_reused() -> ApiError {
    ApiError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        "idempotency_key_reused",
        "this Idempotency-Key was already used with a different request",
    )
}

/// TX2. Applies a definite PSP outcome to a pending attempt. Used by /pay and the reconciler.
///
/// Idempotent: the attempt update is conditional on `status = 'pending'`, so if /pay and the
/// reconciler both learn the outcome, only the first one applies it and the second is a no-op.
/// Lock order is invoice first, then attempt — the same order as TX1 and void.
pub async fn record_outcome(db: &PgPool, attempt_id: Uuid, outcome: PspOutcome) -> ApiResult<()> {
    let (status, failure_code, psp_ref) = match outcome {
        PspOutcome::Succeeded { psp_ref } => ("succeeded", None, Some(psp_ref)),
        PspOutcome::Failed { code } => ("failed", Some(code), None),
        // The PSP never received the charge, so the card was not charged.
        PspOutcome::NotFound => ("failed", Some("not_received_by_psp".to_string()), None),
        PspOutcome::Unknown => return Ok(()),
    };

    let mut tx = db.begin().await?;
    let (invoice_id, business_id): (Uuid, Uuid) =
        sqlx::query_as("SELECT invoice_id, business_id FROM payment_attempts WHERE id = $1")
            .bind(attempt_id)
            .fetch_one(&mut *tx)
            .await?;
    let invoice = lock_invoice(&mut tx, invoice_id, business_id).await?;

    let updated = sqlx::query(
        "UPDATE payment_attempts SET status = $2, failure_code = $3, psp_ref = $4, updated_at = now()
         WHERE id = $1 AND status = 'pending'",
    )
    .bind(attempt_id)
    .bind(status)
    .bind(&failure_code)
    .bind(&psp_ref)
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() == 0 {
        return Ok(()); // already resolved by someone else
    }

    let event_type = if status == "succeeded" {
        if invoice.state.is_payable() {
            set_state(&mut tx, invoice_id, invoice.state, InvoiceState::Paid).await?;
        } else {
            // Unreachable by design: void is blocked while an attempt is pending, and only one
            // attempt can be pending. If it ever happens, money moved on an unpayable invoice
            // and a human must refund it — record the success, don't hide it.
            tracing::error!(%attempt_id, state = invoice.state.as_str(), "charge succeeded on unpayable invoice");
        }
        events::INVOICE_PAID
    } else {
        events::INVOICE_PAYMENT_FAILED
    };

    let invoice = invoices::load_invoice(&mut tx, invoice_id, business_id).await?;
    let attempt = load_attempt(&mut tx, attempt_id).await?;
    let payload = serde_json::json!({ "invoice": invoice, "payment_attempt": attempt });
    events::enqueue(&mut tx, business_id, event_type, payload).await?;
    tx.commit().await?;

    tracing::info!(%attempt_id, %invoice_id, status, "payment attempt resolved");
    Ok(())
}

async fn get_attempt(
    State(state): State<AppState>,
    business: Business,
    Path(id): Path<Uuid>,
) -> ApiResult<axum::Json<PaymentAttempt>> {
    let mut conn = state.db.acquire().await?;
    let attempt = load_attempt(&mut conn, id).await.map_err(|e| match e {
        sqlx::Error::RowNotFound => ApiError::not_found("payment attempt"),
        e => e.into(),
    })?;
    if attempt.business_id != business.id {
        return Err(ApiError::not_found("payment attempt"));
    }
    Ok(axum::Json(attempt))
}
