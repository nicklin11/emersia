//! Device side of in-headset pairing (ADR 0008): listen for a host's beacon,
//! run the SAS exchange, show the code, and finish only when *both* the host
//! user (authentic RESULT) and the headset user (`confirm`) said yes.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use emersia_transport::crypto::DeviceIdentity;
use emersia_transport::pairing::{DeviceExchange, Message, EXCHANGE_TTL};

/// How often COMMIT is re-sent until the host answers.
const RETRY: Duration = Duration::from_millis(500);
const ATTEMPTS: usize = 10;
const READ_SLICE: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairState {
    Searching,
    /// Compare this code with the PC.
    Code {
        code: String,
        host_name: String,
        /// The headset user already confirmed; waiting for the PC.
        confirmed: bool,
    },
    Accepted {
        server: String,
        host_key: [u8; 32],
    },
    Rejected,
    Failed(String),
}

impl PairState {
    /// One line for Java: `searching`, `code|123 456|host|0`,
    /// `accepted|ip:port|hostkeyhex`, `rejected`, `error|message`.
    pub fn to_line(&self) -> String {
        match self {
            Self::Searching => "searching".into(),
            Self::Code {
                code,
                host_name,
                confirmed,
            } => format!(
                "code|{code}|{}|{}",
                host_name.replace('|', " "),
                u8::from(*confirmed)
            ),
            Self::Accepted { server, host_key } => {
                format!("accepted|{server}|{}", hex(host_key))
            }
            Self::Rejected => "rejected".into(),
            Self::Failed(e) => format!("error|{}", e.replace('|', " ")),
        }
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn set(state: &Mutex<PairState>, s: PairState) {
    *state.lock().unwrap_or_else(|p| p.into_inner()) = s;
}

/// Run until paired, rejected, failed or stopped. `discovery` is a socket
/// bound to the discovery port.
pub fn run(
    discovery: &UdpSocket,
    identity: &DeviceIdentity,
    device_name: &str,
    state: &Mutex<PairState>,
    user_confirmed: &AtomicBool,
    stop: &AtomicBool,
) {
    set(state, PairState::Searching);
    let _ = discovery.set_read_timeout(Some(READ_SLICE));
    let mut buf = [0u8; 2048];
    // 1. Wait for a beacon.
    let (host_addr, host_key, host_name) = loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let Ok((n, from)) = discovery.recv_from(&mut buf) else {
            continue;
        };
        if let Some(Message::Beacon {
            host_key,
            stream_port,
            name,
        }) = Message::decode(&buf[..n])
        {
            break (SocketAddr::new(from.ip(), stream_port), host_key, name);
        }
    };
    log::info!("pairing with {host_name} at {host_addr}");
    match exchange(
        host_addr,
        host_key,
        &host_name,
        identity,
        device_name,
        state,
        user_confirmed,
        stop,
    ) {
        Ok(final_state) => set(state, final_state),
        Err(e) => set(state, PairState::Failed(e)),
    }
}

#[allow(clippy::too_many_arguments)]
fn exchange(
    host: SocketAddr,
    host_key: [u8; 32],
    host_name: &str,
    identity: &DeviceIdentity,
    device_name: &str,
    state: &Mutex<PairState>,
    user_confirmed: &AtomicBool,
    stop: &AtomicBool,
) -> Result<PairState, String> {
    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("socket: {e}"))?;
    socket
        .connect(host)
        .map_err(|e| format!("connect {host}: {e}"))?;
    socket
        .set_read_timeout(Some(READ_SLICE))
        .map_err(|e| e.to_string())?;
    let device_key = identity.public_key_bytes();
    let mut ex = DeviceExchange::new(host_key, device_key).ok_or("no randomness")?;
    let mut buf = [0u8; 2048];

