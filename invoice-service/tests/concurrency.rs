//! Required test 1: N concurrent POST /pay for the same invoice.
//! At most one succeeds, the card is charged exactly once, and the final state is consistent.

mod common;

use tokio::task::JoinSet;

const N: usize = 20;

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_payments_charge_at_most_once() {
    let app = common::spawn_app().await;
    let invoice_id = app.open_invoice(5_000).await;

    // N different clients, N different idempotency keys, all at once.
    let mut requests = JoinSet::new();
    for i in 0..N {
        let (app, invoice_id) = (app.clone(), invoice_id.clone());
        requests.spawn(async move { app.pay(&invoice_id, "tok_success", &format!("client-{i}")).await });
    }
    let responses = requests.join_all().await;

    let succeeded = responses.iter().filter(|(status, _)| *status == 200).count();
    assert_eq!(succeeded, 1, "exactly one payment must succeed: {responses:?}");

    // Every loser gets a clear 409: either the winner's attempt was still pending, or the
    // invoice was already paid by the time the loser got the lock.
    for (status, body) in responses.iter().filter(|(status, _)| *status != 200) {
        assert_eq!(*status, 409, "{body}");
        let code = common::error_code(body);
        assert!(code == "payment_in_progress" || code == "invoice_not_payable", "unexpected code {code}");
    }

    // No double charge: one PSP request, one succeeded attempt, for exactly the invoice total.
    assert_eq!(app.psp_charge_requests(), 1, "the PSP must be called exactly once");
    let (succeeded_attempts, charged_cents): (i64, Option<i64>) = sqlx::query_as(
        "SELECT count(*), sum(amount_cents)::bigint FROM payment_attempts
         WHERE invoice_id = $1::uuid AND status = 'succeeded'",
    )
    .bind(&invoice_id)
    .fetch_one(&app.db)
    .await
    .unwrap();
    assert_eq!(succeeded_attempts, 1);
    assert_eq!(charged_cents, Some(5_000));

    // Consistent final state: invoice paid, nothing left pending, exactly one paid event.
    assert_eq!(app.invoice_state(&invoice_id).await, "paid");
    let (pending, paid_events): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM payment_attempts WHERE invoice_id = $1::uuid AND status = 'pending'),
                (SELECT count(*) FROM events WHERE type = 'invoice.paid')",
    )
    .bind(&invoice_id)
    .fetch_one(&app.db)
    .await
    .unwrap();
    assert_eq!(pending, 0);
    assert_eq!(paid_events, 1);
}
