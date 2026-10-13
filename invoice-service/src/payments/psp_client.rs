//! HTTP client for the PSP. Treats it as an unreliable external dependency.
//!
//! The key idea: only an explicit answer from the PSP is a fact. A timeout, a dropped
//! connection or a 5xx tells us nothing about whether the card was charged, so those all map
//! to `Unknown` and the attempt stays `pending` until the reconciler finds out (D2).

use std::time::Duration;

use serde::Deserialize;
use uuid::Uuid;

#[derive(Clone)]
pub struct PspClient {
    http: reqwest::Client,
    base_url: String,
}

/// What we learned from the PSP.
#[derive(Debug)]
pub enum PspOutcome {
    Succeeded { psp_ref: String },
    Failed { code: String },
    /// Charge accepted but not finished, or we couldn't tell. Do not guess.
    Unknown,
    /// Status lookup only: the PSP has no record of this charge, so it was never received.
    NotFound,
}

#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum PspCharge {
    Processing,
    Succeeded { psp_ref: String },
    Failed { code: String },
}

impl From<PspCharge> for PspOutcome {
    fn from(c: PspCharge) -> Self {
        match c {
            PspCharge::Processing => PspOutcome::Unknown,
            PspCharge::Succeeded { psp_ref } => PspOutcome::Succeeded { psp_ref },
            PspCharge::Failed { code } => PspOutcome::Failed { code },
        }
    }
}

impl PspClient {
    pub fn new(base_url: String, timeout: Duration) -> Self {
        let http = reqwest::Client::builder()
            .timeout(timeout) // whole request: connect + send + wait for response
            .build()
            .expect("build PSP http client");
        PspClient { http, base_url }
    }

    /// Creates a charge. `attempt_id` is the PSP idempotency key: retrying with the same
    /// attempt can never charge twice.
    pub async fn charge(&self, attempt_id: Uuid, amount_cents: i64, card_token: &str) -> PspOutcome {
        let result = self
            .http
            .post(format!("{}/charges", self.base_url))
            .header("idempotency-key", attempt_id.to_string())
            .json(&serde_json::json!({ "amount_cents": amount_cents, "card_token": card_token }))
            .send()
            .await;
        self.interpret(attempt_id, result).await
    }

    /// Asks the PSP what happened to a charge (used by the reconciler).
    pub async fn lookup(&self, attempt_id: Uuid) -> PspOutcome {
        let result = self.http.get(format!("{}/charges/{attempt_id}", self.base_url)).send().await;
        if let Ok(resp) = &result {
            if resp.status() == reqwest::StatusCode::NOT_FOUND {
                return PspOutcome::NotFound;
            }
        }
        self.interpret(attempt_id, result).await
    }

    async fn interpret(
        &self,
        attempt_id: Uuid,
        result: Result<reqwest::Response, reqwest::Error>,
    ) -> PspOutcome {
        let resp = match result {
            Ok(resp) => resp,
            Err(err) => {
                let kind = if err.is_timeout() { "timeout" } else { "connection error" };
                tracing::warn!(%attempt_id, error = %err, "PSP {kind}: outcome unknown");
                return PspOutcome::Unknown;
            }
        };
        let status = resp.status();
        if !status.is_success() {
            tracing::warn!(%attempt_id, %status, "PSP returned an error status: outcome unknown");
            return PspOutcome::Unknown;
        }
        match resp.json::<PspCharge>().await {
            Ok(charge) => charge.into(),
            Err(err) => {
                tracing::warn!(%attempt_id, error = %err, "unreadable PSP response: outcome unknown");
                PspOutcome::Unknown
            }
        }
    }
}
