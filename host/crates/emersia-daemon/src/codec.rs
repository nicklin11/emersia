//! H.264 / HEVC bitstream handling: Annex-B parsing, access-unit assembly,
//! parameter-set capture and keyframe detection.
//!
//! The encoder emits a continuous Annex-B byte stream. A client needs three
//! things from it, and this module is where they are separated:
//!
//! 1. **Access units** — one picture's worth of NAL units, so a frame can be
//!    timestamped and flagged as a keyframe or not.
//! 2. **Parameter sets** (SPS/PPS, and VPS for HEVC) — needed before anything
//!    can be decoded. The libworkspaceVR prototype proved that repeating these
//!    *in every packet* is what lets a client that joins mid-stream start
//!    decoding immediately rather than waiting for the next keyframe (ADR 0003).
//!    That is repeated here, not merely carried on keyframes.
//! 3. **A keyframe flag** — so the transport can mark a record as one a decoder
//!    can start from.
//!
//! Annex-B is a byte-oriented format with 3- and 4-byte start codes, so the
//! parser is written against bytes rather than assuming alignment. It is
//! deliberately strict: trailing garbage or a truncated NAL is reported rather
//! than silently forwarded, because a half-parsed NAL produces a corrupt frame
//! that is very hard to diagnose from the far end.

use std::fmt;

/// Video codec carried in a transport payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    H264,
    Hevc,
}

impl Codec {
    /// RTP-style dynamic payload type (ADR 0003). 96 is the conventional type
    /// for H.264; 98 is the conventional type for HEVC.
    pub fn payload_type(self) -> u8 {
        match self {
            Self::H264 => 96,
            Self::Hevc => 98,
        }
    }

    /// Map a payload type back to a codec.
    pub fn from_payload_type(pt: u8) -> Option<Self> {
        match pt {
            96 => Some(Self::H264),
            98 => Some(Self::Hevc),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::Hevc => "hevc",
        }
    }
}

impl fmt::Display for Codec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// H.264 NAL unit types we care about (ITU-T H.264 table 7-1).
mod h264 {
    #[allow(dead_code)]
    pub const SLICE_NON_IDR: u8 = 1;
    pub const IDR: u8 = 5;
    pub const SEI: u8 = 6;
    pub const SPS: u8 = 7;
    pub const PPS: u8 = 8;
    pub const AUD: u8 = 9;
}

/// HEVC NAL unit types (ITU-T H.265 table 7-1).
mod hevc {
    /// Parameter sets: VPS, SPS and PPS all share this type, distinguished by
    /// the first two bits of the payload.
    pub const PARAMETER_SET: u8 = 32;
    pub const IDR_W_RADL: u8 = 19;
    pub const IDR_N_LP: u8 = 20;
    pub const CRA: u8 = 21;
    pub const SEI_PREFIX: u8 = 39;
    pub const SEI_SUFFIX: u8 = 40;
    /// A non-key trailing slice.
    #[allow(dead_code)]
    pub const TRAIL_R: u8 = 1;
}

/// One NAL unit, in Annex-B form (including its start code).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NalUnit {
    /// The NAL unit type, after removing the forbidden-zero bit.
    pub nal_type: u8,
    /// Header bytes only — the `nal_unit_header` of the RBSP, enough to tell
    /// HEVC VPS/SPS/PPS apart.
    pub header: [u8; 2],
    /// The complete Annex-B fragment: start code, header, and payload.
    pub annexb: Vec<u8>,
}

impl NalUnit {
    /// True if this is an SPS or PPS (or, for HEVC, any parameter set).
    pub fn is_parameter_set(&self, codec: Codec) -> bool {
        match codec {
            Codec::H264 => matches!(self.nal_type, h264::SPS | h264::PPS),
            Codec::Hevc => self.nal_type == hevc::PARAMETER_SET,
        }
    }

    /// True if this NAL marks a decodable starting point.
    pub fn is_keyframe_nal(&self, codec: Codec) -> bool {
        match codec {
            Codec::H264 => self.nal_type == h264::IDR,
            Codec::Hevc => {
                matches!(self.nal_type, hevc::IDR_W_RADL | hevc::IDR_N_LP | hevc::CRA)
            }
        }
    }

    /// True if this carries supplemental information (not picture data).
    pub fn is_sei(&self, codec: Codec) -> bool {
        match codec {
            Codec::H264 => self.nal_type == h264::SEI,
            Codec::Hevc => matches!(self.nal_type, hevc::SEI_PREFIX | hevc::SEI_SUFFIX),
        }
    }

