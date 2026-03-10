mod config;
mod control;
mod models;
mod state;
mod status;
mod stream;
mod transcode;
mod upstream;

use axum::{routing::get, Router};
use std::net::SocketAddr;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let config = config::Config::from_env();
    let port = config.port;
    let state = Arc::new(state::AppState::new(config));

    // Spawn periodic cooldown cleanup (every 5 minutes)
    {
        let state = state.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
            loop {
                interval.tick().await;
                state.cleanup_expired_cooldowns();
            }
        });
    }

    let shutdown_state = state.clone();

    let app = Router::new()
        // Control API
        .route(
            "/control/v1/channels/{channel_id}",
            axum::routing::put(control::put_channel),
        )
        .route(
            "/control/v1/channels/{channel_id}",
            axum::routing::delete(control::delete_channel),
        )
        .route(
            "/control/v1/accounts/{account_id}",
            axum::routing::put(control::put_account),
        )
        .route("/control/v1/sync", axum::routing::post(control::sync))
        .route(
            "/control/v1/channels/{channel_id}/cooldowns",
            axum::routing::delete(control::clear_channel_cooldowns),
        )
        .route(
            "/control/v1/cooldowns",
            axum::routing::delete(control::clear_all_cooldowns),
        )
        .route(
            "/control/v1/hdhr",
            axum::routing::post(control::post_hdhr),
        )
        // Stream endpoint
        .route("/stream/{channel_id}", get(stream::stream_channel))
        // Status API
        .route("/status/v1/channels", get(status::channels_status))
        .route(
            "/status/v1/channels/{channel_id}",
            get(status::channel_detail),
        )
        .route("/status/v1/health", get(health))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!("Rust proxy listening on {}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal(shutdown_state))
    .await
    .unwrap();
}

async fn shutdown_signal(state: Arc<state::AppState>) {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("Shutdown signal received, stopping all channels...");

    // Stop all active upstream connections
    for entry in state.active_channels.iter() {
        let _ = entry.value().stop_tx.send(true);
    }

    // Brief pause to let streams close and clients receive EOF
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    tracing::info!("Shutdown complete");
}

async fn health(
    axum::extract::State(state): axum::extract::State<Arc<state::AppState>>,
) -> axum::Json<models::HealthResponse> {
    let elapsed = state.start_time.elapsed().as_secs();
    let active = state.active_channels.len();
    let clients: u32 = state
        .active_channels
        .iter()
        .map(|c| c.clients.len() as u32)
        .sum();

    axum::Json(models::HealthResponse {
        status: "ok".to_string(),
        uptime_seconds: elapsed,
        active_channels: active,
        total_clients: clients,
    })
}
