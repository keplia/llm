mod admin;
mod auth;
mod cache;
mod config;
mod db;
mod error;
mod handlers;
mod providers;
mod state;

use std::net::SocketAddr;
use std::sync::Arc;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "llm_router=info,tower_http=info".into()),
        )
        .init();

    let path = std::env::var("CONFIG_PATH").unwrap_or_else(|_| "config.toml".to_string());
    let cfg = config::Config::load(&path).unwrap_or_else(|e| {
        eprintln!("gagal memuat config: {e}");
        std::process::exit(1);
    });
    let state = Arc::new(state::AppState::new(cfg));

    let app = handlers::build_router(state.clone());

    let addr: SocketAddr = format!("{}:{}", state.config.host, state.config.port)
        .parse()
        .expect("host:port tidak valid");

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| {
            eprintln!("gagal bind ke {addr}: {e}");
            std::process::exit(1);
        });

    tracing::info!("LLM router listening on http://{addr}");
    axum::serve(listener, app).await.unwrap();
}
