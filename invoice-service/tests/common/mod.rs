//! Test harness: starts the real invoice service and the real mock PSP in-process, each test
//! with its own freshly migrated database, so tests can run in parallel without interfering.
//!
//! Needs a Postgres server: `docker compose up -d db`, or set TEST_DATABASE_URL.

#![allow(dead_code)] // each test binary uses a different subset of these helpers

use std::{sync::Arc, time::Duration};

use invoice_service::{app, app::AppState, auth, config::Config, payments::psp_client::PspClient, workers};
use serde_json::{Value, json};
use sqlx::{Connection, PgConnection, PgPool};
use uuid::Uuid;

pub const PSP_TIMEOUT: Duration = Duration::from_millis(500);
/// Normal PSP latency. Long enough that concurrent requests really overlap.
const PSP_DELAY: Duration = Duration::from_millis(150);
/// How long `tok_timeout` takes in tests (30s in the demo).
const PSP_SLOW_DELAY: Duration = Duration::from_millis(1_500);

pub struct TestApp {
    pub base: String,
    pub psp_url: String,
    pub http: reqwest::Client,
    pub api_key: String,
    pub business_id: Uuid,
    pub db: PgPool,
    pub psp: mock_psp::AppState,
}

fn server_url() -> String {
    std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@localhost:5433/postgres".into())
}

async fn fresh_database() -> PgPool {
    let server = server_url();
    let name = format!("test_{}", Uuid::new_v4().simple());
    let mut admin = PgConnection::connect(&server).await.expect("connect to test Postgres server");
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&mut admin)
        .await
        .expect("create test database");
    let (base, _) = server.rsplit_once('/').expect("database url has a path");
    let db = PgPool::connect(&format!("{base}/{name}")).await.expect("connect to test database");
    invoice_service::migrate(&db).await.expect("migrate test database");
    db
}

async fn serve(router: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

pub async fn spawn_app() -> Arc<TestApp> {
    let db = fresh_database().await;

    let psp = mock_psp::AppState::new(PSP_DELAY, PSP_SLOW_DELAY);
    let psp_url = serve(mock_psp::router(psp.clone())).await;

    let config = Config {
        database_url: String::new(),
        bind_addr: String::new(),
        psp_url: psp_url.clone(),
        psp_timeout: PSP_TIMEOUT,
        reconcile_interval: Duration::from_millis(200),
        reconcile_after: PSP_TIMEOUT * 3,
        seed_api_key: None,
        seed_webhook_url: None,
        seed_webhook_secret: None,
    };
    let state = AppState { db: db.clone(), psp: PspClient::new(psp_url.clone(), config.psp_timeout), config };
    tokio::spawn(workers::reconciler::run(state.clone()));
    let base = serve(app::router(state)).await;

    let api_key = auth::generate_key();
    let business_id = auth::create_business(&db, "Test Business", &api_key).await.unwrap();

    Arc::new(TestApp { base, psp_url, http: reqwest::Client::new(), api_key, business_id, db, psp })
}

impl TestApp {
    async fn post(&self, path: &str, body: Value, idempotency_key: Option<&str>) -> (u16, Value) {
        let mut req = self.http.post(format!("{}{path}", self.base)).bearer_auth(&self.api_key).json(&body);
        if let Some(key) = idempotency_key {
            req = req.header("idempotency-key", key);
        }
        let resp = req.send().await.unwrap();
        (resp.status().as_u16(), resp.json().await.unwrap_or(Value::Null))
    }

    pub async fn get(&self, path: &str) -> Value {
        let resp = self.http.get(format!("{}{path}", self.base)).bearer_auth(&self.api_key).send().await.unwrap();
        resp.json().await.unwrap()
    }

    /// Creates a customer and a finalized (open) invoice for `amount_cents`.
    pub async fn open_invoice(&self, amount_cents: i64) -> String {
        let (_, customer) = self.post("/customers", json!({"name": "Test", "email": "t@example.com"}), None).await;
        let (status, invoice) = self
            .post(
                "/invoices",
                json!({
                    "customer_id": customer["id"],
                    "due_date": "2099-01-01",
                    "line_items": [{"description": "Item", "quantity": 1, "unit_amount_cents": amount_cents}],
                }),
                None,
            )
            .await;
        assert_eq!(status, 201, "{invoice}");
        let id = invoice["id"].as_str().unwrap().to_string();
        let (status, _) = self.post(&format!("/invoices/{id}/finalize"), json!({}), None).await;
        assert_eq!(status, 200);
        id
    }

    pub async fn pay(&self, invoice_id: &str, card_token: &str, idempotency_key: &str) -> (u16, Value) {
        self.post(&format!("/invoices/{invoice_id}/pay"), json!({"card_token": card_token}), Some(idempotency_key))
            .await
    }

    pub async fn void(&self, invoice_id: &str) -> (u16, Value) {
        self.post(&format!("/invoices/{invoice_id}/void"), json!({}), None).await
    }

    pub async fn invoice_state(&self, invoice_id: &str) -> String {
        self.get(&format!("/invoices/{invoice_id}")).await["state"].as_str().unwrap().to_string()
    }

    pub async fn attempt(&self, attempt_id: &str) -> Value {
        self.get(&format!("/payment_attempts/{attempt_id}")).await
    }

    /// POST /charges requests the PSP has received (including duplicates).
    pub fn psp_charge_requests(&self) -> usize {
        self.psp.charge_requests.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Polls an attempt until it leaves `pending`, failing the test after `timeout`.
    pub async fn wait_for_resolution(&self, attempt_id: &str, timeout: Duration) -> Value {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let attempt = self.attempt(attempt_id).await;
            if attempt["status"] != "pending" {
                return attempt;
            }
            assert!(tokio::time::Instant::now() < deadline, "attempt {attempt_id} still pending after {timeout:?}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

pub fn error_code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or("")
}
