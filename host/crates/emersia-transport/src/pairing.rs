//! In-headset pairing: discovery beacons and a short-authentication-string
//! (SAS) exchange with commitments (ADR 0008).
//!
//! ```text
//!  host (window open)            device
//!  BEACON(pk_h, port, name) ──►  (broadcast, discovery port)
//!                           ◄──  COMMIT(pk_d, SHA256(n_d), name)
//!  NONCE(pk_d, n_h)         ──►
//!                           ◄──  REVEAL(pk_d, n_d)
//!  both display  code = SAS(pk_h, pk_d, n_d, n_h)  — user compares
//!  RESULT(pk_d, ok, tag)    ──►  (after the host-side user confirms)
//! ```
//!
//! A key-derived code alone could be ground by anyone on the LAN (10^6
//! candidate keys). Here each side's nonce is fixed before it sees the other's:
//! the device commits to `n_d` before learning `n_h`, the host picks `n_h`
//! before learning `n_d`. A man in the middle running two exchanges therefore
//! gets two independent codes, matching with probability 10^-6.
//!
//! All pairing packets are cleartext (there is no key yet) and are answered
//! only while the host user has a pairing window open.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

/// Cleartext packet types (distinct from the transport's 0x01..0x10).
pub mod packet {
    pub const BEACON: u8 = 0x20;
    pub const COMMIT: u8 = 0x21;
    pub const NONCE: u8 = 0x22;
    pub const REVEAL: u8 = 0x23;
    pub const RESULT: u8 = 0x24;
    /// Host → broadcast, always while streaming is possible: "this key is
    /// here". Lets a paired headset follow the host to a new address.
    pub const PRESENCE: u8 = 0x25;
}

/// Encode a presence announcement: `type, version, host_key, stream_port`.
pub fn presence(host_key: &[u8; 32], stream_port: u16) -> Vec<u8> {
    let mut out = vec![packet::PRESENCE, VERSION];
    out.extend_from_slice(host_key);
    out.extend_from_slice(&stream_port.to_be_bytes());
    out
}

/// Decode a presence announcement into `(host_key, stream_port)`.
pub fn parse_presence(bytes: &[u8]) -> Option<([u8; 32], u16)> {
    if bytes.len() != 36 || bytes[0] != packet::PRESENCE || bytes[1] != VERSION {
        return None;
    }
    let key: [u8; 32] = bytes[2..34].try_into().ok()?;
    Some((key, u16::from_be_bytes([bytes[34], bytes[35]])))
}

/// UDP port devices listen on for beacons.
pub const DISCOVERY_PORT: u16 = 47801;
/// Wire version of the pairing messages.
pub const VERSION: u8 = 1;
/// Longest display name carried.
pub const MAX_NAME: usize = 64;
/// How long one exchange may take before it is forgotten.
pub const EXCHANGE_TTL: Duration = Duration::from_secs(120);
/// Concurrent exchanges per window; more are refused (default-deny under load).
pub const MAX_EXCHANGES: usize = 8;

const SAS_CONTEXT: &[u8] = b"emersia/pair-sas/v1";
const COMMIT_CONTEXT: &[u8] = b"emersia/pair-commit/v1";
const RESULT_CONTEXT: &[u8] = b"emersia/pair-result/v1";

fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

/// Commitment to the device nonce.
pub fn commitment(n_d: &[u8; 32]) -> [u8; 32] {
    sha256(&[COMMIT_CONTEXT, n_d])
}

/// The 6-digit code both sides display, as `"123 456"`.
pub fn sas(pk_h: &[u8; 32], pk_d: &[u8; 32], n_d: &[u8; 32], n_h: &[u8; 32]) -> String {
    let d = sha256(&[SAS_CONTEXT, pk_h, pk_d, n_d, n_h]);
    let n = u32::from_be_bytes([d[0], d[1], d[2], d[3]]) % 1_000_000;
    format!("{:03} {:03}", n / 1000, n % 1000)
}

/// Tag binding a RESULT to this exchange, so an off-path sender (who never saw
/// both nonces) cannot forge an "accepted".
pub fn result_tag(
    pk_h: &[u8; 32],
    pk_d: &[u8; 32],
    n_d: &[u8; 32],
    n_h: &[u8; 32],
    accepted: bool,
) -> [u8; 32] {
    sha256(&[RESULT_CONTEXT, pk_h, pk_d, n_d, n_h, &[u8::from(accepted)]])
}

/// Random 32 bytes from the OS.
pub fn nonce() -> Option<[u8; 32]> {
    let mut n = [0u8; 32];
    getrandom::fill(&mut n).ok()?;
    Some(n)
}

