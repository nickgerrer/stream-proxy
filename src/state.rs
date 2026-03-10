use crate::config::Config;
use crate::models::*;
use dashmap::DashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::sync::watch;
use tokio::sync::Mutex;
use tokio::time::Instant;

/// Classification of upstream failure for tiered cooldowns
#[derive(Debug, Clone, PartialEq)]
pub enum FailureKind {
    /// Transient failures (timeouts, stream ended) — short cooldown
    Transient,
    /// Hard failures (HTTP 403/401/451/410) — long cooldown
    Hard,
}

/// Classify a failure error string into a FailureKind
pub fn classify_failure(error: &str) -> FailureKind {
    if error.contains("HTTP 403")
        || error.contains("HTTP 401")
        || error.contains("HTTP 451")
        || error.contains("HTTP 410")
    {
        FailureKind::Hard
    } else {
        FailureKind::Transient
    }
}

/// A stream+account pair that failed and is cooling down
pub struct StreamCooldown {
    pub stream_id: u64,
    pub account_id: u64,
    pub failed_at: Instant,
    pub reason: String,
    pub kind: FailureKind,
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
    pub transcoding: bool,
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
    pub config: Config,
    pub channel_routes: DashMap<String, ChannelRouting>,
    pub active_channels: DashMap<String, Arc<ActiveChannel>>,
    pub accounts: DashMap<u64, AccountState>,
    pub stream_cooldowns: DashMap<String, Vec<StreamCooldown>>,
    /// Per-channel mutex to prevent duplicate startup
    pub starting_channels: DashMap<String, Arc<Mutex<()>>>,
    pub hdhr_tx: watch::Sender<Option<HdhrConfig>>,
    pub hdhr_rx: watch::Receiver<Option<HdhrConfig>>,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        let (hdhr_tx, hdhr_rx) = watch::channel(None);
        Self {
            start_time: Instant::now(),
            config,
            channel_routes: DashMap::new(),
            active_channels: DashMap::new(),
            accounts: DashMap::new(),
            stream_cooldowns: DashMap::new(),
            starting_channels: DashMap::new(),
            hdhr_tx,
            hdhr_rx,
        }
    }

    /// Record that a stream failed for a channel.
    pub fn record_failure(&self, channel_id: &str, stream_id: u64, account_id: u64, reason: String, kind: FailureKind) {
        let mut entry = self.stream_cooldowns.entry(channel_id.to_string()).or_default();
        entry.retain(|c| !(c.stream_id == stream_id && c.account_id == account_id));
        entry.push(StreamCooldown {
            stream_id,
            account_id,
            failed_at: Instant::now(),
            reason,
            kind,
        });
    }

    /// Check if a stream+account is currently cooled down for a channel.
    fn is_cooled_down(&self, channel_id: &str, stream_id: u64, account_id: u64) -> bool {
        if let Some(cooldowns) = self.stream_cooldowns.get(channel_id) {
            cooldowns.iter().any(|c| {
                let ttl = match c.kind {
                    FailureKind::Transient => self.config.transient_cooldown,
                    FailureKind::Hard => self.config.hard_cooldown,
                };
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
        let mut empty_channels = Vec::new();
        for mut entry in self.stream_cooldowns.iter_mut() {
            entry.value_mut().retain(|c| {
                let ttl = match c.kind {
                    FailureKind::Transient => self.config.transient_cooldown,
                    FailureKind::Hard => self.config.hard_cooldown,
                };
                c.failed_at.elapsed() < ttl
            });
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
    /// Priority: (1) other streams on SAME account (free), (2) streams on different accounts (costs a slot).
    pub fn select_next_stream(
        &self,
        channel_id: &str,
        failed_stream_id: u64,
        failed_account_id: u64,
    ) -> Option<(u64, u64, String)> {
        let routing = self.channel_routes.get(channel_id)?;

        // Phase 1: Try other streams on the SAME account (free — no extra connection slot)
        for stream in routing.streams.iter() {
            if stream.id == failed_stream_id {
                continue; // Skip the stream that just failed
            }
            for url_entry in &stream.urls {
                if url_entry.account_id != failed_account_id {
                    continue; // Only same account in this phase
                }
                if self.is_cooled_down(channel_id, stream.id, url_entry.account_id) {
                    continue;
                }
                // Same account — no need to check connection limits (already counted)
                return Some((stream.id, url_entry.account_id, url_entry.url.clone()));
            }
        }

        // Phase 2: Try streams on DIFFERENT accounts (costs a connection slot)
        for stream in routing.streams.iter() {
            for url_entry in &stream.urls {
                if url_entry.account_id == failed_account_id {
                    continue; // Skip same account — exhausted in phase 1
                }
                if self.is_cooled_down(channel_id, stream.id, url_entry.account_id) {
                    continue;
                }
                if let Some(account) = self.accounts.get(&url_entry.account_id) {
                    let current = account.active_connections.load(Ordering::Relaxed);
                    let max = account.max_connections.load(Ordering::Relaxed);
                    if max > 0 && current >= max {
                        continue; // Account at capacity
                    }
                }
                return Some((stream.id, url_entry.account_id, url_entry.url.clone()));
            }
        }

        // Phase 3: All options cooled down — ignore cooldowns (better than killing the channel)
        tracing::warn!(
            "Channel {}: all failover options cooled down, ignoring cooldowns",
            channel_id
        );
        // Prefer same account first, then cross-account
        for stream in routing.streams.iter() {
            if stream.id == failed_stream_id {
                continue;
            }
            for url_entry in &stream.urls {
                if url_entry.account_id != failed_account_id {
                    continue;
                }
                return Some((stream.id, url_entry.account_id, url_entry.url.clone()));
            }
        }
        for stream in routing.streams.iter() {
            for url_entry in &stream.urls {
                if url_entry.account_id == failed_account_id {
                    continue;
                }
                if let Some(account) = self.accounts.get(&url_entry.account_id) {
                    let current = account.active_connections.load(Ordering::Relaxed);
                    let max = account.max_connections.load(Ordering::Relaxed);
                    if max > 0 && current >= max {
                        continue;
                    }
                }
                return Some((stream.id, url_entry.account_id, url_entry.url.clone()));
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
