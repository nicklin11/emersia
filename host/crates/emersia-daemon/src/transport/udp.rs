//! UDP transport: a host endpoint and a device endpoint, driven synchronously
//! so the whole exchange can be tested without threads.
//!
//! Default-deny lives here. A handshake attempt from a device that is not
//! active in the pairing database — or whose presented public key does not
//! match the paired record — is dropped **without a reply**, as ADR 0005
//! requires. A silent drop is what stops the port from becoming an oracle for
//! which device ids exist.

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;

use crate::crypto::{DeviceIdentity, Session};
use crate::transport::handshake::{
    verify_confirm, HandshakeClient, HandshakeServer, DEVICE_ID_LEN,
};
use crate::transport::{
    build_record, open_packet, packet_type, payload_chunks, seal_packet, FrameHeader, ReplayWindow,
    TransportError,
};

/// Largest datagram we will read. Video is chunked upstream; this only guards
/// against absurd packets.
const MAX_DATAGRAM: usize = 65_535;

/// Default socket read timeout, so a stalled peer cannot wedge the endpoint.
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_millis(250);

/// Outcome of offering one packet to the host.
#[derive(Debug, PartialEq, Eq)]
pub enum HostOutcome {
    /// Nothing to send back (the packet was accepted, or dropped silently).
    Silent,
    /// A reply must be sent to the peer.
    Reply(Vec<u8>),
}

/// A connected device.
pub struct Peer {
    pub addr: SocketAddr,
    pub session: Session,
    /// Replay window for records received *from* this device.
    pub window: ReplayWindow,
}

/// A handshake that got as far as a session but is not yet confirmed.
struct Pending {
    device_id: [u8; DEVICE_ID_LEN],
    session: Session,
}

/// Host-side UDP endpoint.
pub struct HostEndpoint {
    socket: UdpSocket,
    host_identity: DeviceIdentity,
    pending: HashMap<SocketAddr, Pending>,
    peers: HashMap<[u8; DEVICE_ID_LEN], Peer>,
    /// Sequence counter for outbound records.
    next_sequence: u32,
}

/// Decides whether a device may start a handshake.
pub type Authorizer<'a> = dyn FnMut(&[u8; DEVICE_ID_LEN], &[u8; 32]) -> bool + 'a;