/// A decoded pairing message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Beacon {
        host_key: [u8; 32],
        stream_port: u16,
        name: String,
    },
    Commit {
        device_key: [u8; 32],
        commitment: [u8; 32],
        name: String,
    },
    Nonce {
        device_key: [u8; 32],
        nonce: [u8; 32],
    },
    Reveal {
        device_key: [u8; 32],
        nonce: [u8; 32],
    },
    Result {
        device_key: [u8; 32],
        accepted: bool,
        tag: [u8; 32],
    },
}

fn put_name(out: &mut Vec<u8>, name: &str) {
    // Truncate on a char boundary.
    let mut end = name.len().min(MAX_NAME);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    out.push(end as u8);
    out.extend_from_slice(&name.as_bytes()[..end]);
}

impl Message {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(100);
        match self {
            Self::Beacon {
                host_key,
                stream_port,
                name,
            } => {
                out.extend_from_slice(&[packet::BEACON, VERSION]);
                out.extend_from_slice(host_key);
                out.extend_from_slice(&stream_port.to_be_bytes());
                put_name(&mut out, name);
            }
            Self::Commit {
                device_key,
                commitment,
                name,
            } => {
                out.extend_from_slice(&[packet::COMMIT, VERSION]);
                out.extend_from_slice(device_key);
                out.extend_from_slice(commitment);
                put_name(&mut out, name);
            }
            Self::Nonce { device_key, nonce } => {
                out.extend_from_slice(&[packet::NONCE, VERSION]);
                out.extend_from_slice(device_key);
                out.extend_from_slice(nonce);
            }
            Self::Reveal { device_key, nonce } => {
                out.extend_from_slice(&[packet::REVEAL, VERSION]);
                out.extend_from_slice(device_key);
                out.extend_from_slice(nonce);
            }
            Self::Result {
                device_key,
                accepted,
                tag,
            } => {
                out.extend_from_slice(&[packet::RESULT, VERSION]);
                out.extend_from_slice(device_key);
                out.push(u8::from(*accepted));
                out.extend_from_slice(tag);
            }
        }
        out
    }

    /// Strict decode; `None` for anything malformed or of another version.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let (&ty, rest) = bytes.split_first()?;
        let (&version, mut rest) = rest.split_first()?;
        if version != VERSION {
            return None;
        }
        fn key(rest: &mut &[u8]) -> Option<[u8; 32]> {
            let (k, tail) = rest.split_at_checked(32)?;
            *rest = tail;
            k.try_into().ok()
        }
        fn name(rest: &mut &[u8]) -> Option<String> {
            let (&len, tail) = rest.split_first()?;
            let len = len as usize;
            if len > MAX_NAME {
                return None;
            }
            let (n, tail) = tail.split_at_checked(len)?;
            *rest = tail;
            String::from_utf8(n.to_vec()).ok()
        }
        let msg = match ty {
            packet::BEACON => {
                let host_key = key(&mut rest)?;
                let (port, tail) = rest.split_at_checked(2)?;
                rest = tail;
                Self::Beacon {
                    host_key,
                    stream_port: u16::from_be_bytes([port[0], port[1]]),
                    name: name(&mut rest)?,
                }
            }
            packet::COMMIT => Self::Commit {
                device_key: key(&mut rest)?,
                commitment: key(&mut rest)?,
                name: name(&mut rest)?,
            },
            packet::NONCE => Self::Nonce {
                device_key: key(&mut rest)?,
                nonce: key(&mut rest)?,
            },
            packet::REVEAL => Self::Reveal {
                device_key: key(&mut rest)?,
                nonce: key(&mut rest)?,
            },
            packet::RESULT => {
                let device_key = key(&mut rest)?;
                let (&ok, tail) = rest.split_first()?;
                rest = tail;
                Self::Result {
                    device_key,
                    accepted: ok == 1,
                    tag: key(&mut rest)?,
                }
            }
            _ => return None,
        };
        rest.is_empty().then_some(msg)
    }
}

/// Whether a datagram is a pairing packet (by its type byte).
pub fn is_pairing_packet(packet: &[u8]) -> bool {
    matches!(packet.first(), Some(0x20..=0x25))
}

/// A device waiting for the host user's decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingDevice {
    pub device_key: [u8; 32],
    pub name: String,
    pub code: String,
}

#[derive(Debug)]
struct Exchange {
    addr: SocketAddr,
    name: String,
    commitment: [u8; 32],
    n_h: [u8; 32],
    n_d: Option<[u8; 32]>,
    started: Instant,
}

