use crate::events::StreamInfoPayload;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// --- HDHR models ---

// Control API input — received from Laravel
#[derive(Debug, Deserialize)]
pub struct HdhrConfigRequest {
    pub device_id: String,
    pub friendly_name: String,
    pub base_url: String,
}

// Internal config used by SSDP task
#[derive(Debug, Clone)]
pub struct HdhrConfig {
    pub device_id: String,
    pub friendly_name: String,
    pub base_url: String,
}

// --- Control API models ---

#[derive(Debug, Deserialize)]
pub struct StreamUrl {
    pub account_id: u64,
    pub url: String,
}

#[derive(Debug, Deserialize)]
pub struct StreamConfig {
    pub id: u64,
    pub urls: Vec<StreamUrl>,
}

#[derive(Debug, Deserialize)]
pub struct ChannelConfig {
    pub streams: Vec<StreamConfig>,
}

#[derive(Debug, Deserialize)]
pub struct AccountConfig {
    pub max_connections: u32,
}

#[derive(Debug, Deserialize)]
pub struct SyncRequest {
    pub channels: HashMap<String, ChannelConfig>,
    pub accounts: HashMap<String, AccountConfig>,
}

// --- Status API models ---

#[derive(Debug, Serialize, Clone)]
pub struct UpstreamStatus {
    pub stream_id: u64,
    pub account_id: u64,
    pub url: String,
    pub connected_since: String,
    pub bytes_transferred: u64,
}

#[derive(Debug, Serialize, Clone)]
pub struct ChannelStatus {
    pub state: String,
    pub clients: u32,
    pub upstream: Option<UpstreamStatus>,
}

#[derive(Debug, Serialize, Clone)]
pub struct ClientInfo {
    pub id: String,
    pub connected_since: String,
    pub bytes_sent: u64,
    pub remote_addr: String,
    pub transcoding: bool,
}

#[derive(Debug, Serialize)]
pub struct AccountStatus {
    pub active_connections: u32,
    pub max_connections: u32,
}

#[derive(Debug, Serialize)]
pub struct ChannelsResponse {
    pub channels: HashMap<String, ChannelStatus>,
    pub accounts: HashMap<String, AccountStatus>,
}

#[derive(Debug, Serialize, Clone)]
pub struct CooldownInfo {
    pub stream_id: u64,
    pub account_id: u64,
    pub failed_at: String,
    pub expires_at: String,
    pub reason: String,
}

#[derive(Debug, Serialize)]
pub struct ChannelDetailResponse {
    #[serde(flatten)]
    pub status: ChannelStatus,
    pub clients: Vec<ClientInfo>,
    pub cooldowns: Vec<CooldownInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<ChannelMetricsInfo>,
}

/// Stream-level metrics exposed in the channel detail endpoint.
#[derive(Debug, Serialize, Clone)]
pub struct ChannelMetricsInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_info: Option<StreamInfoPayload>,
    pub upstream_bitrate_kbps: u64,
    pub continuity_errors: u64,
    pub pts_discontinuities: u64,
    pub pcr_jitter_us: u64,
    pub keyframe_interval_ms: u64,
    pub keepalive_ratio: f64,
    pub peak_viewers: u32,
    pub total_unique_viewers: u32,
    pub upstream_reconnects: u32,
    pub time_to_first_byte_ms: u64,
}

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub uptime_seconds: u64,
    pub active_channels: usize,
    pub total_clients: u32,
}
