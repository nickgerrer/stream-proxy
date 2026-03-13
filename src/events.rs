use crate::state::AppState;
use crate::ts::{AudioCodec, VideoCodec};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use tokio::time::Duration;

/// Maximum buffered events before oldest are dropped.
const BUFFER_CAP: usize = 1000;

// ---------------------------------------------------------------------------
// Event types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProxyEvent {
    ConnectionOpened {
        channel_id: String,
        client_id: String,
        remote_addr: String,
        transcoding: bool,
        timestamp: String,
    },
    ConnectionClosed {
        channel_id: String,
        client_id: String,
        duration_secs: u64,
        bytes_sent: u64,
        timestamp: String,
    },
    StreamInfo {
        channel_id: String,
        stream_id: u64,
        info: StreamInfoPayload,
        timestamp: String,
    },
    StreamFailover {
        channel_id: String,
        from_stream_id: u64,
        from_account_id: u64,
        to_stream_id: u64,
        to_account_id: u64,
        reason: String,
        timestamp: String,
    },
    StreamError {
        channel_id: String,
        error_type: String,
        count: u64,
        timestamp: String,
    },
    ClientLagged {
        channel_id: String,
        client_id: String,
        messages_skipped: u64,
        timestamp: String,
    },
    MetricsSnapshot {
        channel_id: String,
        upstream_bitrate_kbps: u64,
        continuity_errors: u64,
        pts_discontinuities: u64,
        pcr_jitter_us: u64,
        keyframe_interval_ms: u64,
        keepalive_ratio: f32,
        viewer_count: u32,
        timestamp: String,
    },
}

/// Payload carrying detected stream information (codec, resolution, etc.).
#[derive(Debug, Clone, Serialize)]
pub struct StreamInfoPayload {
    pub video_codec: Option<String>,
    pub video_profile: Option<String>,
    pub video_level: Option<String>,
    pub resolution: Option<String>,
    pub fps: Option<f64>,
    pub chroma: Option<String>,
    pub bit_depth: Option<u8>,
    pub audio_codec: Option<String>,
    pub audio_profile: Option<String>,
    pub audio_sample_rate: Option<u32>,
    pub audio_channels: Option<u8>,
}

impl StreamInfoPayload {
    /// Build from the TS-layer `StreamInfo` struct.
    pub fn from_stream_info(info: &crate::ts::StreamInfo) -> Self {
        let (video_codec, video_profile, video_level, resolution, fps, chroma, bit_depth) =
            if let (Some(codec), Some(details)) = (&info.video_codec, &info.video_details) {
                let codec_name = match codec {
                    VideoCodec::H264 => "h264",
                    VideoCodec::Hevc => "hevc",
                    VideoCodec::Mpeg2 => "mpeg2",
                };
                let profile = format_video_profile(codec, details.profile);
                let level = format_video_level(codec, details.level);
                let res = format!("{}x{}", details.width, details.height);
                let fps_val = details
                    .fps_num
                    .zip(details.fps_den)
                    .map(|(n, d)| if d > 0 { n as f64 / d as f64 } else { 0.0 });
                let chroma_str = match details.chroma_format {
                    0 => "monochrome",
                    1 => "4:2:0",
                    2 => "4:2:2",
                    3 => "4:4:4",
                    _ => "unknown",
                };
                (
                    Some(codec_name.to_string()),
                    Some(profile),
                    Some(level),
                    Some(res),
                    fps_val,
                    Some(chroma_str.to_string()),
                    Some(details.bit_depth_luma),
                )
            } else {
                (None, None, None, None, None, None, None)
            };

        let (audio_codec, audio_profile, audio_sample_rate, audio_channels) =
            if let (Some(codec), Some(details)) = (&info.audio_codec, &info.audio_details) {
                let codec_name = match codec {
                    AudioCodec::Aac => "aac",
                    AudioCodec::Ac3 => "ac3",
                    AudioCodec::Eac3 => "eac3",
                    AudioCodec::Mp2 => "mp2",
                };
                let profile = format_audio_profile(codec, details.profile);
                (
                    Some(codec_name.to_string()),
                    Some(profile),
                    Some(details.sample_rate),
                    Some(details.channels),
                )
            } else {
                (None, None, None, None)
            };

        Self {
            video_codec,
            video_profile,
            video_level,
            resolution,
            fps,
            chroma,
            bit_depth,
            audio_codec,
            audio_profile,
            audio_sample_rate,
            audio_channels,
        }
    }
}