/// Host side of pairing: owns the window and every exchange in it. Pure
/// state machine; the caller does the socket I/O and the trust store.
#[derive(Debug)]
pub struct HostPairing {
    host_key: [u8; 32],
    open_until: Option<Instant>,
    exchanges: HashMap<[u8; 32], Exchange>,
}

impl HostPairing {
    pub fn new(host_key: [u8; 32]) -> Self {
        Self {
            host_key,
            open_until: None,
            exchanges: HashMap::new(),
        }
    }

    pub fn open(&mut self, for_how_long: Duration) {
        self.open_until = Some(Instant::now() + for_how_long);
        self.exchanges.clear();
    }

    pub fn close(&mut self) {
        self.open_until = None;
        self.exchanges.clear();
    }

    pub fn is_open(&self) -> bool {
        self.open_until.is_some_and(|t| Instant::now() < t)
    }

    /// Seconds left in the window.
    pub fn remaining(&self) -> Duration {
        self.open_until
            .map(|t| t.saturating_duration_since(Instant::now()))
            .unwrap_or_default()
    }

    /// The beacon to broadcast while open.
    pub fn beacon(&self, stream_port: u16, name: &str) -> Option<Vec<u8>> {
        self.is_open().then(|| {
            Message::Beacon {
                host_key: self.host_key,
                stream_port,
                name: name.to_string(),
            }
            .encode()
        })
    }

    /// Handle one pairing datagram; returns a reply to send to `from`.
    pub fn handle(&mut self, packet: &[u8], from: SocketAddr) -> Option<Vec<u8>> {
        if !self.is_open() {
            return None; // default deny: no window, no answer
        }
        let now = Instant::now();
        self.exchanges
            .retain(|_, e| now.duration_since(e.started) < EXCHANGE_TTL);
        match Message::decode(packet)? {
            Message::Commit {
                device_key,
                commitment,
                name,
            } => {
                if !self.exchanges.contains_key(&device_key)
                    && self.exchanges.len() >= MAX_EXCHANGES
                {
                    return None;
                }
                // A repeated COMMIT (lost NONCE) gets the same nonce back, but
                // a changed commitment restarts the exchange.
                let n_h = match self.exchanges.get(&device_key) {
                    Some(e) if e.commitment == commitment && e.n_d.is_none() => e.n_h,
                    _ => nonce()?,
                };
                self.exchanges.insert(
                    device_key,
                    Exchange {
                        addr: from,
                        name,
                        commitment,
                        n_h,
                        n_d: None,
                        started: now,
                    },
                );
                Some(
                    Message::Nonce {
                        device_key,
                        nonce: n_h,
                    }
                    .encode(),
                )
            }
            Message::Reveal { device_key, nonce } => {
                let e = self.exchanges.get_mut(&device_key)?;
                if e.addr != from || commitment(&nonce) != e.commitment {
                    // A reveal that does not open the commitment ends the
                    // exchange: no second guess against the same nonce.
                    self.exchanges.remove(&device_key);
                    return None;
                }
                e.n_d = Some(nonce);
                None
            }
            _ => None,
        }
    }

