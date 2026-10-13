//! Customers: create, get, list. Every query is scoped to the authenticated business.

use axum::{Router, extract::State, http::StatusCode, routing::get};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    app::AppState,
    auth::Business,
    error::{ApiError, ApiResult},
    extract::{Json, Path, Query},
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/customers", get(list_customers).post(create_customer))
        .route("/customers/{id}", get(get_customer))
}

#[derive(Serialize, sqlx::FromRow)]
pub struct Customer {
    pub id: Uuid,
    pub name: String,
    pub email: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Deserialize)]
pub struct CreateCustomer {
    pub name: String,
    pub email: String,
}

async fn create_customer(
    State(state): State<AppState>,
    business: Business,
    Json(req): Json<CreateCustomer>,
) -> ApiResult<(StatusCode, Json<Customer>)> {
    let name = req.name.trim();
    let email = req.email.trim();
    if name.is_empty() {
        return Err(ApiError::validation("name must not be empty"));
    }
    // Deliberately minimal: real email validation means sending an email.
    if !email.contains('@') {
        return Err(ApiError::validation("email must be a valid email address"));
    }

    let customer = sqlx::query_as::<_, Customer>(
        "INSERT INTO customers (business_id, name, email) VALUES ($1, $2, $3)
         RETURNING id, name, email, created_at",
    )
    .bind(business.id)
    .bind(name)
    .bind(email)
    .fetch_one(&state.db)
    .await?;

    Ok((StatusCode::CREATED, Json(customer)))
}

async fn get_customer(
    State(state): State<AppState>,
    business: Business,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Customer>> {
    // business_id in the WHERE clause: another business's customer is a 404, not a 403,
    // so we don't even confirm that the id exists.
    sqlx::query_as::<_, Customer>(
        "SELECT id, name, email, created_at FROM customers WHERE id = $1 AND business_id = $2",
    )
    .bind(id)
    .bind(business.id)
    .fetch_optional(&state.db)
    .await?
    .map(Json)
    .ok_or_else(|| ApiError::not_found("customer"))
}

/// Cursor pagination shared by list endpoints. UUID v7 ids sort by creation time, so
/// "everything older than this id" is a stable cursor that uses the (business_id, id) index.
#[derive(Deserialize)]
pub struct Page {
    pub limit: Option<i64>,
    pub starting_after: Option<Uuid>,
}

impl Page {
    pub fn limit(&self) -> i64 {
        self.limit.unwrap_or(20).clamp(1, 100)
    }
}

#[derive(Serialize)]
pub struct List<T> {
    pub data: Vec<T>,
    pub has_more: bool,
}

impl<T> List<T> {
    /// Callers fetch `limit + 1` rows; the extra row only tells us whether another page exists.
    pub fn from_rows(mut rows: Vec<T>, limit: i64) -> Self {
        let has_more = rows.len() as i64 > limit;
        rows.truncate(limit as usize);
        List { data: rows, has_more }
    }
}

async fn list_customers(
    State(state): State<AppState>,
    business: Business,
    Query(page): Query<Page>,
) -> ApiResult<Json<List<Customer>>> {
    let limit = page.limit();
    let rows = sqlx::query_as::<_, Customer>(
        "SELECT id, name, email, created_at FROM customers
         WHERE business_id = $1 AND ($2::uuid IS NULL OR id < $2)
         ORDER BY id DESC LIMIT $3",
    )
    .bind(business.id)
    .bind(page.starting_after)
    .bind(limit + 1)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(List::from_rows(rows, limit)))
}