impl HostEndpoint {
    /// Bind a UDP socket for the streaming port.
    ///
    /// A read timeout is set by default so a caller that forgets to pump the
    /// endpoint gets a timeout error instead of blocking forever.
    pub fn bind(addr: &str, host_identity: DeviceIdentity) -> std::io::Result<Self> {
        let socket = UdpSocket::bind(addr)?;
        socket.set_read_timeout(Some(DEFAULT_READ_TIMEOUT))?;
        Ok(Self {
            socket,
            host_identity,
            pending: HashMap::new(),
            peers: HashMap::new(),
            next_sequence: 0,
        })
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Offer one received datagram to the host.
    ///
    /// `authorized` is consulted for every handshake attempt. Returning `false`
    /// drops the packet with no reply.
    pub fn handle_packet(
        &mut self,
        packet: &[u8],
        from: SocketAddr,
        authorized: &mut Authorizer<'_>,
    ) -> HostOutcome {
        match packet.first() {
            Some(&packet_type::HANDSHAKE_INIT) => self.handle_init(packet, from, authorized),
            Some(&packet_type::HANDSHAKE_CONFIRM) => self.handle_confirm(packet, from),
            Some(&packet_type::DATA) => {
                self.handle_data(packet, from);
                HostOutcome::Silent
            }
            // Unknown or empty: drop silently, never answer.
            _ => HostOutcome::Silent,
        }
    }

    fn handle_init(
        &mut self,
        packet: &[u8],
        from: SocketAddr,
        authorized: &mut Authorizer<'_>,
    ) -> HostOutcome {
        let body = match packet.strip_prefix([packet_type::HANDSHAKE_INIT].as_slice()) {
            Some(b) if b.len() >= DEVICE_ID_LEN + 64 => b,
            _ => return HostOutcome::Silent,
        };
        let mut device_id = [0u8; DEVICE_ID_LEN];
        device_id.copy_from_slice(&body[..DEVICE_ID_LEN]);
        let mut device_static = [0u8; 32];
        device_static.copy_from_slice(&body[DEVICE_ID_LEN..DEVICE_ID_LEN + 32]);

        // Default-deny: no reply at all for an unpaired or mismatched peer.
        if !authorized(&device_id, &device_static) {
            return HostOutcome::Silent;
        }

        // The identity borrow ends with this statement, so no in-flight
        // handshake has to hold one.
        let mut server = HandshakeServer::new(&self.host_identity);
        match server.accept_init(packet) {
            Ok(outcome) => {
                self.pending.insert(
                    from,
                    Pending {
                        device_id: outcome.device_id,
                        session: outcome.session,
                    },
                );
                HostOutcome::Reply(outcome.response)
            }
            Err(_) => HostOutcome::Silent,
        }
    }

    fn handle_confirm(&mut self, packet: &[u8], from: SocketAddr) -> HostOutcome {
        let Some(pending) = self.pending.get(&from) else {
            return HostOutcome::Silent;
        };
        // Same rule as HandshakeServer::finish_confirm, one implementation.
        if verify_confirm(&pending.session, packet).is_err() {
            self.pending.remove(&from);
            return HostOutcome::Silent;
        }
        let pending = self.pending.remove(&from).expect("just checked");
        self.peers.insert(
            pending.device_id,
            Peer {
                addr: from,
                session: pending.session,
                window: ReplayWindow::default(),
            },
        );
        HostOutcome::Silent
    }

    fn handle_data(&mut self, packet: &[u8], from: SocketAddr) {
        let Some((_, peer)) = self.peers.iter_mut().find(|(_, p)| p.addr == from) else {
            return;
        };
        // Replay and authentication failures are silent by design.
        let _ = open_packet(
            &peer.session,
            crate::crypto::Direction::DeviceToHost,
            &mut peer.window,
            packet,
        );
    }

    /// Read and dispatch whatever is waiting, up to `max` datagrams.
    ///
    /// Returns how many datagrams were processed. Timeouts are treated as
    /// "nothing waiting" so the engine can keep capturing.
    pub fn pump<F>(&mut self, max: usize, mut authorized: F) -> std::io::Result<usize>
    where
        F: FnMut(&[u8; DEVICE_ID_LEN], &[u8; 32]) -> bool,
    {
        let mut processed = 0;
        let mut buf = [0u8; MAX_DATAGRAM];
        while processed < max {
            let (n, from) = match self.socket.recv_from(&mut buf) {
                Ok(v) => v,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    break
                }
                Err(e) => return Err(e),
            };
            let outcome = self.handle_packet(&buf[..n], from, &mut authorized);
            processed += 1;
            if let HostOutcome::Reply(bytes) = outcome {
                self.socket.send_to(&bytes, from)?;
            }
        }
        Ok(processed)
    }

    /// Number of confirmed peers.
    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }

    /// Disconnect a device — call on revoke.
    pub fn disconnect(&mut self, device_id: &[u8; DEVICE_ID_LEN]) -> bool {
        self.peers.remove(device_id).is_some()
    }

    /// Build and send one *frame*, fragmenting it across datagrams.
    ///
    /// Returns the number of datagrams sent. Each datagram gets its own record
    /// sequence (used by the replay window and as AEAD associated data); all
    /// fragments share one frame sequence so the receiver can reassemble them.
    pub fn send_frame(
        &mut self,
        device_id: &[u8; DEVICE_ID_LEN],
        timestamp: u32,
        keyframe: bool,
        payload_type: u8,
        payload: &[u8],
    ) -> Result<usize, TransportError> {
        let (addr, session) = {
            let peer = self
                .peers
                .get(device_id)
                .ok_or(TransportError::Dropped("no such peer"))?;
            (peer.addr, peer.session.clone())
        };

        let chunks = payload_chunks(payload);
        let frag_count = u16::try_from(chunks.len())
            .map_err(|_| TransportError::PayloadTooLarge(payload.len()))?;
        let frame_sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.wrapping_add(1);

        for (index, chunk) in chunks.iter().enumerate() {
            let record_sequence = self.next_sequence;
            self.next_sequence = self.next_sequence.wrapping_add(1);
            let record = build_record(
                record_sequence,
                frame_sequence,
                timestamp,
                keyframe,
                payload_type,
                chunk,
                index as u16,
                frag_count,
            )?;
            // Single definition of the wire layout: type || seq || nonce || ct.
            let packet = seal_packet(
                &session,
                crate::crypto::Direction::HostToDevice,
                record_sequence,
                &record,
            )?;
            self.socket
                .send_to(&packet, addr)
                .map_err(|_| TransportError::Dropped("send failed"))?;
        }
        Ok(chunks.len())
    }
}