    /// Exchanges that reached a code, for the host user to compare.
    pub fn pending(&self) -> Vec<PendingDevice> {
        let mut out: Vec<_> = self
            .exchanges
            .iter()
            .filter_map(|(pk_d, e)| {
                let n_d = e.n_d?;
                Some(PendingDevice {
                    device_key: *pk_d,
                    name: e.name.clone(),
                    code: sas(&self.host_key, pk_d, &n_d, &e.n_h),
                })
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// The host user decided. Returns `(address, RESULT packet)` to send; the
    /// exchange is finished either way.
    pub fn decide(
        &mut self,
        device_key: &[u8; 32],
        accepted: bool,
    ) -> Option<(SocketAddr, Vec<u8>)> {
        let e = self.exchanges.remove(device_key)?;
        let n_d = e.n_d?;
        let tag = result_tag(&self.host_key, device_key, &n_d, &e.n_h, accepted);
        Some((
            e.addr,
            Message::Result {
                device_key: *device_key,
                accepted,
                tag,
            }
            .encode(),
        ))
    }
}

/// Device side of one exchange.
#[derive(Debug, Clone)]
pub struct DeviceExchange {
    pub host_key: [u8; 32],
    pub device_key: [u8; 32],
    n_d: [u8; 32],
    n_h: Option<[u8; 32]>,
}

impl DeviceExchange {
    pub fn new(host_key: [u8; 32], device_key: [u8; 32]) -> Option<Self> {
        Some(Self {
            host_key,
            device_key,
            n_d: nonce()?,
            n_h: None,
        })
    }

    pub fn commit(&self, name: &str) -> Vec<u8> {
        Message::Commit {
            device_key: self.device_key,
            commitment: commitment(&self.n_d),
            name: name.to_string(),
        }
        .encode()
    }

    /// Take the host's NONCE; returns the REVEAL to send.
    pub fn on_nonce(&mut self, nonce: [u8; 32]) -> Vec<u8> {
        // The first nonce wins; a later different one cannot change the code
        // after it was shown.
        self.n_h.get_or_insert(nonce);
        Message::Reveal {
            device_key: self.device_key,
            nonce: self.n_d,
        }
        .encode()
    }

    /// The code to display, once the host nonce is known.
    pub fn code(&self) -> Option<String> {
        Some(sas(&self.host_key, &self.device_key, &self.n_d, &self.n_h?))
    }

    /// Check a RESULT: `Some(accepted)` if it is authentic for this exchange.
    pub fn verify_result(&self, accepted: bool, tag: &[u8; 32]) -> Option<bool> {
        let n_h = self.n_h?;
        (result_tag(&self.host_key, &self.device_key, &self.n_d, &n_h, accepted) == *tag)
            .then_some(accepted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    #[test]
    fn messages_roundtrip_and_reject_garbage() {
        let msgs = [
            Message::Beacon {
                host_key: [1; 32],
                stream_port: 47800,
                name: "charoite".into(),
            },
            Message::Commit {
                device_key: [2; 32],
                commitment: [3; 32],
                name: "Quest 2".into(),
            },
            Message::Nonce {
                device_key: [2; 32],
                nonce: [4; 32],
            },
            Message::Reveal {
                device_key: [2; 32],
                nonce: [5; 32],
            },
            Message::Result {
                device_key: [2; 32],
                accepted: true,
                tag: [6; 32],
            },
        ];
        for m in msgs {
            let bytes = m.encode();
            assert!(is_pairing_packet(&bytes));
            assert_eq!(Message::decode(&bytes), Some(m));
            assert_eq!(
                Message::decode(&bytes[..bytes.len() - 1]),
                None,
                "truncated"
            );
            let mut long = bytes.clone();
            long.push(0);
            assert_eq!(Message::decode(&long), None, "trailing bytes");
            let mut wrong_version = bytes;
            wrong_version[1] = 9;
            assert_eq!(Message::decode(&wrong_version), None);
        }
        assert!(!is_pairing_packet(&[0x10]));
    }

    #[test]
    fn presence_roundtrips_strictly() {
        let p = presence(&[5; 32], 47800);
        assert!(is_pairing_packet(&p));
        assert_eq!(parse_presence(&p), Some(([5; 32], 47800)));
        assert_eq!(parse_presence(&p[..35]), None);
        let mut v = p.clone();
        v[1] = 9;
        assert_eq!(parse_presence(&v), None);
        assert_eq!(Message::decode(&p), None, "not a pairing message");
    }

    #[test]
    fn long_names_are_truncated_on_a_char_boundary() {
        let name = "é".repeat(40); // 80 bytes
        let m = Message::Commit {
            device_key: [0; 32],
            commitment: [0; 32],
            name,
        };
        let Some(Message::Commit { name, .. }) = Message::decode(&m.encode()) else {
            panic!()
        };
        assert_eq!(name, "é".repeat(32));
    }

    /// Full exchange: both sides compute the same code; RESULT verifies.
    #[test]
    fn honest_exchange_agrees_on_the_code() {
        let pk_h = [7; 32];
        let pk_d = [8; 32];
        let mut host = HostPairing::new(pk_h);
        host.open(Duration::from_secs(60));
        let mut dev = DeviceExchange::new(pk_h, pk_d).unwrap();

        let Some(Message::Nonce { nonce, .. }) = host
            .handle(&dev.commit("Quest 2"), addr(5000))
            .and_then(|p| Message::decode(&p))
        else {
            panic!("host should answer a commit with a nonce")
        };
        let reveal = dev.on_nonce(nonce);
        assert!(host.handle(&reveal, addr(5000)).is_none());

        let pending = host.pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].name, "Quest 2");
        assert_eq!(Some(pending[0].code.clone()), dev.code());
        assert_eq!(pending[0].code.len(), 7);

        let (to, result) = host.decide(&pk_d, true).unwrap();
        assert_eq!(to, addr(5000));
        let Some(Message::Result { accepted, tag, .. }) = Message::decode(&result) else {
            panic!()
        };
        assert_eq!(dev.verify_result(accepted, &tag), Some(true));
        // A flipped verdict does not verify.
        assert_eq!(dev.verify_result(!accepted, &tag), None);
        assert!(host.pending().is_empty(), "decided exchanges are gone");
    }

    #[test]
    fn closed_window_answers_nothing() {
        let mut host = HostPairing::new([7; 32]);
        let dev = DeviceExchange::new([7; 32], [8; 32]).unwrap();
        assert!(host.handle(&dev.commit("x"), addr(1)).is_none());
        assert!(host.beacon(47800, "h").is_none());
        host.open(Duration::from_secs(60));
        assert!(host.beacon(47800, "h").is_some());
        host.close();
        assert!(host.handle(&dev.commit("x"), addr(1)).is_none());
    }

    #[test]
    fn a_reveal_that_does_not_open_the_commitment_kills_the_exchange() {
        let mut host = HostPairing::new([7; 32]);
        host.open(Duration::from_secs(60));
        let dev = DeviceExchange::new([7; 32], [8; 32]).unwrap();
        host.handle(&dev.commit("x"), addr(1)).unwrap();
        let bogus = Message::Reveal {
            device_key: [8; 32],
            nonce: [0; 32],
        }
        .encode();
        assert!(host.handle(&bogus, addr(1)).is_none());
        assert!(host.pending().is_empty());
        assert!(host.decide(&[8; 32], true).is_none());
    }

    #[test]
    fn reveal_from_another_address_is_refused() {
        let mut host = HostPairing::new([7; 32]);
        host.open(Duration::from_secs(60));
        let mut dev = DeviceExchange::new([7; 32], [8; 32]).unwrap();
        let nonce = match Message::decode(&host.handle(&dev.commit("x"), addr(1)).unwrap()) {
            Some(Message::Nonce { nonce, .. }) => nonce,
            _ => panic!(),
        };
        host.handle(&dev.on_nonce(nonce), addr(2));
        assert!(host.pending().is_empty());
    }

    /// A man in the middle runs one exchange with each side; it cannot choose
    /// the host's nonce, so the two codes differ (here: overwhelmingly).
    #[test]
    fn man_in_the_middle_gets_mismatched_codes() {
        let pk_h = [7; 32];
        let pk_d = [8; 32];
        let pk_attacker = [9; 32];
        let mut host = HostPairing::new(pk_h);
        host.open(Duration::from_secs(60));
        // Attacker ↔ host, posing as a device.
        let mut fake_dev = DeviceExchange::new(pk_h, pk_attacker).unwrap();
        let n = match Message::decode(&host.handle(&fake_dev.commit("x"), addr(9)).unwrap()) {
            Some(Message::Nonce { nonce, .. }) => nonce,
            _ => panic!(),
        };
        host.handle(&fake_dev.on_nonce(n), addr(9));
        let host_code = host.pending()[0].code.clone();
        // Device ↔ attacker, posing as the host.
        let mut fake_host = HostPairing::new(pk_attacker);
        fake_host.open(Duration::from_secs(60));
        let mut dev = DeviceExchange::new(pk_attacker, pk_d).unwrap();
        let n = match Message::decode(&fake_host.handle(&dev.commit("q"), addr(1)).unwrap()) {
            Some(Message::Nonce { nonce, .. }) => nonce,
            _ => panic!(),
        };
        dev.on_nonce(n);
        assert_ne!(dev.code().unwrap(), host_code);
    }

    #[test]
    fn exchanges_per_window_are_bounded() {
        let mut host = HostPairing::new([7; 32]);
        host.open(Duration::from_secs(60));
        for i in 0..MAX_EXCHANGES as u8 {
            let d = DeviceExchange::new([7; 32], [i; 32]).unwrap();
            assert!(host.handle(&d.commit("x"), addr(i.into())).is_some());
        }
        let extra = DeviceExchange::new([7; 32], [200; 32]).unwrap();
        assert!(host.handle(&extra.commit("x"), addr(200)).is_none());
    }

    #[test]
    fn sas_is_six_digits_and_depends_on_every_input() {
        let base = sas(&[1; 32], &[2; 32], &[3; 32], &[4; 32]);
        assert!(base.chars().filter(char::is_ascii_digit).count() == 6);
        assert_ne!(base, sas(&[9; 32], &[2; 32], &[3; 32], &[4; 32]));
        assert_ne!(base, sas(&[1; 32], &[9; 32], &[3; 32], &[4; 32]));
        assert_ne!(base, sas(&[1; 32], &[2; 32], &[9; 32], &[4; 32]));
        assert_ne!(base, sas(&[1; 32], &[2; 32], &[3; 32], &[9; 32]));
    }
}
