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
    if let Some(existing) = state.accounts.get(&account_id) {
        existing
            .max_connections
            .store(config.max_connections, std::sync::atomic::Ordering::Relaxed);
    } else {
        state.accounts.insert(
            account_id,
            AccountState {
                max_connections: std::sync::atomic::AtomicU32::new(config.max_connections),
                active_connections: std::sync::atomic::AtomicU32::new(0),
            },
        );
    }
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
    // Update routing table without stopping active channels.
    // Remove channels no longer in the sync payload.
    let new_ids: std::collections::HashSet<&String> = req.channels.keys().collect();
    let old_ids: Vec<String> = state
        .channel_routes
        .iter()
        .map(|e| e.key().clone())
        .collect();
    for id in &old_ids {
        if !new_ids.contains(id) {
            state.channel_routes.remove(id);
            // Stop active stream for removed channel
            if let Some((_, active)) = state.active_channels.remove(id) {
                let _ = active.stop_tx.send(true);
                state.decrement_connections(active.account_id);
                tracing::info!("Sync: stopped removed channel {}", id);
            }
        }
    }

    // Insert/update all channels from payload
    for (id, config) in req.channels {
        state.channel_routes.insert(
            id,
            ChannelRouting {
                streams: config.streams,
            },
        );
    }

    // Update accounts, preserving active connection counts
    let new_account_ids: std::collections::HashSet<u64> = req
        .accounts
        .iter()
        .filter_map(|(id_str, _)| id_str.parse::<u64>().ok())
        .collect();

    // Remove accounts no longer in payload
    let old_account_ids: Vec<u64> =
        state.accounts.iter().map(|e| *e.key()).collect();
    for id in &old_account_ids {
        if !new_account_ids.contains(id) {
            state.accounts.remove(id);
        }
    }

    // Insert/update accounts, preserving active_connections for existing ones
    for (id_str, config) in req.accounts {
        if let Ok(id) = id_str.parse::<u64>() {
            if let Some(existing) = state.accounts.get(&id) {
                // Update max_connections but keep current active count
                existing
                    .max_connections
                    .store(config.max_connections, std::sync::atomic::Ordering::Relaxed);
            } else {
                state.accounts.insert(
                    id,
                    AccountState {
                        max_connections: std::sync::atomic::AtomicU32::new(
                            config.max_connections,
                        ),
                        active_connections: std::sync::atomic::AtomicU32::new(0),
                    },
                );
            }
        }
    }

    state.clear_all_cooldowns();

    let channels = state.channel_routes.len();
    let accounts = state.accounts.len();
    tracing::info!("Sync complete: {} channels, {} accounts", channels, accounts);
    StatusCode::OK
}

pub async fn clear_channel_cooldowns(
    State(state): State<Arc<AppState>>,
    Path(channel_id): Path<String>,
) -> StatusCode {
    state.clear_cooldowns(&channel_id);
    tracing::info!("Channel {}: cooldowns cleared", channel_id);
    StatusCode::OK
}

pub async fn clear_all_cooldowns(
    State(state): State<Arc<AppState>>,
) -> StatusCode {
    state.clear_all_cooldowns();
    tracing::info!("All cooldowns cleared");
    StatusCode::OK
}

pub async fn stop_client(
    State(state): State<Arc<AppState>>,
    Path(client_id): Path<String>,
) -> StatusCode {
    for entry in state.active_channels.iter() {
        if let Some(client) = entry.clients.get(&client_id) {
            client.cancel.notify_one();
            tracing::info!(
                "Client {} on channel {} stopped via control API",
                client_id,
                entry.key()
            );
            return StatusCode::OK;
        }
    }
    tracing::warn!("Client {} not found", client_id);
    StatusCode::NOT_FOUND
}

pub async fn post_hdhr(
    State(state): State<Arc<AppState>>,
    Json(req): Json<HdhrConfigRequest>,
) -> StatusCode {
    tracing::info!(
        device_id = %req.device_id,
        friendly_name = %req.friendly_name,
        base_url = %req.base_url,
        "received HDHR config"
    );
    let _ = state.hdhr_tx.send(Some(HdhrConfig {
        device_id: req.device_id,
        friendly_name: req.friendly_name,
        base_url: req.base_url,
    }));
    StatusCode::OK
}
