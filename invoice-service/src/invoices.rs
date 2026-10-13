//! Invoices: create, get, list (filter by state), and the manual state transitions
//! finalize / void / mark-uncollectible. The `paid` transition lives in `payments`.
//!
//! Concurrency rule used everywhere an invoice's state changes:
//!   every state change first takes the invoice row lock (`SELECT ... FOR UPDATE`), then
//!   decides, then writes, all in one transaction. The row lock is the single serialization
//!   point for an invoice, so "check then write" can never race with another writer.

use std::collections::HashMap;

use axum::{
    Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::{
    app::AppState,
    auth::Business,
    customers::{List, Page},
    domain::{
        invoice_state::InvoiceState,
        money::{LineItemInput, price_line_items},
    },
    error::{ApiError, ApiResult},
    extract::{Json, Path, Query},
    webhooks::events,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/invoices", get(list_invoices).post(create_invoice))
        .route("/invoices/{id}", get(get_invoice))
        .route("/invoices/{id}/finalize", post(finalize))
        .route("/invoices/{id}/void", post(void))
        .route("/invoices/{id}/mark-uncollectible", post(mark_uncollectible))
}

// ─── Response shapes ───────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct Invoice {
    pub id: Uuid,
    pub customer_id: Uuid,
    pub state: InvoiceState,
    pub currency: String,
    pub total_cents: i64,
    pub due_date: NaiveDate,
    pub line_items: Vec<LineItem>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub paid_at: Option<DateTime<Utc>>,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct LineItem {
    #[serde(skip)]
    invoice_id: Uuid,
    pub description: String,
    pub quantity: i64,
    pub unit_amount_cents: i64,
    pub amount_cents: i64,
}

/// Raw row; `state` is TEXT in the DB and converted to the enum on the way out.
#[derive(sqlx::FromRow)]
struct InvoiceRow {
    id: Uuid,
    customer_id: Uuid,
    state: String,
    currency: String,
    total_cents: i64,
    due_date: NaiveDate,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    paid_at: Option<DateTime<Utc>>,
}

fn parse_state(s: &str) -> InvoiceState {
    InvoiceState::parse(s).expect("invoices.state CHECK constraint guarantees a known state")
}

impl InvoiceRow {
    fn into_invoice(self, line_items: Vec<LineItem>) -> Invoice {
        Invoice {
            id: self.id,
            customer_id: self.customer_id,
            state: parse_state(&self.state),
            currency: self.currency,
            total_cents: self.total_cents,
            due_date: self.due_date,
            line_items,
            created_at: self.created_at,
            updated_at: self.updated_at,
            paid_at: self.paid_at,
        }
    }
}

// ─── Shared helpers (also used by payments) ────────────────────────────────────────

pub struct LockedInvoice {
    pub state: InvoiceState,
    pub total_cents: i64,
}

/// Locks the invoice row for the rest of the transaction. A concurrent caller blocks here
/// until we commit, then sees our committed state.
pub async fn lock_invoice(
    conn: &mut PgConnection,
    id: Uuid,
    business_id: Uuid,
) -> ApiResult<LockedInvoice> {
    let row: Option<(String, i64)> = sqlx::query_as(
        "SELECT state, total_cents FROM invoices WHERE id = $1 AND business_id = $2 FOR UPDATE",
    )
    .bind(id)
    .bind(business_id)
    .fetch_optional(&mut *conn)
    .await?;
    let (state, total_cents) = row.ok_or_else(|| ApiError::not_found("invoice"))?;
    Ok(LockedInvoice { state: parse_state(&state), total_cents })
}

/// Writes a state change. Caller must hold the row lock and have checked the transition;
/// the `AND state = $from` guard is a second line of defence, not the primary check.
pub async fn set_state(
    conn: &mut PgConnection,
    id: Uuid,
    from: InvoiceState,
    to: InvoiceState,
) -> ApiResult<()> {
    debug_assert!(from.can_transition_to(to));
    let result = sqlx::query(
        "UPDATE invoices
         SET state = $3, updated_at = now(),
             paid_at = CASE WHEN $3 = 'paid' THEN now() ELSE paid_at END
         WHERE id = $1 AND state = $2",
    )
    .bind(id)
    .bind(from.as_str())
    .bind(to.as_str())
    .execute(&mut *conn)
    .await?;
    if result.rows_affected() != 1 {
        tracing::error!(%id, ?from, ?to, "state guard failed while holding row lock");
        return Err(ApiError::internal());
    }
    Ok(())
}

pub async fn load_invoice(
    conn: &mut PgConnection,
    id: Uuid,
    business_id: Uuid,
) -> ApiResult<Invoice> {
    let row = sqlx::query_as::<_, InvoiceRow>(
        "SELECT id, customer_id, state, currency, total_cents, due_date, created_at, updated_at, paid_at
         FROM invoices WHERE id = $1 AND business_id = $2",
    )
    .bind(id)
    .bind(business_id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| ApiError::not_found("invoice"))?;
    let mut items = load_line_items(conn, &[id]).await?;
    Ok(row.into_invoice(items.remove(&id).unwrap_or_default()))
}

async fn load_line_items(
    conn: &mut PgConnection,
    invoice_ids: &[Uuid],
) -> ApiResult<HashMap<Uuid, Vec<LineItem>>> {
    let rows = sqlx::query_as::<_, LineItem>(
        "SELECT invoice_id, description, quantity, unit_amount_cents, amount_cents
         FROM invoice_line_items WHERE invoice_id = ANY($1) ORDER BY invoice_id, position",
    )
    .bind(invoice_ids)
    .fetch_all(&mut *conn)
    .await?;
    let mut by_invoice: HashMap<Uuid, Vec<LineItem>> = HashMap::new();
    for item in rows {
        by_invoice.entry(item.invoice_id).or_default().push(item);
    }
    Ok(by_invoice)
}

// ─── Handlers ──────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CreateInvoice {
    pub customer_id: Uuid,
    pub due_date: NaiveDate,
    pub line_items: Vec<LineItemInput>,
    // No `total` field: if a client sends one, serde ignores it. The server computes it.
}

