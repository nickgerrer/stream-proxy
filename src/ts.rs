use std::collections::HashMap;

/// Size of a single MPEG-TS packet in bytes.
pub const TS_PACKET_SIZE: usize = 188;

/// TS sync byte — first byte of every valid packet.
const SYNC_BYTE: u8 = 0x47;

/// Null PID — padding packets that carry no useful data.
const NULL_PID: u16 = 0x1FFF;

/// PAT always lives on PID 0.
const PAT_PID: u16 = 0x0000;

/// Known video stream types.
const STREAM_TYPE_MPEG2_VIDEO: u8 = 0x02;
const STREAM_TYPE_H264: u8 = 0x1B;
const STREAM_TYPE_HEVC: u8 = 0x24;

/// Known audio stream types.
const STREAM_TYPE_MP2_A: u8 = 0x03;
const STREAM_TYPE_MP2_B: u8 = 0x04;
const STREAM_TYPE_AAC: u8 = 0x0F;
const STREAM_TYPE_AC3: u8 = 0x81;
const STREAM_TYPE_EAC3: u8 = 0x87;

// ---------------------------------------------------------------------------
// TS packet header (first 4 bytes)
// ---------------------------------------------------------------------------

/// Parsed header of a 188-byte TS packet.
#[derive(Debug, Clone, PartialEq)]
pub struct TsPacketHeader {
    pub sync_byte: u8,
    /// Transport error indicator.
    pub tei: bool,
    /// Payload unit start indicator.
    pub pusi: bool,
    /// 13-bit packet identifier.
    pub pid: u16,
    /// Transport scrambling control (2 bits).
    pub scrambling: u8,
    /// Adaptation field control (2 bits).
    pub adaptation: u8,
    /// Continuity counter (4 bits, 0-15).
    pub continuity: u8,
}

impl TsPacketHeader {
    /// Parse the 4-byte header at the start of `packet`.
    /// Returns `None` if the slice is too short or the sync byte is wrong.
    pub fn parse(packet: &[u8]) -> Option<Self> {
        if packet.len() < 4 || packet[0] != SYNC_BYTE {
            return None;
        }

        Some(Self {
            sync_byte: packet[0],
            tei: (packet[1] & 0x80) != 0,
            pusi: (packet[1] & 0x40) != 0,
            pid: u16::from_be_bytes([packet[1] & 0x1F, packet[2]]),
            scrambling: (packet[3] >> 6) & 0x03,
            adaptation: (packet[3] >> 4) & 0x03,
            continuity: packet[3] & 0x0F,
        })
    }

    /// True when an adaptation field is present (bits: 10 or 11).
    pub fn has_adaptation_field(&self) -> bool {
        self.adaptation & 0x02 != 0
    }

    /// True when payload data follows the header / adaptation field (bits: 01 or 11).
    pub fn has_payload(&self) -> bool {
        self.adaptation & 0x01 != 0
    }
}

// ---------------------------------------------------------------------------
// PAT / PMT parsing
// ---------------------------------------------------------------------------

/// One entry extracted from the PAT: program_number → PMT PID.
#[derive(Debug, Clone, PartialEq)]
pub struct PatEntry {
    pub program_number: u16,
    pub pmt_pid: u16,
}

/// Parse a PAT section from the payload bytes (after the pointer field).
/// Returns the list of program→PMT PID mappings.
pub fn parse_pat(section: &[u8]) -> Vec<PatEntry> {
    // Minimum section: table_id(1) + flags(2) + tid(2) + ver(1) + sec(1) + last(1) = 8 bytes + 4 CRC
    if section.len() < 12 {
        return Vec::new();
    }

    let table_id = section[0];
    if table_id != 0x00 {
        return Vec::new();
    }

    let section_length = (u16::from_be_bytes([section[1] & 0x0F, section[2]])) as usize;
    // section_length includes bytes after the length field up to and including CRC
    // The fixed fields after length: transport_stream_id(2) + version(1) + section_number(1) + last_section(1) = 5
    // Then program loop, then 4-byte CRC
    let header_after_length = 5;
    let crc_size = 4;

    if section_length < header_after_length + crc_size {
        return Vec::new();
    }
    let loop_len = section_length - header_after_length - crc_size;
    let loop_start = 3 + header_after_length; // 3 bytes for table_id + section_length field

    if loop_start + loop_len > section.len() {
        return Vec::new();
    }

    let mut entries = Vec::new();
    let mut i = 0;
    while i + 4 <= loop_len {
        let program_number = u16::from_be_bytes([
            section[loop_start + i],
            section[loop_start + i + 1],
        ]);
        let pmt_pid = u16::from_be_bytes([
            section[loop_start + i + 2] & 0x1F,
            section[loop_start + i + 3],
        ]);

        if program_number != 0 {
            // program_number 0 is the network PID, skip it
            entries.push(PatEntry {
                program_number,
                pmt_pid,
            });
        }

        i += 4;
    }

    entries
}

/// A single elementary stream entry from the PMT.
#[derive(Debug, Clone, PartialEq)]
pub struct PmtStream {
    pub stream_type: u8,
    pub elementary_pid: u16,
    /// Raw descriptor bytes for this stream.
    pub descriptors: Vec<u8>,
}

/// Result of parsing a PMT section.
#[derive(Debug, Clone, PartialEq)]
pub struct PmtInfo {
    pub pcr_pid: u16,
    pub streams: Vec<PmtStream>,
}

/// Parse a PMT section from the payload bytes (after the pointer field).
pub fn parse_pmt(section: &[u8]) -> Option<PmtInfo> {
    if section.len() < 16 {
        return None;
    }

    let table_id = section[0];
    if table_id != 0x02 {
        return None;
    }

    let section_length = (u16::from_be_bytes([section[1] & 0x0F, section[2]])) as usize;
    // Fixed fields after section_length: program_number(2) + version(1) + section_number(1) + last_section(1) + PCR_PID(2) + program_info_length(2) = 9
    let header_after_length = 9;
    let crc_size = 4;

    if section_length < header_after_length + crc_size {
        return None;
    }

    let pcr_pid = u16::from_be_bytes([section[8] & 0x1F, section[9]]);
    let program_info_length =
        (u16::from_be_bytes([section[10] & 0x0F, section[11]])) as usize;

    let stream_loop_start = 12 + program_info_length;
    let stream_loop_end = 3 + section_length - crc_size; // 3 = table_id(1) + section_length_field(2)

    if stream_loop_start > stream_loop_end || stream_loop_end > section.len() {
        return None;
    }

    let mut streams = Vec::new();
    let mut pos = stream_loop_start;

    while pos + 5 <= stream_loop_end {
        let stream_type = section[pos];
        let elementary_pid =
            u16::from_be_bytes([section[pos + 1] & 0x1F, section[pos + 2]]);
        let es_info_length =
            (u16::from_be_bytes([section[pos + 3] & 0x0F, section[pos + 4]])) as usize;

        let desc_start = pos + 5;
        let desc_end = desc_start + es_info_length;

        if desc_end > stream_loop_end {
            break;
        }

        let descriptors = section[desc_start..desc_end].to_vec();

        streams.push(PmtStream {
            stream_type,
            elementary_pid,
            descriptors,
        });

        pos = desc_end;
    }

    Some(PmtInfo { pcr_pid, streams })
}