fn format_video_profile(codec: &VideoCodec, profile_idc: u8) -> String {
    match codec {
        VideoCodec::H264 => match profile_idc {
            66 => "Baseline".to_string(),
            77 => "Main".to_string(),
            88 => "Extended".to_string(),
            100 => "High".to_string(),
            110 => "High 10".to_string(),
            122 => "High 4:2:2".to_string(),
            244 => "High 4:4:4 Predictive".to_string(),
            _ => format!("{}", profile_idc),
        },
        VideoCodec::Hevc => match profile_idc {
            1 => "Main".to_string(),
            2 => "Main 10".to_string(),
            3 => "Main Still Picture".to_string(),
            _ => format!("{}", profile_idc),
        },
        VideoCodec::Mpeg2 => match profile_idc {
            5 => "Simple".to_string(),
            4 => "Main".to_string(),
            3 => "SNR Scalable".to_string(),
            2 => "Spatially Scalable".to_string(),
            1 => "High".to_string(),
            _ => format!("{}", profile_idc),
        },
    }
}

fn format_video_level(codec: &VideoCodec, level: u8) -> String {
    match codec {
        VideoCodec::H264 => {
            // H.264 level_idc: e.g. 41 → "4.1"
            format!("{}.{}", level / 10, level % 10)
        }
        VideoCodec::Hevc => {
            // HEVC level_idc: encoded as level * 30, e.g. 120 → level 4.0
            let l = level as f64 / 30.0;
            format!("{:.1}", l)
        }
        VideoCodec::Mpeg2 => format!("{}", level),
    }
}

fn format_audio_profile(codec: &AudioCodec, profile: u8) -> String {
    match codec {
        AudioCodec::Aac => match profile {
            1 => "Main".to_string(),
            2 => "LC".to_string(),
            3 => "SSR".to_string(),
            4 => "LTP".to_string(),
            5 => "SBR".to_string(),
            _ => format!("{}", profile),
        },
        _ => format!("{}", profile),
    }
}

// ---------------------------------------------------------------------------
// EventCollector
// ---------------------------------------------------------------------------

/// Collects proxy events in a buffer and flushes them to Transmitarr periodically.
pub struct EventCollector {
    buffer: Mutex<Vec<ProxyEvent>>,
    transmitarr_url: String,
    client: reqwest::Client,
}

impl EventCollector {
    pub fn new(transmitarr_url: String) -> Self {
        Self {
            buffer: Mutex::new(Vec::new()),
            transmitarr_url,
            client: reqwest::Client::new(),
        }
    }

    /// Push an event into the buffer. Drops oldest events if buffer is at capacity.
    pub fn push(&self, event: ProxyEvent) {
        let mut buf = self.buffer.lock().unwrap();
        if buf.len() >= BUFFER_CAP {
            // Drop oldest events to make room
            let overflow = buf.len() - BUFFER_CAP + 1;
            buf.drain(..overflow);
        }
        buf.push(event);
    }

    /// Drain all buffered events.
    fn drain(&self) -> Vec<ProxyEvent> {
        let mut buf = self.buffer.lock().unwrap();
        std::mem::take(&mut *buf)
    }