    /// True if this VCL NAL is the first slice of a new picture.
    ///
    /// A picture may be split across several slices, so the NAL type alone
    /// cannot say where one picture ends and the next begins — the slice
    /// header can. H.264 carries `first_mb_in_slice` (a `ue(v)`, zero on the
    /// first slice); HEVC carries `first_slice_segment_in_pic_flag` (the first
    /// bit). Both answer exactly the question an access-unit boundary needs.
    pub fn starts_new_picture(&self, codec: Codec) -> bool {
        // header[0..header_len] is the NAL unit header; the slice header
        // follows immediately after it.
        let hlen = match codec {
            Codec::H264 => 1,
            Codec::Hevc => 2,
        };
        let Some(slice) = self.annexb.get(self.start_code_len() + hlen..) else {
            return false;
        };
        match codec {
            Codec::H264 => exp_golomb_first_mb(slice) == Some(0),
            Codec::Hevc => slice.first().is_some_and(|b| b & 0x80 != 0),
        }
    }

    /// Length of this unit's Annex-B start code (3 or 4 bytes).
    fn start_code_len(&self) -> usize {
        if self.annexb.len() >= 4 && self.annexb[..4] == [0, 0, 0, 1] {
            4
        } else {
            3
        }
    }

    /// True if this is a slice (VCL) NAL, i.e. actual picture data.
    pub fn is_vcl(&self, codec: Codec) -> bool {
        match codec {
            Codec::H264 => matches!(self.nal_type, 1..=5),
            Codec::Hevc => (0..=31).contains(&self.nal_type),
        }
    }
}

/// One encoded picture: a complete access unit in Annex-B form.
#[derive(Debug, Clone)]
pub struct AccessUnit {
    /// The codec this access unit belongs to.
    pub codec: Codec,
    /// All NAL units of this picture, in stream order. Exposed for
    /// diagnostics and tests; the transport sends `annexb` verbatim.
    #[allow(dead_code)]
    pub nals: Vec<NalUnit>,
    /// The access unit re-serialised as Annex-B bytes.
    pub annexb: Vec<u8>,
    /// True if this picture can be decoded without any earlier picture.
    pub keyframe: bool,
    /// Presentation order hint, incremented per access unit.
    #[allow(dead_code)]
    pub index: u64,
}

impl AccessUnit {
    /// RTP-style payload type for this picture's codec (ADR 0003).
    pub fn codec_payload_type(&self) -> u8 {
        self.codec.payload_type()
    }

    /// Bytes of real picture data, excluding parameter sets and SEI.
    #[allow(dead_code)]
    pub fn payload_len(&self, codec: Codec) -> usize {
        self.nals
            .iter()
            .filter(|n| n.is_vcl(codec))
            .map(|n| n.annexb.len())
            .sum()
    }
}

/// Errors from bitstream parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecError {
    /// The stream contained no NAL start code.
    NoStartCode,
    /// A NAL unit was cut short by the end of the buffer.
    TruncatedNal,
    /// A NAL unit had no payload after its header.
    EmptyNal,
    /// More parameter sets accumulated than we are willing to carry per packet.
    TooManyParameterSets(usize),
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoStartCode => write!(f, "bitstream contains no Annex-B start code"),
            Self::TruncatedNal => write!(f, "bitstream ended inside a NAL unit"),
            Self::EmptyNal => write!(f, "NAL unit has no payload"),
            Self::TooManyParameterSets(n) => {
                write!(
                    f,
                    "refusing to carry {n} bytes of parameter sets in one packet"
                )
            }
        }
    }
}

impl std::error::Error for CodecError {}

/// Maximum parameter-set bytes we will prefix to each packet.
///
/// This is a sanity bound, not a tuning knob: real streams send an SPS and a
/// PPS (plus a VPS for HEVC), typically well under 100 bytes together. A stream
/// that sends hundreds is either corrupt or is using parameter sets in a way
/// this MVP does not support, and letting it through would let a peer inflate
/// every packet.
pub const MAX_PARAMETER_SET_BYTES: usize = 1024;

