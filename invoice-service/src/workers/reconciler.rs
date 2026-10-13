//! Resolves payment attempts stuck in `pending` (decisions.md D2).
//!
//! An attempt stays pending when /pay couldn't learn the PSP outcome (timeout, 5xx, dropped
//! connection) or the service crashed between the PSP call and TX2. Every tick, attempts
//! pending longer than `reconcile_after` are looked up at the PSP by attempt id.
//!
//! No row locks are held while calling the PSP. Two reconciler instances may look up the same
//! attempt; that's harmless, because `record_outcome` only applies to a still-pending attempt.

use uuid::Uuid;

use crate::{app::AppState, payments, payments::psp_client::PspOutcome};

const BATCH_SIZE: i64 = 50;

pub async fn run(state: AppState) {
    let mut ticker = tokio::time::interval(state.config.reconcile_interval);
    loop {
        ticker.tick().await;
        if let Err(err) = tick(&state).await {
            tracing::error!(error = %err, "reconciler tick failed");
        }
    }
}

async fn tick(state: &AppState) -> sqlx::Result<()> {
    let stale: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM payment_attempts
         WHERE status = 'pending' AND created_at < now() - make_interval(secs => $1)
         ORDER BY created_at LIMIT $2",
    )
    .bind(state.config.reconcile_after.as_secs_f64())
    .bind(BATCH_SIZE)
    .fetch_all(&state.db)
    .await?;

    for attempt_id in stale {
        let outcome = state.psp.lookup(attempt_id).await;
        tracing::info!(%attempt_id, ?outcome, "reconciler looked up pending attempt");
        if matches!(outcome, PspOutcome::Unknown) {
            continue; // still processing or PSP unreachable: try again next tick
        }
        if let Err(err) = payments::record_outcome(&state.db, attempt_id, outcome).await {
            tracing::error!(%attempt_id, ?err, "reconciler failed to record outcome");
        }
    }
    Ok(())
}
