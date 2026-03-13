use std::collections::HashMap;
use std::time::Instant;

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
    /// PMT version number (5 bits, 0–31), extracted from the section header.
    pub version: u8,
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

    // Byte 5: reserved(2) | version_number(5) | current_next_indicator(1)
    let version = (section[5] >> 1) & 0x1F;

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

    Some(PmtInfo { pcr_pid, streams, version })
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

// ---------------------------------------------------------------------------
// Codec enums and stream info types
// ---------------------------------------------------------------------------

/// Video codec identified from PMT stream type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    Hevc,
    Mpeg2,
}

/// Audio codec identified from PMT stream type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodec {
    Aac,
    Ac3,
    Eac3,
    Mp2,
}

/// Video details parsed from codec-specific headers (SPS, etc.).
#[derive(Debug, Clone, PartialEq)]
pub struct VideoDetails {
    pub width: u16,
    pub height: u16,
    pub profile: u8,
    pub level: u8,
    pub chroma_format: u8,
    pub bit_depth_luma: u8,
    pub frame_mbs_only: bool,
    pub fps_num: Option<u32>,
    pub fps_den: Option<u32>,
}

/// Audio details parsed from codec-specific headers (ADTS, AC3 syncframe, etc.).
#[derive(Debug, Clone, PartialEq)]
pub struct AudioDetails {
    pub profile: u8,
    pub sample_rate: u32,
    pub channels: u8,
}

/// Combined stream information detected from a transport stream.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamInfo {
    pub video_codec: Option<VideoCodec>,
    pub video_details: Option<VideoDetails>,
    pub audio_codec: Option<AudioCodec>,
    pub audio_details: Option<AudioDetails>,
    pub subtitle_pids: Vec<u16>,
    pub total_pids: usize,
    pub detected_at: Instant,
}

// ---------------------------------------------------------------------------
// Bitstream reader for Exp-Golomb coded data
// ---------------------------------------------------------------------------

/// Reads individual bits and Exp-Golomb coded values from a byte slice.
struct BitstreamReader<'a> {
    data: &'a [u8],
    byte_offset: usize,
    bit_offset: u8,
}

impl<'a> BitstreamReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte_offset: 0,
            bit_offset: 0,
        }
    }

    /// Read a single bit. Returns `None` if no more data.
    fn read_bit(&mut self) -> Option<u8> {
        if self.byte_offset >= self.data.len() {
            return None;
        }
        let bit = (self.data[self.byte_offset] >> (7 - self.bit_offset)) & 1;
        self.bit_offset += 1;
        if self.bit_offset == 8 {
            self.bit_offset = 0;
            self.byte_offset += 1;
        }
        Some(bit)
    }

    /// Read `n` bits as a u32 (max 32 bits). Returns `None` if insufficient data.
    fn read_bits(&mut self, n: u8) -> Option<u32> {
        let mut val: u32 = 0;
        for _ in 0..n {
            val = (val << 1) | u32::from(self.read_bit()?);
        }
        Some(val)
    }

    /// Read an unsigned Exp-Golomb coded value (ue(v)).
    fn read_ue(&mut self) -> Option<u32> {
        let mut leading_zeros: u32 = 0;
        loop {
            let bit = self.read_bit()?;
            if bit == 1 {
                break;
            }
            leading_zeros += 1;
            if leading_zeros > 31 {
                return None; // malformed
            }
        }
        if leading_zeros == 0 {
            return Some(0);
        }
        let suffix = self.read_bits(leading_zeros as u8)?;
        Some((1u32 << leading_zeros) - 1 + suffix)
    }

    /// Read a signed Exp-Golomb coded value (se(v)).
    fn read_se(&mut self) -> Option<i32> {
        let code = self.read_ue()?;
        let val = ((code + 1) / 2) as i32;
        if code & 1 == 0 {
            Some(-val)
        } else {
            Some(val)
        }
    }

    /// Skip `n` bits.
    fn skip_bits(&mut self, n: u32) -> Option<()> {
        for _ in 0..n {
            self.read_bit()?;
        }
        Some(())
    }
}

// ---------------------------------------------------------------------------
// NAL unit helpers
// ---------------------------------------------------------------------------

/// Remove emulation prevention bytes (0x00 0x00 0x03 → 0x00 0x00).
/// H.264/HEVC NAL units insert 0x03 after 0x00 0x00 to avoid start code conflicts.
fn remove_emulation_prevention(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if i + 2 < data.len() && data[i] == 0x00 && data[i + 1] == 0x00 && data[i + 2] == 0x03 {
            out.push(0x00);
            out.push(0x00);
            i += 3; // skip the 0x03 byte
        } else {
            out.push(data[i]);
            i += 1;
        }
    }
    out
}

/// Find NAL units in a byte stream. Returns (nal_type, payload) pairs.
/// Searches for 3-byte start codes (0x00 0x00 0x01) or 4-byte (0x00 0x00 0x00 0x01).
fn find_nal_units(data: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut units = Vec::new();
    let mut i = 0;

    while i + 3 < data.len() {
        // Look for start code
        let (start_code_len, found) = if i + 3 < data.len()
            && data[i] == 0x00
            && data[i + 1] == 0x00
            && data[i + 2] == 0x01
        {
            (3, true)
        } else if i + 4 < data.len()
            && data[i] == 0x00
            && data[i + 1] == 0x00
            && data[i + 2] == 0x00
            && data[i + 3] == 0x01
        {
            (4, true)
        } else {
            (0, false)
        };

        if !found {
            i += 1;
            continue;
        }

        let nal_start = i + start_code_len;
        if nal_start >= data.len() {
            break;
        }

        // Find the end of this NAL (next start code or end of data)
        let mut end = nal_start + 1;
        while end + 2 < data.len() {
            if data[end] == 0x00 && data[end + 1] == 0x00 && (data[end + 2] == 0x01 || data[end + 2] == 0x00) {
                break;
            }
            end += 1;
        }
        if end + 2 >= data.len() {
            end = data.len();
        }

        let nal_header = data[nal_start];
        let nal_data = &data[nal_start..end];
        units.push((nal_header, nal_data.to_vec()));

        i = end;
    }

    units
}

// ---------------------------------------------------------------------------
// H.264 SPS parsing
// ---------------------------------------------------------------------------

/// Parsed H.264 Sequence Parameter Set.
#[derive(Debug, Clone, PartialEq)]
pub struct H264Info {
    pub profile_idc: u8,
    pub level_idc: u8,
    pub width: u16,
    pub height: u16,
    pub chroma_format: u8,
    pub bit_depth_luma: u8,
    pub num_ref_frames: u8,
    pub frame_mbs_only: bool,
    pub fps_num: Option<u32>,
    pub fps_den: Option<u32>,
}

/// Parse an H.264 SPS NAL unit. `nal_data` starts at the NAL header byte.
/// NAL unit type 7 = SPS.
pub fn parse_h264_sps(nal_data: &[u8]) -> Option<H264Info> {
    if nal_data.len() < 4 {
        return None;
    }

    // NAL header: forbidden_zero_bit(1) | nal_ref_idc(2) | nal_unit_type(5)
    let nal_type = nal_data[0] & 0x1F;
    if nal_type != 7 {
        return None;
    }

    // Remove emulation prevention bytes from the RBSP
    let rbsp = remove_emulation_prevention(&nal_data[1..]);
    let mut bs = BitstreamReader::new(&rbsp);

    // profile_idc (8 bits)
    let profile_idc = bs.read_bits(8)? as u8;

    // constraint_set flags (8 bits) — skip
    bs.skip_bits(8)?;

    // level_idc (8 bits)
    let level_idc = bs.read_bits(8)? as u8;

    // seq_parameter_set_id
    let _sps_id = bs.read_ue()?;

    let mut chroma_format: u8 = 1; // default 4:2:0
    let mut bit_depth_luma: u8 = 8;

    // High profile and above have extra fields
    if matches!(profile_idc, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        chroma_format = bs.read_ue()? as u8;
        if chroma_format == 3 {
            // separate_colour_plane_flag
            bs.skip_bits(1)?;
        }
        // bit_depth_luma_minus8
        bit_depth_luma = (bs.read_ue()? + 8) as u8;
        // bit_depth_chroma_minus8
        let _bit_depth_chroma = bs.read_ue()?;
        // qpprime_y_zero_transform_bypass_flag
        bs.skip_bits(1)?;
        // seq_scaling_matrix_present_flag
        let scaling_matrix_present = bs.read_bits(1)?;
        if scaling_matrix_present != 0 {
            let count = if chroma_format != 3 { 8 } else { 12 };
            for i in 0..count {
                let seq_scaling_list_present = bs.read_bits(1)?;
                if seq_scaling_list_present != 0 {
                    let size = if i < 6 { 16 } else { 64 };
                    skip_scaling_list(&mut bs, size)?;
                }
            }
        }
    }

    // log2_max_frame_num_minus4
    let _log2_max_frame_num = bs.read_ue()?;

    // pic_order_cnt_type
    let poc_type = bs.read_ue()?;
    match poc_type {
        0 => {
            // log2_max_pic_order_cnt_lsb_minus4
            let _log2_max_poc_lsb = bs.read_ue()?;
        }
        1 => {
            // delta_pic_order_always_zero_flag
            bs.skip_bits(1)?;
            // offset_for_non_ref_pic
            let _offset_non_ref = bs.read_se()?;
            // offset_for_top_to_bottom_field
            let _offset_top_bottom = bs.read_se()?;
            // num_ref_frames_in_pic_order_cnt_cycle
            let num_ref_in_poc = bs.read_ue()?;
            for _ in 0..num_ref_in_poc {
                let _offset = bs.read_se()?;
            }
        }
        _ => {} // poc_type == 2 has no additional fields
    }

    // max_num_ref_frames
    let num_ref_frames = bs.read_ue()? as u8;

    // gaps_in_frame_num_value_allowed_flag
    bs.skip_bits(1)?;

    // pic_width_in_mbs_minus1
    let pic_width_in_mbs = bs.read_ue()? + 1;
    // pic_height_in_map_units_minus1
    let pic_height_in_map_units = bs.read_ue()? + 1;

    // frame_mbs_only_flag
    let frame_mbs_only = bs.read_bits(1)? != 0;
    if !frame_mbs_only {
        // mb_adaptive_frame_field_flag
        bs.skip_bits(1)?;
    }

    // direct_8x8_inference_flag
    bs.skip_bits(1)?;

    // frame_cropping
    let mut crop_left: u32 = 0;
    let mut crop_right: u32 = 0;
    let mut crop_top: u32 = 0;
    let mut crop_bottom: u32 = 0;

    let frame_cropping_flag = bs.read_bits(1)?;
    if frame_cropping_flag != 0 {
        crop_left = bs.read_ue()?;
        crop_right = bs.read_ue()?;
        crop_top = bs.read_ue()?;
        crop_bottom = bs.read_ue()?;
    }

    // Calculate dimensions
    let crop_unit_x: u32 = if chroma_format == 0 { 1 } else { 2 };
    let crop_unit_y: u32 = if chroma_format == 0 {
        if frame_mbs_only { 1 } else { 2 }
    } else {
        if frame_mbs_only { 2 } else { 4 }
    };

    let width = (pic_width_in_mbs * 16 - crop_unit_x * (crop_left + crop_right)) as u16;
    let height = ((2 - if frame_mbs_only { 1u32 } else { 0 }) * pic_height_in_map_units * 16
        - crop_unit_y * (crop_top + crop_bottom)) as u16;

    // VUI parameters
    let mut fps_num: Option<u32> = None;
    let mut fps_den: Option<u32> = None;

    let vui_present = bs.read_bits(1)?;
    if vui_present != 0 {
        // Parse VUI to find timing_info
        if let Some((num, den)) = parse_h264_vui_timing(&mut bs) {
            fps_num = Some(num);
            fps_den = Some(den);
        }
    }

    Some(H264Info {
        profile_idc,
        level_idc,
        width,
        height,
        chroma_format,
        bit_depth_luma,
        num_ref_frames,
        frame_mbs_only,
        fps_num,
        fps_den,
    })
}