/// Incremental Annex-B parser.
///
/// Feed it whatever the encoder produced; it yields complete access units as
/// they become available.
///
/// **Access-unit boundaries are not marked by start codes.** A start code only
/// separates one NAL from the next; which NALs belong to the same picture has to
/// be inferred. The rule used here, and the one every Annex-B consumer needs:
///
/// - an Access Unit Delimiter (AUD, H.264 type 9) always begins a new access
///   unit, which is the unambiguous signal;
/// - otherwise, a parameter set or SEI that arrives *after* VCL NALs have
///   already been seen means the previous picture ended and a new one begins.
///
/// The second rule is what makes in-band parameter sets work. Because we repeat
/// SPS/PPS on every packet (ADR 0003), a naive "everything before the last NAL"
/// rule would merge consecutive pictures into one enormous access unit.
///
/// The parser is incremental and byte-oriented: encoder output arrives in
/// arbitrary chunks, so a NAL is only emitted once the start code of the *next*
/// one has arrived to bound it.
#[derive(Debug)]
pub struct AnnexBParser {
    codec: Codec,
    /// Raw encoder bytes not yet consumed.
    buffer: Vec<u8>,
    /// Parameter sets most recently committed, carried forward to every packet.
    parameter_sets: Vec<u8>,
    /// Sets being collected for the next picture. They are not published until
    /// a VCL NAL confirms that the group belongs to a complete access unit.
    pending_parameter_sets: Vec<u8>,
    /// True while collecting the parameter sets for the current access unit.
    collecting_sets: bool,
    /// NAL units of the access unit currently being assembled.
    current: Vec<NalUnit>,
    /// Whether `current` already contains a VCL (picture) NAL.
    current_has_vcl: bool,
    /// Access units produced, used for the presentation index.
    index: u64,
    /// Guard against unbounded growth if the stream is not Annex-B at all.
    max_buffer: usize,
}

impl AnnexBParser {
    /// A parser for `codec` with a default buffer ceiling.
    pub fn new(codec: Codec) -> Self {
        Self {
            codec,
            buffer: Vec::new(),
            parameter_sets: Vec::new(),
            pending_parameter_sets: Vec::new(),
            collecting_sets: false,
            current: Vec::new(),
            current_has_vcl: false,
            index: 0,
            // A 1080p60 I-frame at a sane bitrate is tens of kilobytes. Allow
            // generous headroom for a very high bitrate but still refuse to
            // grow without bound on garbage input.
            max_buffer: 8 * 1024 * 1024,
        }
    }

    /// The parameter-set prefix currently in force.
    pub fn parameter_sets(&self) -> &[u8] {
        &self.parameter_sets
    }

