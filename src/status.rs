use crate::models::*;
use crate::state::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

pub async fn channels_status(State(state): State<Arc<AppState>>) -> Json<ChannelsResponse> {
    let mut channels = HashMap::new();

    // Include all routed channels (active or idle)
    for entry in state.channel_routes.iter() {
        let channel_id = entry.key().clone();
        let status = if let Some(active) = state.active_channels.get(&channel_id) {
            ChannelStatus {
                state: "active".to_string(),
                clients: active.clients.len() as u32,
                upstream: Some(UpstreamStatus {
                    stream_id: active.stream_id,
                    account_id: active.account_id,
                    url: active.url.clone(),
                    connected_since: format_instant(active.connected_since),
                    bytes_transferred: active.bytes_transferred.load(Ordering::Relaxed),
                }),
            }
        } else {
            ChannelStatus {
                state: "idle".to_string(),
                clients: 0,
                upstream: None,
            }
        };
        channels.insert(channel_id, status);
    }

    let mut accounts = HashMap::new();
    for entry in state.accounts.iter() {
        accounts.insert(
            entry.key().to_string(),
            AccountStatus {
                active_connections: entry.value().active_connections.load(Ordering::Relaxed),
                max_connections: entry.value().max_connections,
            },
        );
    }

    Json(ChannelsResponse { channels, accounts })
}

pub async fn channel_detail(
    State(state): State<Arc<AppState>>,
    Path(channel_id): Path<String>,
) -> Result<Json<ChannelDetailResponse>, StatusCode> {
    if let Some(active) = state.active_channels.get(&channel_id) {
        let clients: Vec<ClientInfo> = active
            .clients
            .iter()
            .map(|c| ClientInfo {
                id: c.id.clone(),
                connected_since: format_instant(c.connected_since),
                bytes_sent: c.bytes_sent.load(Ordering::Relaxed),
                remote_addr: c.remote_addr.clone(),
            })
            .collect();

        Ok(Json(ChannelDetailResponse {
            status: ChannelStatus {
                state: "active".to_string(),
                clients: active.clients.len() as u32,
                upstream: Some(UpstreamStatus {
                    stream_id: active.stream_id,
                    account_id: active.account_id,
                    url: active.url.clone(),
                    connected_since: format_instant(active.connected_since),
                    bytes_transferred: active.bytes_transferred.load(Ordering::Relaxed),
                }),
            },
            clients,
        }))
    } else if state.channel_routes.contains_key(&channel_id) {
        Ok(Json(ChannelDetailResponse {
            status: ChannelStatus {
                state: "idle".to_string(),
                clients: 0,
                upstream: None,
            },
            clients: vec![],
        }))
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

fn format_instant(instant: tokio::time::Instant) -> String {
    let elapsed = instant.elapsed();
    let system_time = std::time::SystemTime::now() - elapsed;
    let datetime: chrono::DateTime<chrono::Utc> = system_time.into();
    datetime.to_rfc3339()
}