// ---------------------------------------------------------------------------
// Helper: classify stream types
// ---------------------------------------------------------------------------

/// Returns `true` if the stream type is a known video codec.
pub fn is_video_stream_type(st: u8) -> bool {
    matches!(st, STREAM_TYPE_MPEG2_VIDEO | STREAM_TYPE_H264 | STREAM_TYPE_HEVC)
}

/// Returns `true` if the stream type is a known audio codec.
pub fn is_audio_stream_type(st: u8) -> bool {
    matches!(
        st,
        STREAM_TYPE_MP2_A | STREAM_TYPE_MP2_B | STREAM_TYPE_AAC | STREAM_TYPE_AC3 | STREAM_TYPE_EAC3
    )
}

/// Human-readable label for a stream type byte.
pub fn stream_type_name(st: u8) -> &'static str {
    match st {
        STREAM_TYPE_MPEG2_VIDEO => "MPEG-2",
        STREAM_TYPE_H264 => "H.264",
        STREAM_TYPE_HEVC => "HEVC",
        STREAM_TYPE_MP2_A | STREAM_TYPE_MP2_B => "MP2",
        STREAM_TYPE_AAC => "AAC",
        STREAM_TYPE_AC3 => "AC3",
        STREAM_TYPE_EAC3 => "EAC3",
        _ => "Unknown",
    }
}

// ---------------------------------------------------------------------------
// Continuity counter tracking
// ---------------------------------------------------------------------------

/// Tracks per-PID continuity counters and counts discontinuities.
#[derive(Debug)]
pub struct ContinuityTracker {
    /// PID → last seen continuity counter.
    counters: HashMap<u16, u8>,
    /// Total discontinuity count across all PIDs.
    pub errors: u64,
}

impl ContinuityTracker {
    pub fn new() -> Self {
        Self {
            counters: HashMap::new(),
            errors: 0,
        }
    }

    /// Check a packet's continuity counter. Returns `true` if a discontinuity was detected.
    pub fn check(&mut self, header: &TsPacketHeader) -> bool {
        // Skip null packets and adaptation-only packets (no payload).
        if header.pid == NULL_PID || !header.has_payload() {
            return false;
        }

        if let Some(&prev) = self.counters.get(&header.pid) {
            let expected = (prev + 1) & 0x0F;
            if header.continuity != expected {
                // Duplicate packets (same CC) are allowed by the spec but we count
                // only actual gaps as errors.
                if header.continuity != prev {
                    self.errors += 1;
                    self.counters.insert(header.pid, header.continuity);
                    return true;
                }
                // Duplicate — update nothing, not an error.
                return false;
            }
        }

        self.counters.insert(header.pid, header.continuity);
        false
    }
}

// ---------------------------------------------------------------------------
// PTS extraction
// ---------------------------------------------------------------------------

/// Extract a 33-bit PTS from PES header bytes starting at `pusi` payload.
/// `payload` must start at the PES start code (0x00 0x00 0x01).
pub fn extract_pts(payload: &[u8]) -> Option<u64> {
    // PES start code: 00 00 01 stream_id
    if payload.len() < 14 {
        return None;
    }
    if payload[0] != 0x00 || payload[1] != 0x00 || payload[2] != 0x01 {
        return None;
    }

    // byte 7: PTS/DTS flags in bits 7-6
    let pts_dts_flags = (payload[7] >> 6) & 0x03;
    if pts_dts_flags < 2 {
        // No PTS present
        return None;
    }

    // PTS starts at byte 9 (5 bytes)
    let b = &payload[9..14];
    let pts = ((u64::from(b[0]) & 0x0E) << 29)
        | (u64::from(b[1]) << 22)
        | ((u64::from(b[2]) & 0xFE) << 14)
        | (u64::from(b[3]) << 7)
        | (u64::from(b[4]) >> 1);

    Some(pts)
}

// ---------------------------------------------------------------------------
// PCR extraction
// ---------------------------------------------------------------------------

/// A parsed PCR value (42 bits total: 33-bit base + 9-bit extension).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pcr {
    /// 33-bit PCR base (90 kHz clock).
    pub base: u64,
    /// 9-bit PCR extension (27 MHz clock).
    pub extension: u16,
}

impl Pcr {
    /// Full PCR value in 27 MHz ticks.
    pub fn as_27mhz(&self) -> u64 {
        self.base * 300 + u64::from(self.extension)
    }
}

/// Extract PCR from the adaptation field of a TS packet.
/// `packet` must be the full 188-byte TS packet.
pub fn extract_pcr(packet: &[u8]) -> Option<Pcr> {
    if packet.len() < TS_PACKET_SIZE {
        return None;
    }

    let header = TsPacketHeader::parse(packet)?;
    if !header.has_adaptation_field() {
        return None;
    }

    let adapt_length = packet[4] as usize;
    if adapt_length < 7 {
        // Need at least 1 byte flags + 6 bytes PCR
        return None;
    }

    // Adaptation field flags at byte 5
    let flags = packet[5];
    let pcr_flag = (flags & 0x10) != 0;
    if !pcr_flag {
        return None;
    }

    // PCR starts at byte 6 (6 bytes)
    if packet.len() < 12 {
        return None;
    }

    let b = &packet[6..12];
    let base = (u64::from(b[0]) << 25)
        | (u64::from(b[1]) << 17)
        | (u64::from(b[2]) << 9)
        | (u64::from(b[3]) << 1)
        | (u64::from(b[4]) >> 7);

    let extension = (u16::from(b[4] & 0x01) << 8) | u16::from(b[5]);

    Some(Pcr { base, extension })
}

// ---------------------------------------------------------------------------
// PTS tracker — detect PTS discontinuities
// ---------------------------------------------------------------------------

