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

    // Build streaming response body
    let client_bytes_clone = client_bytes.clone();
    let active_clone = active.clone();
    let channel_id_clone = channel_id.clone();
    let client_id_clone = client_id.clone();
    let state_clone = state.clone();

    let body_stream = async_stream::stream! {
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
                            tracing::info!("Channel {} broadcast closed", channel_id_clone);
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

        // Cleanup
        active_clone.clients.remove(&client_id_clone);
        tracing::info!(
            "Channel {}: client {} disconnected (sent {} bytes)",
            channel_id_clone,
            client_id_clone,
            client_bytes_clone.load(Ordering::Relaxed)
        );

        // If last client, stop the channel after a delay
        if active_clone.clients.is_empty() {
            let _state_for_cleanup = state_clone.clone();
            let ch_id = channel_id_clone.clone();
            let active_for_cleanup = active_clone.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                if active_for_cleanup.clients.is_empty() {
                    tracing::info!("Channel {}: no clients for 30s, stopping", ch_id);
                    let _ = active_for_cleanup.stop_tx.send(true);
                }
            });
        }
    };

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "video/mp2t")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(body_stream))
        .unwrap()
}
