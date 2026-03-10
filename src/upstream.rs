use crate::state::{ActiveChannel, AppState};
use bytes::Bytes;
use reqwest::Client;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::sync::{broadcast, watch};
use tokio::time::Instant;

const BROADCAST_CAPACITY: usize = 64;
const CHUNK_SIZE: usize = 188 * 1024; // ~188 KB (aligned to TS packet size)

/// Start streaming a channel. Spawns a background task that:
/// - Opens upstream HTTP connection
/// - Reads chunks and broadcasts them
/// - On failure, tries next stream (failover)
/// - Stops when stop signal received or all streams exhausted
pub fn start_channel(
    state: Arc<AppState>,
    channel_id: String,
    stream_id: u64,
    account_id: u64,
    url: String,
) -> Arc<ActiveChannel> {
    let (tx, _) = broadcast::channel::<Bytes>(BROADCAST_CAPACITY);
    let (stop_tx, stop_rx) = watch::channel(false);

    let active = Arc::new(ActiveChannel {
        stream_id,
        account_id,
        url: url.clone(),
        connected_since: Instant::now(),
        bytes_transferred: std::sync::atomic::AtomicU64::new(0),
        sender: tx.clone(),
        clients: dashmap::DashMap::new(),
        stop_tx,
    });

    state.increment_connections(account_id);
    state
        .active_channels
        .insert(channel_id.clone(), active.clone());

    // Spawn the upstream reader task
    let state_clone = state.clone();
    let active_clone = active.clone();
    tokio::spawn(async move {
        upstream_loop(
            state_clone,
            channel_id,
            stream_id,
            account_id,
            url,
            tx,
            stop_rx,
            active_clone,
        )
        .await;
    });

    active
}

async fn upstream_loop(
    state: Arc<AppState>,
    channel_id: String,
    mut stream_id: u64,
    mut account_id: u64,
    mut url: String,
    tx: broadcast::Sender<Bytes>,
    mut stop_rx: watch::Receiver<bool>,
    active: Arc<ActiveChannel>,
) {
    let client = Client::new();
    let mut failover_count: u32 = 0;
    let mut same_url_retries: u32 = 0;

    loop {
        tracing::info!(
            "Channel {}: connecting to upstream {} (stream={}, account={})",
            channel_id,
            url,
            stream_id,
            account_id
        );

        let result = fetch_upstream(&client, &url, &tx, &mut stop_rx, &active).await;

        // Check if we were told to stop
        if *stop_rx.borrow() {
            tracing::info!("Channel {}: stop signal received", channel_id);
            break;
        }

        if let Err(e) = result {
            tracing::warn!("Channel {}: upstream error: {}", channel_id, e);
            let kind = crate::state::classify_failure(&e);

            // For transient errors (not "stream ended"), retry same URL before failover.
            // "stream ended" means the server closed cleanly — retrying won't help.
            let is_retryable = kind == crate::state::FailureKind::Transient
                && !e.contains("stream ended");

            if is_retryable && same_url_retries < state.config.max_same_url_retries {
                same_url_retries += 1;
                tracing::info!(
                    "Channel {}: transient error, retrying same URL ({}/{})",
                    channel_id,
                    same_url_retries,
                    state.config.max_same_url_retries
                );
                tokio::select! {
                    _ = tokio::time::sleep(state.config.retry_delay) => {
                        if *stop_rx.borrow() {
                            break;
                        }
                    }
                    _ = stop_rx.changed() => {
                        tracing::info!("Channel {}: stop signal during retry delay", channel_id);
                        break;
                    }
                }
                continue;
            }

            // Retries exhausted, non-retryable, or hard error — record failure and failover
            state.record_failure(&channel_id, stream_id, account_id, e.clone(), kind);
            same_url_retries = 0;
            failover_count += 1;

            if failover_count >= state.config.max_failovers {
                tracing::error!("Channel {}: max failovers reached", channel_id);
                break;
            }

            if let Some((next_sid, next_aid, next_url)) =
                state.select_next_stream(&channel_id, stream_id, account_id)
            {
                tracing::info!(
                    "Channel {}: failing over to stream={}, account={}",
                    channel_id,
                    next_sid,
                    next_aid
                );
                if next_aid != account_id {
                    // Cross-account failover: release old slot, acquire new one
                    state.decrement_connections(account_id);
                    state.increment_connections(next_aid);
                }
                stream_id = next_sid;
                account_id = next_aid;
                url = next_url;
            } else {
                tracing::error!("Channel {}: no more streams available", channel_id);
                break;
            }
        }
    }

    // Cleanup
    state.decrement_connections(account_id);
    state.active_channels.remove(&channel_id);
    tracing::info!("Channel {}: upstream task exited", channel_id);
}

async fn fetch_upstream(
    client: &Client,
    url: &str,
    tx: &broadcast::Sender<Bytes>,
    stop_rx: &mut watch::Receiver<bool>,
    active: &ActiveChannel,
) -> Result<(), String> {
    use futures_util::StreamExt;

    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("connect error: {}", e))?;

    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }

    let mut byte_stream = response.bytes_stream();
    let mut buffer = Vec::with_capacity(CHUNK_SIZE);

    loop {
        tokio::select! {
            _ = stop_rx.changed() => {
                return Ok(());
            }
            chunk = byte_stream.next() => {
                match chunk {
                    Some(Ok(data)) => {
                        buffer.extend_from_slice(&data);

                        // Flush when buffer is large enough
                        while buffer.len() >= CHUNK_SIZE {
                            let chunk = Bytes::copy_from_slice(&buffer[..CHUNK_SIZE]);
                            buffer.drain(..CHUNK_SIZE);
                            active.bytes_transferred.fetch_add(CHUNK_SIZE as u64, Ordering::Relaxed);

                            // Send to all clients; if no receivers, that's fine
                            let _ = tx.send(chunk);
                        }
                    }
                    Some(Err(e)) => {
                        return Err(format!("read error: {}", e));
                    }
                    None => {
                        // Stream ended — flush remaining buffer
                        if !buffer.is_empty() {
                            let chunk = Bytes::from(buffer);
                            active.bytes_transferred.fetch_add(chunk.len() as u64, Ordering::Relaxed);
                            let _ = tx.send(chunk);
                        }
                        return Err("stream ended".to_string());
                    }
                }
            }
        }
    }
}
