use crate::models::*;
use dashmap::DashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::time::Instant;

/// Per-account connection tracking
pub struct AccountState {
    pub max_connections: AtomicU32,
    pub active_connections: AtomicU32,
}

/// Per-client state
pub struct ClientState {
    pub id: String,
    pub connected_since: Instant,
    pub bytes_sent: AtomicU64,
    pub remote_addr: String,
}

/// Routing config for a channel (from Django push)
pub struct ChannelRouting {
    pub streams: Vec<StreamConfig>,
}

/// Live state for an active channel (upstream running)
pub struct ActiveChannel {
    pub stream_id: u64,
    pub account_id: u64,
    pub url: String,
    pub connected_since: Instant,
    pub bytes_transferred: AtomicU64,
    pub sender: broadcast::Sender<bytes::Bytes>,
    pub clients: DashMap<String, ClientState>,
    pub stop_tx: tokio::sync::watch::Sender<bool>,
}

/// Top-level application state shared across all handlers
pub struct AppState {
    pub start_time: Instant,
    pub channel_routes: DashMap<String, ChannelRouting>,
    pub active_channels: DashMap<String, Arc<ActiveChannel>>,
    pub accounts: DashMap<u64, AccountState>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            start_time: Instant::now(),
            channel_routes: DashMap::new(),
            active_channels: DashMap::new(),
            accounts: DashMap::new(),
        }
    }

    /// Find first available stream+account for a channel, respecting limits.
    pub fn select_stream(&self, channel_id: &str) -> Option<(u64, u64, String)> {
        let routing = self.channel_routes.get(channel_id)?;
        for stream in &routing.streams {
            for url_entry in &stream.urls {
                if let Some(account) = self.accounts.get(&url_entry.account_id) {
                    let current = account.active_connections.load(Ordering::Relaxed);
                    let max = account.max_connections.load(Ordering::Relaxed);
                    if max == 0 || current < max {
                        return Some((stream.id, url_entry.account_id, url_entry.url.clone()));
                    }
                } else {
                    // Account not registered — allow (no limit)
                    return Some((stream.id, url_entry.account_id, url_entry.url.clone()));
                }
            }
        }
        None
    }

    /// Try the next available stream after the current one fails.
    pub fn select_next_stream(
        &self,
        channel_id: &str,
        failed_stream_id: u64,
        failed_account_id: u64,
    ) -> Option<(u64, u64, String)> {
        let routing = self.channel_routes.get(channel_id)?;
        let mut past_failed = false;
        for stream in &routing.streams {
            for url_entry in &stream.urls {
                if stream.id == failed_stream_id && url_entry.account_id == failed_account_id {
                    past_failed = true;
                    continue;
                }
                if !past_failed {
                    continue;
                }
                if let Some(account) = self.accounts.get(&url_entry.account_id) {
                    let current = account.active_connections.load(Ordering::Relaxed);
                    let max = account.max_connections.load(Ordering::Relaxed);
                    if max == 0 || current < max {
                        return Some((stream.id, url_entry.account_id, url_entry.url.clone()));
                    }
                } else {
                    return Some((stream.id, url_entry.account_id, url_entry.url.clone()));
                }
            }
        }
        None
    }

    pub fn increment_connections(&self, account_id: u64) {
        if let Some(account) = self.accounts.get(&account_id) {
            account.active_connections.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn decrement_connections(&self, account_id: u64) {
        if let Some(account) = self.accounts.get(&account_id) {
            // Use fetch_update to prevent underflow (sync replaces accounts with fresh 0 counters
            // while upstream tasks still hold references and decrement on cleanup)
            let _ = account.active_connections.fetch_update(
                Ordering::Relaxed,
                Ordering::Relaxed,
                |current| {
                    if current > 0 {
                        Some(current - 1)
                    } else {
                        None // Don't update if already 0
                    }
                },
            );
        }
    }
}