    /// Feed encoder output, returning every access unit that completed.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<AccessUnit>, CodecError> {
        self.buffer.extend_from_slice(bytes);
        if self.buffer.len() > self.max_buffer {
            return Err(CodecError::TruncatedNal);
        }
        let mut out = Vec::new();
        // Emit complete NALs for as long as a bounding start code is available.
        while let Some(nal) = self.take_next_nal(false)? {
            if let Some(au) = self.accept_nal(nal)? {
                out.push(au);
            }
        }
        Ok(out)
    }

    /// Flush at end of stream: the final NAL is bounded by the buffer end.
    pub fn finish(&mut self) -> Result<Vec<AccessUnit>, CodecError> {
        let mut out = Vec::new();
        if let Some(nal) = self.take_next_nal(true)? {
            if let Some(au) = self.accept_nal(nal)? {
                out.push(au);
            }
        }
        if let Some(au) = self.flush_current() {
            out.push(au);
        }
        Ok(out)
    }

    /// Remove and return the next complete NAL unit, or `None` if the buffer
    /// does not yet contain one.
    ///
    /// With `at_end`, the last NAL in the buffer is considered complete.
    fn take_next_nal(&mut self, at_end: bool) -> Result<Option<NalUnit>, CodecError> {
        let starts = start_code_positions(&self.buffer);
        if starts.is_empty() {
            if self.buffer.len() > MAX_PARAMETER_SET_BYTES {
                return Err(CodecError::NoStartCode);
            }
            return Ok(None);
        }
        // A NAL is only complete once the next start code bounds it, unless we
        // are at the end of the stream.
        let end = match starts.get(1) {
            Some(&(next, _)) => next,
            None if at_end => self.buffer.len(),
            None => return Ok(None),
        };
        let (start, sc_len) = starts[0];
        let body_start = start + sc_len;
        if end <= body_start {
            // Empty NAL (start code immediately followed by another). Drop it.
            self.buffer.drain(..end);
            return self.take_next_nal(at_end);
        }
        // Copy out what we need before mutating the buffer.
        let (nal_type, header, annexb) = {
            let body = &self.buffer[body_start..end];
            let nal_type = nal_type(body, self.codec).ok_or(CodecError::TruncatedNal)?;
            let hlen = header_len(body, self.codec);
            if body.len() <= hlen {
                return Err(CodecError::EmptyNal);
            }
            (
                nal_type,
                [body[0], *body.get(hlen).unwrap_or(&0)],
                self.buffer[start..end].to_vec(),
            )
        };
        // Consume through the end of this NAL. Any trailing zero bytes that
        // belonged to the next start code stay in the buffer; the scanner
        // absorbs them into that code's length.
        self.buffer.drain(..end);
        Ok(Some(NalUnit {
            nal_type,
            header,
            annexb,
        }))
    }

    /// Add a NAL to the access unit under construction, emitting the previous
    /// one if this NAL starts a new picture.
    fn accept_nal(&mut self, nal: NalUnit) -> Result<Option<AccessUnit>, CodecError> {
        let is_aud = self.codec == Codec::H264 && nal.nal_type == h264::AUD;
        // A picture boundary is either explicit (AUD) or read out of the slice
        // header. Parameter sets and SEI arriving after picture data also
        // imply one, which keeps the rule working when a slice header cannot be
        // parsed.
        let slice_boundary =
            self.current_has_vcl && nal.is_vcl(self.codec) && nal.starts_new_picture(self.codec);
        let marker_boundary =
            self.current_has_vcl && (nal.is_parameter_set(self.codec) || nal.is_sei(self.codec));
        let starts_new_picture = is_aud || slice_boundary || marker_boundary;

        let flushed = if starts_new_picture && !self.current.is_empty() {
            self.flush_current()
        } else {
            None
        };

        // Encoders repeat parameter sets on each keyframe. Keep only the
        // current group: once picture data has closed a group, the next
        // parameter set starts a replacement rather than extending history.
        if nal.is_parameter_set(self.codec) {
            if !self.collecting_sets {
                self.pending_parameter_sets.clear();
                self.collecting_sets = true;
            }
            self.pending_parameter_sets.extend_from_slice(&nal.annexb);
            if self.pending_parameter_sets.len() > MAX_PARAMETER_SET_BYTES {
                return Err(CodecError::TooManyParameterSets(
                    self.pending_parameter_sets.len(),
                ));
            }
        }
        if nal.is_vcl(self.codec) {
            if self.collecting_sets {
                self.parameter_sets = std::mem::take(&mut self.pending_parameter_sets);
                self.collecting_sets = false;
            }
            self.current_has_vcl = true;
        }
        self.current.push(nal);
        Ok(flushed)
    }

    /// Emit the access unit under construction, if it holds any NAL.
    fn flush_current(&mut self) -> Option<AccessUnit> {
        if self.current.is_empty() {
            return None;
        }
        let keyframe = self.current.iter().any(|n| n.is_keyframe_nal(self.codec));
        let mut annexb = Vec::new();
        for n in &self.current {
            annexb.extend_from_slice(&n.annexb);
        }
        let au = AccessUnit {
            codec: self.codec,
            nals: std::mem::take(&mut self.current),
            annexb,
            keyframe,
            index: self.index,
        };
        self.current_has_vcl = false;
        self.index += 1;
        Some(au)
    }
}

/// Positions and lengths of Annex-B start codes in `buf`.
///
/// A start code is `00 00 01` (3 bytes) or `00 00 00 01` (4 bytes). We return
/// the offset of the code and its length.
///
/// Two details matter and are easy to get wrong:
///
/// - When `00 00 01` is found, the byte *before* it decides the width. If that
///   byte is also zero the code is the 4-byte form. Reporting a 3-byte code
///   there would leave a stray zero byte inside the previous NAL's payload.
/// - Only one leading zero may be absorbed. A longer run means trailing zero
///   bytes of the previous NAL, which belong to it, not to this code.
fn start_code_positions(buf: &[u8]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 3 <= buf.len() {
        if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 {
            let (start, len) = if i > 0 && buf[i - 1] == 0 {
                (i - 1, 4)
            } else {
                (i, 3)
            };
            out.push((start, len));
            i += len;
        } else {
            i += 1;
        }
    }
    out
}

/// Read `first_mb_in_slice` from the start of an H.264 slice header.
///
/// Returns `None` if the header is truncated or malformed. Emulation prevention
/// bytes (`00 00 03`) are skipped, because the encoder may insert one inside
/// the header and it is not part of the syntax.
fn exp_golomb_first_mb(slice: &[u8]) -> Option<u32> {
    let mut reader = BitReader::new(slice);
    reader.read_ue()
}

/// Minimal bit reader over an RBSP segment, skipping emulation prevention.
struct BitReader<'a> {
    bytes: &'a [u8],
    byte: usize,
    bit: u32,
}