    /// Flush buffered events to Transmitarr. Fire-and-forget on error.
    async fn flush(&self, events: Vec<ProxyEvent>) {
        if events.is_empty() {
            return;
        }

        let url = format!(
            "{}/api/proxy/events",
            self.transmitarr_url.trim_end_matches('/')
        );

        match self.client.post(&url).json(&events).send().await {
            Ok(resp) if resp.status().is_success() => {
                tracing::debug!("Flushed {} events to Transmitarr", events.len());
            }
            Ok(resp) => {
                tracing::warn!(
                    "Transmitarr event push returned HTTP {}: dropped {} events",
                    resp.status(),
                    events.len()
                );
            }
            Err(e) => {
                tracing::warn!(
                    "Transmitarr event push failed ({}): dropped {} events",
                    e,
                    events.len()
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Flush background task
// ---------------------------------------------------------------------------

/// Spawns a background task that periodically flushes events and emits MetricsSnapshots.
pub fn spawn_flush_task(state: Arc<AppState>, collector: Arc<EventCollector>, interval_secs: u64) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
        let prev_errors: Mutex<HashMap<String, (u64, u64, u64)>> = Mutex::new(HashMap::new());

        loop {
            interval.tick().await;

            // Emit MetricsSnapshot (and StreamError deltas) for each active channel
            emit_metrics_snapshots(&state, &collector, &prev_errors);

            // Drain and flush
            let events = collector.drain();
            collector.flush(events).await;
        }
    });
}

/// Emit a MetricsSnapshot event for every active channel.
/// Also emits StreamError events when new errors are detected since last snapshot.
fn emit_metrics_snapshots(
    state: &AppState,
    collector: &EventCollector,
    prev_errors: &Mutex<HashMap<String, (u64, u64, u64)>>,
) {
    let now = now_rfc3339();
    let mut prev = prev_errors.lock().unwrap();

    for entry in state.active_channels.iter() {
        let channel_id = entry.key().clone();
        let active = entry.value();
        let metrics = &active.metrics;

        let cc_errors = metrics.continuity_errors.load(Ordering::Relaxed);
        let pts_disc = metrics.pts_discontinuities.load(Ordering::Relaxed);
        let tei_errors = metrics.tei_errors.load(Ordering::Relaxed);

        // Emit StreamError events for new errors since last snapshot
        let (prev_cc, prev_pts, prev_tei) =
            prev.get(&channel_id).copied().unwrap_or((0, 0, 0));

        if cc_errors > prev_cc {
            collector.push(ProxyEvent::StreamError {
                channel_id: channel_id.clone(),
                error_type: "continuity_error".to_string(),
                count: cc_errors - prev_cc,
                timestamp: now.clone(),
            });
        }
        if pts_disc > prev_pts {
            collector.push(ProxyEvent::StreamError {
                channel_id: channel_id.clone(),
                error_type: "pts_discontinuity".to_string(),
                count: pts_disc - prev_pts,
                timestamp: now.clone(),
            });
        }
        if tei_errors > prev_tei {
            collector.push(ProxyEvent::StreamError {
                channel_id: channel_id.clone(),
                error_type: "tei_error".to_string(),
                count: tei_errors - prev_tei,
                timestamp: now.clone(),
            });
        }

        prev.insert(channel_id.clone(), (cc_errors, pts_disc, tei_errors));

        let bitrate_bps = metrics.current_bitrate_bps(10);
        let bitrate_kbps = bitrate_bps / 1000;

        collector.push(ProxyEvent::MetricsSnapshot {
            channel_id,
            upstream_bitrate_kbps: bitrate_kbps,
            continuity_errors: cc_errors,
            pts_discontinuities: pts_disc,
            pcr_jitter_us: metrics.pcr_jitter_us.load(Ordering::Relaxed),
            keyframe_interval_ms: metrics.keyframe_interval_ms.load(Ordering::Relaxed),
            keepalive_ratio: metrics.keepalive_ratio() as f32,
            viewer_count: active.clients.len() as u32,
            timestamp: now.clone(),
        });
    }

    // Clean up tracking for channels that are no longer active
    prev.retain(|k, _| state.active_channels.contains_key(k));
}

/// Current UTC timestamp as RFC 3339 string.
pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    // -----------------------------------------------------------------------
    // Event serialization roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn event_serialization_roundtrip_connection_opened() {
        let event = ProxyEvent::ConnectionOpened {
            channel_id: "ch-1".to_string(),
            client_id: "client-abc".to_string(),
            remote_addr: "192.168.1.1:12345".to_string(),
            transcoding: false,
            timestamp: "2026-03-13T00:00:00Z".to_string(),
        };

        let json = serde_json::to_string(&event).unwrap();
        let deser: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(deser["type"], "connection_opened");
        assert_eq!(deser["channel_id"], "ch-1");
        assert_eq!(deser["client_id"], "client-abc");
        assert_eq!(deser["transcoding"], false);
    }

