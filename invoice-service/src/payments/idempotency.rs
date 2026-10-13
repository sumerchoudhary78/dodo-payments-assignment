//! Idempotency-Key storage (decisions.md D3).
//!
//! A key maps to the payment attempt it created. A replay re-renders that attempt's CURRENT
//! state, so a retry after a timeout gets the real outcome, not a stale "pending".
//! The key row is inserted in the same transaction as the attempt, before the PSP call.

use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use uuid::Uuid;

pub const MAX_KEY_LEN: usize = 255;

/// Hash of what the client asked for. Built from the parsed request (not raw bytes), so a
/// retry with different whitespace or key order still matches. The invoice id is included,
/// so reusing a key on a different invoice is detected as a mismatch.
pub fn request_hash(invoice_id: Uuid, card_token: &str) -> Vec<u8> {
    let canonical = format!("POST /invoices/{invoice_id}/pay\n{card_token}");
    Sha256::digest(canonical.as_bytes()).to_vec()
}

pub struct StoredKey {
    pub request_hash: Vec<u8>,
    pub attempt_id: Uuid,
}

pub async fn find(conn: &mut PgConnection, business_id: Uuid, key: &str) -> sqlx::Result<Option<StoredKey>> {
    let row: Option<(Vec<u8>, Uuid)> = sqlx::query_as(
        "SELECT request_hash, attempt_id FROM idempotency_keys WHERE business_id = $1 AND key = $2",
    )
    .bind(business_id)
    .bind(key)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.map(|(request_hash, attempt_id)| StoredKey { request_hash, attempt_id }))
}

pub async fn insert(
    conn: &mut PgConnection,
    business_id: Uuid,
    key: &str,
    request_hash: &[u8],
    attempt_id: Uuid,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO idempotency_keys (business_id, key, request_hash, attempt_id) VALUES ($1, $2, $3, $4)",
    )
    .bind(business_id)
    .bind(key)
    .bind(request_hash)
    .bind(attempt_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}
