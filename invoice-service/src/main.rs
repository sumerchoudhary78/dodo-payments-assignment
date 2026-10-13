//! Invoice & Payment Service.
//!
//! Usage:
//!   invoice-service                        start the HTTP server (default)
//!   invoice-service create-business NAME   create a business, print its API key once

mod app;
mod auth;
mod config;
mod customers;
mod domain;
mod error;
mod extract;
mod invoices;
mod payments;
mod webhooks;
mod workers;

use sqlx::postgres::PgPoolOptions;

use crate::{app::AppState, config::Config, payments::psp_client::PspClient};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,sqlx=warn".into()),
        )
        .with_target(false)
        .init();

    let config = Config::from_env();
    let db = PgPoolOptions::new()
        .max_connections(20)
        .connect(&config.database_url)
        .await
        .expect("failed to connect to database");

    // Migrations are embedded in the binary at compile time and applied on every start;
    // sqlx records applied versions in `_sqlx_migrations`, so this is a no-op after the first run.
    sqlx::migrate!("../migrations").run(&db).await.expect("failed to run migrations");

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("serve") => serve(db, config).await,
        Some("create-business") => {
            let name = args.get(1).expect("usage: invoice-service create-business NAME");
            let key = auth::generate_key();
            let id = auth::create_business(&db, name, &key).await.expect("create business");
            println!("business_id: {id}\napi_key:     {key}\n(the key is shown once; only its hash is stored)");
        }
        Some(other) => {
            eprintln!("unknown command: {other}");
            std::process::exit(2);
        }
    }
}

async fn serve(db: sqlx::PgPool, config: Config) {
    if let Some(key) = &config.seed_api_key {
        auth::seed_demo_business(&db, key).await.expect("seed demo business");
    }

    let psp = PspClient::new(config.psp_url.clone(), config.psp_timeout);
    let state = AppState { db, config, psp };

    // Background workers run in the same process, independent of any request.
    tokio::spawn(workers::reconciler::run(state.clone()));

    let listener = tokio::net::TcpListener::bind(&state.config.bind_addr).await.expect("bind");
    tracing::info!(addr = %state.config.bind_addr, "invoice service listening");
    axum::serve(listener, app::router(state)).await.expect("server");
}
