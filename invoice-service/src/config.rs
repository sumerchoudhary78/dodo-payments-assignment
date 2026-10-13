//! Configuration, read once from environment variables at startup.

use std::time::Duration;

#[derive(Clone)]
pub struct Config {
    pub database_url: String,
    pub bind_addr: String,
    pub psp_url: String,
    /// How long `/pay` waits for the PSP before answering 202 (D2: 5s).
    pub psp_timeout: Duration,
    /// How often the reconciler looks for stuck pending attempts (D2: 10s).
    pub reconcile_interval: Duration,
    /// How old a pending attempt must be before the reconciler asks the PSP about it (D2: 30s).
    pub reconcile_after: Duration,
    /// If set, a demo business with this API key is created at startup (D5: zero-step demo).
    pub seed_api_key: Option<String>,
    /// If set together with the seed key, the demo business also gets this webhook endpoint
    /// with this known secret, so the demo receiver can verify signatures with no manual setup.
    pub seed_webhook_url: Option<String>,
    pub seed_webhook_secret: Option<String>,
}

impl Config {
    pub fn from_env() -> Self {
        let config = Config {
            database_url: required("DATABASE_URL"),
            bind_addr: optional("BIND_ADDR").unwrap_or_else(|| "0.0.0.0:8080".into()),
            psp_url: optional("PSP_URL").unwrap_or_else(|| "http://localhost:8081".into()),
            psp_timeout: ms("PSP_TIMEOUT_MS", 5_000),
            reconcile_interval: ms("RECONCILE_INTERVAL_MS", 10_000),
            reconcile_after: ms("RECONCILE_AFTER_MS", 30_000),
            seed_api_key: optional("SEED_API_KEY"),
            seed_webhook_url: optional("SEED_WEBHOOK_URL"),
            seed_webhook_secret: optional("SEED_WEBHOOK_SECRET"),
        };
        // Safety invariant: the reconciler must never look up an attempt whose /pay request
        // could still be in flight. A PSP "not found" for an in-flight charge would be marked
        // failed, and the late-arriving charge would then succeed: a possible double charge.
        assert!(
            config.reconcile_after >= config.psp_timeout * 3,
            "RECONCILE_AFTER_MS must be at least 3x PSP_TIMEOUT_MS"
        );
        config
    }
}

fn required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("missing required env var {name}"))
}

fn optional(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn ms(name: &str, default: u64) -> Duration {
    Duration::from_millis(optional(name).and_then(|v| v.parse().ok()).unwrap_or(default))
}
