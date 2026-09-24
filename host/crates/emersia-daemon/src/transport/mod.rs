//! UDP transport: packet framing, replay defence, and the forward-secret
//! handshake (ADR 0006).
//!
//! Everything above UDP is carried inside the AEAD, so an on-path attacker sees
//! only packet type, sizes and timing. Packet types are a single clear byte so
//! a receiver can route before any key exists; an unrecognised type, an unknown
//! device, or a device that is not active is **dropped without a reply**, per
//! the ADR 0005 default-deny rule.

pub mod handshake;
pub mod udp;

use crate::crypto::{Direction, Session};

/// Cleartext packet type byte.
pub mod packet_type {
    /// Device → host: start of the handshake.
    pub const HANDSHAKE_INIT: u8 = 0x01;
    /// Host → device: handshake response carrying the host ephemeral key.
    pub const HANDSHAKE_RESPONSE: u8 = 0x02;
    /// Device → host: final confirmation.
    pub const HANDSHAKE_CONFIRM: u8 = 0x03;
    /// Protected media/frame record.
    pub const DATA: u8 = 0x10;
}

/// Size of the cleartext frame header inside an encrypted record.
pub const FRAME_HEADER_LEN: usize = 24;

/// Conservative datagram budget for one record. Well under any typical path
/// MTU, so a frame fragment is never fragmented by the IP layer.
pub const RECORD_MTU: usize = 1200;

/// RTP video clock rate (ADR 0003): timestamps count in 90 kHz units.
pub const VIDEO_CLOCK_HZ: u32 = 90_000;

/// A decoded frame header, RTP-style per ADR 0003.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub version: u8,
    /// Bit 0 marks a keyframe (encoder can start decoding here).
    pub keyframe: bool,
    /// Codec/payload type, assigned by the encoder stage.
    pub payload_type: u8,
    /// Monotonic per-*record* sequence; also the AEAD associated data and the
    /// value the replay window checks. Every datagram has its own.
    pub sequence: u32,
    /// Identifies the frame this fragment belongs to, for reassembly. Shared by
    /// all fragments of one frame — which is exactly why it is separate from
    /// `sequence`: the replay window would otherwise drop fragments 2..n.
    pub frame_sequence: u32,
    /// Presentation timestamp in 90 kHz units.
    pub timestamp: u32,
    /// Length of *this record's* payload (a fragment, not the whole frame).
    pub payload_len: u16,
    /// Index of this fragment within the frame.
    pub frag_index: u16,
    /// Total fragments in this frame.
    pub frag_count: u16,
}

impl FrameHeader {
    /// Serialize the header (big-endian, 20 bytes).
    pub fn to_bytes(self) -> [u8; FRAME_HEADER_LEN] {
        let mut out = [0u8; FRAME_HEADER_LEN];
        out[0] = self.version;
        out[1] = u8::from(self.keyframe);
        out[2] = self.payload_type;
        out[3] = 0; // reserved
        out[4..8].copy_from_slice(&self.sequence.to_be_bytes());
        out[8..12].copy_from_slice(&self.frame_sequence.to_be_bytes());
        out[12..16].copy_from_slice(&self.timestamp.to_be_bytes());
        out[16..18].copy_from_slice(&self.payload_len.to_be_bytes());
        out[18..20].copy_from_slice(&self.frag_index.to_be_bytes());
        out[20..22].copy_from_slice(&self.frag_count.to_be_bytes());
        out[22..24].copy_from_slice(&[0, 0]); // reserved tail
        out
    }

    /// Parse a header, rejecting unknown versions and truncated input.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < FRAME_HEADER_LEN {
            return None;
        }
        let version = bytes[0];
        if version != 1 {
            return None;
        }
        // Payload length is validated against the buffer by `parse_record`;
        // this function only parses the fixed-size header.
        let payload_len = u16::from_be_bytes([bytes[16], bytes[17]]);
        let frag_index = u16::from_be_bytes([bytes[18], bytes[19]]);
        let frag_count = u16::from_be_bytes([bytes[20], bytes[21]]);
        if frag_count == 0 || frag_index >= frag_count {
            return None; // nonsensical fragmentation
        }
        Some(Self {
            version,
            keyframe: bytes[1] & 0x01 != 0,
            payload_type: bytes[2],
            sequence: u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            frame_sequence: u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
            timestamp: u32::from_be_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]),
            payload_len,
            frag_index,
            frag_count,
        })
    }
}

/// Payload type currently emitted by the host (raw frame; encoder lands later).
pub const PAYLOAD_TYPE_RAW: u8 = 96;

