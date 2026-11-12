//! The stream endpoint's receive side, on its own thread.
//!
//! Capture blocks until the compositor repaints — up to `FRAME_TIMEOUT` on an
//! idle niri desktop — and the engine used to pump the UDP socket only between
//! captures. Handshakes, revocations and device messages therefore waited for
//! screen damage. This thread blocks in `recv_from` on a clone of the socket
//! and takes the endpoint lock only to process a datagram, so the engine's
//! `send_frame` calls are never held up by a read.

use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::inject::Injector;
use crate::service::Service;
use crate::transport::control;
use crate::transport::udp::HostEndpoint;
use crate::transport_pairing as pairing;

/// Read timeout: how quickly the thread notices shutdown and revocations when
/// no datagram arrives.
const READ_TIMEOUT: Duration = Duration::from_millis(100);
/// A session with no record for this long is closed (clients ping at 1 Hz).
const PEER_TIMEOUT: Duration = Duration::from_secs(6);

/// Presence announcement period (always, while the stream port is bound).
const PRESENCE_INTERVAL: Duration = Duration::from_secs(2);

/// Beacon period while a pairing window is open.
const BEACON_INTERVAL: Duration = Duration::from_millis(500);

/// Directed IPv4 broadcast address of every up, non-loopback interface.
///
/// The limited broadcast 255.255.255.255 leaves through the default route
/// only; a headset on another interface (a Wi-Fi hotspot next to wired
/// Ethernet, the reference setup) would never see it. The address is computed
/// from address and netmask rather than read from the interface: hotspot
/// setups often configure no broadcast address at all (the kernel then reports
/// 0.0.0.0, which delivers the beacon to this host only).
fn broadcast_addresses() -> Vec<std::net::Ipv4Addr> {
    let mut out = Vec::new();
    let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs allocates a list we walk read-only and free once.
    unsafe {
        if libc::getifaddrs(&mut ifap) != 0 {
            return out;
        }
        let mut cur = ifap;
        while let Some(ifa) = cur.as_ref() {
            let flags = ifa.ifa_flags;
            let up = flags & libc::IFF_UP as u32 != 0;
            let loopback = flags & libc::IFF_LOOPBACK as u32 != 0;
            let ipv4 = |sa: *mut libc::sockaddr| {
                (!sa.is_null() && (*sa).sa_family as i32 == libc::AF_INET)
                    .then(|| u32::from_be((*(sa as *const libc::sockaddr_in)).sin_addr.s_addr))
            };
            if let (true, false, Some(addr), Some(mask)) =
                (up, loopback, ipv4(ifa.ifa_addr), ipv4(ifa.ifa_netmask))
            {
                if let Some(bcast) = directed_broadcast(addr, mask) {
                    if !out.contains(&bcast) {
                        out.push(bcast);
                    }
                }
            }
            cur = ifa.ifa_next;
        }
        libc::freeifaddrs(ifap);
    }
    out
}

/// `addr | !mask`, or `None` for point-to-point style /31–/32 networks
/// (VPN tunnels like tailscale), which have no broadcast.
fn directed_broadcast(addr: u32, mask: u32) -> Option<std::net::Ipv4Addr> {
    (mask.count_ones() <= 30).then(|| std::net::Ipv4Addr::from(addr | !mask))
}

/// Largest datagram accepted.
const MAX_DATAGRAM: usize = 65_535;

/// The endpoint shared by the engine (sends) and this thread (receives).
pub type SharedEndpoint = Arc<Mutex<HostEndpoint>>;

pub fn lock(endpoint: &SharedEndpoint) -> MutexGuard<'_, HostEndpoint> {
    // Endpoint state is per-peer data with no cross-field invariant a panic
    // could break halfway; recovering beats killing the stream.
    endpoint.lock().unwrap_or_else(|e| e.into_inner())
}