async fn create_invoice(
    State(state): State<AppState>,
    business: Business,
    Json(req): Json<CreateInvoice>,
) -> ApiResult<(StatusCode, Json<Invoice>)> {
    let (priced, total) = price_line_items(&req.line_items).map_err(ApiError::validation)?;
    if req.due_date < Utc::now().date_naive() {
        return Err(ApiError::validation("due_date must not be in the past"));
    }

    let mut tx = state.db.begin().await?;

    let customer_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM customers WHERE id = $1 AND business_id = $2)",
    )
    .bind(req.customer_id)
    .bind(business.id)
    .fetch_one(&mut *tx)
    .await?;
    if !customer_exists {
        return Err(ApiError::not_found("customer"));
    }

    let invoice_id: Uuid = sqlx::query_scalar(
        "INSERT INTO invoices (business_id, customer_id, total_cents, due_date)
         VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(business.id)
    .bind(req.customer_id)
    .bind(total)
    .bind(req.due_date)
    .fetch_one(&mut *tx)
    .await?;

    for (position, item) in priced.iter().enumerate() {
        sqlx::query(
            "INSERT INTO invoice_line_items
               (invoice_id, position, description, quantity, unit_amount_cents, amount_cents)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(invoice_id)
        .bind(position as i32)
        .bind(item.description)
        .bind(item.quantity)
        .bind(item.unit_amount_cents)
        .bind(item.amount_cents)
        .execute(&mut *tx)
        .await?;
    }

    let invoice = load_invoice(&mut tx, invoice_id, business.id).await?;
    let payload = serde_json::json!({ "invoice": invoice });
    events::enqueue(&mut tx, business.id, events::INVOICE_CREATED, payload).await?;
    tx.commit().await?;

    tracing::info!(invoice_id = %invoice.id, total_cents = total, "invoice created");
    Ok((StatusCode::CREATED, Json(invoice)))
}

async fn get_invoice(
    State(state): State<AppState>,
    business: Business,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Invoice>> {
    let mut conn = state.db.acquire().await?;
    Ok(Json(load_invoice(&mut conn, id, business.id).await?))
}

#[derive(Deserialize)]
struct ListQuery {
    state: Option<String>,
    #[serde(flatten)]
    page: Page,
}

async fn list_invoices(
    State(state): State<AppState>,
    business: Business,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<List<Invoice>>> {
    let filter = match query.state.as_deref() {
        None => None,
        Some(s) => Some(InvoiceState::parse(s).ok_or_else(|| {
            ApiError::validation("state must be one of: draft, open, paid, void, uncollectible")
        })?),
    };
    let limit = query.page.limit();

    let mut conn = state.db.acquire().await?;
    let rows = sqlx::query_as::<_, InvoiceRow>(
        "SELECT id, customer_id, state, currency, total_cents, due_date, created_at, updated_at, paid_at
         FROM invoices
         WHERE business_id = $1
           AND ($2::text IS NULL OR state = $2)
           AND ($3::uuid IS NULL OR id < $3)
         ORDER BY id DESC LIMIT $4",
    )
    .bind(business.id)
    .bind(filter.map(InvoiceState::as_str))
    .bind(query.page.starting_after)
    .bind(limit + 1)
    .fetch_all(&mut *conn)
    .await?;

    let ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
    let mut items = load_line_items(&mut conn, &ids).await?;
    let invoices = rows
        .into_iter()
        .map(|row| {
            let line_items = items.remove(&row.id).unwrap_or_default();
            row.into_invoice(line_items)
        })
        .collect();
    Ok(Json(List::from_rows(invoices, limit)))
}

async fn finalize(state: State<AppState>, business: Business, id: Path<Uuid>) -> ApiResult<Json<Invoice>> {
    transition(state, business, id, InvoiceState::Open).await
}

async fn void(state: State<AppState>, business: Business, id: Path<Uuid>) -> ApiResult<Json<Invoice>> {
    transition(state, business, id, InvoiceState::Void).await
}

async fn mark_uncollectible(
    state: State<AppState>,
    business: Business,
    id: Path<Uuid>,
) -> ApiResult<Json<Invoice>> {
    transition(state, business, id, InvoiceState::Uncollectible).await
}

async fn transition(
    State(state): State<AppState>,
    business: Business,
    Path(id): Path<Uuid>,
    to: InvoiceState,
) -> ApiResult<Json<Invoice>> {
    let mut tx = state.db.begin().await?;
    let current = lock_invoice(&mut tx, id, business.id).await?;

    if !current.state.can_transition_to(to) {
        let terminal = if current.state.is_terminal() { " (a terminal state)" } else { "" };
        return Err(ApiError::conflict(
            "invalid_state_transition",
            format!("cannot move invoice from {}{terminal} to {}", current.state.as_str(), to.as_str()),
        ));
    }

    // Voiding while a charge is in flight could leave money taken on a void invoice (D1).
    // This read happens AFTER we hold the row lock, and /pay inserts its pending attempt while
    // holding the same lock, so the answer can't change underneath us.
    if to == InvoiceState::Void {
        let pending: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM payment_attempts WHERE invoice_id = $1 AND status = 'pending')",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        if pending {
            return Err(ApiError::conflict(
                "payment_in_progress",
                "cannot void an invoice while a payment attempt is pending",
            ));
        }
    }

    set_state(&mut tx, id, current.state, to).await?;
    let invoice = load_invoice(&mut tx, id, business.id).await?;
    tx.commit().await?;

    tracing::info!(invoice_id = %id, from = current.state.as_str(), to = to.as_str(), "invoice state changed");
    Ok(Json(invoice))
}