impl<'a> BitReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            byte: 0,
            bit: 0,
        }
    }

    /// Next raw bit, or `None` at end of input.
    ///
    /// After reading a `00 00` prefix the next byte is skipped if it is `03`,
    /// which is the escape that prevents a start code from appearing inside a
    /// NAL unit's payload.
    fn read_bit(&mut self) -> Option<u8> {
        if self.byte >= self.bytes.len() {
            return None;
        }
        let value = (self.bytes[self.byte] >> (7 - self.bit)) & 1;
        self.bit += 1;
        if self.bit == 8 {
            self.bit = 0;
            // Peek for the emulation prevention sequence.
            if self.bytes.get(self.byte) == Some(&0)
                && self.bytes.get(self.byte + 1) == Some(&0)
                && self.bytes.get(self.byte + 2) == Some(&3)
            {
                self.byte += 3;
            } else {
                self.byte += 1;
            }
        }
        Some(value)
    }

    /// Read an unsigned Exp-Golomb code: `ue(v)`.
    ///
    /// Leading zero bits give the value's magnitude; the remaining bits carry
    /// it. Malformed input (all zeros to the end) yields `None` rather than a
    /// plausible-looking number.
    fn read_ue(&mut self) -> Option<u32> {
        let mut leading = 0u32;
        while self.read_bit()? == 0 {
            leading += 1;
            if leading > 32 {
                return None;
            }
        }
        if leading == 0 {
            return Some(0);
        }
        let mut value = 1u32;
        for _ in 0..leading {
            value = (value << 1) | u32::from(self.read_bit()?);
        }
        Some(value - 1)
    }
}

/// Extract the NAL unit type from a NAL body (header + payload, no start code).
fn nal_type(body: &[u8], codec: Codec) -> Option<u8> {
    let first = *body.first()?;
    match codec {
        Codec::H264 => Some(first & 0x1F),
        // HEVC: the type occupies bits 1..6 of the first byte.
        Codec::Hevc => Some((first >> 1) & 0x3F),
    }
}