/// Device-side endpoint.
pub struct ClientEndpoint {
    socket: UdpSocket,
    handshake: Option<HandshakeClient>,
    session: Option<Session>,
    window: ReplayWindow,
}

impl ClientEndpoint {
    pub fn new(
        server_addr: &str,
        device_id: [u8; DEVICE_ID_LEN],
        identity: DeviceIdentity,
        host_static: [u8; 32],
    ) -> std::io::Result<Self> {
        let socket = UdpSocket::bind("0.0.0.0:0")?;
        let server_addr: SocketAddr = server_addr
            .parse()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        socket.connect(server_addr)?;
        Ok(Self {
            socket,
            handshake: Some(HandshakeClient::new(device_id, identity, host_static)),
            session: None,
            window: ReplayWindow::default(),
        })
    }

    /// Send `init`. Returns nothing; the caller drives the exchange.
    pub fn send_init(&mut self) -> Result<(), TransportError> {
        let client = self
            .handshake
            .as_mut()
            .ok_or(TransportError::Dropped("handshake already started"))?;
        let init = client.init();
        self.socket
            .send(&init)
            .map(|_| ())
            .map_err(|_| TransportError::Dropped("send failed"))
    }

    /// Read one raw datagram (used for the handshake response).
    pub fn recv_raw(&mut self, timeout: Duration) -> Result<Vec<u8>, TransportError> {
        self.socket
            .set_read_timeout(Some(timeout))
            .map_err(|_| TransportError::Dropped("timeout setup failed"))?;
        let mut buf = [0u8; MAX_DATAGRAM];
        let n = self
            .socket
            .recv(&mut buf)
            .map_err(|_| TransportError::Dropped("no datagram"))?;
        Ok(buf[..n].to_vec())
    }

    /// Consume the host's response, send `confirm`, and return the session.
    pub fn complete(&mut self, response: &[u8]) -> Result<Session, TransportError> {
        let client = self
            .handshake
            .as_mut()
            .ok_or(TransportError::Dropped("handshake already complete"))?;
        let session = client
            .finish(response)
            .map_err(|_| TransportError::Dropped("handshake rejected"))?;
        let confirm = client.confirm(&session);
        self.socket
            .send(&confirm)
            .map_err(|_| TransportError::Dropped("send confirm failed"))?;
        self.session = Some(session.clone());
        Ok(session)
    }

    /// Blocking receive of one record, for a real socket read.
    pub fn recv_record(
        &mut self,
        timeout: Duration,
    ) -> Result<(FrameHeader, Vec<u8>), TransportError> {
        let session = self
            .session
            .as_ref()
            .ok_or(TransportError::Dropped("no session"))?;
        self.socket
            .set_read_timeout(Some(timeout))
            .map_err(|_| TransportError::Dropped("timeout setup failed"))?;
        let mut buf = [0u8; MAX_DATAGRAM];
        let n = self
            .socket
            .recv(&mut buf)
            .map_err(|_| TransportError::Dropped("no record"))?;
        open_packet(
            session,
            crate::crypto::Direction::HostToDevice,
            &mut self.window,
            &buf[..n],
        )
    }

