//! Invoice & Payment Service, as a library so integration tests can start the real app
//! in-process. The binary in `main.rs` is a thin wrapper around this.

pub mod app;
pub mod auth;
pub mod config;
pub mod customers;
pub mod domain;
pub mod error;
pub mod extract;
pub mod invoices;
pub mod payments;
pub mod seed;
pub mod webhooks;
pub mod workers;

/// Applies migrations embedded at compile time. sqlx records applied versions in
/// `_sqlx_migrations` (guarded by an advisory lock), so this is a no-op after the first run.
pub async fn migrate(db: &sqlx::PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    sqlx::migrate!("../migrations").run(db).await
}
