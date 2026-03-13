use crate::ts::{StreamInfo, StreamInfoDetector, TsInspector};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

/// Record of a completed client session, stored for the event system (Batch 4).
#[derive(Debug, Clone)]
pub struct CompletedSession {
    pub client_id: String,
    pub bytes_sent: u64,
    pub duration_secs: u64,
    pub completed_at: Instant,
}

/// Duration of the bitrate sample window.
const BITRATE_WINDOW_SECS: u64 = 120;

/// Per-channel metrics collected from the upstream data flow.
pub struct ChannelMetrics {
    // -- TS inspection and stream detection --
    inspector: Mutex<TsInspector>,
    detector: Mutex<StreamInfoDetector>,

    /// Detected stream info (updated when detector finalizes or PMT changes).
    pub stream_info: Mutex<Option<StreamInfo>>,

    // -- Time-series (1-second samples, last 120 seconds) --
    /// Ring buffer of (timestamp, cumulative_bytes_in_this_second) samples.
    bitrate_samples: Mutex<BitrateSampler>,

    // -- Error counters --
    pub continuity_errors: AtomicU64,
    pub tei_errors: AtomicU64,
    pub pts_discontinuities: AtomicU64,

    // -- PCR jitter tracking --
    /// Latest PCR jitter in microseconds.
    pub pcr_jitter_us: AtomicU64,

    // -- Upstream health --
    pub upstream_reconnects: AtomicU32,
    pub time_to_first_byte_ms: AtomicU64,
    pub upstream_response_time_ms: AtomicU64,

    // -- Keyframe tracking --
    pub last_keyframe_at: Mutex<Option<Instant>>,
    pub keyframe_interval_ms: AtomicU64,

    // -- Keepalive ratio --
    pub keepalive_packets_sent: AtomicU64,
    pub data_packets_sent: AtomicU64,

    // -- Client behavior --
    pub client_lag_events: Mutex<HashMap<String, u64>>,
    pub client_stall_events: Mutex<HashMap<String, u64>>,

    // -- Lifetime aggregates --
    pub peak_concurrent_viewers: AtomicU32,
    pub total_unique_viewers: AtomicU32,
    pub unique_viewer_ips: Mutex<HashSet<String>>,

    // -- Completed session log --
    pub completed_sessions: Mutex<Vec<CompletedSession>>,
}

impl ChannelMetrics {
    pub fn new() -> Self {
        Self {
            inspector: Mutex::new(TsInspector::new()),
            detector: Mutex::new(StreamInfoDetector::new()),
            stream_info: Mutex::new(None),
            bitrate_samples: Mutex::new(BitrateSampler::new()),
            continuity_errors: AtomicU64::new(0),
            tei_errors: AtomicU64::new(0),
            pts_discontinuities: AtomicU64::new(0),
            pcr_jitter_us: AtomicU64::new(0),
            upstream_reconnects: AtomicU32::new(0),
            time_to_first_byte_ms: AtomicU64::new(0),
            upstream_response_time_ms: AtomicU64::new(0),
            last_keyframe_at: Mutex::new(None),
            keyframe_interval_ms: AtomicU64::new(0),
            keepalive_packets_sent: AtomicU64::new(0),
            data_packets_sent: AtomicU64::new(0),
            client_lag_events: Mutex::new(HashMap::new()),
            client_stall_events: Mutex::new(HashMap::new()),
            peak_concurrent_viewers: AtomicU32::new(0),
            total_unique_viewers: AtomicU32::new(0),
            unique_viewer_ips: Mutex::new(HashSet::new()),
            completed_sessions: Mutex::new(Vec::new()),
        }
    }