    // 2. COMMIT until NONCE.
    let commit = ex.commit(device_name);
    let mut nonce = None;
    'attempts: for _ in 0..ATTEMPTS {
        let _ = socket.send(&commit);
        let deadline = Instant::now() + RETRY;
        while Instant::now() < deadline {
            if stop.load(Ordering::Relaxed) {
                return Err("cancelled".into());
            }
            if let Ok(n) = socket.recv(&mut buf) {
                if let Some(Message::Nonce {
                    device_key: k,
                    nonce: n_h,
                }) = Message::decode(&buf[..n])
                {
                    if k == device_key {
                        nonce = Some(n_h);
                        break 'attempts;
                    }
                }
            }
        }
    }
    let nonce = nonce.ok_or_else(|| {
        format!(
            "{host_name} did not answer (is UDP {} allowed through its firewall?)",
            host.port()
        )
    })?;

    // 3. REVEAL (twice: it is never answered, so loss would be silent).
    let reveal = ex.on_nonce(nonce);
    let _ = socket.send(&reveal);
    let _ = socket.send(&reveal);
    let code = ex.code().ok_or("no code")?;
    // The code is shown on screen anyway; logging it makes pairing problems
    // diagnosable over adb.
    log::info!("showing pairing code {code} for {host_name}");

    // 4. Wait for the PC user's decision and the headset user's.
    let started = Instant::now();
    let mut host_said: Option<bool> = None;
    loop {
        if stop.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        if started.elapsed() > EXCHANGE_TTL {
            return Err("timed out waiting for confirmation".into());
        }
        let confirmed = user_confirmed.load(Ordering::Relaxed);
        match host_said {
            Some(false) => return Ok(PairState::Rejected),
            Some(true) if confirmed => {
                log::info!("paired with {host_name} at {host}");
                return Ok(PairState::Accepted {
                    server: host.to_string(),
                    host_key,
                });
            }
            _ => {}
        }
        set(
            state,
            PairState::Code {
                code: code.clone(),
                host_name: host_name.to_string(),
                confirmed,
            },
        );
        if host_said.is_none() {
            if let Ok(n) = socket.recv(&mut buf) {
                if let Some(Message::Result {
                    device_key: k,
                    accepted,
                    tag,
                }) = Message::decode(&buf[..n])
                {
                    // Unauthentic results (wrong tag) are ignored, not fatal.
                    if k == device_key {
                        host_said = ex.verify_result(accepted, &tag);
                        match host_said {
                            Some(true) => {
                                log::info!("{host_name} confirmed; waiting for the headset user")
                            }
                            Some(false) => log::info!("{host_name} rejected the pairing"),
                            None => log::warn!("ignoring an unauthentic pairing result"),
                        }
                    }
                }
            }
        } else {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use emersia_transport::pairing::HostPairing;
    use std::sync::Arc;

    /// The whole flow over loopback against the real host state machine.
    fn run_flow(host_accepts: bool, device_confirms: bool) -> PairState {
        let host_identity = DeviceIdentity::generate().unwrap();
        let host_key = host_identity.public_key_bytes();
        let host_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        host_sock
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let mut host = HostPairing::new(host_key);
        host.open(Duration::from_secs(30));

        let discovery = UdpSocket::bind("127.0.0.1:0").unwrap();
        let discovery_addr = discovery.local_addr().unwrap();
        let state = Arc::new(Mutex::new(PairState::Searching));
        let confirmed = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let device = {
            let (state, confirmed, stop) = (state.clone(), confirmed.clone(), stop.clone());
            std::thread::spawn(move || {
                let identity = DeviceIdentity::generate().unwrap();
                run(&discovery, &identity, "Quest 2", &state, &confirmed, &stop);
            })
        };

        let port = host_sock.local_addr().unwrap().port();
        let mut buf = [0u8; 2048];
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut decided = false;
        loop {
            assert!(Instant::now() < deadline, "flow did not finish");
            let _ = host_sock.send_to(&host.beacon(port, "charoite").unwrap(), discovery_addr);
            if let Ok((n, from)) = host_sock.recv_from(&mut buf) {
                if let Some(reply) = host.handle(&buf[..n], from) {
                    host_sock.send_to(&reply, from).unwrap();
                }
            }
            let line = state.lock().unwrap().clone();
            if let PairState::Code { code, .. } = &line {
                if !decided {
                    let pending = host.pending();
                    assert_eq!(&pending[0].code, code, "both sides show the same code");
                    let (to, result) = host.decide(&pending[0].device_key, host_accepts).unwrap();
                    host_sock.send_to(&result, to).unwrap();
                    decided = true;
                    if device_confirms {
                        confirmed.store(true, Ordering::Relaxed);
                    } else {
                        stop.store(true, Ordering::Relaxed);
                    }
                }
            }
            if device.is_finished() {
                break;
            }
        }
        device.join().unwrap();
        let s = state.lock().unwrap().clone();
        s
    }

    #[test]
    fn both_confirm_pairs_and_pins_the_beaconed_host() {
        match run_flow(true, true) {
            PairState::Accepted { server, .. } => assert!(server.starts_with("127.0.0.1:")),
            other => panic!("expected accepted, got {other:?}"),
        }
    }

    #[test]
    fn host_rejection_is_reported() {
        assert_eq!(run_flow(false, true), PairState::Rejected);
    }

    #[test]
    fn headset_cancel_never_pairs() {
        assert!(!matches!(run_flow(true, false), PairState::Accepted { .. }));
    }

    #[test]
    fn state_lines_are_parseable() {
        assert_eq!(PairState::Searching.to_line(), "searching");
        assert_eq!(
            PairState::Code {
                code: "123 456".into(),
                host_name: "a|b".into(),
                confirmed: true
            }
            .to_line(),
            "code|123 456|a b|1"
        );
        assert!(PairState::Accepted {
            server: "1.2.3.4:5".into(),
            host_key: [0xab; 32]
        }
        .to_line()
        .starts_with("accepted|1.2.3.4:5|abab"));
    }
}
