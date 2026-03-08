use crate::models::*;
use dashmap::DashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::time::Instant;

/// How long a failed stream is skipped before retrying (default 30 min)
pub fn cooldown_duration() -> Duration {
    Duration::from_secs(1800)
}

/// A stream+account pair that failed and is cooling down
pub struct StreamCooldown {
    pub stream_id: u64,
    pub account_id: u64,
    pub failed_at: Instant,
    pub reason: String,
}

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
    pub stream_cooldowns: DashMap<String, Vec<StreamCooldown>>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            start_time: Instant::now(),
            channel_routes: DashMap::new(),
            active_channels: DashMap::new(),
            accounts: DashMap::new(),
            stream_cooldowns: DashMap::new(),
        }
    }

    /// Record that a stream failed for a channel.
    pub fn record_failure(&self, channel_id: &str, stream_id: u64, account_id: u64, reason: String) {
        let mut entry = self.stream_cooldowns.entry(channel_id.to_string()).or_default();
        entry.retain(|c| !(c.stream_id == stream_id && c.account_id == account_id));
        entry.push(StreamCooldown {
            stream_id,
            account_id,
            failed_at: Instant::now(),
            reason,
        });
    }

    /// Check if a stream+account is currently cooled down for a channel.
    fn is_cooled_down(&self, channel_id: &str, stream_id: u64, account_id: u64) -> bool {
        if let Some(cooldowns) = self.stream_cooldowns.get(channel_id) {
            let ttl = cooldown_duration();
            cooldowns.iter().any(|c| {
                c.stream_id == stream_id
                    && c.account_id == account_id
                    && c.failed_at.elapsed() < ttl
            })
        } else {
            false
        }
    }

    /// Remove expired cooldowns for all channels.
    pub fn cleanup_expired_cooldowns(&self) {
        let ttl = cooldown_duration();
        let mut empty_channels = Vec::new();
        for mut entry in self.stream_cooldowns.iter_mut() {
            entry.value_mut().retain(|c| c.failed_at.elapsed() < ttl);
            if entry.value().is_empty() {
                empty_channels.push(entry.key().clone());
            }
        }
        for ch in empty_channels {
            self.stream_cooldowns.remove(&ch);
        }
    }

    /// Clear all cooldowns for a specific channel.
    pub fn clear_cooldowns(&self, channel_id: &str) {
        self.stream_cooldowns.remove(channel_id);
    }

    /// Clear all cooldowns globally.
    pub fn clear_all_cooldowns(&self) {
        self.stream_cooldowns.clear();
    }

    /// Find first available stream+account for a channel, respecting limits and cooldowns.
    pub fn select_stream(&self, channel_id: &str) -> Option<(u64, u64, String)> {
        let routing = self.channel_routes.get(channel_id)?;
        let mut all_cooled_down = true;

        // First pass: skip cooled-down streams
        for stream in &routing.streams {
            for url_entry in &stream.urls {
                if self.is_cooled_down(channel_id, stream.id, url_entry.account_id) {
                    continue;
                }
                all_cooled_down = false;

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

        // If ALL streams are cooled down, ignore cooldowns (better than nothing)
        if all_cooled_down {
            tracing::warn!(
                "Channel {}: all streams cooled down, ignoring cooldowns",
                channel_id
            );
            for stream in &routing.streams {
                for url_entry in &stream.urls {
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
