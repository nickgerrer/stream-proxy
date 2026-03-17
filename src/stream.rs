use crate::events::{self, ProxyEvent};
use crate::state::{AppState, ClientState};
use crate::upstream;
use axum::{
    body::Body,
    extract::{ConnectInfo, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use serde::Deserialize;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::sync::{broadcast, mpsc};
use tokio::time::Instant;

const MAX_FFMPEG_RESTARTS: u32 = 3;

#[derive(Debug, Deserialize)]
pub struct StreamParams {
    #[serde(default)]
    pub transcode: Option<bool>,
}

/// TS null packet (188 bytes) used as keepalive
fn ts_null_packet() -> Bytes {
    let mut pkt = vec![0u8; 188];
    pkt[0] = 0x47; // Sync byte
    pkt[1] = 0x1F; // PID 0x1FFF (null packet)
    pkt[2] = 0xFF;
    pkt[3] = 0x10; // Adaptation field control: payload only
    Bytes::from(pkt)
}

/// Guard that cleans up client state when dropped (i.e. when client disconnects)
struct ClientGuard {
    channel_id: String,
    client_id: String,
    active: Arc<crate::state::ActiveChannel>,
    state: Arc<AppState>,
    bytes_sent: Arc<AtomicU64>,
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        let bytes_sent = self.bytes_sent.load(Ordering::Relaxed);

        // Capture session duration from ClientState before removing from the map
        let duration_secs = self
            .active
            .clients
            .get(&self.client_id)
            .map(|client| client.connected_since.elapsed().as_secs())
            .unwrap_or(0);

        // Record session completion into ChannelMetrics
        self.active
            .metrics
            .record_session_end(&self.client_id, bytes_sent, duration_secs);

        // Fire ConnectionClosed event
        if let Some(collector) = &self.state.events {
            collector.push(ProxyEvent::ConnectionClosed {
                channel_id: self.channel_id.clone(),
                client_id: self.client_id.clone(),
                duration_secs,
                bytes_sent,
                timestamp: events::now_rfc3339(),
            });
        }

        self.active.clients.remove(&self.client_id);
        tracing::info!(
            "Channel {}: client {} disconnected (sent {} bytes, duration {}s)",
            self.channel_id,
            self.client_id,
            bytes_sent,
            duration_secs
        );

        // If last client, stop the channel immediately
        if self.active.clients.is_empty() {
            tracing::info!("Channel {}: no clients remaining, stopping", self.channel_id);
            let _ = self.active.stop_tx.send(true);
        }
    }
}

fn build_raw_response(
    mut rx: broadcast::Receiver<Bytes>,
    active: Arc<crate::state::ActiveChannel>,
    client_id: String,
    client_bytes: Arc<AtomicU64>,
    guard: ClientGuard,
    state: Arc<AppState>,
    channel_id: String,
) -> Response {
    let (tx, body_rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(32);

    tokio::spawn(async move {
        let _guard = guard;
        let keepalive = ts_null_packet();
        let mut keepalive_interval = tokio::time::interval(std::time::Duration::from_millis(500));

        loop {
            tokio::select! {
                result = rx.recv() => {
                    match result {
                        Ok(chunk) => {
                            let len = chunk.len() as u64;
                            client_bytes.fetch_add(len, Ordering::Relaxed);
                            if let Some(client) = active.clients.get(&client_id) {
                                client.bytes_sent.fetch_add(len, Ordering::Relaxed);
                            }
                            active.metrics.record_data_sent();
                            if tx.send(Ok(chunk)).await.is_err() {
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!("Client {} lagged {} messages", client_id, n);
                            active.metrics.record_client_lag(&client_id, n);

                            if let Some(collector) = &state.events {
                                collector.push(ProxyEvent::ClientLagged {
                                    channel_id: channel_id.clone(),
                                    client_id: client_id.clone(),
                                    messages_skipped: n,
                                    timestamp: events::now_rfc3339(),
                                });
                            }
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            tracing::info!("Broadcast closed for client {}", client_id);
                            break;
                        }
                    }
                }
                _ = keepalive_interval.tick() => {
                    active.metrics.record_keepalive_sent();
                    if tx.send(Ok(keepalive.clone())).await.is_err() {
                        break;
                    }
                }
                _ = tx.closed() => {
                    tracing::info!("Client {} disconnected (body dropped)", client_id);
                    break;
                }
            }
        }
    });

    let body_stream = futures_util::stream::unfold(body_rx, |mut rx| async {
        rx.recv().await.map(|item| (item, rx))
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "video/mp2t")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(body_stream))
        .unwrap()
}

/// Raw passthrough loop shared by transcoded response fallback paths.
async fn raw_passthrough(
    tx: &mpsc::Sender<Result<Bytes, std::io::Error>>,
    active: &Arc<crate::state::ActiveChannel>,
    client_id: &str,
    client_bytes: &Arc<AtomicU64>,
    state: &Arc<AppState>,
    channel_id: &str,
    keepalive: &Bytes,
) {
    let mut raw_rx = active.sender.subscribe();
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
    loop {
        tokio::select! {
            result = raw_rx.recv() => {
                match result {
                    Ok(chunk) => {
                        let len = chunk.len() as u64;
                        client_bytes.fetch_add(len, Ordering::Relaxed);
                        if let Some(client) = active.clients.get(client_id) {
                            client.bytes_sent.fetch_add(len, Ordering::Relaxed);
                        }
                        active.metrics.record_data_sent();
                        if tx.send(Ok(chunk)).await.is_err() {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("Client {} lagged {} messages", client_id, n);
                        active.metrics.record_client_lag(client_id, n);
                        if let Some(collector) = &state.events {
                            collector.push(ProxyEvent::ClientLagged {
                                channel_id: channel_id.to_string(),
                                client_id: client_id.to_string(),
                                messages_skipped: n,
                                timestamp: events::now_rfc3339(),
                            });
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
            _ = interval.tick() => {
                active.metrics.record_keepalive_sent();
                if tx.send(Ok(keepalive.clone())).await.is_err() {
                    return;
                }
            }
            _ = tx.closed() => {
                return;
            }
        }
    }
}

fn build_transcoded_response(
    active: Arc<crate::state::ActiveChannel>,
    client_id: String,
    client_bytes: Arc<AtomicU64>,
    guard: ClientGuard,
    state: Arc<AppState>,
    channel_id: String,
) -> Response {
    let (tx, body_rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(32);

    tokio::spawn(async move {
        let _guard = guard;
        let mut restarts = 0u32;
        let keepalive = ts_null_packet();

        'outer: loop {
            let new_rx = active.sender.subscribe();
            match crate::transcode::spawn_ffmpeg(new_rx, &client_id) {
                Ok((mut child, writer)) => {
                    let mut stdout = child.stdout.take().expect("stdout was piped");
                    let mut buf = vec![0u8; 65536];
                    let mut keepalive_interval =
                        tokio::time::interval(std::time::Duration::from_secs(2));

                    let mut should_restart = false;

                    loop {
                        tokio::select! {
                            result = AsyncReadExt::read(&mut stdout, &mut buf) => {
                                match result {
                                    Ok(0) => {
                                        // FFmpeg closed stdout — process exited
                                        writer.abort();
                                        restarts += 1;
                                        if restarts <= MAX_FFMPEG_RESTARTS {
                                            tracing::warn!(
                                                "Client {}: FFmpeg exited, restarting ({}/{})",
                                                client_id, restarts, MAX_FFMPEG_RESTARTS
                                            );
                                            should_restart = true;
                                            tokio::time::sleep(
                                                std::time::Duration::from_millis(100),
                                            ).await;
                                            break; // break inner loop
                                        }
                                        tracing::error!(
                                            "Client {}: FFmpeg restart limit reached, falling back to raw",
                                            client_id
                                        );
                                        raw_passthrough(&tx, &active, &client_id, &client_bytes, &state, &channel_id, &keepalive).await;
                                        break 'outer;
                                    }
                                    Ok(n) => {
                                        let data = Bytes::copy_from_slice(&buf[..n]);
                                        let len = data.len() as u64;
                                        client_bytes.fetch_add(len, Ordering::Relaxed);
                                        if let Some(client) = active.clients.get(&client_id) {
                                            client.bytes_sent.fetch_add(len, Ordering::Relaxed);
                                        }
                                        active.metrics.record_data_sent();
                                        if tx.send(Ok(data)).await.is_err() {
                                            break 'outer;
                                        }
                                    }
                                    Err(e) => {
                                        tracing::error!(
                                            "Client {}: FFmpeg stdout read error: {}",
                                            client_id, e
                                        );
                                        break; // break inner loop (outer will also exit)
                                    }
                                }
                            }
                            _ = keepalive_interval.tick() => {
                                active.metrics.record_keepalive_sent();
                                if tx.send(Ok(keepalive.clone())).await.is_err() {
                                    break 'outer;
                                }
                            }
                            _ = tx.closed() => {
                                tracing::info!("Client {} disconnected (body dropped)", client_id);
                                break 'outer;
                            }
                        }
                    }

                    // After inner loop: check if we should restart or exit
                    if should_restart {
                        continue; // continue outer loop
                    }
                    break; // exit outer loop
                }
                Err(e) => {
                    tracing::error!(
                        "Client {}: FFmpeg spawn failed: {}, falling back to raw",
                        client_id, e
                    );
                    raw_passthrough(&tx, &active, &client_id, &client_bytes, &state, &channel_id, &keepalive).await;
                    break; // exit outer loop
                }
            }
        }
    });

    let body_stream = futures_util::stream::unfold(body_rx, |mut rx| async {
        rx.recv().await.map(|item| (item, rx))
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "video/mp2t")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(body_stream))
        .unwrap()
}

pub async fn stream_channel(
    State(state): State<Arc<AppState>>,
    Path(channel_id): Path<String>,
    Query(params): Query<StreamParams>,
    headers: HeaderMap,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    // Get or start the channel (with startup lock to prevent races)
    let active = if let Some(existing) = state.active_channels.get(&channel_id) {
        existing.value().clone()
    } else {
        // Get or create a per-channel startup mutex
        let startup_lock = state
            .starting_channels
            .entry(channel_id.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();

        let _guard = startup_lock.lock().await;

        // Re-check after acquiring lock — another request may have started it
        if let Some(existing) = state.active_channels.get(&channel_id) {
            existing.value().clone()
        } else {
            let (stream_id, account_id, url) = match state.select_stream(&channel_id) {
                Some(s) => s,
                None => {
                    return (StatusCode::SERVICE_UNAVAILABLE, "No streams available")
                        .into_response();
                }
            };

            let active = upstream::start_channel(
                state.clone(),
                channel_id.clone(),
                stream_id,
                account_id,
                url,
            );

            // Clean up the startup lock entry
            state.starting_channels.remove(&channel_id);

            active
        }
    };

    // Register client
    let client_id = uuid::Uuid::new_v4().to_string();
    let client_bytes = Arc::new(AtomicU64::new(0));
    let transcode = params.transcode.unwrap_or(false);
    active.clients.insert(
        client_id.clone(),
        ClientState {
            id: client_id.clone(),
            connected_since: Instant::now(),
            bytes_sent: AtomicU64::new(0),
            remote_addr: addr.to_string(),
            user_agent: user_agent.clone(),
            transcoding: transcode,
        },
    );

    // Register viewer for metrics tracking
    let client_count = active.clients.len() as u32;
    active.metrics.register_viewer(&addr.to_string(), client_count);

    tracing::info!(
        "Channel {}: client {} connected from {}{}",
        channel_id,
        client_id,
        addr,
        if transcode { " (transcoding)" } else { "" }
    );

    // Fire ConnectionOpened event
    if let Some(collector) = &state.events {
        collector.push(ProxyEvent::ConnectionOpened {
            channel_id: channel_id.clone(),
            client_id: client_id.clone(),
            remote_addr: addr.to_string(),
            user_agent: user_agent.clone(),
            stream_id: active.stream_id,
            account_id: active.account_id,
            transcoding: transcode,
            timestamp: events::now_rfc3339(),
        });
    }

    // Create drop guard for cleanup on client disconnect
    let guard = ClientGuard {
        channel_id: channel_id.clone(),
        client_id: client_id.clone(),
        active: active.clone(),
        state: state.clone(),
        bytes_sent: client_bytes.clone(),
    };

    if transcode {
        build_transcoded_response(active, client_id, client_bytes, guard, state, channel_id)
    } else {
        let rx = active.sender.subscribe();
        build_raw_response(rx, active, client_id, client_bytes, guard, state, channel_id)
    }
}