    #[test]
    fn event_serialization_roundtrip_metrics_snapshot() {
        let event = ProxyEvent::MetricsSnapshot {
            channel_id: "ch-2".to_string(),
            upstream_bitrate_kbps: 4500,
            continuity_errors: 3,
            pts_discontinuities: 1,
            pcr_jitter_us: 120,
            keyframe_interval_ms: 2000,
            keepalive_ratio: 0.02,
            viewer_count: 5,
            timestamp: "2026-03-13T00:00:00Z".to_string(),
        };

        let json = serde_json::to_string(&event).unwrap();
        let deser: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(deser["type"], "metrics_snapshot");
        assert_eq!(deser["upstream_bitrate_kbps"], 4500);
        assert_eq!(deser["keepalive_ratio"], 0.02);
    }

    #[test]
    fn event_serialization_stream_info() {
        let event = ProxyEvent::StreamInfo {
            channel_id: "ch-3".to_string(),
            stream_id: 42,
            info: StreamInfoPayload {
                video_codec: Some("h264".to_string()),
                video_profile: Some("High".to_string()),
                video_level: Some("4.1".to_string()),
                resolution: Some("1920x1080".to_string()),
                fps: Some(29.97),
                chroma: Some("4:2:0".to_string()),
                bit_depth: Some(8),
                audio_codec: Some("aac".to_string()),
                audio_profile: Some("LC".to_string()),
                audio_sample_rate: Some(44100),
                audio_channels: Some(2),
            },
            timestamp: "2026-03-13T00:00:00Z".to_string(),
        };

        let json = serde_json::to_string(&event).unwrap();
        let deser: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(deser["type"], "stream_info");
        assert_eq!(deser["info"]["video_codec"], "h264");
        assert_eq!(deser["info"]["resolution"], "1920x1080");
        assert_eq!(deser["info"]["audio_channels"], 2);
    }

    #[test]
    fn event_serialization_all_variants() {
        let events = vec![
            ProxyEvent::ConnectionOpened {
                channel_id: "ch".into(),
                client_id: "c".into(),
                remote_addr: "addr".into(),
                transcoding: true,
                timestamp: "ts".into(),
            },
            ProxyEvent::ConnectionClosed {
                channel_id: "ch".into(),
                client_id: "c".into(),
                duration_secs: 100,
                bytes_sent: 999,
                timestamp: "ts".into(),
            },
            ProxyEvent::StreamFailover {
                channel_id: "ch".into(),
                from_stream_id: 1,
                from_account_id: 2,
                to_stream_id: 3,
                to_account_id: 4,
                reason: "stream ended".into(),
                timestamp: "ts".into(),
            },
            ProxyEvent::StreamError {
                channel_id: "ch".into(),
                error_type: "continuity_error".into(),
                count: 5,
                timestamp: "ts".into(),
            },
            ProxyEvent::ClientLagged {
                channel_id: "ch".into(),
                client_id: "c".into(),
                messages_skipped: 10,
                timestamp: "ts".into(),
            },
        ];

        for event in events {
            let json = serde_json::to_string(&event).unwrap();
            let _: serde_json::Value = serde_json::from_str(&json).unwrap();
        }
    }