/// Start the receive thread. It exits when the service shuts down.
pub fn spawn(
    endpoint: SharedEndpoint,
    service: Arc<Service>,
    input_enabled: bool,
    notify: bool,
) -> std::io::Result<JoinHandle<()>> {
    let socket = lock(&endpoint).try_clone_socket()?;
    socket.set_read_timeout(Some(READ_TIMEOUT))?;
    // Pairing beacons are broadcast from the stream socket, so a headset
    // learns the stream port from the beacon's source.
    socket.set_broadcast(true)?;
    std::thread::Builder::new()
        .name("emersia-net".into())
        .spawn(move || run(socket, endpoint, service, input_enabled, notify))
}

fn run(
    socket: std::net::UdpSocket,
    endpoint: SharedEndpoint,
    service: Arc<Service>,
    input_enabled: bool,
    notify: bool,
) {
    let mut buf = vec![0u8; MAX_DATAGRAM];
    let mut known_peers: std::collections::BTreeSet<[u8; 8]> = Default::default();
    // View-only devices whose dropped input has been logged (once each).
    let mut view_only_noted: std::collections::BTreeSet<[u8; 8]> = Default::default();
    let mut injector = Injector::default();
    let stream_port = socket.local_addr().map(|a| a.port()).unwrap_or(0);
    let host_name = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|h| h.trim().to_string())
        .unwrap_or_else(|_| "emersia host".into());
    let mut last_beacon: Option<std::time::Instant> = None;
    let mut last_presence: Option<std::time::Instant> = None;
    let host_key =
        crate::crypto::parse_public_key_hex(service.host_public_key()).unwrap_or([0; 32]);
    while !service.is_shutting_down() && !crate::signal_received() {
        let received = match socket.recv_from(&mut buf) {
            Ok(v) => Some(v),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) =>
            {
                None
            }
            Err(e) => {
                eprintln!("emersia-daemon: stream receive error: {e}");
                std::thread::sleep(READ_TIMEOUT);
                None
            }
        };

        // In-headset pairing (ADR 0008): cleartext, answered only while the
        // user has a window open, and never handed to the endpoint.
        let received = match received {
            Some((n, from)) if pairing::is_pairing_packet(&buf[..n]) => {
                let reply = service.lock_headset_pairing().handle(&buf[..n], from);
                if let Some(reply) = reply {
                    let _ = socket.send_to(&reply, from);
                }
                None
            }
            other => other,
        };
        for (to, packet) in service.take_pairing_outbox() {
            let _ = socket.send_to(&packet, to);
        }
        if last_beacon.is_none_or(|t| t.elapsed() >= BEACON_INTERVAL) {
            last_beacon = Some(std::time::Instant::now());
            let beacon = service
                .lock_headset_pairing()
                .beacon(stream_port, &host_name);
            if let Some(beacon) = beacon {
                for addr in broadcast_addresses() {
                    let _ = socket.send_to(&beacon, (addr, pairing::DISCOVERY_PORT));
                }
            }
        }
        // Presence: key + port only (no hostname), so a paired headset can
        // follow this host to a new address. Unauthenticated by design — the
        // headset still runs the handshake against its pinned key.
        if last_presence.is_none_or(|t| t.elapsed() >= PRESENCE_INTERVAL) {
            last_presence = Some(std::time::Instant::now());
            let packet = pairing::presence(&host_key, stream_port);
            for addr in broadcast_addresses() {
                let _ = socket.send_to(&packet, (addr, pairing::DISCOVERY_PORT));
            }
        }

        let mut ep = lock(&endpoint);
        // A revoked device loses its session immediately, not at the next
        // captured frame.
        for id in service.take_revoked() {
            if ep.disconnect(&id) {
                eprintln!(
                    "emersia-daemon: revoked device {} disconnected from the stream",
                    hex::encode(id)
                );
            }
        }
        if let Some((n, from)) = received {
            // The authorizer reads the trust store at the moment of the
            // decision, so a device that pairs and immediately connects is
            // never refused from a stale snapshot.
            let mut allowed = |id: &[u8; 8], pubkey: &[u8; 32]| {
                let got_id = hex::encode(id);
                let got_key = hex::encode(pubkey);
                service.lock_pairing().devices().iter().any(|d| {
                    d.id == got_id && d.public_key.eq_ignore_ascii_case(&got_key) && !d.revoked
                })
            };
            if let Err(e) = ep.dispatch(&buf[..n], from, &mut allowed) {
                eprintln!("emersia-daemon: stream reply failed: {e}");
            }
        }
        // Liveness: clients ping every second; silence means the headset is
        // gone, so stop encoding and sending for it.
        for id in ep.expire_idle(PEER_TIMEOUT) {
            eprintln!(
                "emersia-daemon: device {} went silent; session closed",
                hex::encode(id)
            );
        }
        let records = ep.take_control();
        // Clock probes are answered at once, under the lock already held, so
        // the reply's host time is as close to the probe's arrival as possible.
        for record in records.iter().filter(|r| r.payload_type == control::PING) {
            if let Some(device_us) = control::parse_ping(&record.payload) {
                let host_us = service.stream_epoch().elapsed().as_micros() as u64;
                let pong = control::pong(device_us, host_us);
                let _ = ep.send_frame(&record.device_id, 0, false, control::PONG, &pong);
            }
        }
        let peers = ep.peer_count();
        // Inject outside the endpoint lock: uinput writes must not delay the
        // engine's frame sends.
        drop(ep);
        for record in records {
            match record.payload_type {
                // Payload: optional stream id (absent = stream 0).
                control::KEYFRAME_REQUEST => {
                    service.request_keyframe(record.payload.first().copied().unwrap_or(0))
                }
                control::INPUT if input_enabled => {
                    // View-only devices (#140) watch but never reach uinput.
                    // The trust store is read per record, so a toggle applies
                    // immediately.
                    let device = hex::encode(record.device_id);
                    if !service.lock_pairing().input_allowed(&device) {
                        if view_only_noted.insert(record.device_id) {
                            // Anything it was holding when it lost control is let go.
                            injector.release_all();
                            eprintln!("emersia-daemon: device {device} is view-only; its input is ignored");
                        }
                        continue;
                    }
                    view_only_noted.remove(&record.device_id);
                    match crate::input::decode(&record.payload) {
                        Ok(events) => {
                            injector.apply(&events, |stream| service.input_layout_for(stream))
                        }
                        Err(e) => eprintln!("emersia-daemon: bad input record from {device}: {e}"),
                    }
                }
                _ => {}
            }
        }
        if peers == 0 {
            injector.release_all();
        }
        // Visible consent: announce sessions starting and ending.
        let current: std::collections::BTreeSet<[u8; 8]> =
            lock(&endpoint).peer_ids().into_iter().collect();
        if current != known_peers {
            let name = |id: &[u8; 8]| {
                let hex_id = hex::encode(id);
                service
                    .lock_pairing()
                    .devices()
                    .iter()
                    .find(|d| d.id == hex_id)
                    .map(|d| d.name.clone())
                    .unwrap_or(hex_id)
            };
            for id in current.difference(&known_peers) {
                if notify {
                    crate::notify::send(crate::notify::Event::Connected, &name(id));
                }
            }
            for id in known_peers.difference(&current) {
                if notify {
                    crate::notify::send(crate::notify::Event::Disconnected, &name(id));
                }
            }
            known_peers = current;
        }
        service.set_stream_peers(peers);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directed_broadcast_comes_from_address_and_mask() {
        let ip = |s: &str| u32::from(s.parse::<std::net::Ipv4Addr>().unwrap());
        assert_eq!(
            directed_broadcast(ip("192.168.60.1"), ip("255.255.255.0")),
            Some("192.168.60.255".parse().unwrap())
        );
        assert_eq!(
            directed_broadcast(ip("172.18.19.95"), ip("255.255.224.0")),
            Some("172.18.31.255".parse().unwrap())
        );
        assert_eq!(
            directed_broadcast(ip("100.64.0.1"), ip("255.255.255.255")),
            None
        );
    }
}