/// Skip a scaling list in H.264 SPS.
fn skip_scaling_list(bs: &mut BitstreamReader, size: usize) -> Option<()> {
    let mut last_scale: i32 = 8;
    let mut next_scale: i32 = 8;
    for _ in 0..size {
        if next_scale != 0 {
            let delta = bs.read_se()?;
            next_scale = (last_scale + delta + 256) % 256;
        }
        last_scale = if next_scale == 0 { last_scale } else { next_scale };
    }
    Some(())
}

/// Parse VUI timing info from H.264 SPS. Returns (fps_num, fps_den) if found.
fn parse_h264_vui_timing(bs: &mut BitstreamReader) -> Option<(u32, u32)> {
    // aspect_ratio_info_present_flag
    let ar_present = bs.read_bits(1)?;
    if ar_present != 0 {
        let ar_idc = bs.read_bits(8)?;
        if ar_idc == 255 {
            // Extended_SAR
            bs.skip_bits(32)?; // sar_width(16) + sar_height(16)
        }
    }

    // overscan_info_present_flag
    let overscan_present = bs.read_bits(1)?;
    if overscan_present != 0 {
        bs.skip_bits(1)?; // overscan_appropriate_flag
    }

    // video_signal_type_present_flag
    let video_signal_present = bs.read_bits(1)?;
    if video_signal_present != 0 {
        bs.skip_bits(3)?; // video_format
        bs.skip_bits(1)?; // video_full_range_flag
        let colour_desc_present = bs.read_bits(1)?;
        if colour_desc_present != 0 {
            bs.skip_bits(24)?; // colour_primaries(8) + transfer_chars(8) + matrix_coeffs(8)
        }
    }

    // chroma_loc_info_present_flag
    let chroma_loc_present = bs.read_bits(1)?;
    if chroma_loc_present != 0 {
        let _chroma_sample_loc_top = bs.read_ue()?;
        let _chroma_sample_loc_bottom = bs.read_ue()?;
    }

    // timing_info_present_flag
    let timing_present = bs.read_bits(1)?;
    if timing_present != 0 {
        let num_units_in_tick = bs.read_bits(32)?;
        let time_scale = bs.read_bits(32)?;

        if num_units_in_tick > 0 && time_scale > 0 {
            // H.264 timing: fps = time_scale / (2 * num_units_in_tick)
            // We store the raw values; caller can compute the ratio.
            return Some((time_scale, num_units_in_tick * 2));
        }
    }

    None
}

// ---------------------------------------------------------------------------
// HEVC SPS parsing
// ---------------------------------------------------------------------------

/// Parsed HEVC Sequence Parameter Set.
#[derive(Debug, Clone, PartialEq)]
pub struct HevcInfo {
    pub general_profile_idc: u8,
    pub general_tier_flag: bool,
    pub general_level_idc: u8,
    pub width: u16,
    pub height: u16,
    pub chroma_format: u8,
    pub bit_depth_luma: u8,
}

/// Parse an HEVC SPS NAL unit. `nal_data` starts at the NAL header bytes.
/// HEVC NAL header is 2 bytes. NAL unit type 33 = SPS.
pub fn parse_hevc_sps(nal_data: &[u8]) -> Option<HevcInfo> {
    if nal_data.len() < 4 {
        return None;
    }

    // HEVC NAL header: 2 bytes
    // forbidden_zero_bit(1) | nal_unit_type(6) | nuh_layer_id(6) | nuh_temporal_id_plus1(3)
    let nal_type = (nal_data[0] >> 1) & 0x3F;
    if nal_type != 33 {
        return None;
    }

    let rbsp = remove_emulation_prevention(&nal_data[2..]);
    let mut bs = BitstreamReader::new(&rbsp);

    // sps_video_parameter_set_id (4 bits)
    bs.skip_bits(4)?;

    // sps_max_sub_layers_minus1 (3 bits)
    let max_sub_layers = bs.read_bits(3)? as u8;

    // sps_temporal_id_nesting_flag (1 bit)
    bs.skip_bits(1)?;

    // profile_tier_level
    let (general_profile_idc, general_tier_flag, general_level_idc) =
        parse_hevc_profile_tier_level(&mut bs, max_sub_layers)?;

    // sps_seq_parameter_set_id
    let _sps_id = bs.read_ue()?;

    // chroma_format_idc
    let chroma_format = bs.read_ue()? as u8;
    if chroma_format == 3 {
        // separate_colour_plane_flag
        bs.skip_bits(1)?;
    }

    // pic_width_in_luma_samples
    let width = bs.read_ue()? as u16;
    // pic_height_in_luma_samples
    let height = bs.read_ue()? as u16;

    // conformance_window_flag — we skip cropping for HEVC since the dimensions
    // already represent the cropped output size in most practical streams.
    let conformance_window = bs.read_bits(1)?;
    if conformance_window != 0 {
        let _left = bs.read_ue()?;
        let _right = bs.read_ue()?;
        let _top = bs.read_ue()?;
        let _bottom = bs.read_ue()?;
    }

    // bit_depth_luma_minus8
    let bit_depth_luma = (bs.read_ue()? + 8) as u8;

    Some(HevcInfo {
        general_profile_idc,
        general_tier_flag,
        general_level_idc,
        width,
        height,
        chroma_format,
        bit_depth_luma,
    })
}

/// Parse profile_tier_level from HEVC bitstream.
/// Returns (general_profile_idc, general_tier_flag, general_level_idc).
fn parse_hevc_profile_tier_level(
    bs: &mut BitstreamReader,
    max_sub_layers: u8,
) -> Option<(u8, bool, u8)> {
    // general_profile_space (2 bits)
    bs.skip_bits(2)?;
    // general_tier_flag (1 bit)
    let general_tier_flag = bs.read_bits(1)? != 0;
    // general_profile_idc (5 bits)
    let general_profile_idc = bs.read_bits(5)? as u8;

    // general_profile_compatibility_flags (32 bits)
    bs.skip_bits(32)?;

    // general_constraint_indicator_flags (48 bits)
    bs.skip_bits(48)?;

    // general_level_idc (8 bits)
    let general_level_idc = bs.read_bits(8)? as u8;

    // sub_layer flags
    if max_sub_layers > 1 {
        let mut sub_layer_profile_present = Vec::new();
        let mut sub_layer_level_present = Vec::new();
        for _ in 0..max_sub_layers - 1 {
            sub_layer_profile_present.push(bs.read_bits(1)?);
            sub_layer_level_present.push(bs.read_bits(1)?);
        }
        // Padding for remaining 2-bit pairs up to 8
        if max_sub_layers - 1 < 8 {
            let remaining = 8 - (max_sub_layers - 1) as u32;
            bs.skip_bits(remaining * 2)?;
        }
        for i in 0..(max_sub_layers - 1) as usize {
            if sub_layer_profile_present[i] != 0 {
                // sub_layer_profile_space(2) + sub_layer_tier_flag(1) + sub_layer_profile_idc(5)
                // + compatibility_flags(32) + constraint_flags(48)
                bs.skip_bits(2 + 1 + 5 + 32 + 48)?;
            }
            if sub_layer_level_present[i] != 0 {
                bs.skip_bits(8)?;
            }
        }
    }

    Some((general_profile_idc, general_tier_flag, general_level_idc))
}