    // -----------------------------------------------------------------------
    // Buffer cap behavior
    // -----------------------------------------------------------------------

    #[test]
    fn buffer_cap_drops_oldest_events() {
        let collector = EventCollector::new("http://localhost".to_string());

        // Fill buffer to capacity
        for i in 0..BUFFER_CAP {
            collector.push(ProxyEvent::MetricsSnapshot {
                channel_id: format!("ch-{}", i),
                upstream_bitrate_kbps: 0,
                continuity_errors: 0,
                pts_discontinuities: 0,
                pcr_jitter_us: 0,
                keyframe_interval_ms: 0,
                keepalive_ratio: 0.0,
                viewer_count: 0,
                timestamp: "ts".into(),
            });
        }

        // Buffer should be at capacity
        {
            let buf = collector.buffer.lock().unwrap();
            assert_eq!(buf.len(), BUFFER_CAP);
        }

        // Push one more — should drop oldest
        collector.push(ProxyEvent::ConnectionOpened {
            channel_id: "overflow".into(),
            client_id: "c".into(),
            remote_addr: "addr".into(),
            transcoding: false,
            timestamp: "ts".into(),
        });

        let buf = collector.buffer.lock().unwrap();
        assert_eq!(buf.len(), BUFFER_CAP);

        // Newest event should be the overflow one
        match &buf[buf.len() - 1] {
            ProxyEvent::ConnectionOpened { channel_id, .. } => {
                assert_eq!(channel_id, "overflow");
            }
            _ => panic!("Expected ConnectionOpened as last event"),
        }

        // Oldest surviving event should be ch-1 (ch-0 was dropped)
        match &buf[0] {
            ProxyEvent::MetricsSnapshot { channel_id, .. } => {
                assert_eq!(channel_id, "ch-1");
            }
            _ => panic!("Expected MetricsSnapshot as first event"),
        }
    }

    // -----------------------------------------------------------------------
    // Drain empties the buffer
    // -----------------------------------------------------------------------

    #[test]
    fn drain_returns_all_events_and_empties_buffer() {
        let collector = EventCollector::new("http://localhost".to_string());

        collector.push(ProxyEvent::ConnectionOpened {
            channel_id: "ch-1".into(),
            client_id: "c".into(),
            remote_addr: "addr".into(),
            transcoding: false,
            timestamp: "ts".into(),
        });
        collector.push(ProxyEvent::ConnectionClosed {
            channel_id: "ch-1".into(),
            client_id: "c".into(),
            duration_secs: 10,
            bytes_sent: 500,
            timestamp: "ts".into(),
        });

        let events = collector.drain();
        assert_eq!(events.len(), 2);

        // Buffer should now be empty
        let events2 = collector.drain();
        assert!(events2.is_empty());
    }

    // -----------------------------------------------------------------------
    // Concurrent push
    // -----------------------------------------------------------------------

