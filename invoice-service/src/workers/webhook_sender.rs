//! Delivers webhooks from the outbox (decisions.md D4).
//!
//! Each tick claims up to BATCH_SIZE due deliveries by pushing their `next_attempt_at` forward
//! by LEASE (a short transaction with `FOR UPDATE SKIP LOCKED`, committed immediately). The HTTP
//! calls then happen with no transaction open. If the process dies mid-send, the lease simply
//! expires and the delivery is picked up again: at-least-once delivery.
//!
//! Retry schedule after the Nth failed attempt: 30s, 2m, 10m, 30m, 1h, 3h, 6h.
//! The 8th failure marks the delivery `failed` (total budget 38,550s = 10h42m30s).

use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::task::JoinSet;
use uuid::Uuid;

use crate::{app::AppState, webhooks::signing};

const TICK: Duration = Duration::from_secs(1);
const BATCH_SIZE: i64 = 20;
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
/// Must exceed HTTP_TIMEOUT so a delivery isn't re-claimed while still being sent.
const LEASE_MS: i64 = 60_000;
pub const MAX_ATTEMPTS: i32 = 8;

/// Delay before the next attempt, given how many attempts have failed so far.
/// `None` means the retry budget is exhausted.
pub fn retry_delay(failed_attempts: i32) -> Option<Duration> {
    const SCHEDULE_SECS: [u64; MAX_ATTEMPTS as usize - 1] = [30, 120, 600, 1_800, 3_600, 10_800, 21_600];
    let index = usize::try_from(failed_attempts).ok()?.checked_sub(1)?;
    SCHEDULE_SECS.get(index).map(|&s| Duration::from_secs(s))
}

#[derive(sqlx::FromRow)]
struct Due {
    id: Uuid,
    attempt_count: i32,
    url: String,
    secret: String,
    event_id: Uuid,
    event_type: String,
    payload: serde_json::Value,
    event_created_at: DateTime<Utc>,
}

pub async fn run(state: AppState) {
    let http = reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none()) // a redirect is not a successful delivery
        .build()
        .expect("build webhook http client");
    let mut ticker = tokio::time::interval(TICK);
    loop {
        ticker.tick().await;
        if let Err(err) = tick(&state, &http).await {
            tracing::error!(error = %err, "webhook sender tick failed");
        }
    }
}

async fn tick(state: &AppState, http: &reqwest::Client) -> sqlx::Result<()> {
    let due = sqlx::query_as::<_, Due>(
        "WITH claimed AS (
             UPDATE webhook_deliveries
             SET next_attempt_at = now() + $2 * interval '1 millisecond'
             WHERE id IN (
                 SELECT id FROM webhook_deliveries
                 WHERE status = 'pending' AND next_attempt_at <= now()
                 ORDER BY next_attempt_at
                 LIMIT $1
                 FOR UPDATE SKIP LOCKED
             )
             RETURNING id, attempt_count, event_id, endpoint_id
         )
         SELECT c.id, c.attempt_count, w.url, w.secret,
                e.id AS event_id, e.type AS event_type, e.payload, e.created_at AS event_created_at
         FROM claimed c
         JOIN webhook_endpoints w ON w.id = c.endpoint_id
         JOIN events e ON e.id = c.event_id",
    )
    .bind(BATCH_SIZE)
    .bind(LEASE_MS)
    .fetch_all(&state.db)
    .await?;

    // Send concurrently so one slow endpoint doesn't delay everyone else's webhooks.
    let mut sends = JoinSet::new();
    for delivery in due {
        let (db, http) = (state.db.clone(), http.clone());
        sends.spawn(async move {
            if let Err(err) = deliver(&db, &http, delivery).await {
                tracing::error!(error = %err, "failed to record webhook delivery result");
            }
        });
    }
    sends.join_all().await;
    Ok(())
}

async fn deliver(db: &sqlx::PgPool, http: &reqwest::Client, d: Due) -> sqlx::Result<()> {
    let body = serde_json::to_vec(&serde_json::json!({
        "id": d.event_id,
        "type": d.event_type,
        "created_at": d.event_created_at,
        "data": d.payload,
    }))
    .expect("event serializes");
    let signature = signing::sign(&d.secret, Utc::now().timestamp(), &body);

    let result = http
        .post(&d.url)
        .header("content-type", "application/json")
        .header(signing::HEADER, signature)
        .body(body)
        .send()
        .await;

    let attempts = d.attempt_count + 1;
    let (status_code, error) = match &result {
        Ok(resp) if resp.status().is_success() => {
            sqlx::query(
                "UPDATE webhook_deliveries SET status = 'delivered', attempt_count = $2,
                     last_status_code = $3, last_error = NULL, delivered_at = now()
                 WHERE id = $1",
            )
            .bind(d.id)
            .bind(attempts)
            .bind(i32::from(resp.status().as_u16()))
            .execute(db)
            .await?;
            tracing::info!(delivery_id = %d.id, event = %d.event_type, url = %d.url, attempts, "webhook delivered");
            return Ok(());
        }
        Ok(resp) => (Some(i32::from(resp.status().as_u16())), format!("HTTP {}", resp.status())),
        Err(err) => (None, err.to_string()),
    };

    match retry_delay(attempts) {
        Some(delay) => {
            sqlx::query(
                "UPDATE webhook_deliveries SET attempt_count = $2, last_status_code = $3, last_error = $4,
                     next_attempt_at = now() + $5 * interval '1 millisecond'
                 WHERE id = $1",
            )
            .bind(d.id)
            .bind(attempts)
            .bind(status_code)
            .bind(&error)
            .bind(delay.as_millis() as i64)
            .execute(db)
            .await?;
            tracing::warn!(delivery_id = %d.id, url = %d.url, attempts, retry_in_secs = delay.as_secs(), %error, "webhook failed, will retry");
        }
        None => {
            sqlx::query(
                "UPDATE webhook_deliveries SET status = 'failed', attempt_count = $2,
                     last_status_code = $3, last_error = $4
                 WHERE id = $1",
            )
            .bind(d.id)
            .bind(attempts)
            .bind(status_code)
            .bind(&error)
            .execute(db)
            .await?;
            tracing::error!(delivery_id = %d.id, url = %d.url, attempts, %error, "webhook retry budget exhausted");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_matches_design_and_stops_after_eight_attempts() {
        let delays: Vec<u64> = (1..MAX_ATTEMPTS).map(|n| retry_delay(n).unwrap().as_secs()).collect();
        assert_eq!(delays, [30, 120, 600, 1_800, 3_600, 10_800, 21_600]);
        assert_eq!(retry_delay(MAX_ATTEMPTS), None);
        assert_eq!(delays.iter().sum::<u64>(), 38_550); // 10h42m30s
    }
}
