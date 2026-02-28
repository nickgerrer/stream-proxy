mod control;
mod models;
mod state;
mod status;
mod stream;
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

    let state = Arc::new(state::AppState::new());

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

    let addr = SocketAddr::from(([0, 0, 0, 0], 8888));
    tracing::info!("Rust proxy listening on {}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .unwrap();
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
