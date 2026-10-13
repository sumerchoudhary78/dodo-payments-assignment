//! Required test 3: PSP timeouts and errors never leave the invoice in a bad state.
//! Plus failure mode (c): the PSP charged the card but we crashed before recording it.

mod common;

use std::time::{Duration, Instant};

use invoice_service::payments::idempotency;
use serde_json::json;
use uuid::Uuid;

const RESOLVE_WITHIN: Duration = Duration::from_secs(10);

#[tokio::test]
async fn psp_timeout_returns_202_and_reconciler_settles_it() {
    let app = common::spawn_app().await;
    let invoice_id = app.open_invoice(1_000).await;

    // The endpoint must not hang for the PSP: it answers right after our PSP timeout.
    let started = Instant::now();
    let (status, attempt) = app.pay(&invoice_id, "tok_timeout", "slow-1").await;
    let elapsed = started.elapsed();
    assert_eq!(status, 202, "{attempt}");
    assert_eq!(attempt["status"], "pending");
    assert!(elapsed < common::PSP_TIMEOUT + Duration::from_millis(500), "took {elapsed:?}");

    // While the outcome is unknown, the invoice is untouched and protected.
    assert_eq!(app.invoice_state(&invoice_id).await, "open");
    let (status, body) = app.pay(&invoice_id, "tok_success", "impatient-retry").await;
    assert_eq!((status, common::error_code(&body)), (409, "payment_in_progress"));
    let (status, body) = app.void(&invoice_id).await;
    assert_eq!((status, common::error_code(&body)), (409, "payment_in_progress"));

    // The PSP finishes the charge; the reconciler learns it via a status lookup.
    let attempt_id = attempt["id"].as_str().unwrap();
    let resolved = app.wait_for_resolution(attempt_id, RESOLVE_WITHIN).await;
    assert_eq!(resolved["status"], "succeeded");
    assert_eq!(app.invoice_state(&invoice_id).await, "paid");
    assert_eq!(app.psp_charge_requests(), 1, "the reconciler looks up, it never charges again");

    // Retrying the original key now returns the real outcome, not a stale 202.
    let (status, replay) = app.pay(&invoice_id, "tok_timeout", "slow-1").await;
    assert_eq!(status, 200);
    assert_eq!(replay["status"], "succeeded");
}

#[tokio::test]
async fn psp_network_error_is_unknown_then_resolved_and_invoice_stays_payable() {
    let app = common::spawn_app().await;
    let invoice_id = app.open_invoice(1_000).await;

    // A 500 from the PSP is ambiguous, so it is not reported as a failure.
    let (status, attempt) = app.pay(&invoice_id, "tok_network_error", "net-1").await;
    assert_eq!(status, 202, "{attempt}");
    assert_eq!(app.invoice_state(&invoice_id).await, "open");

    // The reconciler finds out the charge actually failed.
    let resolved = app.wait_for_resolution(attempt["id"].as_str().unwrap(), RESOLVE_WITHIN).await;
    assert_eq!(resolved["status"], "failed");
    assert_eq!(resolved["failure_code"], "processing_error");

    // Not stuck: the invoice is still open and can be paid with another card.
    assert_eq!(app.invoice_state(&invoice_id).await, "open");
    let (status, _) = app.pay(&invoice_id, "tok_success", "net-2").await;
    assert_eq!(status, 200);
    assert_eq!(app.invoice_state(&invoice_id).await, "paid");
}

/// Failure mode (c): TX1 committed, the PSP charged the card, then the service died before
/// TX2. We reproduce that state directly, then check that a client retry does not charge
/// again and the reconciler completes the payment.
#[tokio::test]
async fn crash_after_psp_success_is_recovered_without_double_charge() {
    let app = common::spawn_app().await;
    let invoice_id = app.open_invoice(1_000).await;
    let invoice_uuid: Uuid = invoice_id.parse().unwrap();

    // What TX1 would have written: a pending attempt and its idempotency key.
    let attempt_id: Uuid = sqlx::query_scalar(
        "INSERT INTO payment_attempts (business_id, invoice_id, amount_cents, payment_method)
         VALUES ($1, $2, 1000, 'tok_success') RETURNING id",
    )
    .bind(app.business_id)
    .bind(invoice_uuid)
    .fetch_one(&app.db)
    .await
    .unwrap();
    let hash = idempotency::request_hash(invoice_uuid, "tok_success");
    sqlx::query("INSERT INTO idempotency_keys (business_id, key, request_hash, attempt_id) VALUES ($1, 'crash-1', $2, $3)")
        .bind(app.business_id)
        .bind(&hash)
        .bind(attempt_id)
        .execute(&app.db)
        .await
        .unwrap();

    // The PSP call that succeeded just before the "crash" (TX2 never runs).
    let charge = app
        .http
        .post(format!("{}/charges", app.psp_url))
        .header("idempotency-key", attempt_id.to_string())
        .json(&json!({"amount_cents": 1000, "card_token": "tok_success"}))
        .send()
        .await
        .unwrap();
    assert!(charge.status().is_success());
    assert_eq!(app.psp_charge_requests(), 1);

    // The client retries with the same key: it gets the existing attempt, no new charge.
    let (status, body) = app.pay(&invoice_id, "tok_success", "crash-1").await;
    assert_eq!(status, 202, "{body}");
    assert_eq!(body["id"], attempt_id.to_string());
    // A different key can't start a second charge while the outcome is unknown.
    let (status, _) = app.pay(&invoice_id, "tok_success", "crash-2").await;
    assert_eq!(status, 409);

    // The reconciler asks the PSP, learns about the success, and completes the payment.
    let resolved = app.wait_for_resolution(&attempt_id.to_string(), RESOLVE_WITHIN).await;
    assert_eq!(resolved["status"], "succeeded");
    assert_eq!(app.invoice_state(&invoice_id).await, "paid");
    assert_eq!(app.psp_charge_requests(), 1, "the customer was charged exactly once");
}