/// Length of the NAL unit header for a codec (excluding start code).
fn header_len(body: &[u8], codec: Codec) -> usize {
    match codec {
        // H.264 always has a 1-byte nal_unit_header.
        Codec::H264 => 1,
        // HEVC always has a 2-byte nal_unit_header.
        Codec::Hevc => 2,
    }
    .max(1)
    .min(body.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an Annex-B NAL of `codec` with the given type and payload length.
    ///
    /// H.264 stores the type in the low 5 bits of byte 0; HEVC stores it in
    /// bits 1..6, so the header byte differs per codec.
    fn nal(codec: Codec, nal_type: u8, payload_len: usize) -> Vec<u8> {
        let header0 = match codec {
            Codec::H264 => nal_type & 0x1F,
            Codec::Hevc => (nal_type & 0x3F) << 1,
        };
        let mut v = vec![0, 0, 0, 1, header0];
        v.extend(std::iter::repeat_n(0xAA, payload_len));
        v
    }

    /// An H.264 NAL, for the many H.264-focused tests below.
    fn h264_nal(nal_type: u8, payload_len: usize) -> Vec<u8> {
        nal(Codec::H264, nal_type, payload_len)
    }

    /// An HEVC VCL NAL. `first_slice_segment_in_pic_flag` (bit 7 of the byte
    /// after the 2-byte NAL header) marks the start of a new picture; the rest
    /// of the filler is opaque.
    fn hevc_vcl(nal_type: u8, new_picture: bool, payload_len: usize) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 1, (nal_type & 0x3F) << 1, 0];
        v.push(if new_picture { 0x80 } else { 0x00 });
        v.extend(std::iter::repeat_n(0xAA, payload_len));
        v
    }

    /// An H.264 access unit: parameter sets plus one slice.
    ///
    /// Real encoders send parameter sets with a keyframe and omit them from
    /// delta frames, which is exactly the case the in-band prefix exists to
    /// cover, so `with_sets` exists to model both.
    fn access_unit(keyframe: bool) -> Vec<u8> {
        access_unit_with(keyframe, true)
    }

    fn access_unit_with(keyframe: bool, with_sets: bool) -> Vec<u8> {
        let mut v = Vec::new();
        if with_sets {
            v.extend(h264_nal(h264::SPS, 10));
            v.extend(h264_nal(h264::PPS, 4));
        }
        if keyframe {
            v.extend(h264_nal(h264::IDR, 100));
        } else {
            v.extend(h264_nal(h264::SLICE_NON_IDR, 100));
        }
        v
    }

    /// Push and flush, which is how a caller drains a finite stream.
    fn parse_all(parser: &mut AnnexBParser, stream: &[u8]) -> Vec<AccessUnit> {
        let mut aus = parser.push(stream).unwrap();
        aus.extend(parser.finish().unwrap());
        aus
    }

    #[test]
    fn codec_payload_types_roundtrip() {
        assert_eq!(Codec::H264.payload_type(), 96);
        assert_eq!(Codec::Hevc.payload_type(), 98);
        assert_eq!(Codec::from_payload_type(96), Some(Codec::H264));
        assert_eq!(Codec::from_payload_type(98), Some(Codec::Hevc));
        assert_eq!(Codec::from_payload_type(97), None);
    }

    #[test]
    fn start_code_positions_distinguishes_three_and_four_byte_codes() {
        // Layout: 00 00 00 01 | 67 01 02 | 00 00 00 01 | 68 03
        let buf = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3];
        let pos = start_code_positions(&buf);
        assert_eq!(pos.len(), 2);
        assert_eq!(pos[0], (0, 4), "first 4-byte start code");
        // The zero at index 7 is the leading byte of the second 4-byte code,
        // not a trailing zero of the previous NAL.
        assert_eq!(pos[1], (7, 4), "second 4-byte start code");
    }

    #[test]
    fn three_byte_start_code_is_not_claimed_as_four() {
        // `aa 00 00 01` — the byte before the code is not zero.
        let buf = [0xAA, 0, 0, 1, 0x67, 0x11];
        assert_eq!(start_code_positions(&buf), vec![(1, 3)]);
    }

    #[test]
    fn extra_leading_zeros_are_not_absorbed() {
        // Five zeros then 01. The last four bytes form the 4-byte start code;
        // the earlier zeros are trailing_zero_8bits of whatever preceded, so
        // they must not be pulled into this code.
        let buf = [0, 0, 0, 0, 0, 1, 0x67];
        assert_eq!(start_code_positions(&buf), vec![(2, 4)]);
    }

    #[test]
    fn annexb_is_split_into_nals() {
        let mut p = AnnexBParser::new(Codec::H264);
        let stream = [access_unit(true), access_unit(false)].concat();
        let aus = parse_all(&mut p, &stream);
        assert_eq!(aus.len(), 2, "two pictures");
        for au in &aus {
            assert_eq!(au.nals.len(), 3, "SPS + PPS + one slice each");
        }
    }

    #[test]
    fn in_band_parameter_sets_split_access_units() {
        // This is the case the old parser got wrong: parameter sets repeat, so
        // "everything before the last NAL" merges two pictures into one.
        let mut p = AnnexBParser::new(Codec::H264);
        let aus = parse_all(&mut p, &[access_unit(true), access_unit(false)].concat());
        assert_eq!(aus.len(), 2, "a repeated SPS ends the previous picture");
    }

    #[test]
    fn keyframe_is_detected_from_idr() {
        let mut p = AnnexBParser::new(Codec::H264);
        let aus = parse_all(&mut p, &[access_unit(true), access_unit(false)].concat());
        assert!(aus[0].keyframe, "IDR present means keyframe");
        assert!(!aus[1].keyframe, "no IDR means not a keyframe");
    }

    #[test]
    fn aud_delimits_access_units() {
        // With an explicit AUD, the boundary is unambiguous.
        let mut p = AnnexBParser::new(Codec::H264);
        let stream = [
            h264_nal(h264::AUD, 2),
            h264_nal(h264::SPS, 10),
            h264_nal(h264::PPS, 4),
            h264_nal(h264::IDR, 50),
            h264_nal(h264::AUD, 2),
            h264_nal(h264::SLICE_NON_IDR, 50),
        ]
        .concat();
        let aus = parse_all(&mut p, &stream);
        assert_eq!(aus.len(), 2);
        assert!(aus[0].keyframe);
        assert!(!aus[1].keyframe);
    }

    #[test]
    fn parameter_sets_are_captured_and_repeated_in_every_packet() {
        let mut p = AnnexBParser::new(Codec::H264);
        // Only the keyframe carries parameter sets, as a real encoder does.
        // The delta frames must be delimited by their slice headers instead.
        let stream = [
            access_unit_with(true, true),
            access_unit_with(false, false),
            access_unit_with(false, false),
        ]
        .concat();
        let aus = parse_all(&mut p, &stream);
        assert_eq!(aus.len(), 3, "slice headers delimit the pictures");
        let sets = p.parameter_sets();
        assert!(!sets.is_empty(), "parameter sets were captured");
        // The parameter sets must survive to the last picture, because every
        // packet is prefixed with them (ADR 0003) and a delta packet carries
        // none of its own.
        assert!(
            sets.windows(4).any(|w| w == [0, 0, 0, 1]),
            "captured prefix is Annex-B"
        );
        assert!(
            !aus[2].nals.iter().any(|n| n.is_parameter_set(Codec::H264)),
            "the delta picture itself carries no parameter sets"
        );
    }

    #[test]
    fn hevc_parameter_sets_and_keyframes() {
        let mut p = AnnexBParser::new(Codec::Hevc);
        // First picture: IDR. Second: one trailing slice. A third VCL NAL
        // with the flag set must open a third picture, proving the boundary is
        // read from the slice header and not guessed from NAL types.
        let stream = [
            nal(Codec::Hevc, hevc::PARAMETER_SET, 20),
            hevc_vcl(hevc::IDR_W_RADL, true, 100),
            hevc_vcl(hevc::TRAIL_R, true, 60),
            hevc_vcl(hevc::TRAIL_R, true, 40),
        ]
        .concat();
        let aus = parse_all(&mut p, &stream);
        assert_eq!(aus.len(), 3, "each flagged slice opens a picture");
        assert!(aus[0].keyframe, "IDR_W_RADL is a keyframe");
        assert!(!aus[1].keyframe, "a trailing slice is not");
        assert!(!aus[2].keyframe);
    }

    #[test]
    fn hevc_cra_counts_as_keyframe() {
        let mut p = AnnexBParser::new(Codec::Hevc);
        let stream = [
            nal(Codec::Hevc, hevc::PARAMETER_SET, 12),
            hevc_vcl(hevc::CRA, true, 80),
            hevc_vcl(hevc::TRAIL_R, true, 40),
        ]
        .concat();
        let aus = parse_all(&mut p, &stream);
        assert_eq!(aus.len(), 2, "CRA picture, then the trailing picture");
        assert!(aus[0].keyframe, "CRA is a random-access point");
        assert!(!aus[1].keyframe);
    }

    #[test]
    fn chunked_input_matches_whole_stream() {
        let stream = [access_unit(true), access_unit(false), access_unit(true)].concat();
        let mut whole = AnnexBParser::new(Codec::H264);
        let expected = parse_all(&mut whole, &stream);

        // Feed the identical bytes in small, awkward chunks that split NAL
        // headers and start codes.
        let mut p = AnnexBParser::new(Codec::H264);
        let mut got = Vec::new();
        for piece in stream.chunks(7) {
            got.extend(p.push(piece).unwrap());
        }
        got.extend(p.finish().unwrap());

        assert_eq!(expected.len(), got.len(), "same number of access units");
        for (a, b) in expected.iter().zip(got.iter()) {
            assert_eq!(a.annexb, b.annexb, "same bytes");
            assert_eq!(a.keyframe, b.keyframe, "same keyframe flag");
            assert_eq!(a.index, b.index, "same index");
        }
    }

    #[test]
    fn one_byte_at_a_time_is_identical() {
        let stream = [access_unit(true), access_unit(false)].concat();
        let mut whole = AnnexBParser::new(Codec::H264);
        let expected = parse_all(&mut whole, &stream);

        let mut p = AnnexBParser::new(Codec::H264);
        let mut got = Vec::new();
        for byte in &stream {
            got.extend(p.push(std::slice::from_ref(byte)).unwrap());
        }
        got.extend(p.finish().unwrap());
        assert_eq!(expected.len(), got.len());
        for (a, b) in expected.iter().zip(got.iter()) {
            assert_eq!(a.annexb, b.annexb);
        }
    }

    #[test]
    fn garbage_without_start_code_is_rejected() {
        let mut p = AnnexBParser::new(Codec::H264);
        let junk = vec![0xFF; MAX_PARAMETER_SET_BYTES + 10];
        assert_eq!(p.push(&junk).unwrap_err(), CodecError::NoStartCode);
    }

    #[test]
    fn empty_stream_yields_nothing() {
        let mut p = AnnexBParser::new(Codec::H264);
        assert!(p.push(&[]).unwrap().is_empty());
        assert!(p.finish().unwrap().is_empty());
    }

    #[test]
    fn repeated_parameter_sets_replace_rather_than_accumulate() {
        let one_set_len = h264_nal(h264::SPS, 10).len() + h264_nal(h264::PPS, 4).len();
        let stream = (0..100).flat_map(|_| access_unit(true)).collect::<Vec<_>>();
        let mut p = AnnexBParser::new(Codec::H264);
        let aus = parse_all(&mut p, &stream);
        assert_eq!(aus.len(), 100, "every keyframe is its own picture");
        assert_eq!(
            p.parameter_sets().len(),
            one_set_len,
            "only the latest SPS and PPS remain in force"
        );
    }

    #[test]
    fn partial_new_parameter_set_does_not_replace_the_in_force_set() {
        let old_set = access_unit(true);
        let new_sps = h264_nal(h264::SPS, 30);
        let new_pps = h264_nal(h264::PPS, 20);
        let mut p = AnnexBParser::new(Codec::H264);
        let first = parse_all(&mut p, &old_set);
        assert_eq!(first.len(), 1);
        let old_len = p.parameter_sets().len();
        assert!(p.push(&new_sps).unwrap().is_empty());
        assert_eq!(p.parameter_sets().len(), old_len);
        assert!(p.push(&new_pps).unwrap().is_empty());
        assert_eq!(p.parameter_sets().len(), old_len);
        assert!(p.push(&h264_nal(h264::IDR, 100)).unwrap().is_empty());
        assert_eq!(p.parameter_sets().len(), old_len);
        p.finish().unwrap();
        assert_eq!(p.parameter_sets().len(), new_sps.len() + new_pps.len());
    }

    /// Parse a real encoder bitstream captured into `tests/fixtures`.
    ///
    /// The expected frame count comes from `ffprobe -count_frames`, so this
    /// checks the parser against an independent implementation rather than
    /// against itself. Skipped when the fixtures are absent (e.g. a partial
    /// checkout) rather than failing.
    fn parse_fixture(codec: Codec, name: &str) -> Option<(Vec<AccessUnit>, usize)> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        let bytes = std::fs::read(&path).ok()?;
        let mut p = AnnexBParser::new(codec);
        let mut aus = p.push(&bytes).unwrap();
        aus.extend(p.finish().unwrap());
        let expected: usize = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(format!("{name}.frames")),
        )
        .ok()?
        .trim()
        .parse()
        .ok()?;
        Some((aus, expected))
    }

    #[test]
    fn real_h264_stream_matches_ffprobe_frame_count() {
        let Some((aus, expected)) = parse_fixture(Codec::H264, "real.h264") else {
            eprintln!("skipping: fixture missing");
            return;
        };
        assert_eq!(
            aus.len(),
            expected,
            "parser found the same number of pictures ffprobe did"
        );
        // A 30-frame GOP must yield exactly two keyframes.
        let keyframes = aus.iter().filter(|a| a.keyframe).count();
        assert_eq!(keyframes, 2, "one keyframe per GOP");
        // The first picture must be a keyframe: a stream that starts with a
        // delta frame is unusable.
        assert!(aus[0].keyframe, "stream opens on a keyframe");
    }

    #[test]
    fn real_hevc_stream_matches_ffprobe_frame_count() {
        let Some((aus, expected)) = parse_fixture(Codec::Hevc, "real.h265") else {
            eprintln!("skipping: fixture missing");
            return;
        };
        assert_eq!(aus.len(), expected);
        assert!(aus[0].keyframe, "stream opens on a keyframe");
        assert_eq!(aus.iter().filter(|a| a.keyframe).count(), 2);
    }

    #[test]
    fn real_stream_reassembles_to_decodable_bytes() {
        // The concatenation of every access unit's Annex-B bytes must reproduce
        // the input bitstream exactly. If it does, a decoder handed the
        // reassembled output sees exactly what ffmpeg produced.
        let Some(path) = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join("real.h264")
            .exists()
            .then(|| {
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures")
                    .join("real.h264")
            })
        else {
            eprintln!("skipping: fixture missing");
            return;
        };
        let original = std::fs::read(&path).unwrap();
        let mut p = AnnexBParser::new(Codec::H264);
        let mut aus = p.push(&original).unwrap();
        aus.extend(p.finish().unwrap());
        let rebuilt: Vec<u8> = aus.iter().flat_map(|a| a.annexb.clone()).collect();
        assert_eq!(
            rebuilt, original,
            "parse + reassemble is byte-identical to the encoder output"
        );
    }

    #[test]
    fn payload_len_counts_picture_bytes() {
        let mut p = AnnexBParser::new(Codec::H264);
        let aus = parse_all(&mut p, &access_unit(true));
        let au = &aus[0];
        let slice = au
            .nals
            .iter()
            .find(|n| n.is_keyframe_nal(Codec::H264))
            .unwrap();
        assert_eq!(au.payload_len(Codec::H264), slice.annexb.len());
    }
}
