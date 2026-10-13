//! The transactional outbox.
//!
//! `enqueue` must be called with the SAME transaction that makes the state change. If the
//! transaction commits, the event and its deliveries exist; if it rolls back, neither does.
//! Nothing is sent from the request path — the delivery worker picks rows up later.

use sqlx::PgConnection;
use uuid::Uuid;

pub const INVOICE_CREATED: &str = "invoice.created";
pub const INVOICE_PAID: &str = "invoice.paid";
pub const INVOICE_PAYMENT_FAILED: &str = "invoice.payment_failed";

pub async fn enqueue(
    conn: &mut PgConnection,
    business_id: Uuid,
    event_type: &str,
    data: serde_json::Value,
) -> sqlx::Result<()> {
    let event_id: Uuid = sqlx::query_scalar(
        "INSERT INTO events (business_id, type, payload) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(business_id)
    .bind(event_type)
    .bind(data)
    .fetch_one(&mut *conn)
    .await?;

    // Fan out: one delivery row per active endpoint, each retried independently.
    sqlx::query(
        "INSERT INTO webhook_deliveries (event_id, endpoint_id)
         SELECT $1, id FROM webhook_endpoints WHERE business_id = $2 AND disabled_at IS NULL",
    )
    .bind(event_id)
    .bind(business_id)
    .execute(&mut *conn)
    .await?;

    Ok(())
}