/// Tracks PTS per PID and detects discontinuities (gaps > 1 second).
#[derive(Debug)]
pub struct PtsTracker {
    /// PID → last PTS value (in 90 kHz ticks).
    last_pts: HashMap<u16, u64>,
    /// Total PTS discontinuity count.
    pub discontinuities: u64,
}

/// 1 second in 90 kHz PTS ticks.
const PTS_ONE_SECOND: u64 = 90_000;

/// Full PCR range in 27 MHz ticks (33-bit base * 300 + max 9-bit extension).
const PCR_WRAP: u64 = (1u64 << 33) * 300 + 299;

impl PtsTracker {
    pub fn new() -> Self {
        Self {
            last_pts: HashMap::new(),
            discontinuities: 0,
        }
    }

    /// Record a PTS for a given PID. Returns `true` if a discontinuity (>1s gap) was detected.
    pub fn record(&mut self, pid: u16, pts: u64) -> bool {
        if let Some(&prev) = self.last_pts.get(&pid) {
            // PTS is 33 bits and wraps around. Calculate forward distance.
            let diff = if pts >= prev {
                pts - prev
            } else {
                // Wrap-around: max 33-bit value is (1 << 33) - 1
                ((1u64 << 33) - prev) + pts
            };

            self.last_pts.insert(pid, pts);

            if diff > PTS_ONE_SECOND {
                self.discontinuities += 1;
                return true;
            }
        } else {
            self.last_pts.insert(pid, pts);
        }

        false
    }
}

// ---------------------------------------------------------------------------
// PCR jitter tracker
// ---------------------------------------------------------------------------

/// Tracks PCR values and calculates jitter (deviation from expected).
#[derive(Debug)]
pub struct PcrTracker {
    /// Last PCR value in 27 MHz ticks.
    last_pcr: Option<u64>,
    /// Last PCR wall-clock instant.
    last_instant: Option<std::time::Instant>,
    /// Accumulated jitter samples (absolute deviation in 27 MHz ticks).
    pub jitter_sum: u64,
    /// Number of jitter samples.
    pub jitter_count: u64,
    /// Maximum jitter observed (27 MHz ticks).
    pub jitter_max: u64,
}

impl PcrTracker {
    pub fn new() -> Self {
        Self {
            last_pcr: None,
            last_instant: None,
            jitter_sum: 0,
            jitter_count: 0,
            jitter_max: 0,
        }
    }

    /// Record a PCR observation. Returns the jitter sample in 27 MHz ticks, if one was computed.
    pub fn record(&mut self, pcr: &Pcr) -> Option<u64> {
        let now = std::time::Instant::now();
        let pcr_27mhz = pcr.as_27mhz();

        let result = if let (Some(prev_pcr), Some(prev_instant)) =
            (self.last_pcr, self.last_instant)
        {
            let wall_elapsed = now.duration_since(prev_instant);
            let wall_ticks = (wall_elapsed.as_nanos() as u64 * 27) / 1_000; // ns → 27 MHz ticks
            let pcr_elapsed = if pcr_27mhz >= prev_pcr {
                pcr_27mhz - prev_pcr
            } else {
                // PCR wrap-around: 42-bit range in 27 MHz ticks
                (PCR_WRAP - prev_pcr) + pcr_27mhz
            };

            let jitter = pcr_elapsed.abs_diff(wall_ticks);

            self.jitter_sum += jitter;
            self.jitter_count += 1;
            if jitter > self.jitter_max {
                self.jitter_max = jitter;
            }

            Some(jitter)
        } else {
            None
        };

        self.last_pcr = Some(pcr_27mhz);
        self.last_instant = Some(now);
        result
    }

    /// Average jitter in 27 MHz ticks.
    pub fn average_jitter(&self) -> f64 {
        if self.jitter_count == 0 {
            0.0
        } else {
            self.jitter_sum as f64 / self.jitter_count as f64
        }
    }
}

// ---------------------------------------------------------------------------
// TsInspector — top-level struct that processes chunks
// ---------------------------------------------------------------------------

/// Top-level TS inspector. Processes chunks of TS data, extracts PAT/PMT,
/// tracks continuity, PTS, and PCR.
#[derive(Debug)]
pub struct TsInspector {
    /// PMT PID discovered from PAT (first program).
    pub pmt_pid: Option<u16>,
    /// Parsed PMT info (stream list, PCR PID).
    pub pmt_info: Option<PmtInfo>,
    /// Continuity counter tracker.
    pub continuity: ContinuityTracker,
    /// PTS tracker for video PID.
    pub pts_tracker: PtsTracker,
    /// PCR jitter tracker.
    pub pcr_tracker: PcrTracker,
    /// Video PID (derived from PMT).
    pub video_pid: Option<u16>,
}

impl TsInspector {
    pub fn new() -> Self {
        Self {
            pmt_pid: None,
            pmt_info: None,
            continuity: ContinuityTracker::new(),
            pts_tracker: PtsTracker::new(),
            pcr_tracker: PcrTracker::new(),
            video_pid: None,
        }
    }

    /// Process a chunk of TS data. The chunk must be aligned to 188-byte boundaries.
    /// Any trailing bytes that don't form a complete packet are ignored.
    pub fn process_chunk(&mut self, data: &[u8]) {
        let mut offset = 0;

        while offset + TS_PACKET_SIZE <= data.len() {
            let packet = &data[offset..offset + TS_PACKET_SIZE];
            self.process_packet(packet);
            offset += TS_PACKET_SIZE;
        }
    }

    /// Process a single 188-byte TS packet.
    fn process_packet(&mut self, packet: &[u8]) {
        let header = match TsPacketHeader::parse(packet) {
            Some(h) => h,
            None => return,
        };

        // Continuity tracking
        self.continuity.check(&header);

        // PCR extraction from adaptation field
        if header.has_adaptation_field() {
            if let Some(pcr) = extract_pcr(packet) {
                self.pcr_tracker.record(&pcr);
            }
        }

        // Only process packets with payload
        if !header.has_payload() {
            return;
        }

        let payload_start = self.payload_offset(packet, &header);
        if payload_start >= TS_PACKET_SIZE {
            return;
        }

        let payload = &packet[payload_start..];

        // PAT parsing
        if header.pid == PAT_PID && header.pusi {
            self.handle_pat(payload);
        }

        // PMT parsing
        if let Some(pmt_pid) = self.pmt_pid {
            if header.pid == pmt_pid && header.pusi {
                self.handle_pmt(payload);
            }
        }

        // PTS extraction for video PID
        if let Some(vid_pid) = self.video_pid {
            if header.pid == vid_pid && header.pusi {
                // Skip pointer field for PES — PES packets don't use the pointer field
                // mechanism the same way as PSI. The PES start code follows directly.
                if let Some(pts) = extract_pts(payload) {
                    self.pts_tracker.record(vid_pid, pts);
                }
            }
        }
    }