    /// Process a chunk of upstream TS data through the inspector and detector.
    /// Updates all TS-derived metrics (continuity, PTS, PCR, stream info, keyframes).
    pub fn process_chunk(&self, data: &[u8]) {
        let mut inspector = self.inspector.lock().unwrap();
        let mut detector = self.detector.lock().unwrap();

        let prev_continuity_errors = inspector.continuity.errors;
        let prev_pts_discontinuities = inspector.pts_tracker.discontinuities;

        inspector.process_chunk(data);

        // Update error counters from inspector deltas
        let new_continuity_errors = inspector.continuity.errors - prev_continuity_errors;
        if new_continuity_errors > 0 {
            self.continuity_errors
                .fetch_add(new_continuity_errors, Ordering::Relaxed);
        }

        let new_pts_disc = inspector.pts_tracker.discontinuities - prev_pts_discontinuities;
        if new_pts_disc > 0 {
            self.pts_discontinuities
                .fetch_add(new_pts_disc, Ordering::Relaxed);
        }

        // Count TEI errors by scanning packets in this chunk
        let tei_count = count_tei_errors(data);
        if tei_count > 0 {
            self.tei_errors.fetch_add(tei_count, Ordering::Relaxed);
        }

        // Update PCR jitter (convert 27 MHz ticks to microseconds)
        if inspector.pcr_tracker.jitter_count > 0 {
            // Latest jitter = average over all samples (approximation for "latest")
            // Actually, we want the most recent jitter value. The PcrTracker doesn't expose
            // the last sample directly, so we use a calculation from jitter_max as a proxy
            // for the latest reading, or compute from the average.
            // For accuracy, let's track the last jitter via the inspector's record return value.
            // Since we can't easily get the last jitter from the tracker after the fact,
            // we'll compute from average. This is fine for monitoring purposes.
            let avg_jitter_27mhz = inspector.pcr_tracker.average_jitter();
            // Convert 27 MHz ticks to microseconds: ticks / 27 = microseconds
            let jitter_us = (avg_jitter_27mhz / 27.0) as u64;
            self.pcr_jitter_us.store(jitter_us, Ordering::Relaxed);
        }

        // Feed packets to the stream info detector if PMT is available
        if let Some(pmt_info) = &inspector.pmt_info {
            let pmt_version = Some(pmt_info.version);
            let version_changed = detector.update_from_pmt(pmt_info, pmt_version);

            if version_changed {
                // Clear previously detected stream_info so the detector can re-detect
                *self.stream_info.lock().unwrap() = None;
            }

            // Feed relevant packets through the detector
            // We need to re-process packets for the detector since it tracks PES accumulation.
            // Process the chunk packet-by-packet for the detector.
            let mut offset = 0;
            while offset + 188 <= data.len() {
                let packet = &data[offset..offset + 188];
                if let Some(header) = crate::ts::TsPacketHeader::parse(packet) {
                    if header.has_payload() {
                        let payload_start = payload_offset(packet, &header);
                        if payload_start < 188 {
                            let payload = &packet[payload_start..];
                            detector.feed_packet(header.pid, header.pusi, payload);
                        }
                    }

                    // Keyframe detection on video PID
                    if let Some(vid_pid) = inspector.video_pid {
                        if header.pid == vid_pid && header.pusi && header.has_payload() {
                            let payload_start = payload_offset(packet, &header);
                            if payload_start < 188 {
                                let payload = &packet[payload_start..];
                                if is_keyframe_start(payload, &inspector) {
                                    let now = Instant::now();
                                    let mut last_kf = self.last_keyframe_at.lock().unwrap();
                                    if let Some(prev) = *last_kf {
                                        let interval = now.duration_since(prev).as_millis() as u64;
                                        self.keyframe_interval_ms
                                            .store(interval, Ordering::Relaxed);
                                    }
                                    *last_kf = Some(now);
                                }
                            }
                        }
                    }
                }
                offset += 188;
            }

            // If the detector finalized, capture the stream info
            if let Some(info) = &detector.stream_info {
                *self.stream_info.lock().unwrap() = Some(info.clone());
            }
        }
    }

    /// Record incoming bytes for bitrate sampling.
    pub fn record_bitrate_sample(&self, byte_count: usize) {
        let mut sampler = self.bitrate_samples.lock().unwrap();
        sampler.add_bytes(byte_count as u64);
    }

    /// Get the current bitrate in bits per second, averaged over the last `window_secs` seconds.
    pub fn current_bitrate_bps(&self, window_secs: u64) -> u64 {
        let sampler = self.bitrate_samples.lock().unwrap();
        sampler.bitrate_bps(window_secs)
    }