// ---------------------------------------------------------------------------
// AAC ADTS header parsing
// ---------------------------------------------------------------------------

/// AAC sample rate lookup table indexed by sample_rate_index (0-12).
const AAC_SAMPLE_RATES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

/// Parsed AAC ADTS frame header info.
#[derive(Debug, Clone, PartialEq)]
pub struct AacInfo {
    pub profile: u8,
    pub sample_rate: u32,
    pub channels: u8,
}

/// Parse an ADTS header from audio PES payload.
/// Looks for the 0xFFF sync word.
pub fn parse_adts_header(data: &[u8]) -> Option<AacInfo> {
    // ADTS header is at least 7 bytes
    if data.len() < 7 {
        return None;
    }

    // Find ADTS sync word (12 bits: 0xFFF)
    for i in 0..data.len().saturating_sub(6) {
        if data[i] == 0xFF && (data[i + 1] & 0xF0) == 0xF0 {
            // Found sync word
            let d = &data[i..];
            if d.len() < 7 {
                return None;
            }

            // profile (2 bits at byte 2, bits 7-6) — 0=Main, 1=LC, 2=SSR, 3=LTP
            let profile = (d[2] >> 6) & 0x03;

            // sampling_frequency_index (4 bits at byte 2, bits 5-2)
            let sample_rate_index = ((d[2] >> 2) & 0x0F) as usize;
            if sample_rate_index >= AAC_SAMPLE_RATES.len() {
                continue; // invalid index, try next sync word
            }
            let sample_rate = AAC_SAMPLE_RATES[sample_rate_index];

            // channel_configuration (3 bits: byte 2 bit 0 + byte 3 bits 7-6)
            let channels = ((d[2] & 0x01) << 2) | ((d[3] >> 6) & 0x03);

            return Some(AacInfo {
                profile,
                sample_rate,
                channels,
            });
        }
    }

    None
}

// ---------------------------------------------------------------------------
// AC3 / EAC3 syncframe header parsing
// ---------------------------------------------------------------------------

/// AC3 sample rate lookup table indexed by fscod (0-2).
const AC3_SAMPLE_RATES: [u32; 3] = [48000, 44100, 32000];

/// AC3 channel count lookup by acmod (0-7).
const AC3_CHANNELS: [u8; 8] = [2, 1, 2, 3, 3, 4, 4, 5];

/// Parsed AC3 syncframe header info.
#[derive(Debug, Clone, PartialEq)]
pub struct Ac3Info {
    pub sample_rate: u32,
    pub channels: u8,
    pub lfe: bool,
}

/// Parse an AC3 syncframe header from audio PES payload.
/// Looks for the 0x0B77 sync word.
pub fn parse_ac3_header(data: &[u8]) -> Option<Ac3Info> {
    if data.len() < 8 {
        return None;
    }

    for i in 0..data.len().saturating_sub(7) {
        if data[i] == 0x0B && data[i + 1] == 0x77 {
            let d = &data[i..];
            if d.len() < 8 {
                return None;
            }

            // Byte 4: fscod (2 bits) | frmsizecod (6 bits)
            let fscod = (d[4] >> 6) as usize;
            if fscod >= AC3_SAMPLE_RATES.len() {
                continue;
            }
            let sample_rate = AC3_SAMPLE_RATES[fscod];

            // Byte 5: bsid (5 bits) | bsmod (3 bits)
            let bsid = d[5] >> 3;

            // Byte 6: acmod (3 bits) | ...
            let acmod = (d[6] >> 5) as usize;

            // LFE flag position depends on acmod — for simplicity, parse the
            // bits after acmod. cmixlev (2 bits if acmod & 1 && acmod != 1),
            // surmixlev (2 bits if acmod & 4), then lfeon (1 bit).
            let mut bit_pos: u8 = 3; // past acmod in byte 6
            if (acmod & 0x01) != 0 && acmod != 1 {
                bit_pos += 2; // cmixlev
            }
            if (acmod & 0x04) != 0 {
                bit_pos += 2; // surmixlev
            }

            // Read LFE bit from the correct position
            let lfe_byte_offset = i + 6 + (bit_pos / 8) as usize;
            let lfe_bit_offset = 7 - (bit_pos % 8);
            let lfe = if lfe_byte_offset < data.len() {
                (data[lfe_byte_offset] >> lfe_bit_offset) & 1 != 0
            } else {
                false
            };

            let base_channels = AC3_CHANNELS[acmod];

            // bsid <= 10 is standard AC3
            if bsid <= 10 {
                return Some(Ac3Info {
                    sample_rate,
                    channels: base_channels + if lfe { 1 } else { 0 },
                    lfe,
                });
            }
        }
    }

    None
}

/// Parsed EAC3 syncframe header info.
#[derive(Debug, Clone, PartialEq)]
pub struct Eac3Info {
    pub sample_rate: u32,
    pub channels: u8,
    pub lfe: bool,
}

/// Parse an EAC3 (E-AC3 / DD+) syncframe header.
/// Same sync word (0x0B77) but bsid > 10.
pub fn parse_eac3_header(data: &[u8]) -> Option<Eac3Info> {
    if data.len() < 8 {
        return None;
    }

    for i in 0..data.len().saturating_sub(7) {
        if data[i] == 0x0B && data[i + 1] == 0x77 {
            let d = &data[i..];
            if d.len() < 8 {
                return None;
            }

            // EAC3 layout differs from AC3
            // Bytes 2-3: frame size word
            // Byte 4 bits 7-6: fscod
            let fscod = (d[4] >> 6) as usize;

            // Byte 4 bits 5-0 + byte 5 bits 7-3 give other fields
            // Byte 5 bits 7-3: bsid (5 bits)
            let bsid = (d[5] >> 3) & 0x1F;
            if bsid <= 10 {
                continue; // standard AC3, not EAC3
            }

            let sample_rate = if fscod < AC3_SAMPLE_RATES.len() {
                AC3_SAMPLE_RATES[fscod]
            } else {
                // fscod=3 means numblkscod has sample rate / 2
                // For simplicity, use 48000 as fallback
                48000
            };

            // acmod is at byte 6 bits 7-5 in the independent substream
            let acmod = (d[6] >> 5) as usize;
            let lfeon = (d[6] >> 4) & 0x01 != 0;

            let base_channels = AC3_CHANNELS[acmod];

            return Some(Eac3Info {
                sample_rate,
                channels: base_channels + if lfeon { 1 } else { 0 },
                lfe: lfeon,
            });
        }
    }

    None
}

// ---------------------------------------------------------------------------
// PES payload extraction from TsInspector
// ---------------------------------------------------------------------------

/// Subtitle stream types (DVB, SCTE).
const STREAM_TYPE_SUBTITLE: u8 = 0x06;

/// Map a PMT stream type to a VideoCodec.
fn video_codec_from_stream_type(st: u8) -> Option<VideoCodec> {
    match st {
        STREAM_TYPE_H264 => Some(VideoCodec::H264),
        STREAM_TYPE_HEVC => Some(VideoCodec::Hevc),
        STREAM_TYPE_MPEG2_VIDEO => Some(VideoCodec::Mpeg2),
        _ => None,
    }
}

