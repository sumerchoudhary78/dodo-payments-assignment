//! Mock payment processor (PSP).
//!
//! Behaves like a real external PSP in the two ways the invoice service relies on:
//!   1. `POST /charges` is idempotent on the `Idempotency-Key` header — the same key never
//!      charges twice, it returns the existing charge.
//!   2. `GET /charges/{key}` reports what happened to a charge, so a caller that timed out
//!      or crashed can find out the real outcome later.
//!
//! The outcome is decided by the card token (see `charge::Outcome::from_token`).
//! State is in memory only: restarting the mock forgets every charge (a real PSP is durable).
//!
//! It is a library so the invoice service's integration tests can run it in-process;
//! at runtime it is a separate binary and shares no code with the invoice service.

pub mod charge;
pub mod handlers;

use axum::{
    Router,
    routing::{get, post},
};

pub use handlers::AppState;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/charges", post(handlers::create_charge))
        .route("/charges/{key}", get(handlers::get_charge))
        .with_state(state)
}