    /// Calculate the byte offset where payload data starts within the packet.
    fn payload_offset(&self, packet: &[u8], header: &TsPacketHeader) -> usize {
        let mut offset = 4; // skip 4-byte header

        if header.has_adaptation_field() {
            if offset < packet.len() {
                let adapt_len = packet[offset] as usize;
                offset += 1 + adapt_len;
            } else {
                return TS_PACKET_SIZE; // invalid
            }
        }

        offset
    }

    /// Handle a PAT packet: extract first program's PMT PID.
    fn handle_pat(&mut self, payload: &[u8]) {
        // PSI sections have a pointer field first
        if payload.is_empty() {
            return;
        }
        let pointer = payload[0] as usize;
        let section_start = 1 + pointer;
        if section_start >= payload.len() {
            return;
        }

        let entries = parse_pat(&payload[section_start..]);
        if let Some(first) = entries.first() {
            self.pmt_pid = Some(first.pmt_pid);
        }
    }

    /// Handle a PMT packet: extract stream info.
    fn handle_pmt(&mut self, payload: &[u8]) {
        if payload.is_empty() {
            return;
        }
        let pointer = payload[0] as usize;
        let section_start = 1 + pointer;
        if section_start >= payload.len() {
            return;
        }

        if let Some(info) = parse_pmt(&payload[section_start..]) {
            // Find the video PID
            self.video_pid = info
                .streams
                .iter()
                .find(|s| is_video_stream_type(s.stream_type))
                .map(|s| s.elementary_pid);

            self.pmt_info = Some(info);
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Helpers to build TS packets for testing
    // -----------------------------------------------------------------------

    /// Build a minimal 188-byte TS packet with the given header fields.
    fn make_packet(pid: u16, continuity: u8, adaptation: u8, payload: &[u8]) -> Vec<u8> {
        let mut pkt = vec![0u8; TS_PACKET_SIZE];
        pkt[0] = SYNC_BYTE;
        pkt[1] = ((pid >> 8) & 0x1F) as u8;
        pkt[2] = (pid & 0xFF) as u8;
        pkt[3] = (adaptation << 4) | (continuity & 0x0F);

        let start = 4;
        let copy_len = payload.len().min(TS_PACKET_SIZE - start);
        pkt[start..start + copy_len].copy_from_slice(&payload[..copy_len]);

        pkt
    }

    /// Build a TS packet with PUSI set.
    fn make_packet_pusi(pid: u16, continuity: u8, adaptation: u8, payload: &[u8]) -> Vec<u8> {
        let mut pkt = make_packet(pid, continuity, adaptation, payload);
        pkt[1] |= 0x40; // set PUSI
        pkt
    }

    /// Build a TS packet with an adaptation field containing a PCR.
    fn make_pcr_packet(pid: u16, continuity: u8, pcr_base: u64, pcr_ext: u16) -> Vec<u8> {
        let mut pkt = vec![0u8; TS_PACKET_SIZE];
        pkt[0] = SYNC_BYTE;
        pkt[1] = ((pid >> 8) & 0x1F) as u8;
        pkt[2] = (pid & 0xFF) as u8;
        // adaptation=0b11 (adaptation + payload), continuity
        pkt[3] = 0x30 | (continuity & 0x0F);

        // Adaptation field
        pkt[4] = 7; // adaptation field length (1 flag + 6 PCR)
        pkt[5] = 0x10; // PCR flag set

        // PCR (6 bytes at offset 6)
        pkt[6] = ((pcr_base >> 25) & 0xFF) as u8;
        pkt[7] = ((pcr_base >> 17) & 0xFF) as u8;
        pkt[8] = ((pcr_base >> 9) & 0xFF) as u8;
        pkt[9] = ((pcr_base >> 1) & 0xFF) as u8;
        pkt[10] = (((pcr_base & 0x01) << 7) as u8) | 0x7E | ((pcr_ext >> 8) as u8 & 0x01);
        pkt[11] = (pcr_ext & 0xFF) as u8;

        pkt
    }

    /// Build a minimal PAT section as raw bytes (table_id=0x00).
    /// Single program mapping: program_number → pmt_pid.
    fn make_pat_section(program_number: u16, pmt_pid: u16) -> Vec<u8> {
        // table_id(1) + section_syntax_indicator + section_length(2) + transport_stream_id(2) + version(1) + section_number(1) + last_section(1) + program_loop(4) + CRC(4)
        let section_length: u16 = 5 + 4 + 4; // header_after_length + 1 program entry + CRC
        let mut section = Vec::new();
        section.push(0x00); // table_id
        section.push(0xB0 | ((section_length >> 8) as u8 & 0x0F));
        section.push((section_length & 0xFF) as u8);
        section.push(0x00); // transport_stream_id high
        section.push(0x01); // transport_stream_id low
        section.push(0xC1); // version=0, current=1
        section.push(0x00); // section_number
        section.push(0x00); // last_section_number
        // Program entry
        section.push((program_number >> 8) as u8);
        section.push((program_number & 0xFF) as u8);
        section.push(0xE0 | ((pmt_pid >> 8) as u8 & 0x1F));
        section.push((pmt_pid & 0xFF) as u8);
        // CRC32 (dummy — we don't validate it)
        section.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        section
    }

    /// Build a minimal PMT section.
    fn make_pmt_section(pcr_pid: u16, streams: &[(u8, u16)]) -> Vec<u8> {
        // Calculate lengths
        let stream_loop_len: usize = streams.len() * 5; // each stream: type(1) + pid(2) + es_info_length(2)
        let section_length: u16 = (9 + stream_loop_len + 4) as u16; // header + streams + CRC

        let mut section = Vec::new();
        section.push(0x02); // table_id
        section.push(0xB0 | ((section_length >> 8) as u8 & 0x0F));
        section.push((section_length & 0xFF) as u8);
        section.push(0x00); // program_number high
        section.push(0x01); // program_number low
        section.push(0xC1); // version=0, current=1
        section.push(0x00); // section_number
        section.push(0x00); // last_section_number
        section.push(0xE0 | ((pcr_pid >> 8) as u8 & 0x1F));
        section.push((pcr_pid & 0xFF) as u8);
        section.push(0xF0); // program_info_length high (0)
        section.push(0x00); // program_info_length low (0)

        for &(stream_type, pid) in streams {
            section.push(stream_type);
            section.push(0xE0 | ((pid >> 8) as u8 & 0x1F));
            section.push((pid & 0xFF) as u8);
            section.push(0xF0); // ES_info_length high (0)
            section.push(0x00); // ES_info_length low (0)
        }

        // CRC32 (dummy)
        section.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        section
    }

    /// Build PES header bytes with a PTS value.
    fn make_pes_with_pts(pts: u64) -> Vec<u8> {
        let mut pes = Vec::new();
        // Start code
        pes.push(0x00);
        pes.push(0x00);
        pes.push(0x01);
        pes.push(0xE0); // stream_id: video
        pes.push(0x00); // PES packet length high
        pes.push(0x00); // PES packet length low (can be 0 for video)
        pes.push(0x80); // marker bits
        pes.push(0x80); // PTS only (bits 7-6 = 10)
        pes.push(0x05); // PES header data length

        // 5-byte PTS encoding
        let b0 = 0x21 | (((pts >> 30) & 0x07) << 1) as u8;
        let b1 = ((pts >> 22) & 0xFF) as u8;
        let b2 = (((pts >> 15) & 0x7F) << 1) as u8 | 0x01;
        let b3 = ((pts >> 7) & 0xFF) as u8;
        let b4 = (((pts & 0x7F) << 1) | 0x01) as u8;

        pes.push(b0);
        pes.push(b1);
        pes.push(b2);
        pes.push(b3);
        pes.push(b4);

        pes
    }

    // -----------------------------------------------------------------------
    // Header parsing tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_header_parse_basic() {
        // PID=0x0100, CC=5, adaptation=01 (payload only)
        let packet = make_packet(0x0100, 5, 0b01, &[]);
        let header = TsPacketHeader::parse(&packet).unwrap();

        assert_eq!(header.sync_byte, 0x47);
        assert!(!header.tei);
        assert!(!header.pusi);
        assert_eq!(header.pid, 0x0100);
        assert_eq!(header.scrambling, 0);
        assert_eq!(header.adaptation, 0b01);
        assert_eq!(header.continuity, 5);
        assert!(header.has_payload());
        assert!(!header.has_adaptation_field());
    }

    #[test]
    fn test_header_parse_with_pusi_and_tei() {
        let mut packet = make_packet(0x1234, 0x0F, 0b01, &[]);
        packet[1] |= 0xC0; // set TEI + PUSI

        let header = TsPacketHeader::parse(&packet).unwrap();

        assert!(header.tei);
        assert!(header.pusi);
        assert_eq!(header.pid, 0x1234);
        assert_eq!(header.continuity, 0x0F);
    }

    #[test]
    fn test_header_parse_adaptation_field() {
        let packet = make_packet(0x0050, 3, 0b10, &[]); // adaptation only
        let header = TsPacketHeader::parse(&packet).unwrap();

        assert!(header.has_adaptation_field());
        assert!(!header.has_payload());
        assert_eq!(header.adaptation, 0b10);
    }

    #[test]
    fn test_header_parse_adaptation_and_payload() {
        let packet = make_packet(0x0050, 3, 0b11, &[]); // both
        let header = TsPacketHeader::parse(&packet).unwrap();

        assert!(header.has_adaptation_field());
        assert!(header.has_payload());
    }

    #[test]
    fn test_header_parse_null_pid() {
        let packet = make_packet(NULL_PID, 0, 0b01, &[]);
        let header = TsPacketHeader::parse(&packet).unwrap();
        assert_eq!(header.pid, NULL_PID);
    }

    #[test]
    fn test_header_parse_invalid_sync() {
        let mut packet = make_packet(0x0100, 0, 0b01, &[]);
        packet[0] = 0x00; // wrong sync byte
        assert!(TsPacketHeader::parse(&packet).is_none());
    }

    #[test]
    fn test_header_parse_too_short() {
        assert!(TsPacketHeader::parse(&[0x47, 0x00, 0x00]).is_none());
        assert!(TsPacketHeader::parse(&[]).is_none());
    }

    #[test]
    fn test_header_parse_scrambling() {
        let mut packet = make_packet(0x0100, 0, 0b01, &[]);
        packet[3] |= 0xC0; // scrambling = 0b11
        let header = TsPacketHeader::parse(&packet).unwrap();
        assert_eq!(header.scrambling, 0b11);
    }

    // -----------------------------------------------------------------------
    // PAT parsing tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_pat_parse_single_program() {
        let section = make_pat_section(1, 0x0100);
        let entries = parse_pat(&section);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].program_number, 1);
        assert_eq!(entries[0].pmt_pid, 0x0100);
    }

    #[test]
    fn test_pat_parse_skips_network_pid() {
        // program_number=0 is the network PID entry, should be skipped
        let mut section = make_pat_section(0, 0x0010);
        // Extend section_length to add a second entry
        // Rebuild with two entries: program 0 (skip) + program 1 (keep)
        let section_length: u16 = 5 + 8 + 4; // 2 entries
        section.clear();
        section.push(0x00);
        section.push(0xB0 | ((section_length >> 8) as u8 & 0x0F));
        section.push((section_length & 0xFF) as u8);
        section.push(0x00);
        section.push(0x01);
        section.push(0xC1);
        section.push(0x00);
        section.push(0x00);
        // Entry 1: program 0 → PID 0x0010
        section.extend_from_slice(&[0x00, 0x00, 0xE0, 0x10]);
        // Entry 2: program 1 → PID 0x0100
        section.extend_from_slice(&[0x00, 0x01, 0xE1, 0x00]);
        section.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);

        let entries = parse_pat(&section);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].program_number, 1);
        assert_eq!(entries[0].pmt_pid, 0x0100);
    }

    #[test]
    fn test_pat_parse_empty() {
        assert!(parse_pat(&[]).is_empty());
        assert!(parse_pat(&[0x00]).is_empty());
    }

    #[test]
    fn test_pat_parse_wrong_table_id() {
        let mut section = make_pat_section(1, 0x0100);
        section[0] = 0x01; // wrong table_id
        assert!(parse_pat(&section).is_empty());
    }

    // -----------------------------------------------------------------------
    // PMT parsing tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_pmt_parse_h264_aac() {
        let section = make_pmt_section(0x0100, &[
            (STREAM_TYPE_H264, 0x0100),
            (STREAM_TYPE_AAC, 0x0101),
        ]);

        let info = parse_pmt(&section).unwrap();

        assert_eq!(info.pcr_pid, 0x0100);
        assert_eq!(info.streams.len(), 2);
        assert_eq!(info.streams[0].stream_type, STREAM_TYPE_H264);
        assert_eq!(info.streams[0].elementary_pid, 0x0100);
        assert_eq!(info.streams[1].stream_type, STREAM_TYPE_AAC);
        assert_eq!(info.streams[1].elementary_pid, 0x0101);
    }

    #[test]
    fn test_pmt_parse_hevc_ac3() {
        let section = make_pmt_section(0x0200, &[
            (STREAM_TYPE_HEVC, 0x0200),
            (STREAM_TYPE_AC3, 0x0201),
        ]);

        let info = parse_pmt(&section).unwrap();

        assert_eq!(info.pcr_pid, 0x0200);
        assert_eq!(info.streams.len(), 2);
        assert_eq!(info.streams[0].stream_type, STREAM_TYPE_HEVC);
        assert_eq!(info.streams[1].stream_type, STREAM_TYPE_AC3);
    }

    #[test]
    fn test_pmt_parse_wrong_table_id() {
        let mut section = make_pmt_section(0x0100, &[(STREAM_TYPE_H264, 0x0100)]);
        section[0] = 0x00; // wrong table_id
        assert!(parse_pmt(&section).is_none());
    }

    #[test]
    fn test_pmt_parse_empty() {
        assert!(parse_pmt(&[]).is_none());
    }

    // -----------------------------------------------------------------------
    // Stream type classification tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_stream_type_classification() {
        assert!(is_video_stream_type(STREAM_TYPE_H264));
        assert!(is_video_stream_type(STREAM_TYPE_HEVC));
        assert!(is_video_stream_type(STREAM_TYPE_MPEG2_VIDEO));
        assert!(!is_video_stream_type(STREAM_TYPE_AAC));

        assert!(is_audio_stream_type(STREAM_TYPE_AAC));
        assert!(is_audio_stream_type(STREAM_TYPE_AC3));
        assert!(is_audio_stream_type(STREAM_TYPE_EAC3));
        assert!(is_audio_stream_type(STREAM_TYPE_MP2_A));
        assert!(is_audio_stream_type(STREAM_TYPE_MP2_B));
        assert!(!is_audio_stream_type(STREAM_TYPE_H264));
    }

    #[test]
    fn test_stream_type_names() {
        assert_eq!(stream_type_name(STREAM_TYPE_H264), "H.264");
        assert_eq!(stream_type_name(STREAM_TYPE_HEVC), "HEVC");
        assert_eq!(stream_type_name(STREAM_TYPE_MPEG2_VIDEO), "MPEG-2");
        assert_eq!(stream_type_name(STREAM_TYPE_AAC), "AAC");
        assert_eq!(stream_type_name(STREAM_TYPE_AC3), "AC3");
        assert_eq!(stream_type_name(STREAM_TYPE_EAC3), "EAC3");
        assert_eq!(stream_type_name(STREAM_TYPE_MP2_A), "MP2");
        assert_eq!(stream_type_name(STREAM_TYPE_MP2_B), "MP2");
        assert_eq!(stream_type_name(0xFF), "Unknown");
    }

    // -----------------------------------------------------------------------
    // Continuity counter tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_continuity_normal_sequence() {
        let mut tracker = ContinuityTracker::new();

        for cc in 0..16u8 {
            let pkt = make_packet(0x0100, cc, 0b01, &[]);
            let header = TsPacketHeader::parse(&pkt).unwrap();
            assert!(!tracker.check(&header), "CC {} should not be a discontinuity", cc);
        }

        assert_eq!(tracker.errors, 0);
    }

    #[test]
    fn test_continuity_wrap_around() {
        let mut tracker = ContinuityTracker::new();

        // Start at 14
        let pkt = make_packet(0x0100, 14, 0b01, &[]);
        tracker.check(&TsPacketHeader::parse(&pkt).unwrap());

        // 15
        let pkt = make_packet(0x0100, 15, 0b01, &[]);
        assert!(!tracker.check(&TsPacketHeader::parse(&pkt).unwrap()));

        // Wrap to 0
        let pkt = make_packet(0x0100, 0, 0b01, &[]);
        assert!(!tracker.check(&TsPacketHeader::parse(&pkt).unwrap()));

        assert_eq!(tracker.errors, 0);
    }

    #[test]
    fn test_continuity_gap_detected() {
        let mut tracker = ContinuityTracker::new();

        let pkt = make_packet(0x0100, 3, 0b01, &[]);
        tracker.check(&TsPacketHeader::parse(&pkt).unwrap());

        // Jump from 3 to 7 (skip 4, 5, 6)
        let pkt = make_packet(0x0100, 7, 0b01, &[]);
        assert!(tracker.check(&TsPacketHeader::parse(&pkt).unwrap()));

        assert_eq!(tracker.errors, 1);
    }

    #[test]
    fn test_continuity_duplicate_not_error() {
        let mut tracker = ContinuityTracker::new();

        let pkt = make_packet(0x0100, 5, 0b01, &[]);
        tracker.check(&TsPacketHeader::parse(&pkt).unwrap());

        // Duplicate: same CC
        let pkt = make_packet(0x0100, 5, 0b01, &[]);
        assert!(!tracker.check(&TsPacketHeader::parse(&pkt).unwrap()));

        assert_eq!(tracker.errors, 0);
    }

    #[test]
    fn test_continuity_skips_null_pid() {
        let mut tracker = ContinuityTracker::new();

        let pkt = make_packet(NULL_PID, 0, 0b01, &[]);
        assert!(!tracker.check(&TsPacketHeader::parse(&pkt).unwrap()));

        let pkt = make_packet(NULL_PID, 5, 0b01, &[]);
        assert!(!tracker.check(&TsPacketHeader::parse(&pkt).unwrap()));

        assert_eq!(tracker.errors, 0);
    }

    #[test]
    fn test_continuity_skips_adaptation_only() {
        let mut tracker = ContinuityTracker::new();

        // adaptation=0b10 means adaptation only, no payload
        let pkt = make_packet(0x0100, 0, 0b10, &[]);
        assert!(!tracker.check(&TsPacketHeader::parse(&pkt).unwrap()));

        let pkt = make_packet(0x0100, 5, 0b10, &[]);
        assert!(!tracker.check(&TsPacketHeader::parse(&pkt).unwrap()));

        assert_eq!(tracker.errors, 0);
    }

    #[test]
    fn test_continuity_independent_pids() {
        let mut tracker = ContinuityTracker::new();

        // PID 0x100: CC=0
        let pkt = make_packet(0x0100, 0, 0b01, &[]);
        tracker.check(&TsPacketHeader::parse(&pkt).unwrap());

        // PID 0x101: CC=0 (independent)
        let pkt = make_packet(0x0101, 0, 0b01, &[]);
        tracker.check(&TsPacketHeader::parse(&pkt).unwrap());

        // PID 0x100: CC=1 (sequential)
        let pkt = make_packet(0x0100, 1, 0b01, &[]);
        assert!(!tracker.check(&TsPacketHeader::parse(&pkt).unwrap()));

        // PID 0x101: CC=1 (sequential)
        let pkt = make_packet(0x0101, 1, 0b01, &[]);
        assert!(!tracker.check(&TsPacketHeader::parse(&pkt).unwrap()));

        assert_eq!(tracker.errors, 0);
    }

    // -----------------------------------------------------------------------
    // PTS extraction tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_pts_extraction_known_value() {
        let pts_value: u64 = 126000; // 1.4 seconds at 90kHz
        let pes = make_pes_with_pts(pts_value);
        let extracted = extract_pts(&pes).unwrap();
        assert_eq!(extracted, pts_value);
    }

    #[test]
    fn test_pts_extraction_zero() {
        let pes = make_pes_with_pts(0);
        let extracted = extract_pts(&pes).unwrap();
        assert_eq!(extracted, 0);
    }

    #[test]
    fn test_pts_extraction_large_value() {
        // Near 33-bit max
        let pts_value: u64 = (1u64 << 33) - 1;
        let pes = make_pes_with_pts(pts_value);
        let extracted = extract_pts(&pes).unwrap();
        assert_eq!(extracted, pts_value);
    }

    #[test]
    fn test_pts_extraction_no_pts_flag() {
        let mut pes = make_pes_with_pts(1000);
        pes[7] = 0x00; // clear PTS/DTS flags
        assert!(extract_pts(&pes).is_none());
    }

    #[test]
    fn test_pts_extraction_too_short() {
        assert!(extract_pts(&[]).is_none());
        assert!(extract_pts(&[0x00, 0x00, 0x01]).is_none());
    }

    #[test]
    fn test_pts_extraction_wrong_start_code() {
        let mut pes = make_pes_with_pts(1000);
        pes[2] = 0x00; // break start code
        assert!(extract_pts(&pes).is_none());
    }

    // -----------------------------------------------------------------------
    // PTS tracker discontinuity tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_pts_tracker_normal_increment() {
        let mut tracker = PtsTracker::new();

        // Consecutive frames at ~30fps: PTS increments by 3003 (90000/29.97)
        assert!(!tracker.record(0x100, 0));
        assert!(!tracker.record(0x100, 3003));
        assert!(!tracker.record(0x100, 6006));

        assert_eq!(tracker.discontinuities, 0);
    }

    #[test]
    fn test_pts_tracker_discontinuity() {
        let mut tracker = PtsTracker::new();

        tracker.record(0x100, 90_000); // 1 second
        // Jump >1 second
        assert!(tracker.record(0x100, 90_000 + PTS_ONE_SECOND + 1));

        assert_eq!(tracker.discontinuities, 1);
    }

    #[test]
    fn test_pts_tracker_wrap_around() {
        let mut tracker = PtsTracker::new();

        let max_pts = (1u64 << 33) - 1;
        tracker.record(0x100, max_pts - 1000);
        // Wrap around by a small amount — not a discontinuity
        assert!(!tracker.record(0x100, 1000));

        assert_eq!(tracker.discontinuities, 0);
    }

    #[test]
    fn test_pts_tracker_independent_pids() {
        let mut tracker = PtsTracker::new();

        tracker.record(0x100, 0);
        // Different PID — first observation, not a discontinuity
        assert!(!tracker.record(0x101, 5_000_000));

        assert_eq!(tracker.discontinuities, 0);
    }

    // -----------------------------------------------------------------------
    // PCR extraction tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_pcr_extraction() {
        let pcr_base: u64 = 123456789;
        let pcr_ext: u16 = 100;

        let pkt = make_pcr_packet(0x0100, 0, pcr_base, pcr_ext);
        let pcr = extract_pcr(&pkt).unwrap();

        assert_eq!(pcr.base, pcr_base);
        assert_eq!(pcr.extension, pcr_ext);
        assert_eq!(pcr.as_27mhz(), pcr_base * 300 + pcr_ext as u64);
    }

    #[test]
    fn test_pcr_extraction_zero() {
        let pkt = make_pcr_packet(0x0100, 0, 0, 0);
        let pcr = extract_pcr(&pkt).unwrap();
        assert_eq!(pcr.base, 0);
        assert_eq!(pcr.extension, 0);
    }

    #[test]
    fn test_pcr_extraction_no_adaptation() {
        // Payload only, no adaptation field
        let pkt = make_packet(0x0100, 0, 0b01, &[]);
        assert!(extract_pcr(&pkt).is_none());
    }

    #[test]
    fn test_pcr_extraction_no_pcr_flag() {
        let mut pkt = make_pcr_packet(0x0100, 0, 100, 0);
        pkt[5] = 0x00; // clear PCR flag
        assert!(extract_pcr(&pkt).is_none());
    }

    // -----------------------------------------------------------------------
    // PCR tracker tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_pcr_tracker_first_sample_no_jitter() {
        let mut tracker = PcrTracker::new();
        let pcr = Pcr { base: 90000, extension: 0 };
        assert!(tracker.record(&pcr).is_none());
    }

    #[test]
    fn test_pcr_tracker_jitter_computed() {
        let mut tracker = PcrTracker::new();

        let pcr1 = Pcr { base: 90000, extension: 0 };
        tracker.record(&pcr1);

        // Simulate a tiny wall-clock delay then record a second PCR
        let pcr2 = Pcr { base: 90000 + 2700, extension: 0 }; // 30ms in 90kHz → 2700
        let jitter = tracker.record(&pcr2);
        assert!(jitter.is_some());
        assert_eq!(tracker.jitter_count, 1);
    }

    #[test]
    fn test_pcr_tracker_wrap_around() {
        let mut tracker = PcrTracker::new();

        // Place PCR near the 42-bit wrap point.
        // PCR base is 33 bits; max base = (1 << 33) - 1.
        let near_max_base: u64 = (1u64 << 33) - 1;
        let pcr1 = Pcr { base: near_max_base, extension: 0 };
        tracker.record(&pcr1);

        // Wrap around to a small PCR value. The forward distance in 27 MHz
        // ticks is small, so jitter should remain reasonable (not near u64::MAX).
        let pcr2 = Pcr { base: 1000, extension: 0 };
        let jitter = tracker.record(&pcr2);

        assert!(jitter.is_some());
        // The PCR elapsed should be the small forward distance across the wrap,
        // not a huge value near u64::MAX.
        let pcr1_27mhz = near_max_base * 300;
        let pcr2_27mhz = 1000u64 * 300;
        let expected_elapsed = (PCR_WRAP - pcr1_27mhz) + pcr2_27mhz;
        // Jitter = |pcr_elapsed - wall_ticks|. Wall time is near-zero so
        // jitter ≈ expected_elapsed, which is well below u64::MAX.
        assert!(
            tracker.jitter_max < expected_elapsed * 2,
            "jitter_max {} should be bounded, not near u64::MAX",
            tracker.jitter_max
        );
    }

    // -----------------------------------------------------------------------
    // TsInspector integration tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_inspector_discovers_pmt_pid_from_pat() {
        let mut inspector = TsInspector::new();

        let pat_section = make_pat_section(1, 0x0100);
        // Wrap in a TS packet with pointer=0
        let mut payload = vec![0x00]; // pointer field
        payload.extend_from_slice(&pat_section);
        let pkt = make_packet_pusi(PAT_PID, 0, 0b01, &payload);

        inspector.process_chunk(&pkt);

        assert_eq!(inspector.pmt_pid, Some(0x0100));
    }

    #[test]
    fn test_inspector_discovers_streams_from_pmt() {
        let mut inspector = TsInspector::new();

        // First, feed PAT to discover PMT PID
        let pat_section = make_pat_section(1, 0x0100);
        let mut pat_payload = vec![0x00];
        pat_payload.extend_from_slice(&pat_section);
        let pat_pkt = make_packet_pusi(PAT_PID, 0, 0b01, &pat_payload);

        // Then, feed PMT on PID 0x0100
        let pmt_section = make_pmt_section(0x0200, &[
            (STREAM_TYPE_H264, 0x0200),
            (STREAM_TYPE_AAC, 0x0201),
        ]);
        let mut pmt_payload = vec![0x00];
        pmt_payload.extend_from_slice(&pmt_section);
        let pmt_pkt = make_packet_pusi(0x0100, 0, 0b01, &pmt_payload);

        // Build chunk with both packets
        let mut chunk = Vec::new();
        chunk.extend_from_slice(&pat_pkt);
        chunk.extend_from_slice(&pmt_pkt);

        inspector.process_chunk(&chunk);

        assert_eq!(inspector.pmt_pid, Some(0x0100));
        let pmt = inspector.pmt_info.as_ref().unwrap();
        assert_eq!(pmt.pcr_pid, 0x0200);
        assert_eq!(pmt.streams.len(), 2);
        assert_eq!(inspector.video_pid, Some(0x0200));
    }

    #[test]
    fn test_inspector_tracks_continuity_errors() {
        let mut inspector = TsInspector::new();

        // CC=0, then CC=5 (gap)
        let pkt1 = make_packet(0x0100, 0, 0b01, &[]);
        let pkt2 = make_packet(0x0100, 5, 0b01, &[]);

        let mut chunk = Vec::new();
        chunk.extend_from_slice(&pkt1);
        chunk.extend_from_slice(&pkt2);

        inspector.process_chunk(&chunk);

        assert_eq!(inspector.continuity.errors, 1);
    }

    #[test]
    fn test_inspector_extracts_pts() {
        let mut inspector = TsInspector::new();

        // Set up PAT + PMT so inspector knows the video PID
        let pat_section = make_pat_section(1, 0x0100);
        let mut pat_payload = vec![0x00];
        pat_payload.extend_from_slice(&pat_section);
        let pat_pkt = make_packet_pusi(PAT_PID, 0, 0b01, &pat_payload);

        let pmt_section = make_pmt_section(0x0200, &[(STREAM_TYPE_H264, 0x0200)]);
        let mut pmt_payload = vec![0x00];
        pmt_payload.extend_from_slice(&pmt_section);
        let pmt_pkt = make_packet_pusi(0x0100, 0, 0b01, &pmt_payload);

        // Now a video PES packet with PTS
        let pts_value: u64 = 90_000; // 1 second
        let pes = make_pes_with_pts(pts_value);
        let video_pkt = make_packet_pusi(0x0200, 0, 0b01, &pes);

        let mut chunk = Vec::new();
        chunk.extend_from_slice(&pat_pkt);
        chunk.extend_from_slice(&pmt_pkt);
        chunk.extend_from_slice(&video_pkt);

        inspector.process_chunk(&chunk);

        // The PTS should have been recorded
        assert_eq!(*inspector.pts_tracker.last_pts.get(&0x0200).unwrap(), pts_value);
    }

    #[test]
    fn test_inspector_extracts_pcr() {
        let mut inspector = TsInspector::new();

        let pcr_pkt = make_pcr_packet(0x0100, 0, 90_000, 0);
        inspector.process_chunk(&pcr_pkt);

        assert_eq!(inspector.pcr_tracker.last_pcr, Some(90_000 * 300));
    }

    #[test]
    fn test_inspector_handles_partial_packets() {
        let mut inspector = TsInspector::new();

        // Less than 188 bytes — should be silently ignored
        let data = vec![0x47; 100];
        inspector.process_chunk(&data);

        assert_eq!(inspector.continuity.errors, 0);
    }

    #[test]
    fn test_inspector_handles_multiple_chunks() {
        let mut inspector = TsInspector::new();

        // First chunk: PAT
        let pat_section = make_pat_section(1, 0x0100);
        let mut pat_payload = vec![0x00];
        pat_payload.extend_from_slice(&pat_section);
        let pat_pkt = make_packet_pusi(PAT_PID, 0, 0b01, &pat_payload);
        inspector.process_chunk(&pat_pkt);

        assert_eq!(inspector.pmt_pid, Some(0x0100));

        // Second chunk: PMT (on the PID we discovered)
        let pmt_section = make_pmt_section(0x0200, &[
            (STREAM_TYPE_HEVC, 0x0200),
            (STREAM_TYPE_EAC3, 0x0201),
        ]);
        let mut pmt_payload = vec![0x00];
        pmt_payload.extend_from_slice(&pmt_section);
        let pmt_pkt = make_packet_pusi(0x0100, 0, 0b01, &pmt_payload);
        inspector.process_chunk(&pmt_pkt);

        let pmt = inspector.pmt_info.as_ref().unwrap();
        assert_eq!(pmt.streams.len(), 2);
        assert_eq!(pmt.streams[0].stream_type, STREAM_TYPE_HEVC);
        assert_eq!(pmt.streams[1].stream_type, STREAM_TYPE_EAC3);
        assert_eq!(inspector.video_pid, Some(0x0200));
    }
}