/// Map a PMT stream type to an AudioCodec.
fn audio_codec_from_stream_type(st: u8) -> Option<AudioCodec> {
    match st {
        STREAM_TYPE_AAC => Some(AudioCodec::Aac),
        STREAM_TYPE_AC3 => Some(AudioCodec::Ac3),
        STREAM_TYPE_EAC3 => Some(AudioCodec::Eac3),
        STREAM_TYPE_MP2_A | STREAM_TYPE_MP2_B => Some(AudioCodec::Mp2),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// StreamInfoDetector — detects codec details from PES payloads
// ---------------------------------------------------------------------------

/// Maximum PES accumulator buffer size (128 KB).
/// Prevents unbounded memory growth on corrupt or unexpected streams.
const PES_ACCUMULATOR_CAP: usize = 128 * 1024;

/// Accumulates PES payload data for a PID, handling multi-packet assembly.
struct PesAccumulator {
    data: Vec<u8>,
    active: bool,
}

impl PesAccumulator {
    fn new() -> Self {
        Self {
            data: Vec::new(),
            active: false,
        }
    }

    /// Start a new PES packet (called on PUSI).
    fn start(&mut self, payload: &[u8]) {
        self.data.clear();
        self.data.extend_from_slice(payload);
        self.active = true;
    }

    /// Continue accumulating payload data.
    /// Stops accumulating if the buffer exceeds `PES_ACCUMULATOR_CAP`.
    fn append(&mut self, payload: &[u8]) {
        if self.active {
            if self.data.len() + payload.len() > PES_ACCUMULATOR_CAP {
                self.data.clear();
                self.active = false;
                return;
            }
            self.data.extend_from_slice(payload);
        }
    }

    /// Get accumulated data.
    fn data(&self) -> &[u8] {
        &self.data
    }
}

/// Detects stream codec information from PES payloads.
/// Designed to detect once on first keyframe, and re-detect on PMT version change.
pub struct StreamInfoDetector {
    /// Detected stream info (set once detection completes).
    pub stream_info: Option<StreamInfo>,
    /// Video PES accumulator.
    video_pes: PesAccumulator,
    /// Audio PES accumulator.
    audio_pes: PesAccumulator,
    /// Video PID to watch.
    video_pid: Option<u16>,
    /// Audio PID to watch.
    audio_pid: Option<u16>,
    /// Video stream type from PMT.
    video_stream_type: Option<u8>,
    /// Audio stream type from PMT.
    audio_stream_type: Option<u8>,
    /// Whether video details have been detected.
    video_detected: bool,
    /// Whether audio details have been detected.
    audio_detected: bool,
    /// Detected video details.
    video_codec: Option<VideoCodec>,
    video_details: Option<VideoDetails>,
    /// Detected audio details.
    audio_codec: Option<AudioCodec>,
    audio_details: Option<AudioDetails>,
    /// Subtitle PIDs found in PMT.
    subtitle_pids: Vec<u16>,
    /// Total PID count from PMT.
    total_pids: usize,
    /// Last PMT version seen (for re-detection on change).
    last_pmt_version: Option<u8>,
}

impl StreamInfoDetector {
    pub fn new() -> Self {
        Self {
            stream_info: None,
            video_pes: PesAccumulator::new(),
            audio_pes: PesAccumulator::new(),
            video_pid: None,
            audio_pid: None,
            video_stream_type: None,
            audio_stream_type: None,
            video_detected: false,
            audio_detected: false,
            video_codec: None,
            video_details: None,
            audio_codec: None,
            audio_details: None,
            subtitle_pids: Vec::new(),
            total_pids: 0,
            last_pmt_version: None,
        }
    }

    /// Update detection targets from PMT info. Returns `true` if PMT version changed.
    pub fn update_from_pmt(&mut self, pmt: &PmtInfo, pmt_version: Option<u8>) -> bool {
        let version_changed = match (self.last_pmt_version, pmt_version) {
            (Some(prev), Some(new)) => prev != new,
            (None, Some(_)) => true,
            _ => self.video_pid.is_none(), // first time
        };

        if version_changed {
            // Reset detection state
            self.video_detected = false;
            self.audio_detected = false;
            self.video_details = None;
            self.audio_details = None;
            self.video_codec = None;
            self.audio_codec = None;
            self.stream_info = None;
            self.last_pmt_version = pmt_version;
        }

        // Find video stream
        self.video_pid = None;
        self.audio_pid = None;
        self.subtitle_pids.clear();
        self.total_pids = pmt.streams.len();

        for stream in &pmt.streams {
            if is_video_stream_type(stream.stream_type) && self.video_pid.is_none() {
                self.video_pid = Some(stream.elementary_pid);
                self.video_stream_type = Some(stream.stream_type);
                self.video_codec = video_codec_from_stream_type(stream.stream_type);
            } else if is_audio_stream_type(stream.stream_type) && self.audio_pid.is_none() {
                self.audio_pid = Some(stream.elementary_pid);
                self.audio_stream_type = Some(stream.stream_type);
                self.audio_codec = audio_codec_from_stream_type(stream.stream_type);
            } else if stream.stream_type == STREAM_TYPE_SUBTITLE {
                self.subtitle_pids.push(stream.elementary_pid);
            }
        }

        version_changed
    }

    /// Feed a TS packet payload to the detector. Call for every packet with payload.
    pub fn feed_packet(&mut self, pid: u16, pusi: bool, payload: &[u8]) {
        if self.stream_info.is_some() {
            return; // Already detected
        }

        // Video PES accumulation
        if Some(pid) == self.video_pid && !self.video_detected {
            if pusi {
                self.video_pes.start(payload);
            } else {
                self.video_pes.append(payload);
            }
            self.try_detect_video();
        }

        // Audio PES accumulation
        if Some(pid) == self.audio_pid && !self.audio_detected {
            if pusi {
                self.audio_pes.start(payload);
            } else {
                self.audio_pes.append(payload);
            }
            self.try_detect_audio();
        }

        // Check if detection is complete
        self.try_finalize();
    }

    /// Attempt to parse video codec details from accumulated PES data.
    fn try_detect_video(&mut self) {
        // MPEG-2 — mark as detected without detailed parsing
        if self.video_stream_type == Some(STREAM_TYPE_MPEG2_VIDEO) {
            self.video_detected = true;
            return;
        }

        let pes_data = self.video_pes.data();
        if pes_data.len() < 14 {
            return;
        }

        // Skip PES header to get to elementary stream data
        let es_data = match pes_payload_start(pes_data) {
            Some(start) if start < pes_data.len() => &pes_data[start..],
            _ => return,
        };

        match self.video_stream_type {
            Some(STREAM_TYPE_H264) => {
                // Look for SPS NAL unit (type 7)
                for (nal_header, nal_data) in find_nal_units(es_data) {
                    let nal_type = nal_header & 0x1F;
                    if nal_type == 7 {
                        if let Some(info) = parse_h264_sps(&nal_data) {
                            self.video_details = Some(VideoDetails {
                                width: info.width,
                                height: info.height,
                                profile: info.profile_idc,
                                level: info.level_idc,
                                chroma_format: info.chroma_format,
                                bit_depth_luma: info.bit_depth_luma,
                                frame_mbs_only: info.frame_mbs_only,
                                fps_num: info.fps_num,
                                fps_den: info.fps_den,
                            });
                            self.video_detected = true;
                            return;
                        }
                    }
                }
            }
            Some(STREAM_TYPE_HEVC) => {
                // Look for SPS NAL unit (type 33)
                for (nal_header, nal_data) in find_nal_units(es_data) {
                    let nal_type = (nal_header >> 1) & 0x3F;
                    if nal_type == 33 {
                        if let Some(info) = parse_hevc_sps(&nal_data) {
                            self.video_details = Some(VideoDetails {
                                width: info.width,
                                height: info.height,
                                profile: info.general_profile_idc,
                                level: info.general_level_idc,
                                chroma_format: info.chroma_format,
                                bit_depth_luma: info.bit_depth_luma,
                                frame_mbs_only: true, // HEVC uses different signaling
                                fps_num: None,
                                fps_den: None,
                            });
                            self.video_detected = true;
                            return;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Attempt to parse audio codec details from accumulated PES data.
    fn try_detect_audio(&mut self) {
        // MP2 — mark detected without detailed parsing
        if matches!(self.audio_stream_type, Some(STREAM_TYPE_MP2_A | STREAM_TYPE_MP2_B)) {
            self.audio_detected = true;
            return;
        }

        let pes_data = self.audio_pes.data();
        if pes_data.len() < 10 {
            return;
        }

        // Skip PES header
        let es_data = match pes_payload_start(pes_data) {
            Some(start) if start < pes_data.len() => &pes_data[start..],
            _ => return,
        };

        match self.audio_stream_type {
            Some(STREAM_TYPE_AAC) => {
                if let Some(info) = parse_adts_header(es_data) {
                    self.audio_details = Some(AudioDetails {
                        profile: info.profile,
                        sample_rate: info.sample_rate,
                        channels: info.channels,
                    });
                    self.audio_detected = true;
                }
            }
            Some(STREAM_TYPE_AC3) => {
                if let Some(info) = parse_ac3_header(es_data) {
                    self.audio_details = Some(AudioDetails {
                        profile: 0,
                        sample_rate: info.sample_rate,
                        channels: info.channels,
                    });
                    self.audio_detected = true;
                }
            }
            Some(STREAM_TYPE_EAC3) => {
                if let Some(info) = parse_eac3_header(es_data) {
                    self.audio_details = Some(AudioDetails {
                        profile: 0,
                        sample_rate: info.sample_rate,
                        channels: info.channels,
                    });
                    self.audio_detected = true;
                }
            }
            _ => {}
        }
    }

    /// Check if both video and audio have been detected, and finalize StreamInfo.
    fn try_finalize(&mut self) {
        // We need at least one of video/audio to be either detected or absent
        let video_done = self.video_detected || self.video_pid.is_none();
        let audio_done = self.audio_detected || self.audio_pid.is_none();

        if video_done && audio_done {
            self.stream_info = Some(StreamInfo {
                video_codec: self.video_codec,
                video_details: self.video_details.clone(),
                audio_codec: self.audio_codec,
                audio_details: self.audio_details.clone(),
                subtitle_pids: self.subtitle_pids.clone(),
                total_pids: self.total_pids,
                detected_at: Instant::now(),
            });
        }
    }

    /// Force re-detection (e.g., on PMT version change).
    pub fn reset(&mut self) {
        self.video_detected = false;
        self.audio_detected = false;
        self.video_details = None;
        self.audio_details = None;
        self.stream_info = None;
        self.video_pes = PesAccumulator::new();
        self.audio_pes = PesAccumulator::new();
    }
}

/// Calculate the start of elementary stream data within a PES packet.
/// Returns the byte offset past the PES header.
fn pes_payload_start(pes: &[u8]) -> Option<usize> {
    // PES start code: 00 00 01 stream_id
    if pes.len() < 9 {
        return None;
    }
    if pes[0] != 0x00 || pes[1] != 0x00 || pes[2] != 0x01 {
        return None;
    }
    // Byte 8: PES header data length
    let header_data_len = pes[8] as usize;
    let start = 9 + header_data_len;
    Some(start)
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
        assert_eq!(info.version, 0);
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

    // -----------------------------------------------------------------------
    // Bitstream reader tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_bitstream_read_bits() {
        let data = [0b10110001, 0b01010101];
        let mut bs = BitstreamReader::new(&data);

        assert_eq!(bs.read_bits(1).unwrap(), 1);
        assert_eq!(bs.read_bits(1).unwrap(), 0);
        assert_eq!(bs.read_bits(3).unwrap(), 0b110);
        assert_eq!(bs.read_bits(3).unwrap(), 0b001);
        // Now at byte 1
        assert_eq!(bs.read_bits(8).unwrap(), 0b01010101);
    }

    #[test]
    fn test_bitstream_read_ue() {
        // ue(0) = 1 (single bit "1")
        let data = [0b10000000];
        let mut bs = BitstreamReader::new(&data);
        assert_eq!(bs.read_ue().unwrap(), 0);

        // ue(1) = 010 = 2^1 - 1 + 0 = 1
        let data = [0b01000000];
        let mut bs = BitstreamReader::new(&data);
        assert_eq!(bs.read_ue().unwrap(), 1);

        // ue(2) = 011 = 2^1 - 1 + 1 = 2
        let data = [0b01100000];
        let mut bs = BitstreamReader::new(&data);
        assert_eq!(bs.read_ue().unwrap(), 2);

        // ue(3) = 00100 = 2^2 - 1 + 0 = 3
        let data = [0b00100000];
        let mut bs = BitstreamReader::new(&data);
        assert_eq!(bs.read_ue().unwrap(), 3);

        // ue(6) = 00111 = 2^2 - 1 + 3 = 6
        let data = [0b00111000];
        let mut bs = BitstreamReader::new(&data);
        assert_eq!(bs.read_ue().unwrap(), 6);
    }

    #[test]
    fn test_bitstream_read_se() {
        // se(0) from ue(0) = 0
        let data = [0b10000000];
        let mut bs = BitstreamReader::new(&data);
        assert_eq!(bs.read_se().unwrap(), 0);

        // se from ue(1) = 1 (odd → positive, (1+1)/2 = 1)
        let data = [0b01000000];
        let mut bs = BitstreamReader::new(&data);
        assert_eq!(bs.read_se().unwrap(), 1);

        // se from ue(2) = -1 (even → negative, -(2+1)/2 = -1)
        let data = [0b01100000];
        let mut bs = BitstreamReader::new(&data);
        assert_eq!(bs.read_se().unwrap(), -1);

        // se from ue(3) = 2 (odd → positive, (3+1)/2 = 2)
        let data = [0b00100000];
        let mut bs = BitstreamReader::new(&data);
        assert_eq!(bs.read_se().unwrap(), 2);

        // se from ue(4) = -2 (even → negative, -(4+1)/2 = -2)
        let data = [0b00101000];
        let mut bs = BitstreamReader::new(&data);
        assert_eq!(bs.read_se().unwrap(), -2);
    }

    #[test]
    fn test_bitstream_exhaustion() {
        let data = [0xFF];
        let mut bs = BitstreamReader::new(&data);
        assert!(bs.read_bits(8).is_some());
        assert!(bs.read_bit().is_none());
    }

    // -----------------------------------------------------------------------
    // Emulation prevention byte removal tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_remove_emulation_prevention_basic() {
        // 00 00 03 should become 00 00
        let input = vec![0x00, 0x00, 0x03, 0x01, 0x02];
        let output = remove_emulation_prevention(&input);
        assert_eq!(output, vec![0x00, 0x00, 0x01, 0x02]);
    }

    #[test]
    fn test_remove_emulation_prevention_multiple() {
        let input = vec![0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x03];
        let output = remove_emulation_prevention(&input);
        assert_eq!(output, vec![0x00, 0x00, 0x00, 0x00, 0x03]);
    }

    #[test]
    fn test_remove_emulation_prevention_none() {
        let input = vec![0x01, 0x02, 0x03, 0x04];
        let output = remove_emulation_prevention(&input);
        assert_eq!(output, input);
    }

    #[test]
    fn test_remove_emulation_prevention_empty() {
        let output = remove_emulation_prevention(&[]);
        assert!(output.is_empty());
    }

    // -----------------------------------------------------------------------
    // NAL unit finding tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_find_nal_units_3byte_start_code() {
        // Two NAL units with 3-byte start codes
        let data = vec![
            0x00, 0x00, 0x01, 0x67, 0xAA, 0xBB, // NAL type 7 (SPS)
            0x00, 0x00, 0x01, 0x68, 0xCC, 0xDD, // NAL type 8 (PPS)
        ];
        let units = find_nal_units(&data);
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].0 & 0x1F, 7); // SPS
        assert_eq!(units[1].0 & 0x1F, 8); // PPS
    }

    #[test]
    fn test_find_nal_units_4byte_start_code() {
        let data = vec![
            0x00, 0x00, 0x00, 0x01, 0x67, 0xAA, 0xBB, // NAL type 7
        ];
        let units = find_nal_units(&data);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].0 & 0x1F, 7);
    }

    // -----------------------------------------------------------------------
    // H.264 SPS parsing tests
    // -----------------------------------------------------------------------

    /// Build a minimal H.264 SPS NAL for Baseline profile, 1920x1080, level 4.0.
    fn make_h264_sps_baseline_1080p() -> Vec<u8> {
        // NAL header (type 7 = SPS) + hand-crafted RBSP
        let mut sps = Vec::new();
        sps.push(0x67); // NAL header: forbidden=0, ref_idc=3, type=7

        // Build RBSP bitstream
        let mut rbsp = Vec::new();

        // profile_idc = 66 (Baseline)
        rbsp.push(66);

        // constraint_set_flags = 0x40 (constraint_set0_flag set)
        rbsp.push(0x40);

        // level_idc = 40 (Level 4.0)
        rbsp.push(40);

        // seq_parameter_set_id = 0 → ue(0) = 1 bit "1"
        // chroma_format_idc — not present for Baseline (profile_idc=66)
        // log2_max_frame_num_minus4 = 0 → ue(0) = "1"
        // pic_order_cnt_type = 0 → ue(0) = "1"
        // log2_max_pic_order_cnt_lsb_minus4 = 0 → ue(0) = "1"
        // max_num_ref_frames = 4 → ue(4) = "00101"
        // gaps_in_frame_num_value_allowed_flag = 0
        // pic_width_in_mbs_minus1 = 119 → ue(119) = "0000001_1110000"
        // pic_height_in_map_units_minus1 = 67 → ue(67) = "000001_0001000"
        // frame_mbs_only_flag = 1
        // direct_8x8_inference_flag = 1
        // frame_cropping_flag = 1
        // crop_left = 0 → ue(0)
        // crop_right = 0 → ue(0)
        // crop_top = 0 → ue(0)
        // crop_bottom = 4 → ue(4) = "00101"
        // vui_parameters_present_flag = 0

        // Encode as bits:
        // sps_id=ue(0): 1
        // log2_max_frame_num_minus4=ue(0): 1
        // poc_type=ue(0): 1
        // log2_max_poc_lsb=ue(0): 1
        // max_num_ref_frames=ue(4): 00101
        // gaps=0
        // pic_width_in_mbs_minus1=ue(119): 119+1=120, so ue(119). 119 = 2^7 - 9 = not exact.
        //   Leading zeros: floor(log2(119+1)) = floor(log2(120)) = 6
        //   ue(119) = 6 leading zeros + 1 + 6 suffix bits = 0000001_111000 (120-64=56, wait)
        //   ue(n) = 0...01<suffix> where prefix zeros = floor(log2(n+1))
        //   n=119: n+1=120. log2(120)=6.9, floor=6. code_num in [2^7-1..2^7-1] no.
        //   Leading zeros = 6, then 1, then 6 bits = 120 - 2^6 = 120-64 = 56 = 0b111000
        //   So: 0000001 111000
        //   13 bits total
        //
        // pic_height_in_map_units_minus1=ue(67): 67+1=68. floor(log2(68))=6.
        //   Leading zeros=6, suffix = 68-64=4=0b000100
        //   0000001 000100
        //   13 bits total
        //
        // frame_mbs_only_flag=1: 1 bit
        // direct_8x8_inference_flag=1: 1 bit
        // frame_cropping_flag=1: 1 bit
        // crop_left=ue(0): 1
        // crop_right=ue(0): 1
        // crop_top=ue(0): 1
        // crop_bottom=ue(4): 00101 (5 bits) — crop 4*2=8 pixels → 1088-8=1080
        // vui_present=0: 1 bit

        // Let me encode this carefully with a bit writer
        let mut bits: Vec<u8> = Vec::new();

        // sps_id=0 → 1
        bits.push(1);
        // log2_max_frame_num_minus4=0 → 1
        bits.push(1);
        // poc_type=0 → 1
        bits.push(1);
        // log2_max_poc_lsb=0 → 1
        bits.push(1);
        // max_num_ref_frames=4 → 00101
        bits.extend_from_slice(&[0, 0, 1, 0, 1]);
        // gaps=0
        bits.push(0);
        // pic_width_in_mbs_minus1=119 → 0000001_111000
        bits.extend_from_slice(&[0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 0, 0, 0]);
        // pic_height_in_map_units_minus1=67 → 0000001_000100
        bits.extend_from_slice(&[0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0]);
        // frame_mbs_only_flag=1
        bits.push(1);
        // direct_8x8_inference_flag=1
        bits.push(1);
        // frame_cropping_flag=1
        bits.push(1);
        // crop_left=0 → 1
        bits.push(1);
        // crop_right=0 → 1
        bits.push(1);
        // crop_top=0 → 1
        bits.push(1);
        // crop_bottom=4 → 00101
        bits.extend_from_slice(&[0, 0, 1, 0, 1]);
        // vui_present=0
        bits.push(0);

        // Convert bits to bytes
        let mut byte_val: u8 = 0;
        let mut bit_count = 0;
        for &b in &bits {
            byte_val = (byte_val << 1) | b;
            bit_count += 1;
            if bit_count == 8 {
                rbsp.push(byte_val);
                byte_val = 0;
                bit_count = 0;
            }
        }
        if bit_count > 0 {
            byte_val <<= 8 - bit_count;
            rbsp.push(byte_val);
        }

        sps.extend_from_slice(&rbsp);
        sps
    }

    #[test]
    fn test_h264_sps_parse_baseline_1080p() {
        let sps_nal = make_h264_sps_baseline_1080p();
        let info = parse_h264_sps(&sps_nal).unwrap();

        assert_eq!(info.profile_idc, 66); // Baseline
        assert_eq!(info.level_idc, 40);   // Level 4.0
        assert_eq!(info.width, 1920);
        assert_eq!(info.height, 1080);
        assert_eq!(info.chroma_format, 1); // 4:2:0 default
        assert_eq!(info.bit_depth_luma, 8);
        assert_eq!(info.num_ref_frames, 4);
        assert!(info.frame_mbs_only);
    }

    /// Build a minimal H.264 SPS for High profile, 1280x720, level 3.1, with VUI timing.
    fn make_h264_sps_high_720p_with_vui() -> Vec<u8> {
        let mut sps = Vec::new();
        sps.push(0x67); // NAL header: type 7

        // profile_idc = 100 (High)
        sps.push(100);
        // constraint_set_flags
        sps.push(0x00);
        // level_idc = 31
        sps.push(31);

        // Now we need to encode the RBSP with bit-level precision
        let mut bits: Vec<u8> = Vec::new();

        // sps_id=0 → 1
        bits.push(1);

        // === High profile extra fields ===
        // chroma_format_idc=1 (4:2:0) → ue(1) = 010
        bits.extend_from_slice(&[0, 1, 0]);
        // bit_depth_luma_minus8=0 → ue(0) = 1
        bits.push(1);
        // bit_depth_chroma_minus8=0 → ue(0) = 1
        bits.push(1);
        // qpprime_y_zero_transform_bypass_flag=0
        bits.push(0);
        // seq_scaling_matrix_present_flag=0
        bits.push(0);

        // log2_max_frame_num_minus4=0 → 1
        bits.push(1);
        // poc_type=0 → 1
        bits.push(1);
        // log2_max_poc_lsb=0 → 1
        bits.push(1);
        // max_num_ref_frames=3 → ue(3) = 00100
        bits.extend_from_slice(&[0, 0, 1, 0, 0]);
        // gaps=0
        bits.push(0);

        // pic_width_in_mbs_minus1=79 → ue(79). 79+1=80, floor(log2(80))=6
        // 6 zeros + 1 + suffix(80-64=16=0b010000)
        bits.extend_from_slice(&[0, 0, 0, 0, 0, 0, 1, 0, 1, 0, 0, 0, 0]);

        // pic_height_in_map_units_minus1=44 → ue(44). 44+1=45, floor(log2(45))=5
        // 5 zeros + 1 + suffix(45-32=13=0b01101)
        bits.extend_from_slice(&[0, 0, 0, 0, 0, 1, 0, 1, 1, 0, 1]);

        // frame_mbs_only_flag=1
        bits.push(1);
        // direct_8x8_inference_flag=1
        bits.push(1);
        // frame_cropping_flag=0
        bits.push(0);

        // vui_parameters_present_flag=1
        bits.push(1);

        // === VUI parameters ===
        // aspect_ratio_info_present_flag=0
        bits.push(0);
        // overscan_info_present_flag=0
        bits.push(0);
        // video_signal_type_present_flag=0
        bits.push(0);
        // chroma_loc_info_present_flag=0
        bits.push(0);
        // timing_info_present_flag=1
        bits.push(1);

        // num_units_in_tick = 1001 (32 bits) = 0x000003E9
        for bit in 0..32u8 {
            bits.push(((1001u32 >> (31 - bit)) & 1) as u8);
        }
        // time_scale = 60000 (32 bits) = 0x0000EA60
        for bit in 0..32u8 {
            bits.push(((60000u32 >> (31 - bit)) & 1) as u8);
        }

        // Convert bits to bytes
        let mut rbsp = Vec::new();
        let mut byte_val: u8 = 0;
        let mut bit_count = 0;
        for &b in &bits {
            byte_val = (byte_val << 1) | b;
            bit_count += 1;
            if bit_count == 8 {
                rbsp.push(byte_val);
                byte_val = 0;
                bit_count = 0;
            }
        }
        if bit_count > 0 {
            byte_val <<= 8 - bit_count;
            rbsp.push(byte_val);
        }

        sps.extend_from_slice(&rbsp);
        sps
    }

    #[test]
    fn test_h264_sps_parse_high_720p_with_vui() {
        let sps_nal = make_h264_sps_high_720p_with_vui();
        let info = parse_h264_sps(&sps_nal).unwrap();

        assert_eq!(info.profile_idc, 100); // High
        assert_eq!(info.level_idc, 31);    // Level 3.1
        assert_eq!(info.width, 1280);
        assert_eq!(info.height, 720);
        assert_eq!(info.chroma_format, 1); // 4:2:0
        assert_eq!(info.bit_depth_luma, 8);
        assert!(info.frame_mbs_only);

        // fps = time_scale / (2 * num_units_in_tick) = 60000 / (2 * 1001) = 29.97
        assert_eq!(info.fps_num, Some(60000));
        assert_eq!(info.fps_den, Some(2002));
    }

    #[test]
    fn test_h264_sps_wrong_nal_type() {
        // NAL type 8 (PPS), not SPS
        let data = vec![0x68, 0x00, 0x00, 0x00];
        assert!(parse_h264_sps(&data).is_none());
    }

    #[test]
    fn test_h264_sps_too_short() {
        assert!(parse_h264_sps(&[0x67]).is_none());
        assert!(parse_h264_sps(&[]).is_none());
    }

    // -----------------------------------------------------------------------
    // HEVC SPS parsing tests
    // -----------------------------------------------------------------------

    /// Build a minimal HEVC SPS NAL for Main profile, 1920x1080.
    fn make_hevc_sps_main_1080p() -> Vec<u8> {
        let mut sps = Vec::new();
        // HEVC NAL header (2 bytes): forbidden=0, type=33 (SPS), layer_id=0, tid=1
        // byte0: 0 | 100010 | 0 = 0x42 (but shifted: forbidden(1)=0, type(6)=33, so byte0 = (33<<1)&0x7E = 0x42)
        sps.push(0x42);
        // byte1: nuh_layer_id(6)=0 | nuh_temporal_id_plus1(3)=1 → 0b00000001 = 0x01
        sps.push(0x01);

        let mut bits: Vec<u8> = Vec::new();

        // sps_video_parameter_set_id (4 bits) = 0
        bits.extend_from_slice(&[0, 0, 0, 0]);

        // sps_max_sub_layers_minus1 (3 bits) = 0
        bits.extend_from_slice(&[0, 0, 0]);

        // sps_temporal_id_nesting_flag (1 bit) = 1
        bits.push(1);

        // === profile_tier_level ===
        // general_profile_space (2 bits) = 0
        bits.extend_from_slice(&[0, 0]);
        // general_tier_flag (1 bit) = 0 (Main tier)
        bits.push(0);
        // general_profile_idc (5 bits) = 1 (Main)
        bits.extend_from_slice(&[0, 0, 0, 0, 1]);
        // general_profile_compatibility_flags (32 bits) — set bit for Main
        for i in 0..32u8 {
            bits.push(if i == 1 { 1 } else { 0 }); // profile 1 compatibility
        }
        // general_constraint_indicator_flags (48 bits) = all zero
        for _ in 0..48 {
            bits.push(0);
        }
        // general_level_idc (8 bits) = 120 (Level 4.0 = 4.0 * 30 = 120)
        for bit in 0..8u8 {
            bits.push((120u8 >> (7 - bit)) & 1);
        }
        // No sub_layer flags since max_sub_layers=0

        // sps_seq_parameter_set_id = 0 → ue(0) = 1
        bits.push(1);

        // chroma_format_idc = 1 (4:2:0) → ue(1) = 010
        bits.extend_from_slice(&[0, 1, 0]);

        // pic_width_in_luma_samples = 1920 → ue(1920)
        // 1920+1=1921, floor(log2(1921))=10, prefix=10 zeros+1, suffix=1921-1024=897
        // 897 in 10 bits = 0b1110000001
        bits.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        for bit in (0..10).rev() {
            bits.push(((897u32 >> bit) & 1) as u8);
        }

        // pic_height_in_luma_samples = 1080 → ue(1080)
        // 1080+1=1081, floor(log2(1081))=10, prefix=10 zeros+1, suffix=1081-1024=57
        // 57 in 10 bits = 0b0000111001
        bits.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        for bit in (0..10).rev() {
            bits.push(((57u32 >> bit) & 1) as u8);
        }

        // conformance_window_flag = 0
        bits.push(0);

        // bit_depth_luma_minus8 = 0 → ue(0) = 1
        bits.push(1);

        // Convert bits to bytes
        let mut rbsp = Vec::new();
        let mut byte_val: u8 = 0;
        let mut bit_count = 0;
        for &b in &bits {
            byte_val = (byte_val << 1) | b;
            bit_count += 1;
            if bit_count == 8 {
                rbsp.push(byte_val);
                byte_val = 0;
                bit_count = 0;
            }
        }
        if bit_count > 0 {
            byte_val <<= 8 - bit_count;
            rbsp.push(byte_val);
        }

        sps.extend_from_slice(&rbsp);
        sps
    }

    #[test]
    fn test_hevc_sps_parse_main_1080p() {
        let sps_nal = make_hevc_sps_main_1080p();
        let info = parse_hevc_sps(&sps_nal).unwrap();

        assert_eq!(info.general_profile_idc, 1); // Main
        assert!(!info.general_tier_flag);          // Main tier
        assert_eq!(info.general_level_idc, 120);   // Level 4.0
        assert_eq!(info.width, 1920);
        assert_eq!(info.height, 1080);
        assert_eq!(info.chroma_format, 1);         // 4:2:0
        assert_eq!(info.bit_depth_luma, 8);
    }

    #[test]
    fn test_hevc_sps_wrong_nal_type() {
        // NAL type 34 (PPS), not SPS (33)
        // type=34 → byte0 = (34<<1) = 0x44
        let data = vec![0x44, 0x01, 0x00, 0x00];
        assert!(parse_hevc_sps(&data).is_none());
    }

    #[test]
    fn test_hevc_sps_too_short() {
        assert!(parse_hevc_sps(&[0x42]).is_none());
        assert!(parse_hevc_sps(&[]).is_none());
    }

    // -----------------------------------------------------------------------
    // AAC ADTS header parsing tests
    // -----------------------------------------------------------------------

    /// Build an ADTS header for AAC-LC, 48kHz, stereo.
    fn make_adts_header(profile: u8, sample_rate_index: u8, channels: u8) -> Vec<u8> {
        let mut header = vec![0u8; 7];

        // Sync word (12 bits): 0xFFF
        header[0] = 0xFF;
        header[1] = 0xF0;

        // ID (1 bit) = 0 (MPEG-4)
        // Layer (2 bits) = 0
        // protection_absent (1 bit) = 1
        header[1] |= 0x01;

        // profile (2 bits) | sampling_frequency_index (4 bits) | private_bit (1 bit) | channel_config (3 bits top)
        header[2] = (profile << 6) | (sample_rate_index << 2) | ((channels >> 2) & 0x01);
        header[3] = ((channels & 0x03) << 6) | 0x00;

        // frame_length = 7 (header only, for testing)
        let frame_length: u16 = 7;
        header[3] |= ((frame_length >> 11) & 0x03) as u8;
        header[4] = ((frame_length >> 3) & 0xFF) as u8;
        header[5] = ((frame_length & 0x07) << 5) as u8 | 0x1F;
        header[6] = 0xFC;

        header
    }

    #[test]
    fn test_adts_parse_aac_lc_48khz_stereo() {
        // profile=1 (LC), sample_rate_index=3 (48kHz), channels=2
        let header = make_adts_header(1, 3, 2);
        let info = parse_adts_header(&header).unwrap();

        assert_eq!(info.profile, 1);  // LC
        assert_eq!(info.sample_rate, 48000);
        assert_eq!(info.channels, 2);
    }

    #[test]
    fn test_adts_parse_aac_main_44100_mono() {
        // profile=0 (Main), sample_rate_index=4 (44100), channels=1
        let header = make_adts_header(0, 4, 1);
        let info = parse_adts_header(&header).unwrap();

        assert_eq!(info.profile, 0);  // Main
        assert_eq!(info.sample_rate, 44100);
        assert_eq!(info.channels, 1);
    }

    #[test]
    fn test_adts_parse_aac_ssr_32khz() {
        // profile=2 (SSR), sample_rate_index=5 (32kHz), channels=6 (5.1)
        let header = make_adts_header(2, 5, 6);
        let info = parse_adts_header(&header).unwrap();

        assert_eq!(info.profile, 2);
        assert_eq!(info.sample_rate, 32000);
        assert_eq!(info.channels, 6);
    }

    #[test]
    fn test_adts_sample_rate_table() {
        // Verify all sample rates in the lookup table
        let expected = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];
        for (idx, &rate) in expected.iter().enumerate() {
            let header = make_adts_header(1, idx as u8, 2);
            let info = parse_adts_header(&header).unwrap();
            assert_eq!(info.sample_rate, rate, "sample_rate_index {} should be {}", idx, rate);
        }
    }

    #[test]
    fn test_adts_parse_too_short() {
        assert!(parse_adts_header(&[0xFF, 0xF0]).is_none());
        assert!(parse_adts_header(&[]).is_none());
    }

    #[test]
    fn test_adts_parse_no_sync() {
        let data = vec![0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06];
        assert!(parse_adts_header(&data).is_none());
    }

    #[test]
    fn test_adts_parse_with_leading_bytes() {
        // ADTS sync word not at offset 0
        let mut data = vec![0x00, 0x00, 0x00];
        data.extend_from_slice(&make_adts_header(1, 3, 2));
        let info = parse_adts_header(&data).unwrap();
        assert_eq!(info.profile, 1);
        assert_eq!(info.sample_rate, 48000);
    }

    // -----------------------------------------------------------------------
    // AC3 header parsing tests
    // -----------------------------------------------------------------------

    /// Build a minimal AC3 syncframe header.
    fn make_ac3_header(fscod: u8, acmod: u8, bsid: u8) -> Vec<u8> {
        let mut data = vec![0u8; 8];
        // Sync word
        data[0] = 0x0B;
        data[1] = 0x77;
        // CRC (2 bytes) — dummy
        data[2] = 0x00;
        data[3] = 0x00;
        // fscod (2 bits) | frmsizecod (6 bits)
        data[4] = (fscod << 6) | 0x04; // frmsizecod=4 (some valid value)
        // bsid (5 bits) | bsmod (3 bits)
        data[5] = (bsid << 3) | 0x00;
        // acmod (3 bits) | cmixlev/surmixlev/lfeon...
        data[6] = (acmod << 5) | 0x00;
        data[7] = 0x00;
        data
    }

    #[test]
    fn test_ac3_parse_48khz_stereo() {
        // fscod=0 (48kHz), acmod=2 (stereo=2ch), bsid=8
        let data = make_ac3_header(0, 2, 8);
        let info = parse_ac3_header(&data).unwrap();

        assert_eq!(info.sample_rate, 48000);
        // acmod=2 → 2 channels (stereo), no LFE since bit is 0
        assert_eq!(info.channels, 2);
    }

    #[test]
    fn test_ac3_parse_44100() {
        // fscod=1 (44100), acmod=7 (5ch), bsid=6
        let data = make_ac3_header(1, 7, 6);
        let info = parse_ac3_header(&data).unwrap();

        assert_eq!(info.sample_rate, 44100);
        assert_eq!(info.channels, 5);
    }

    #[test]
    fn test_ac3_parse_too_short() {
        assert!(parse_ac3_header(&[0x0B, 0x77]).is_none());
    }

    #[test]
    fn test_ac3_parse_no_sync() {
        let data = vec![0x00; 8];
        assert!(parse_ac3_header(&data).is_none());
    }

    // -----------------------------------------------------------------------
    // EAC3 header parsing tests
    // -----------------------------------------------------------------------

    /// Build a minimal EAC3 syncframe header.
    fn make_eac3_header(fscod: u8, acmod: u8, lfeon: bool) -> Vec<u8> {
        let mut data = vec![0u8; 8];
        data[0] = 0x0B;
        data[1] = 0x77;
        // frame_size (bytes 2-3)
        data[2] = 0x00;
        data[3] = 0x40;
        // fscod (2 bits) | ...
        data[4] = (fscod << 6) | 0x00;
        // bsid (5 bits) = 16 (EAC3) | bsmod (3 bits)
        data[5] = (16 << 3) | 0x00;
        // acmod (3 bits) | lfeon (1 bit) | ...
        data[6] = (acmod << 5) | (if lfeon { 0x10 } else { 0x00 });
        data[7] = 0x00;
        data
    }

    #[test]
    fn test_eac3_parse_48khz_51() {
        // fscod=0 (48kHz), acmod=7 (3/2 = 5ch), lfeon=true
        let data = make_eac3_header(0, 7, true);
        let info = parse_eac3_header(&data).unwrap();

        assert_eq!(info.sample_rate, 48000);
        assert_eq!(info.channels, 6); // 5 + LFE
        assert!(info.lfe);
    }

    #[test]
    fn test_eac3_parse_stereo() {
        // fscod=0 (48kHz), acmod=2 (stereo), lfeon=false
        let data = make_eac3_header(0, 2, false);
        let info = parse_eac3_header(&data).unwrap();

        assert_eq!(info.sample_rate, 48000);
        assert_eq!(info.channels, 2);
        assert!(!info.lfe);
    }

    // -----------------------------------------------------------------------
    // PES payload start tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_pes_payload_start() {
        let pes = make_pes_with_pts(90000);
        let start = pes_payload_start(&pes).unwrap();
        // PES header: 9 bytes fixed + 5 bytes PTS = 14
        assert_eq!(start, 14);
    }

    #[test]
    fn test_pes_payload_start_invalid() {
        assert!(pes_payload_start(&[]).is_none());
        assert!(pes_payload_start(&[0x00, 0x00]).is_none());
        assert!(pes_payload_start(&[0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]).is_none());
    }

    // -----------------------------------------------------------------------
    // StreamInfoDetector tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_detector_update_from_pmt() {
        let mut detector = StreamInfoDetector::new();
        let pmt = PmtInfo {
            pcr_pid: 0x0100,
            version: 0,
            streams: vec![
                PmtStream { stream_type: STREAM_TYPE_H264, elementary_pid: 0x0100, descriptors: vec![] },
                PmtStream { stream_type: STREAM_TYPE_AAC, elementary_pid: 0x0101, descriptors: vec![] },
            ],
        };

        let changed = detector.update_from_pmt(&pmt, Some(1));
        assert!(changed);
        assert_eq!(detector.video_pid, Some(0x0100));
        assert_eq!(detector.audio_pid, Some(0x0101));
        assert_eq!(detector.video_codec, Some(VideoCodec::H264));
        assert_eq!(detector.audio_codec, Some(AudioCodec::Aac));
    }

    #[test]
    fn test_detector_pmt_version_change_resets() {
        let mut detector = StreamInfoDetector::new();
        let pmt = PmtInfo {
            pcr_pid: 0x0100,
            version: 0,
            streams: vec![
                PmtStream { stream_type: STREAM_TYPE_H264, elementary_pid: 0x0100, descriptors: vec![] },
            ],
        };

        detector.update_from_pmt(&pmt, Some(1));
        detector.video_detected = true; // Simulate detection

        let changed = detector.update_from_pmt(&pmt, Some(2));
        assert!(changed);
        assert!(!detector.video_detected); // Should be reset
    }

    #[test]
    fn test_detector_same_pmt_version_no_reset() {
        let mut detector = StreamInfoDetector::new();
        let pmt = PmtInfo {
            pcr_pid: 0x0100,
            version: 0,
            streams: vec![
                PmtStream { stream_type: STREAM_TYPE_H264, elementary_pid: 0x0100, descriptors: vec![] },
            ],
        };

        detector.update_from_pmt(&pmt, Some(1));
        detector.video_detected = true;

        let changed = detector.update_from_pmt(&pmt, Some(1));
        assert!(!changed);
    }

    #[test]
    fn test_detector_subtitle_pids() {
        let mut detector = StreamInfoDetector::new();
        let pmt = PmtInfo {
            pcr_pid: 0x0100,
            version: 0,
            streams: vec![
                PmtStream { stream_type: STREAM_TYPE_H264, elementary_pid: 0x0100, descriptors: vec![] },
                PmtStream { stream_type: STREAM_TYPE_AAC, elementary_pid: 0x0101, descriptors: vec![] },
                PmtStream { stream_type: STREAM_TYPE_SUBTITLE, elementary_pid: 0x0200, descriptors: vec![] },
                PmtStream { stream_type: STREAM_TYPE_SUBTITLE, elementary_pid: 0x0201, descriptors: vec![] },
            ],
        };

        detector.update_from_pmt(&pmt, Some(1));
        assert_eq!(detector.subtitle_pids, vec![0x0200, 0x0201]);
        assert_eq!(detector.total_pids, 4);
    }

    #[test]
    fn test_detector_codec_mapping() {
        assert_eq!(video_codec_from_stream_type(STREAM_TYPE_H264), Some(VideoCodec::H264));
        assert_eq!(video_codec_from_stream_type(STREAM_TYPE_HEVC), Some(VideoCodec::Hevc));
        assert_eq!(video_codec_from_stream_type(STREAM_TYPE_MPEG2_VIDEO), Some(VideoCodec::Mpeg2));
        assert_eq!(video_codec_from_stream_type(0xFF), None);

        assert_eq!(audio_codec_from_stream_type(STREAM_TYPE_AAC), Some(AudioCodec::Aac));
        assert_eq!(audio_codec_from_stream_type(STREAM_TYPE_AC3), Some(AudioCodec::Ac3));
        assert_eq!(audio_codec_from_stream_type(STREAM_TYPE_EAC3), Some(AudioCodec::Eac3));
        assert_eq!(audio_codec_from_stream_type(STREAM_TYPE_MP2_A), Some(AudioCodec::Mp2));
        assert_eq!(audio_codec_from_stream_type(STREAM_TYPE_MP2_B), Some(AudioCodec::Mp2));
        assert_eq!(audio_codec_from_stream_type(0xFF), None);
    }

    #[test]
    fn test_detector_feed_h264_aac_pes() {
        let mut detector = StreamInfoDetector::new();
        let pmt = PmtInfo {
            pcr_pid: 0x0100,
            version: 0,
            streams: vec![
                PmtStream { stream_type: STREAM_TYPE_H264, elementary_pid: 0x0100, descriptors: vec![] },
                PmtStream { stream_type: STREAM_TYPE_AAC, elementary_pid: 0x0101, descriptors: vec![] },
            ],
        };
        detector.update_from_pmt(&pmt, Some(1));

        // Build a PES packet containing an H.264 SPS NAL
        let sps_nal = make_h264_sps_baseline_1080p();
        let mut video_pes = make_pes_with_pts(90000);
        // Add NAL start code + SPS after the PES header
        video_pes.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        video_pes.extend_from_slice(&sps_nal);

        // Feed video PES
        detector.feed_packet(0x0100, true, &video_pes);

        // Build an audio PES with ADTS header
        let mut audio_pes = Vec::new();
        audio_pes.extend_from_slice(&[0x00, 0x00, 0x01, 0xC0]); // PES start code, audio stream
        audio_pes.extend_from_slice(&[0x00, 0x00]); // packet length
        audio_pes.extend_from_slice(&[0x80, 0x80, 0x05]); // flags + header length
        audio_pes.extend_from_slice(&[0x21, 0x00, 0x01, 0x00, 0x01]); // PTS
        audio_pes.extend_from_slice(&make_adts_header(1, 3, 2)); // AAC-LC, 48kHz, stereo

        detector.feed_packet(0x0101, true, &audio_pes);

        // Both should be detected now
        assert!(detector.stream_info.is_some());
        let info = detector.stream_info.as_ref().unwrap();
        assert_eq!(info.video_codec, Some(VideoCodec::H264));
        assert_eq!(info.audio_codec, Some(AudioCodec::Aac));

        let vd = info.video_details.as_ref().unwrap();
        assert_eq!(vd.width, 1920);
        assert_eq!(vd.height, 1080);
        assert_eq!(vd.profile, 66); // Baseline

        let ad = info.audio_details.as_ref().unwrap();
        assert_eq!(ad.profile, 1); // LC
        assert_eq!(ad.sample_rate, 48000);
        assert_eq!(ad.channels, 2);
    }

    #[test]
    fn test_detector_no_audio_stream() {
        let mut detector = StreamInfoDetector::new();
        let pmt = PmtInfo {
            pcr_pid: 0x0100,
            version: 0,
            streams: vec![
                PmtStream { stream_type: STREAM_TYPE_H264, elementary_pid: 0x0100, descriptors: vec![] },
            ],
        };
        detector.update_from_pmt(&pmt, Some(1));

        // Feed video only — should finalize without audio
        let sps_nal = make_h264_sps_baseline_1080p();
        let mut video_pes = make_pes_with_pts(90000);
        video_pes.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        video_pes.extend_from_slice(&sps_nal);

        detector.feed_packet(0x0100, true, &video_pes);

        assert!(detector.stream_info.is_some());
        let info = detector.stream_info.as_ref().unwrap();
        assert_eq!(info.video_codec, Some(VideoCodec::H264));
        assert_eq!(info.audio_codec, None);
        assert!(info.audio_details.is_none());
    }

    #[test]
    fn test_detector_already_detected_ignores_packets() {
        let mut detector = StreamInfoDetector::new();
        let pmt = PmtInfo {
            pcr_pid: 0x0100,
            version: 0,
            streams: vec![
                PmtStream { stream_type: STREAM_TYPE_MPEG2_VIDEO, elementary_pid: 0x0100, descriptors: vec![] },
            ],
        };
        detector.update_from_pmt(&pmt, Some(1));

        // MPEG2 is detected immediately without detailed parsing
        let video_pes = make_pes_with_pts(90000);
        detector.feed_packet(0x0100, true, &video_pes);

        assert!(detector.stream_info.is_some());

        // Feeding more packets should be a no-op
        detector.feed_packet(0x0100, true, &video_pes);
        // No crash, still detected
        assert!(detector.stream_info.is_some());
    }

    #[test]
    fn test_detector_reset() {
        let mut detector = StreamInfoDetector::new();
        let pmt = PmtInfo {
            pcr_pid: 0x0100,
            version: 0,
            streams: vec![
                PmtStream { stream_type: STREAM_TYPE_MPEG2_VIDEO, elementary_pid: 0x0100, descriptors: vec![] },
            ],
        };
        detector.update_from_pmt(&pmt, Some(1));

        let video_pes = make_pes_with_pts(90000);
        detector.feed_packet(0x0100, true, &video_pes);
        assert!(detector.stream_info.is_some());

        detector.reset();
        assert!(detector.stream_info.is_none());
        assert!(!detector.video_detected);
    }

    #[test]
    fn test_pes_accumulator_cap() {
        let mut acc = PesAccumulator::new();
        // Start with a small payload
        acc.start(&[0x01; 64]);
        assert!(acc.active);
        assert_eq!(acc.data().len(), 64);

        // Append data just under the cap — should succeed
        let chunk = vec![0x02; 1024];
        for _ in 0..126 {
            acc.append(&chunk);
        }
        assert!(acc.active);
        assert_eq!(acc.data().len(), 64 + 126 * 1024);

        // Append that pushes over PES_ACCUMULATOR_CAP — buffer should be cleared and deactivated
        let overflow = vec![0x03; 2048];
        acc.append(&overflow);
        assert!(!acc.active);
        assert!(acc.data().is_empty());

        // Subsequent appends are no-ops while inactive
        acc.append(&[0x04; 64]);
        assert!(acc.data().is_empty());

        // A new PUSI start reactivates the accumulator
        acc.start(&[0x05; 32]);
        assert!(acc.active);
        assert_eq!(acc.data().len(), 32);
    }

    #[test]
    fn test_pmt_version_extraction() {
        let mut section = make_pmt_section(0x0100, &[
            (STREAM_TYPE_H264, 0x0100),
        ]);

        // Default make_pmt_section sets version=0 (byte 5 = 0xC1 = 11_00000_1)
        let info = parse_pmt(&section).unwrap();
        assert_eq!(info.version, 0);

        // Set version to 15: byte 5 = 11_01111_1 = 0xDF
        section[5] = 0xDF;
        let info = parse_pmt(&section).unwrap();
        assert_eq!(info.version, 15);

        // Set version to 31 (max): byte 5 = 11_11111_1 = 0xFF
        section[5] = 0xFF;
        let info = parse_pmt(&section).unwrap();
        assert_eq!(info.version, 31);
    }
}
