use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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

#[derive(Debug, Serialize)]
pub struct ChannelDetailResponse {
    #[serde(flatten)]
    pub status: ChannelStatus,
    pub clients: Vec<ClientInfo>,
}

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub uptime_seconds: u64,
    pub active_channels: usize,
    pub total_clients: u32,
}