    #[cfg(test)]
    pub fn session(&self) -> Option<&Session> {
        self.session.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl ClientEndpoint {
        fn local_addr(&self) -> std::io::Result<SocketAddr> {
            self.socket.local_addr()
        }
    }

    use crate::codec::Codec;
    use crate::transport::Reassembler;

    fn dev_id(n: u8) -> [u8; DEVICE_ID_LEN] {
        let mut out = [0u8; DEVICE_ID_LEN];
        for (i, b) in out.iter_mut().enumerate() {
            *b = n.wrapping_add(i as u8);
        }
        out
    }

    /// Run the whole handshake over real loopback sockets, synchronously.
    fn established() -> (HostEndpoint, ClientEndpoint, [u8; DEVICE_ID_LEN]) {
        let host_identity = DeviceIdentity::generate().unwrap();
        let mut host = HostEndpoint::bind("127.0.0.1:0", host_identity).unwrap();
        let host_static = host.host_identity.public_key_bytes();
        let addr = host.local_addr().unwrap();

        let device_identity = DeviceIdentity::generate().unwrap();
        let device_pub = device_identity.public_key_bytes();
        let id = dev_id(3);
        let mut client =
            ClientEndpoint::new(&addr.to_string(), id, device_identity, host_static).unwrap();
        let from = client.local_addr().unwrap();

        // 1. init
        client.send_init().unwrap();
        let mut buf = [0u8; MAX_DATAGRAM];
        let (n, _) = host.socket.recv_from(&mut buf).unwrap();
        let mut allow = |cid: &[u8; DEVICE_ID_LEN], pk: &[u8; 32]| cid == &id && pk == &device_pub;
        let response = match host.handle_packet(&buf[..n], from, &mut allow) {
            HostOutcome::Reply(r) => r,
            HostOutcome::Silent => panic!("host refused a paired device"),
        };

        // 2. response -> confirm
        client.complete(&response).unwrap();
        let (n, _) = host.socket.recv_from(&mut buf).unwrap();
        assert_eq!(
            host.handle_packet(&buf[..n], from, &mut allow),
            HostOutcome::Silent
        );
        assert_eq!(host.peer_count(), 1, "peer confirmed after the exchange");
        (host, client, id)
    }

    #[test]
    fn handshake_over_real_sockets_connects_a_peer() {
        let (mut host, _client, id) = established();
        assert_eq!(host.peer_count(), 1);
        assert!(host
            .send_frame(&id, 0, true, Codec::H264.payload_type(), b"hello")
            .is_ok());
    }

    #[test]
    fn unpaired_device_is_dropped_without_reply() {
        let host_identity = DeviceIdentity::generate().unwrap();
        let mut host = HostEndpoint::bind("127.0.0.1:0", host_identity).unwrap();
        let host_static = host.host_identity.public_key_bytes();
        let addr = host.local_addr().unwrap();

        let device = DeviceIdentity::generate().unwrap();
        let id = dev_id(3);
        let mut client = ClientEndpoint::new(&addr.to_string(), id, device, host_static).unwrap();
        let from = client.local_addr().unwrap();
        client.send_init().unwrap();

        let mut buf = [0u8; MAX_DATAGRAM];
        let (n, _) = host.socket.recv_from(&mut buf).unwrap();
        // Default-deny: this device is not paired.
        let mut deny = |_: &[u8; DEVICE_ID_LEN], _: &[u8; 32]| false;
        assert_eq!(
            host.handle_packet(&buf[..n], from, &mut deny),
            HostOutcome::Silent,
            "an unpaired device must get no reply at all"
        );
        assert_eq!(host.peer_count(), 0);
    }

    #[test]
    fn unknown_packet_types_are_dropped_silently() {
        let (mut host, _client, _id) = established();
        // 0xEE is not a type this host knows: no reply, no state change.
        let mut allow = |_: &[u8; DEVICE_ID_LEN], _: &[u8; 32]| true;
        let strangers: Vec<SocketAddr> = vec![
            "127.0.0.1:1".parse().unwrap(),
            "127.0.0.1:2".parse().unwrap(),
        ];
        for addr in strangers {
            assert_eq!(
                host.handle_packet(&[0xEE, 1, 2, 3], addr, &mut allow),
                HostOutcome::Silent
            );
        }
        assert_eq!(host.peer_count(), 1, "no state was created");
    }

    #[test]
    fn record_travels_host_to_device_and_is_authenticated() {
        let (mut host, mut client, id) = established();
        let payload = vec![0xABu8; 900];
        let datagrams = host
            .send_frame(&id, 90_000, true, Codec::H264.payload_type(), &payload)
            .unwrap();
        assert_eq!(datagrams, 1, "a small payload fits one datagram");
        let (header, got) = client
            .recv_record(Duration::from_secs(2))
            .expect("record should arrive");
        // Frame sequence identifies the frame; record sequence identifies this
        // datagram and is what the replay window and AEAD are bound to.
        assert_eq!(header.frame_sequence, 0);
        assert_eq!(header.sequence, 1);
        assert!(header.keyframe);
        assert_eq!(got, payload);
    }

    #[test]
    fn a_large_frame_is_fragmented_and_reassembled() {
        let (mut host, mut client, id) = established();
        // Comfortably more than one datagram's payload budget.
        let payload: Vec<u8> = (0..20_000u32).map(|i| (i % 251) as u8).collect();
        let datagrams = host
            .send_frame(&id, 90_000, true, Codec::H264.payload_type(), &payload)
            .unwrap();
        assert!(datagrams > 1, "expected fragmentation, got {datagrams}");

        let mut reassembler = Reassembler::default();
        let mut frames = 0;
        for _ in 0..datagrams {
            let (header, chunk) = client.recv_record(Duration::from_secs(2)).unwrap();
            // Every fragment has its own record sequence (so the replay window
            // accepts each) but shares the frame sequence.
            if let Some(whole) = reassembler.push(header, &chunk) {
                frames += 1;
                assert_eq!(whole, payload, "reassembled frame must be intact");
            }
        }
        assert_eq!(frames, 1, "exactly one complete frame");
    }

    #[test]
    fn sending_to_an_unconnected_device_fails() {
        let (mut host, _client, _id) = established();
        let ghost = dev_id(99);
        assert!(matches!(
            host.send_frame(&ghost, 0, false, Codec::H264.payload_type(), b"x"),
            Err(TransportError::Dropped(_))
        ));
    }

    #[test]
    fn revoke_disconnects_a_live_peer() {
        let (mut host, _client, id) = established();
        assert_eq!(host.peer_count(), 1);
        assert!(host.disconnect(&id), "revoke should drop the live peer");
        assert_eq!(host.peer_count(), 0);
        assert!(matches!(
            host.send_frame(&id, 0, false, Codec::H264.payload_type(), b"x"),
            Err(TransportError::Dropped(_))
        ));
    }

    #[test]
    fn replayed_record_is_dropped_by_the_receiver() {
        let (mut host, mut client, id) = established();
        host.send_frame(&id, 0, false, Codec::H264.payload_type(), b"one")
            .unwrap();
        client.recv_record(Duration::from_secs(2)).unwrap();

        // Re-send the identical datagram by hand on the wire.
        let mut packet = Vec::new();
        packet.push(packet_type::DATA);
        packet.extend_from_slice(&0u32.to_be_bytes());
        let record =
            build_record(0, 0, 0, false, Codec::H264.payload_type(), b"one", 0, 1).unwrap();
        let sealed = {
            let session = client.session().unwrap();
            crate::crypto::seal(session, crate::crypto::Direction::HostToDevice, 0, &record)
                .unwrap()
        };
        packet.extend_from_slice(&sealed);
        client.socket.send(&packet).unwrap();
        assert!(
            client.recv_record(Duration::from_millis(300)).is_err(),
            "a replayed record must be rejected"
        );
    }
}