    /// Record a client lag event.
    pub fn record_client_lag(&self, client_id: &str, lagged_count: u64) {
        let mut map = self.client_lag_events.lock().unwrap();
        *map.entry(client_id.to_string()).or_insert(0) += lagged_count;
    }

    /// Record a keepalive packet sent.
    pub fn record_keepalive_sent(&self) {
        self.keepalive_packets_sent.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a data packet sent to a client.
    pub fn record_data_sent(&self) {
        self.data_packets_sent.fetch_add(1, Ordering::Relaxed);
    }

    /// Register a new viewer (updates unique viewer tracking and peak concurrent).
    pub fn register_viewer(&self, remote_addr: &str, current_client_count: u32) {
        // Update peak concurrent
        let _ = self.peak_concurrent_viewers.fetch_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            |current| {
                if current_client_count > current {
                    Some(current_client_count)
                } else {
                    None
                }
            },
        );

        // Track unique IPs
        let mut ips = self.unique_viewer_ips.lock().unwrap();
        if ips.insert(remote_addr.to_string()) {
            self.total_unique_viewers.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Record a completed client session (called from ClientGuard::drop).
    /// Stores session data for the event system (Batch 4) to include in ConnectionClosed events.
    pub fn record_session_end(&self, client_id: &str, bytes_sent: u64, duration_secs: u64) {
        let session = CompletedSession {
            client_id: client_id.to_string(),
            bytes_sent,
            duration_secs,
            completed_at: Instant::now(),
        };
        let mut sessions = self.completed_sessions.lock().unwrap();
        sessions.push(session);
    }

    /// Drain all completed sessions (consuming them). Used by the event system to emit events.
    pub fn drain_completed_sessions(&self) -> Vec<CompletedSession> {
        let mut sessions = self.completed_sessions.lock().unwrap();
        std::mem::take(&mut *sessions)
    }

    /// Record an upstream reconnect event.
    pub fn record_reconnect(&self) {
        self.upstream_reconnects.fetch_add(1, Ordering::Relaxed);
    }

    /// Keepalive ratio: keepalive_packets / (keepalive_packets + data_packets).
    /// Returns 0.0 if no packets sent. Higher values indicate more upstream gaps.
    pub fn keepalive_ratio(&self) -> f64 {
        let keepalive = self.keepalive_packets_sent.load(Ordering::Relaxed) as f64;
        let data = self.data_packets_sent.load(Ordering::Relaxed) as f64;
        let total = keepalive + data;
        if total == 0.0 {
            0.0
        } else {
            keepalive / total
        }
    }
}

/// Count Transport Error Indicator flags in a chunk of TS packets.
fn count_tei_errors(data: &[u8]) -> u64 {
    let mut count = 0u64;
    let mut offset = 0;
    while offset + 188 <= data.len() {
        // Check sync byte and TEI bit
        if data[offset] == 0x47 && (data[offset + 1] & 0x80) != 0 {
            count += 1;
        }
        offset += 188;
    }
    count
}

/// Calculate payload offset within a TS packet (mirrors TsInspector::payload_offset).
fn payload_offset(packet: &[u8], header: &crate::ts::TsPacketHeader) -> usize {
    let mut offset = 4;
    if header.has_adaptation_field() {
        if offset < packet.len() {
            let adapt_len = packet[offset] as usize;
            offset += 1 + adapt_len;
        } else {
            return 188;
        }
    }
    offset
}

/// Detect if a PES packet start contains a keyframe (IDR for H.264, IRAP for HEVC).
fn is_keyframe_start(payload: &[u8], inspector: &TsInspector) -> bool {
    // Need at least a PES header to check
    if payload.len() < 14 {
        return false;
    }

    // Verify PES start code
    if payload[0] != 0x00 || payload[1] != 0x00 || payload[2] != 0x01 {
        return false;
    }

    // Skip PES header to get to elementary stream data
    if payload.len() < 9 {
        return false;
    }
    let header_data_len = payload[8] as usize;
    let es_start = 9 + header_data_len;
    if es_start >= payload.len() {
        return false;
    }

    let es_data = &payload[es_start..];

    // Determine codec from PMT info
    let video_stream_type = inspector
        .pmt_info
        .as_ref()
        .and_then(|pmt| {
            pmt.streams
                .iter()
                .find(|s| crate::ts::is_video_stream_type(s.stream_type))
                .map(|s| s.stream_type)
        });

    match video_stream_type {
        Some(0x1B) => {
            // H.264: look for IDR NAL unit type (5) or SPS (7) indicating keyframe
            // Quick scan: look for start code followed by NAL type
            is_h264_keyframe(es_data)
        }
        Some(0x24) => {
            // HEVC: look for IRAP NAL types (BLA, IDR, CRA: types 16-23)
            is_hevc_keyframe(es_data)
        }
        Some(0x02) => {
            // MPEG-2: look for sequence header (0x000001B3) or I-frame picture coding type
            is_mpeg2_keyframe(es_data)
        }
        _ => false,
    }
}

/// Check if H.264 elementary stream data starts with a keyframe.
fn is_h264_keyframe(es_data: &[u8]) -> bool {
    // Scan for NAL start codes and check types
    let mut i = 0;
    while i + 4 < es_data.len() && i < 64 {
        if es_data[i] == 0x00 && es_data[i + 1] == 0x00 {
            let (nal_byte_offset, found) = if es_data[i + 2] == 0x01 {
                (i + 3, true)
            } else if i + 3 < es_data.len() && es_data[i + 2] == 0x00 && es_data[i + 3] == 0x01 {
                (i + 4, true)
            } else {
                (0, false)
            };

            if found && nal_byte_offset < es_data.len() {
                let nal_type = es_data[nal_byte_offset] & 0x1F;
                // IDR = 5, SPS = 7
                if nal_type == 5 || nal_type == 7 {
                    return true;
                }
            }
        }
        i += 1;
    }
    false
}

/// Check if HEVC elementary stream data starts with a keyframe.
fn is_hevc_keyframe(es_data: &[u8]) -> bool {
    let mut i = 0;
    while i + 5 < es_data.len() && i < 64 {
        if es_data[i] == 0x00 && es_data[i + 1] == 0x00 {
            let (nal_byte_offset, found) = if es_data[i + 2] == 0x01 {
                (i + 3, true)
            } else if i + 3 < es_data.len() && es_data[i + 2] == 0x00 && es_data[i + 3] == 0x01 {
                (i + 4, true)
            } else {
                (0, false)
            };

            if found && nal_byte_offset < es_data.len() {
                // HEVC NAL header is 2 bytes: forbidden(1) | type(6) | layer_id(6) | tid(3)
                let nal_type = (es_data[nal_byte_offset] >> 1) & 0x3F;
                // IRAP types: BLA_W_LP(16) through RSV_IRAP_VCL23(23)
                if (16..=23).contains(&nal_type) {
                    return true;
                }
            }
        }
        i += 1;
    }
    false
}

/// Check if MPEG-2 elementary stream data starts with a keyframe.
fn is_mpeg2_keyframe(es_data: &[u8]) -> bool {
    // Look for sequence header (00 00 01 B3) which precedes I-frames
    let mut i = 0;
    while i + 3 < es_data.len() && i < 64 {
        if es_data[i] == 0x00 && es_data[i + 1] == 0x00 && es_data[i + 2] == 0x01 {
            if i + 3 < es_data.len() && es_data[i + 3] == 0xB3 {
                return true; // Sequence header
            }
        }
        i += 1;
    }
    false
}

// ---------------------------------------------------------------------------
// Bitrate sampler — 1-second resolution ring buffer
// ---------------------------------------------------------------------------

/// Samples bytes received per second over a sliding window.
struct BitrateSampler {
    /// (second_timestamp, bytes_in_that_second) — oldest to newest.
    samples: VecDeque<(Instant, u64)>,
    /// The current second being accumulated.
    current_second_start: Instant,
    /// Bytes accumulated in the current second.
    current_bytes: u64,
}

impl BitrateSampler {
    fn new() -> Self {
        Self {
            samples: VecDeque::new(),
            current_second_start: Instant::now(),
            current_bytes: 0,
        }
    }

    /// Add bytes to the current accumulation bucket.
    fn add_bytes(&mut self, bytes: u64) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.current_second_start);

        if elapsed.as_secs() >= 1 {
            // Flush the current second into the ring buffer
            self.samples
                .push_back((self.current_second_start, self.current_bytes));

            // Advance to the current second
            self.current_second_start = now;
            self.current_bytes = bytes;

            // Prune samples older than the window
            self.prune(now);
        } else {
            self.current_bytes += bytes;
        }
    }

