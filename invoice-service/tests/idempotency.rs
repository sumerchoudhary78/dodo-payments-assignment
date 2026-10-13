//! Required test 2: retrying with the same Idempotency-Key returns the same response
//! without a second PSP call.

mod common;

use tokio::task::JoinSet;

#[tokio::test]
async fn retry_with_same_key_returns_same_response_without_second_psp_call() {
    let app = common::spawn_app().await;
    let invoice_id = app.open_invoice(2_500).await;

    let (first_status, first_body) = app.pay(&invoice_id, "tok_success", "order-42").await;
    assert_eq!(first_status, 200, "{first_body}");
    assert_eq!(first_body["status"], "succeeded");
    assert_eq!(app.psp_charge_requests(), 1);

    // The retry: same key, same body. Same response, and the PSP is not called again.
    let (retry_status, retry_body) = app.pay(&invoice_id, "tok_success", "order-42").await;
    assert_eq!(retry_status, first_status);
    assert_eq!(retry_body, first_body);
    assert_eq!(app.psp_charge_requests(), 1, "a replay must not reach the PSP");

    // Same key, different body: rejected, still no PSP call.
    let (status, body) = app.pay(&invoice_id, "tok_card_declined", "order-42").await;
    assert_eq!(status, 422);
    assert_eq!(common::error_code(&body), "idempotency_key_reused");
    assert_eq!(app.psp_charge_requests(), 1);
}

#[tokio::test]
async fn declined_payment_replays_the_decline() {
    let app = common::spawn_app().await;
    let invoice_id = app.open_invoice(2_500).await;

    let first = app.pay(&invoice_id, "tok_card_declined", "attempt-1").await;
    assert_eq!(first.0, 402);
    assert_eq!(common::error_code(&first.1), "card_declined");

    let retry = app.pay(&invoice_id, "tok_card_declined", "attempt-1").await;
    assert_eq!(retry, first);
    assert_eq!(app.psp_charge_requests(), 1);
    assert_eq!(app.invoice_state(&invoice_id).await, "open", "a decline doesn't change the invoice");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_retries_with_same_key_share_one_attempt() {
    let app = common::spawn_app().await;
    let invoice_id = app.open_invoice(2_500).await;

    let mut requests = JoinSet::new();
    for _ in 0..10 {
        let (app, invoice_id) = (app.clone(), invoice_id.clone());
        requests.spawn(async move { app.pay(&invoice_id, "tok_success", "same-key").await });
    }
    let responses = requests.join_all().await;

    // The first request creates the attempt; the rest replay it. Depending on timing they see
    // it pending (202) or succeeded (200) — but always the same attempt, never a 409.
    let attempt_id = &responses[0].1["id"];
    for (status, body) in &responses {
        assert!(*status == 200 || *status == 202, "{status} {body}");
        assert_eq!(&body["id"], attempt_id);
    }
    assert_eq!(app.psp_charge_requests(), 1);
}
