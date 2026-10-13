//! Configuration, read once from environment variables at startup.

use std::time::Duration;

#[derive(Clone)]
pub struct Config {
    pub database_url: String,
    pub bind_addr: String,
    pub psp_url: String,
    /// How long `/pay` waits for the PSP before answering 202 (D2: 5s).
    pub psp_timeout: Duration,
    /// If set, a demo business with this API key is created at startup (D5: zero-step demo).
    pub seed_api_key: Option<String>,
}

impl Config {
    pub fn from_env() -> Self {
        Config {
            database_url: required("DATABASE_URL"),
            bind_addr: optional("BIND_ADDR").unwrap_or_else(|| "0.0.0.0:8080".into()),
            psp_url: optional("PSP_URL").unwrap_or_else(|| "http://localhost:8081".into()),
            psp_timeout: Duration::from_millis(
                optional("PSP_TIMEOUT_MS").and_then(|v| v.parse().ok()).unwrap_or(5_000),
            ),
            seed_api_key: optional("SEED_API_KEY"),
        }
    }
}

fn required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("missing required env var {name}"))
}

fn optional(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}
