use crate::models::*;
use crate::state::*;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use std::sync::Arc;

pub async fn put_channel(
    State(state): State<Arc<AppState>>,
    Path(channel_id): Path<String>,
    Json(config): Json<ChannelConfig>,
) -> StatusCode {
    state.channel_routes.insert(
        channel_id.clone(),
        ChannelRouting {
            streams: config.streams,
        },
    );
    tracing::info!("Channel {} config updated", channel_id);
    StatusCode::OK
}

pub async fn delete_channel(
    State(state): State<Arc<AppState>>,
    Path(channel_id): Path<String>,
) -> StatusCode {
    state.channel_routes.remove(&channel_id);

    // Stop active stream if running
    if let Some((_, active)) = state.active_channels.remove(&channel_id) {
        let _ = active.stop_tx.send(true);
        state.decrement_connections(active.account_id);
        tracing::info!("Channel {} stopped and removed", channel_id);
    } else {
        tracing::info!("Channel {} config removed", channel_id);
    }

    StatusCode::OK
}

pub async fn put_account(
    State(state): State<Arc<AppState>>,
    Path(account_id): Path<u64>,
    Json(config): Json<AccountConfig>,
) -> StatusCode {
    state.accounts.insert(
        account_id,
        AccountState {
            max_connections: config.max_connections,
            active_connections: std::sync::atomic::AtomicU32::new(0),
        },
    );
    tracing::info!(
        "Account {} limit set to {}",
        account_id,
        config.max_connections
    );
    StatusCode::OK
}

pub async fn sync(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SyncRequest>,
) -> StatusCode {
    // Stop all active channels
    for entry in state.active_channels.iter() {
        let _ = entry.value().stop_tx.send(true);
    }
    state.active_channels.clear();

    // Replace routing table
    state.channel_routes.clear();
    for (id, config) in req.channels {
        state.channel_routes.insert(
            id,
            ChannelRouting {
                streams: config.streams,
            },
        );
    }

    // Replace accounts
    state.accounts.clear();
    for (id_str, config) in req.accounts {
        if let Ok(id) = id_str.parse::<u64>() {
            state.accounts.insert(
                id,
                AccountState {
                    max_connections: config.max_connections,
                    active_connections: std::sync::atomic::AtomicU32::new(0),
                },
            );
        }
    }

    let channels = state.channel_routes.len();
    let accounts = state.accounts.len();
    tracing::info!("Sync complete: {} channels, {} accounts", channels, accounts);
    StatusCode::OK
}
