use crate::state::{AppState, ClientState};
use crate::upstream;
use axum::{
    body::Body,
    extract::{ConnectInfo, Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::time::Instant;

/// TS null packet (188 bytes) used as keepalive
fn ts_null_packet() -> Bytes {
    let mut pkt = vec![0u8; 188];
    pkt[0] = 0x47; // Sync byte
    pkt[1] = 0x1F; // PID 0x1FFF (null packet)
    pkt[2] = 0xFF;
    pkt[3] = 0x10; // Adaptation field control: payload only
    Bytes::from(pkt)
}

/// Guard that cleans up client state when dropped (i.e. when client disconnects)
struct ClientGuard {
    channel_id: String,
    client_id: String,
    active: Arc<crate::state::ActiveChannel>,
    state: Arc<AppState>,
    bytes_sent: Arc<AtomicU64>,
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.active.clients.remove(&self.client_id);
        tracing::info!(
            "Channel {}: client {} disconnected (sent {} bytes)",
            self.channel_id,
            self.client_id,
            self.bytes_sent.load(Ordering::Relaxed)
        );

        // If last client, stop the channel immediately
        if self.active.clients.is_empty() {
            tracing::info!("Channel {}: no clients remaining, stopping", self.channel_id);
            let _ = self.active.stop_tx.send(true);
        }
    }
}

pub async fn stream_channel(
    State(state): State<Arc<AppState>>,
    Path(channel_id): Path<String>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
    // Get or start the channel
    let active = if let Some(existing) = state.active_channels.get(&channel_id) {
        existing.value().clone()
    } else {
        // Select a stream + account
        let (stream_id, account_id, url) = match state.select_stream(&channel_id) {
            Some(s) => s,
            None => {
                return (StatusCode::SERVICE_UNAVAILABLE, "No streams available").into_response();
            }
        };

        upstream::start_channel(state.clone(), channel_id.clone(), stream_id, account_id, url)
    };

    // Subscribe to broadcast channel
    let mut rx = active.sender.subscribe();

    // Register client
    let client_id = uuid::Uuid::new_v4().to_string();
    let client_bytes = Arc::new(AtomicU64::new(0));
    active.clients.insert(
        client_id.clone(),
        ClientState {
            id: client_id.clone(),
            connected_since: Instant::now(),
            bytes_sent: AtomicU64::new(0),
            remote_addr: addr.to_string(),
        },
    );

    tracing::info!(
        "Channel {}: client {} connected from {}",
        channel_id,
        client_id,
        addr
    );

    // Create drop guard for cleanup on client disconnect
    let guard = ClientGuard {
        channel_id: channel_id.clone(),
        client_id: client_id.clone(),
        active: active.clone(),
        state: state.clone(),
        bytes_sent: client_bytes.clone(),
    };

    // Build streaming response body
    let client_bytes_clone = client_bytes.clone();
    let active_clone = active.clone();
    let client_id_clone = client_id.clone();

    let body_stream = async_stream::stream! {
        // Hold the guard — it will run cleanup when this stream is dropped
        let _guard = guard;
        let keepalive = ts_null_packet();
        let mut keepalive_interval = tokio::time::interval(std::time::Duration::from_millis(500));

        loop {
            tokio::select! {
                result = rx.recv() => {
                    match result {
                        Ok(chunk) => {
                            let len = chunk.len() as u64;
                            client_bytes_clone.fetch_add(len, Ordering::Relaxed);
                            if let Some(client) = active_clone.clients.get(&client_id_clone) {
                                client.bytes_sent.fetch_add(len, Ordering::Relaxed);
                            }
                            yield Ok::<_, std::io::Error>(chunk);
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!("Client {} lagged {} messages", client_id_clone, n);
                            // Continue — client will catch up
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            tracing::info!("Broadcast closed for client {}", client_id_clone);
                            break;
                        }
                    }
                }
                _ = keepalive_interval.tick() => {
                    // Only send keepalive if no data recently
                    yield Ok::<_, std::io::Error>(keepalive.clone());
                }
            }
        }
        // Guard is dropped here too (normal exit), running cleanup
    };

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "video/mp2t")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(body_stream))
        .unwrap()
}
