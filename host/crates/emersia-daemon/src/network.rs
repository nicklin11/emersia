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

use crate::service::Service;
use crate::transport::control;
use crate::transport::udp::HostEndpoint;

/// Read timeout: how quickly the thread notices shutdown and revocations when
/// no datagram arrives.
const READ_TIMEOUT: Duration = Duration::from_millis(100);
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
pub fn spawn(endpoint: SharedEndpoint, service: Arc<Service>) -> std::io::Result<JoinHandle<()>> {
    let socket = lock(&endpoint).try_clone_socket()?;
    socket.set_read_timeout(Some(READ_TIMEOUT))?;
    std::thread::Builder::new()
        .name("emersia-net".into())
        .spawn(move || run(socket, endpoint, service))
}

fn run(socket: std::net::UdpSocket, endpoint: SharedEndpoint, service: Arc<Service>) {
    let mut buf = vec![0u8; MAX_DATAGRAM];
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
        for record in ep.take_control() {
            // Input lands in M2.5; other record types are ignored.
            if record.payload_type == control::KEYFRAME_REQUEST {
                service.request_keyframe();
            }
        }
        service.set_stream_peers(ep.peer_count());
    }
}