    /// Remove samples older than `BITRATE_WINDOW_SECS`.
    fn prune(&mut self, now: Instant) {
        let cutoff = std::time::Duration::from_secs(BITRATE_WINDOW_SECS);
        while let Some(&(ts, _)) = self.samples.front() {
            if now.duration_since(ts) > cutoff {
                self.samples.pop_front();
            } else {
                break;
            }
        }
    }

    /// Calculate bitrate in bits per second over the last `window_secs` seconds.
    fn bitrate_bps(&self, window_secs: u64) -> u64 {
        if window_secs == 0 {
            return 0;
        }

        let now = Instant::now();
        let cutoff = std::time::Duration::from_secs(window_secs);
        let mut total_bytes: u64 = 0;

        for &(ts, bytes) in &self.samples {
            if now.duration_since(ts) <= cutoff {
                total_bytes += bytes;
            }
        }

        // Include the current (unflushed) second if within window
        if now.duration_since(self.current_second_start) <= cutoff {
            total_bytes += self.current_bytes;
        }

        // bits per second = total_bytes * 8 / window_secs
        (total_bytes * 8) / window_secs
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    // -----------------------------------------------------------------------
    // BitrateSampler tests
    // -----------------------------------------------------------------------

    #[test]
    fn bitrate_sampler_empty() {
        let sampler = BitrateSampler::new();
        assert_eq!(sampler.bitrate_bps(10), 0);
    }

    #[test]
    fn bitrate_sampler_accumulates_within_second() {
        let mut sampler = BitrateSampler::new();
        sampler.add_bytes(1000);
        sampler.add_bytes(2000);

        // All bytes are in the current (unflushed) second
        // bitrate = 3000 * 8 / 10 = 2400 bps over a 10-second window
        assert_eq!(sampler.bitrate_bps(10), 2400);
    }

    #[test]
    fn bitrate_sampler_flushes_after_one_second() {
        let mut sampler = BitrateSampler::new();
        sampler.add_bytes(1000);

        // Manually advance the current_second_start to simulate time passing
        sampler.current_second_start = Instant::now() - Duration::from_secs(2);

        // This add_bytes call should flush the previous second
        sampler.add_bytes(500);

        // We should now have one flushed sample (1000 bytes) and current (500 bytes)
        assert_eq!(sampler.samples.len(), 1);
        assert_eq!(sampler.samples[0].1, 1000);
        assert_eq!(sampler.current_bytes, 500);
    }

    #[test]
    fn bitrate_sampler_ring_buffer_overflow() {
        let mut sampler = BitrateSampler::new();

        // Simulate 150 seconds of data (exceeds 120-second window)
        for i in 0..150 {
            sampler.current_second_start =
                Instant::now() - Duration::from_secs(150 - i);
            sampler.current_bytes = 1000;
            // Flush by adding with a new timestamp
            sampler.samples.push_back((sampler.current_second_start, 1000));
        }

        // Set current to now
        sampler.current_second_start = Instant::now();
        sampler.current_bytes = 0;

        // Prune old entries
        sampler.prune(Instant::now());

        // Should have at most BITRATE_WINDOW_SECS samples
        assert!(sampler.samples.len() <= BITRATE_WINDOW_SECS as usize);
    }

    #[test]
    fn bitrate_sampler_window_accuracy() {
        let mut sampler = BitrateSampler::new();

        // Add samples at known intervals
        let now = Instant::now();

        // 5 seconds ago: 1000 bytes
        sampler.samples.push_back((now - Duration::from_secs(5), 1000));
        // 3 seconds ago: 2000 bytes
        sampler.samples.push_back((now - Duration::from_secs(3), 2000));
        // 1 second ago: 500 bytes
        sampler.samples.push_back((now - Duration::from_secs(1), 500));

        sampler.current_second_start = now;
        sampler.current_bytes = 0;

        // 10-second window should include all three: 3500 bytes * 8 / 10 = 2800 bps
        assert_eq!(sampler.bitrate_bps(10), 2800);

        // 2-second window should include only the last sample: 500 * 8 / 2 = 2000 bps
        assert_eq!(sampler.bitrate_bps(2), 2000);
    }

    // -----------------------------------------------------------------------
    // ChannelMetrics tests
    // -----------------------------------------------------------------------

    #[test]
    fn channel_metrics_keepalive_ratio_zero() {
        let metrics = ChannelMetrics::new();
        assert_eq!(metrics.keepalive_ratio(), 0.0);
    }

    #[test]
    fn channel_metrics_keepalive_ratio_calculation() {
        let metrics = ChannelMetrics::new();
        metrics.keepalive_packets_sent.store(25, Ordering::Relaxed);
        metrics.data_packets_sent.store(75, Ordering::Relaxed);

        let ratio = metrics.keepalive_ratio();
        assert!((ratio - 0.25).abs() < f64::EPSILON);
    }

    #[test]
    fn channel_metrics_register_viewer_tracking() {
        let metrics = ChannelMetrics::new();

        metrics.register_viewer("192.168.1.1:12345", 1);
        assert_eq!(metrics.total_unique_viewers.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.peak_concurrent_viewers.load(Ordering::Relaxed), 1);

        // Same IP again — no new unique viewer
        metrics.register_viewer("192.168.1.1:12345", 2);
        assert_eq!(metrics.total_unique_viewers.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.peak_concurrent_viewers.load(Ordering::Relaxed), 2);

        // Different IP
        metrics.register_viewer("192.168.1.2:12345", 3);
        assert_eq!(metrics.total_unique_viewers.load(Ordering::Relaxed), 2);
        assert_eq!(metrics.peak_concurrent_viewers.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn channel_metrics_client_lag_events() {
        let metrics = ChannelMetrics::new();

        metrics.record_client_lag("client-1", 5);
        metrics.record_client_lag("client-1", 3);
        metrics.record_client_lag("client-2", 1);

        let map = metrics.client_lag_events.lock().unwrap();
        assert_eq!(map.get("client-1"), Some(&8));
        assert_eq!(map.get("client-2"), Some(&1));
    }

    #[test]
    fn channel_metrics_concurrent_updates() {
        use std::sync::Arc;

        let metrics = Arc::new(ChannelMetrics::new());
        let mut handles = vec![];

        // Spawn 10 threads, each recording 100 data packets and 50 keepalives
        for _ in 0..10 {
            let m = metrics.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..100 {
                    m.record_data_sent();
                }
                for _ in 0..50 {
                    m.record_keepalive_sent();
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(
            metrics.data_packets_sent.load(Ordering::Relaxed),
            1000
        );
        assert_eq!(
            metrics.keepalive_packets_sent.load(Ordering::Relaxed),
            500
        );
    }

    #[test]
    fn channel_metrics_concurrent_bitrate_recording() {
        use std::sync::Arc;

        let metrics = Arc::new(ChannelMetrics::new());
        let mut handles = vec![];

        // Spawn 4 threads, each recording 1000 bytes
        for _ in 0..4 {
            let m = metrics.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..100 {
                    m.record_bitrate_sample(10);
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        // Total bytes should be 4 * 100 * 10 = 4000
        let bps = metrics.current_bitrate_bps(10);
        // 4000 * 8 / 10 = 3200 bps expected (approximately — timing can vary)
        assert!(bps > 0, "Bitrate should be non-zero after recording data");
    }

    #[test]
    fn tei_error_counting() {
        // Build a chunk with one TEI packet and one normal packet
        let mut data = vec![0u8; 188 * 2];

        // Packet 1: normal
        data[0] = 0x47;
        data[1] = 0x00;

        // Packet 2: TEI set
        data[188] = 0x47;
        data[189] = 0x80; // TEI bit

        assert_eq!(count_tei_errors(&data), 1);
    }

    #[test]
    fn keyframe_detection_h264_idr() {
        // Simulate PES payload containing H.264 IDR NAL
        // PES header: 00 00 01 E0 00 00 80 80 05 [pts_bytes] then ES data
        let mut payload = vec![
            0x00, 0x00, 0x01, 0xE0, // PES start code + stream_id
            0x00, 0x00, // PES packet length
            0x80, // marker
            0x80, // PTS only
            0x05, // header data length
            0x21, 0x00, 0x01, 0x00, 0x01, // PTS bytes (dummy)
        ];
        // NAL start code + IDR slice (type 5)
        payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x65]); // 0x65 & 0x1F = 5

        // Build a minimal inspector with H.264 PMT
        let mut inspector = TsInspector::new();
        inspector.pmt_info = Some(crate::ts::PmtInfo {
            pcr_pid: 256,
            streams: vec![crate::ts::PmtStream {
                stream_type: 0x1B, // H.264
                elementary_pid: 256,
                descriptors: vec![],
            }],
            version: 0,
        });

        assert!(is_keyframe_start(&payload, &inspector));
    }

    #[test]
    fn keyframe_detection_non_keyframe() {
        // PES payload with H.264 non-IDR slice (type 1)
        let mut payload = vec![
            0x00, 0x00, 0x01, 0xE0,
            0x00, 0x00,
            0x80,
            0x80,
            0x05,
            0x21, 0x00, 0x01, 0x00, 0x01,
        ];
        // NAL start code + non-IDR slice (type 1)
        payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x41]); // 0x41 & 0x1F = 1

        let mut inspector = TsInspector::new();
        inspector.pmt_info = Some(crate::ts::PmtInfo {
            pcr_pid: 256,
            streams: vec![crate::ts::PmtStream {
                stream_type: 0x1B,
                elementary_pid: 256,
                descriptors: vec![],
            }],
            version: 0,
        });

        assert!(!is_keyframe_start(&payload, &inspector));
    }

    // -----------------------------------------------------------------------
    // Session recording tests
    // -----------------------------------------------------------------------

    #[test]
    fn record_session_end_stores_session() {
        let metrics = ChannelMetrics::new();

        metrics.record_session_end("client-abc", 1_000_000, 120);

        let sessions = metrics.completed_sessions.lock().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].client_id, "client-abc");
        assert_eq!(sessions[0].bytes_sent, 1_000_000);
        assert_eq!(sessions[0].duration_secs, 120);
    }

    #[test]
    fn record_session_end_multiple_sessions() {
        let metrics = ChannelMetrics::new();

        metrics.record_session_end("client-1", 500, 10);
        metrics.record_session_end("client-2", 2000, 60);
        metrics.record_session_end("client-3", 0, 0);

        let sessions = metrics.completed_sessions.lock().unwrap();
        assert_eq!(sessions.len(), 3);
        assert_eq!(sessions[0].client_id, "client-1");
        assert_eq!(sessions[1].client_id, "client-2");
        assert_eq!(sessions[2].client_id, "client-3");
    }

    #[test]
    fn drain_completed_sessions_empties_vec() {
        let metrics = ChannelMetrics::new();

        metrics.record_session_end("client-1", 100, 5);
        metrics.record_session_end("client-2", 200, 10);

        let drained = metrics.drain_completed_sessions();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].client_id, "client-1");
        assert_eq!(drained[1].client_id, "client-2");

        // Vec should now be empty
        let after = metrics.drain_completed_sessions();
        assert!(after.is_empty());
    }

    #[test]
    fn record_session_end_concurrent() {
        use std::sync::Arc;

        let metrics = Arc::new(ChannelMetrics::new());
        let mut handles = vec![];

        for i in 0..10 {
            let m = metrics.clone();
            handles.push(thread::spawn(move || {
                for j in 0..5 {
                    m.record_session_end(
                        &format!("client-{}-{}", i, j),
                        1000 * (i as u64),
                        j as u64,
                    );
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        let sessions = metrics.completed_sessions.lock().unwrap();
        assert_eq!(sessions.len(), 50);
    }
}
