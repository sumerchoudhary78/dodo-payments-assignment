use std::time::Duration;

use mock_psp::AppState;

fn env_ms(name: &str, default: u64) -> Duration {
    let ms = std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default);
    Duration::from_millis(ms)
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_target(false).init();

    let state = AppState::new(env_ms("PSP_NORMAL_DELAY_MS", 100), env_ms("PSP_TIMEOUT_DELAY_MS", 30_000));

    let addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8081".into());
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind");
    tracing::info!(%addr, "mock PSP listening");
    axum::serve(listener, mock_psp::router(state)).await.expect("server");
}
