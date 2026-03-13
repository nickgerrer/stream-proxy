use crate::events::StreamInfoPayload;
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
                max_connections: entry.value().max_connections.load(Ordering::Relaxed),
            },
        );
    }

    Json(ChannelsResponse { channels, accounts })
}

pub async fn channel_detail(
    State(state): State<Arc<AppState>>,
    Path(channel_id): Path<String>,
) -> Result<Json<ChannelDetailResponse>, StatusCode> {
    let cooldowns: Vec<CooldownInfo> = state
        .stream_cooldowns
        .get(&channel_id)
        .map(|entry| {
            entry
                .iter()
                .filter(|c| {
                    let ttl = match c.kind {
                        crate::state::FailureKind::Transient => state.config.transient_cooldown,
                        crate::state::FailureKind::Hard => state.config.hard_cooldown,
                    };
                    c.failed_at.elapsed() < ttl
                })
                .map(|c| {
                    let ttl = match c.kind {
                        crate::state::FailureKind::Transient => state.config.transient_cooldown,
                        crate::state::FailureKind::Hard => state.config.hard_cooldown,
                    };
                    let failed_elapsed = c.failed_at.elapsed();
                    let failed_time = std::time::SystemTime::now() - failed_elapsed;
                    let expires_time = failed_time + ttl;
                    CooldownInfo {
                        stream_id: c.stream_id,
                        account_id: c.account_id,
                        failed_at: format_system_time(failed_time),
                        expires_at: format_system_time(expires_time),
                        reason: c.reason.clone(),
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    if let Some(active) = state.active_channels.get(&channel_id) {
        let clients: Vec<ClientInfo> = active
            .clients
            .iter()
            .map(|c| ClientInfo {
                id: c.id.clone(),
                connected_since: format_instant(c.connected_since),
                bytes_sent: c.bytes_sent.load(Ordering::Relaxed),
                remote_addr: c.remote_addr.clone(),
                transcoding: c.transcoding,
            })
            .collect();

        let metrics = build_metrics_info(&active.metrics);

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
            cooldowns,
            metrics: Some(metrics),
        }))
    } else if state.channel_routes.contains_key(&channel_id) {
        Ok(Json(ChannelDetailResponse {
            status: ChannelStatus {
                state: "idle".to_string(),
                clients: 0,
                upstream: None,
            },
            clients: vec![],
            cooldowns,
            metrics: None,
        }))
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

fn build_metrics_info(metrics: &crate::metrics::ChannelMetrics) -> ChannelMetricsInfo {
    let stream_info = metrics
        .stream_info
        .lock()
        .unwrap()
        .as_ref()
        .map(|info| StreamInfoPayload::from_stream_info(info));

    let bitrate_bps = metrics.current_bitrate_bps(10);

    ChannelMetricsInfo {
        stream_info,
        upstream_bitrate_kbps: bitrate_bps / 1000,
        continuity_errors: metrics.continuity_errors.load(Ordering::Relaxed),
        pts_discontinuities: metrics.pts_discontinuities.load(Ordering::Relaxed),
        pcr_jitter_us: metrics.pcr_jitter_us.load(Ordering::Relaxed),
        keyframe_interval_ms: metrics.keyframe_interval_ms.load(Ordering::Relaxed),
        keepalive_ratio: metrics.keepalive_ratio(),
        peak_viewers: metrics.peak_concurrent_viewers.load(Ordering::Relaxed),
        total_unique_viewers: metrics.total_unique_viewers.load(Ordering::Relaxed),
        upstream_reconnects: metrics.upstream_reconnects.load(Ordering::Relaxed),
        time_to_first_byte_ms: metrics.time_to_first_byte_ms.load(Ordering::Relaxed),
    }
}

fn format_system_time(t: std::time::SystemTime) -> String {
    let datetime: chrono::DateTime<chrono::Utc> = t.into();
    datetime.to_rfc3339()
}

fn format_instant(instant: tokio::time::Instant) -> String {
    let elapsed = instant.elapsed();
    let system_time = std::time::SystemTime::now() - elapsed;
    let datetime: chrono::DateTime<chrono::Utc> = system_time.into();
    datetime.to_rfc3339()
}