    #[test]
    fn concurrent_push_does_not_lose_events() {
        let collector = Arc::new(EventCollector::new("http://localhost".to_string()));
        let mut handles = vec![];

        let events_per_thread = 50;
        let thread_count = 10;

        for t in 0..thread_count {
            let c = collector.clone();
            handles.push(thread::spawn(move || {
                for i in 0..events_per_thread {
                    c.push(ProxyEvent::MetricsSnapshot {
                        channel_id: format!("ch-{}-{}", t, i),
                        upstream_bitrate_kbps: 0,
                        continuity_errors: 0,
                        pts_discontinuities: 0,
                        pcr_jitter_us: 0,
                        keyframe_interval_ms: 0,
                        keepalive_ratio: 0.0,
                        viewer_count: 0,
                        timestamp: "ts".into(),
                    });
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        let total = thread_count * events_per_thread;
        let events = collector.drain();
        // All 500 events should be present (500 < BUFFER_CAP=1000)
        assert_eq!(events.len(), total);
    }

    // -----------------------------------------------------------------------
    // StreamInfoPayload construction
    // -----------------------------------------------------------------------

    #[test]
    fn stream_info_payload_from_full_info() {
        let info = crate::ts::StreamInfo {
            video_codec: Some(VideoCodec::H264),
            video_details: Some(crate::ts::VideoDetails {
                width: 1920,
                height: 1080,
                profile: 100,
                level: 41,
                chroma_format: 1,
                bit_depth_luma: 8,
                frame_mbs_only: true,
                fps_num: Some(30000),
                fps_den: Some(1001),
            }),
            audio_codec: Some(AudioCodec::Aac),
            audio_details: Some(crate::ts::AudioDetails {
                profile: 2,
                sample_rate: 44100,
                channels: 2,
            }),
            subtitle_pids: vec![],
            total_pids: 3,
            detected_at: std::time::Instant::now(),
        };

        let payload = StreamInfoPayload::from_stream_info(&info);

        assert_eq!(payload.video_codec.as_deref(), Some("h264"));
        assert_eq!(payload.video_profile.as_deref(), Some("High"));
        assert_eq!(payload.video_level.as_deref(), Some("4.1"));
        assert_eq!(payload.resolution.as_deref(), Some("1920x1080"));
        assert!((payload.fps.unwrap() - 29.97).abs() < 0.01);
        assert_eq!(payload.chroma.as_deref(), Some("4:2:0"));
        assert_eq!(payload.bit_depth, Some(8));
        assert_eq!(payload.audio_codec.as_deref(), Some("aac"));
        assert_eq!(payload.audio_profile.as_deref(), Some("LC"));
        assert_eq!(payload.audio_sample_rate, Some(44100));
        assert_eq!(payload.audio_channels, Some(2));
    }

    #[test]
    fn stream_info_payload_from_partial_info() {
        let info = crate::ts::StreamInfo {
            video_codec: None,
            video_details: None,
            audio_codec: Some(AudioCodec::Ac3),
            audio_details: Some(crate::ts::AudioDetails {
                profile: 0,
                sample_rate: 48000,
                channels: 6,
            }),
            subtitle_pids: vec![],
            total_pids: 1,
            detected_at: std::time::Instant::now(),
        };

        let payload = StreamInfoPayload::from_stream_info(&info);

        assert!(payload.video_codec.is_none());
        assert!(payload.resolution.is_none());
        assert_eq!(payload.audio_codec.as_deref(), Some("ac3"));
        assert_eq!(payload.audio_channels, Some(6));
    }

    // -----------------------------------------------------------------------
    // Flush task (async)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn flush_task_fires_at_interval() {
        // We can't easily test the real flush task without a server, but we can
        // verify that drain + flush with an unreachable URL doesn't panic.
        let collector = EventCollector::new("http://127.0.0.1:1".to_string());

        collector.push(ProxyEvent::ConnectionOpened {
            channel_id: "ch-1".into(),
            client_id: "c".into(),
            remote_addr: "addr".into(),
            transcoding: false,
            timestamp: "ts".into(),
        });

        let events = collector.drain();
        assert_eq!(events.len(), 1);

        // Flush to unreachable URL should not panic (fire-and-forget)
        collector.flush(events).await;

        // Buffer should be empty after drain
        let remaining = collector.drain();
        assert!(remaining.is_empty());
    }

    #[tokio::test]
    async fn flush_empty_is_noop() {
        let collector = EventCollector::new("http://127.0.0.1:1".to_string());
        let events = collector.drain();
        assert!(events.is_empty());
        // Should return immediately without making any HTTP request
        collector.flush(events).await;
    }
}