/// Build a complete record (header + payload) ready to seal.
#[allow(clippy::too_many_arguments)]
pub fn build_record(
    sequence: u32,
    frame_sequence: u32,
    timestamp: u32,
    keyframe: bool,
    payload_type: u8,
    payload: &[u8],
    frag_index: u16,
    frag_count: u16,
) -> Result<Vec<u8>, TransportError> {
    let len =
        u16::try_from(payload.len()).map_err(|_| TransportError::PayloadTooLarge(payload.len()))?;
    let header = FrameHeader {
        version: 1,
        keyframe,
        payload_type,
        sequence,
        frame_sequence,
        timestamp,
        payload_len: len,
        frag_index,
        frag_count,
    };
    let mut out = Vec::with_capacity(FRAME_HEADER_LEN + payload.len());
    out.extend_from_slice(&header.to_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

/// Split a decrypted record into its header and payload.
pub fn parse_record(record: &[u8]) -> Result<(FrameHeader, &[u8]), TransportError> {
    let header = FrameHeader::from_bytes(record).ok_or(TransportError::MalformedRecord)?;
    // A header that claims more payload than the record holds is malformed.
    let start = FRAME_HEADER_LEN;
    let end = start + header.payload_len as usize;
    let payload = record
        .get(start..end)
        .ok_or(TransportError::MalformedRecord)?;
    Ok((header, payload))
}

/// A sliding replay window over 32-bit sequence numbers.
///
/// Accepts anything newer than the highest seen, or within `WIDTH` positions
/// behind it exactly once. This is the standard RTP-style bitmap check; it
/// turns an authenticated-but-replayed record into a rejected one, which is the
/// gap ADR 0006 flagged.
#[derive(Debug, Default)]
pub struct ReplayWindow {
    highest: Option<u32>,
    bitmap: u64,
}

impl ReplayWindow {
    /// Width of the accepted reordering window, in sequence numbers.
    pub const WIDTH: u32 = 64;

    /// Record `seq`; return `true` if it is acceptable, `false` if it is a
    /// duplicate or too old.
    pub fn accept(&mut self, seq: u32) -> bool {
        let Some(highest) = self.highest else {
            self.highest = Some(seq);
            self.bitmap = 1;
            return true;
        };
        // Serial-number arithmetic: sequence numbers wrap at 2^32, so compare
        // the signed difference rather than the raw values.
        let forward = seq.wrapping_sub(highest);
        if forward != 0 && forward < 0x8000_0000 {
            let shift = forward;
            self.bitmap = if shift >= Self::WIDTH {
                1
            } else {
                (self.bitmap << shift) | 1
            };
            self.highest = Some(seq);
            return true;
        }
        if forward == 0 {
            return false; // duplicate of the highest
        }
        let behind = highest.wrapping_sub(seq);
        if behind >= Self::WIDTH {
            // Too old to distinguish from the far past.
            return false;
        }
        let mask = 1u64 << behind;
        if self.bitmap & mask != 0 {
            return false; // already seen
        }
        self.bitmap |= mask;
        true
    }
}

/// Split a frame payload into slices that each fit inside one record.
///
/// The caller turns each slice into a record, assigning its own record
/// sequence while sharing the frame sequence.
pub fn payload_chunks(payload: &[u8]) -> Vec<&[u8]> {
    let budget = RECORD_MTU - FRAME_HEADER_LEN;
    if payload.len() <= budget {
        return vec![payload];
    }
    payload.chunks(budget).collect()
}

/// Reassembles fragmented records back into whole frames.
#[derive(Debug, Default)]
pub struct Reassembler {
    current: Option<(u32, Vec<u8>, u16, u16)>,
}

impl Reassembler {
    /// Feed one record; yields a complete frame when the last fragment lands.
    pub fn push(&mut self, header: FrameHeader, payload: &[u8]) -> Option<Vec<u8>> {
        if header.frag_count == 1 {
            return Some(payload.to_vec());
        }
        match &mut self.current {
            Some((seq, buf, next, _)) if *seq == header.frame_sequence => {
                buf.extend_from_slice(payload);
                *next += 1;
                if *next >= header.frag_count {
                    let (_, done, _, _) = self.current.take()?;
                    return Some(done);
                }
                None
            }
            // A new frame supersedes any incomplete one.
            _ => {
                let mut buf = Vec::new();
                buf.extend_from_slice(payload);
                if header.frag_index != 0 {
                    // Out-of-order start: drop until the next clean frame.
                    self.current = None;
                    return None;
                }
                self.current = Some((header.frame_sequence, buf, 1, header.frag_count));
                None
            }
        }
    }
}

/// Errors from the transport layer.
#[derive(Debug, PartialEq, Eq)]
pub enum TransportError {
    /// Frame payload exceeds the 16-bit length field.
    PayloadTooLarge(usize),
    /// Record shorter than its header, or an unknown header version.
    MalformedRecord,
    /// A packet arrived that we cannot or must not answer.
    Dropped(&'static str),
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PayloadTooLarge(n) => write!(f, "frame payload of {n} bytes is too large"),
            Self::MalformedRecord => write!(f, "malformed frame record"),
            Self::Dropped(why) => write!(f, "packet dropped: {why}"),
        }
    }
}

impl std::error::Error for TransportError {}

/// Wire layout for a data packet: `type || sequence || nonce || ciphertext`.
///
/// The sequence travels in the clear so a receiver can window it before
/// spending AEAD work; it is bound into the associated data, so it cannot be
/// altered in flight. This function is the single definition of that layout —
/// `open_packet` is its inverse.
pub fn seal_packet(
    session: &Session,
    direction: Direction,
    sequence: u32,
    record: &[u8],
) -> Result<Vec<u8>, TransportError> {
    let sealed = crate::crypto::seal(session, direction, sequence as u64, record)
        .map_err(|_| TransportError::MalformedRecord)?;
    let mut out = Vec::with_capacity(5 + sealed.len());
    out.push(packet_type::DATA);
    out.extend_from_slice(&sequence.to_be_bytes());
    out.extend_from_slice(&sealed);
    Ok(out)
}

/// Open a data packet, enforcing the replay window first.
pub fn open_packet(
    session: &Session,
    direction: Direction,
    window: &mut ReplayWindow,
    packet: &[u8],
) -> Result<(FrameHeader, Vec<u8>), TransportError> {
    if packet.first() != Some(&packet_type::DATA) {
        return Err(TransportError::Dropped("not a data packet"));
    }
    // Peek at the header to learn the sequence used as associated data. The
    // AEAD check below is what actually authenticates it, so an attacker
    // cannot spoof a sequence into acceptance.
    // Layout: type || sequence || nonce || ciphertext (see `seal_packet`).
    let sealed = &packet[1..];
    if sealed.len() < 4 {
        return Err(TransportError::Dropped("packet too short"));
    }
    let sequence = u32::from_be_bytes([sealed[0], sealed[1], sealed[2], sealed[3]]);
    if !window.accept(sequence) {
        return Err(TransportError::Dropped("replayed or too old"));
    }
    let plaintext = crate::crypto::open(session, direction, sequence as u64, &sealed[4..])
        .map_err(|_| TransportError::Dropped("authentication failed"))?;
    let (header, payload) = parse_record(&plaintext)?;
    Ok((header, payload.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{DeviceIdentity, Session};

    fn session() -> Session {
        let (host, device) = (
            DeviceIdentity::generate().unwrap(),
            DeviceIdentity::generate().unwrap(),
        );
        Session::derive(
            &host.agree(&device.public_key_bytes()).unwrap(),
            b"emersia/v1",
        )
        .unwrap()
    }

    #[test]
    fn frame_header_roundtrips() {
        let h = FrameHeader {
            version: 1,
            keyframe: true,
            payload_type: PAYLOAD_TYPE_RAW,
            sequence: 0xdead_beef,
            frame_sequence: 0x0bad_f00d,
            timestamp: 123_456,
            payload_len: 900,
            frag_index: 0,
            frag_count: 1,
        };
        let bytes = h.to_bytes();
        assert_eq!(bytes.len(), FRAME_HEADER_LEN);
        assert_eq!(FrameHeader::from_bytes(&bytes).unwrap(), h);
    }

    #[test]
    fn record_roundtrips_with_payload() {
        let payload = vec![7u8; 300];
        let record = build_record(5, 5, 90_000, true, PAYLOAD_TYPE_RAW, &payload, 0, 1).unwrap();
        let (header, parsed) = parse_record(&record).unwrap();
        assert_eq!(header.sequence, 5);
        assert!(header.keyframe);
        assert_eq!(parsed, payload.as_slice());
    }

    #[test]
    fn oversized_payload_is_rejected() {
        let huge = vec![0u8; 70_000];
        assert!(matches!(
            build_record(1, 1, 0, false, PAYLOAD_TYPE_RAW, &huge, 0, 1),
            Err(TransportError::PayloadTooLarge(_))
        ));
    }

    #[test]
    fn truncated_and_unknown_records_are_rejected() {
        assert!(parse_record(&[1, 2, 3]).is_err());
        let mut record = build_record(1, 1, 0, false, PAYLOAD_TYPE_RAW, b"abc", 0, 1).unwrap();
        record[0] = 9; // unknown version
        assert!(parse_record(&record).is_err());
        // Header claims more payload than is present.
        let mut lying = record;
        lying[0] = 1;
        lying[16..18].copy_from_slice(&999u16.to_be_bytes());
        assert!(parse_record(&lying).is_err());
    }

    #[test]
    fn replay_window_accepts_in_order_stream() {
        let mut w = ReplayWindow::default();
        for seq in 0..1000 {
            assert!(w.accept(seq), "seq {seq} should be fresh");
        }
    }

    #[test]
    fn replay_window_rejects_duplicates() {
        let mut w = ReplayWindow::default();
        assert!(w.accept(10));
        assert!(!w.accept(10), "exact duplicate must be rejected");
    }

    #[test]
    fn replay_window_allows_reordering_within_window() {
        let mut w = ReplayWindow::default();
        assert!(w.accept(100));
        assert!(w.accept(101));
        // Late but inside the window: acceptable exactly once.
        assert!(w.accept(99));
        assert!(!w.accept(99), "and only once");
    }

    #[test]
    fn replay_window_rejects_too_old() {
        let mut w = ReplayWindow::default();
        assert!(w.accept(10_000));
        assert!(
            !w.accept(10_000 - ReplayWindow::WIDTH),
            "outside the window must be dropped"
        );
    }

    #[test]
    fn replay_window_handles_sequence_wrap() {
        let mut w = ReplayWindow::default();
        // Just below the wrap point, then just above it.
        assert!(w.accept(u32::MAX - 1));
        assert!(w.accept(u32::MAX));
        assert!(w.accept(0), "wrapped sequence is newer");
        assert!(!w.accept(u32::MAX), "the pre-wrap value is now far behind");
    }

    #[test]
    fn packet_seal_open_roundtrip() {
        let s = session();
        let payload = vec![42u8; 128];
        let record = build_record(9, 9, 90_000, false, PAYLOAD_TYPE_RAW, &payload, 0, 1).unwrap();
        // Sequence travels in the clear so the receiver can order before/while
        // authenticating; the AEAD AAD binds it, so it cannot be altered.
        let mut packet = Vec::new();
        packet.push(packet_type::DATA);
        packet.extend_from_slice(&9u32.to_be_bytes());
        packet.extend_from_slice(
            &crate::crypto::seal(&s, Direction::HostToDevice, 9, &record).unwrap(),
        );

        let mut w = ReplayWindow::default();
        let (header, out) = open_packet(&s, Direction::HostToDevice, &mut w, &packet).unwrap();
        assert_eq!(header.sequence, 9);
        assert_eq!(out, payload);
    }

    #[test]
    fn tampered_sequence_is_dropped() {
        let s = session();
        let record = build_record(9, 9, 0, false, PAYLOAD_TYPE_RAW, b"payload", 0, 1).unwrap();
        let mut packet = vec![packet_type::DATA];
        packet.extend_from_slice(&9u32.to_be_bytes());
        packet.extend_from_slice(
            &crate::crypto::seal(&s, Direction::HostToDevice, 9, &record).unwrap(),
        );
        // Rewrite the cleartext sequence: the AEAD AAD no longer matches.
        packet[1..5].copy_from_slice(&10u32.to_be_bytes());
        let mut w = ReplayWindow::default();
        assert!(matches!(
            open_packet(&s, Direction::HostToDevice, &mut w, &packet),
            Err(TransportError::Dropped(_))
        ));
    }

    #[test]
    fn wrong_direction_packet_is_dropped() {
        let s = session();
        let record = build_record(1, 1, 0, false, PAYLOAD_TYPE_RAW, b"x", 0, 1).unwrap();
        let mut packet = vec![packet_type::DATA];
        packet.extend_from_slice(&1u32.to_be_bytes());
        packet.extend_from_slice(
            &crate::crypto::seal(&s, Direction::HostToDevice, 1, &record).unwrap(),
        );
        let mut w = ReplayWindow::default();
        assert!(matches!(
            open_packet(&s, Direction::DeviceToHost, &mut w, &packet),
            Err(TransportError::Dropped(_))
        ));
    }

    #[test]
    fn replayed_packet_is_dropped_second_time() {
        let s = session();
        let record = build_record(1, 1, 0, false, PAYLOAD_TYPE_RAW, b"x", 0, 1).unwrap();
        let mut packet = vec![packet_type::DATA];
        packet.extend_from_slice(&1u32.to_be_bytes());
        packet.extend_from_slice(
            &crate::crypto::seal(&s, Direction::HostToDevice, 1, &record).unwrap(),
        );
        let mut w = ReplayWindow::default();
        assert!(open_packet(&s, Direction::HostToDevice, &mut w, &packet).is_ok());
        assert!(
            matches!(
                open_packet(&s, Direction::HostToDevice, &mut w, &packet),
                Err(TransportError::Dropped(_))
            ),
            "a network replay of an accepted record must be dropped"
        );
    }

    #[test]
    fn non_data_packet_type_is_dropped() {
        let s = session();
        let mut w = ReplayWindow::default();
        assert!(matches!(
            open_packet(
                &s,
                Direction::HostToDevice,
                &mut w,
                &[packet_type::HANDSHAKE_INIT, 0, 0, 0, 0]
            ),
            Err(TransportError::Dropped(_))
        ));
    }
}
