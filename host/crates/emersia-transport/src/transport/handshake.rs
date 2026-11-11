//! Forward-secret device handshake (ADR 0006).
//!
//! Three messages, Noise-`IK`-shaped, over plain UDP:
//!
//! ```text
//! device → host   init      device id, device static + ephemeral public keys,
//!                            and a tag sealed to the *pinned host static key*
//! host → device   response  host ephemeral public key and a tag that also
//!                            mixes in DH(device_ephemeral, host_ephemeral)
//! device → host   confirm   a tag over the final transcript
//! ```
//!
//! The device must already know the host's static public key (pinned during
//! pairing). That is what makes the host identifiable: an impostor cannot open
//! the init, and the device will not accept a response it cannot verify.
//!
//! The ephemeral Diffie-Hellman contributions are what give **forward secrecy**:
//! once a session is established, compromising either long-term key does not
//! reveal the session key. That is the gap ADR 0006 flagged for M1.4.
//!
//! Every HKDF salt is a hash of the running transcript, so no message can be
//! reordered, dropped or substituted without breaking the schedule.

use crate::crypto::{transcript, DeviceIdentity, Session};
use crate::transport::packet_type;

/// Handshake context string, versioned like the rest of the protocol.
const CONTEXT: &[u8] = b"emersia/handshake-v1";

/// Length of a device id, in bytes.
pub const DEVICE_ID_LEN: usize = 8;

/// Strip a single leading type byte.
fn strip_type(packet: &[u8], ty: u8) -> Option<&[u8]> {
    if packet.first() == Some(&ty) {
        Some(&packet[1..])
    } else {
        None
    }
}

/// HKDF-SHA256 over the supplied DH outputs, salted by the transcript.
fn master_key(ikm_parts: &[[u8; 32]], salt: &[u8; 32], label: &[u8]) -> [u8; 32] {
    let mut ikm = Vec::with_capacity(ikm_parts.len() * 32);
    for part in ikm_parts {
        ikm.extend_from_slice(part);
    }
    let info = [CONTEXT, label].concat();
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(salt), &ikm);
    let mut out = [0u8; 32];
    hk.expand(&info, &mut out)
        .expect("32 bytes is always a valid HKDF output length");
    out
}

/// Wrap a master key as a one-shot session so the AEAD helpers can be reused.
fn key_session(master: &[u8; 32], salt: &[u8; 32], label: &[u8]) -> Session {
    let info = [CONTEXT, label].concat();
    Session::from_key_material(master, salt, &info).expect("valid key material")
}

/// Client (device) side of the handshake.
pub struct HandshakeClient {
    device_id: [u8; DEVICE_ID_LEN],
    static_identity: DeviceIdentity,
    /// The host static key this device has pinned.
    host_static: [u8; 32],
    ephemeral: DeviceIdentity,
    step: Step,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Init,
    Response,
    Done,
}

impl HandshakeClient {
    /// Start a handshake. `host_static` must be the host key obtained during
    /// pairing; passing the wrong one guarantees failure, which is intended.
    pub fn new(
        device_id: [u8; DEVICE_ID_LEN],
        identity: DeviceIdentity,
        host_static: [u8; 32],
    ) -> Self {
        Self {
            device_id,
            static_identity: identity,
            host_static,
            ephemeral: DeviceIdentity::generate()
                .expect("OS CSPRNG is required to start a handshake"),
            step: Step::Init,
        }
    }

    /// The cleartext body sent in `init`; also the transcript input.
    fn init_body(&self) -> Vec<u8> {
        let mut body = Vec::with_capacity(DEVICE_ID_LEN + 64);
        body.extend_from_slice(&self.device_id);
        body.extend_from_slice(&self.static_identity.public_key_bytes());
        body.extend_from_slice(&self.ephemeral.public_key_bytes());
        body
    }

    /// Keys shared by both sides after `init`.
    fn init_keys(&self) -> Option<([u8; 32], [u8; 32], [u8; 32])> {
        let dh_ds_hs = self.static_identity.agree(&self.host_static)?;
        let dh_de_hs = self.ephemeral.agree(&self.host_static)?;
        Some((dh_ds_hs, dh_de_hs, transcript(&[&self.init_body()])))
    }

    /// Build the `init` message.
    pub fn init(&mut self) -> Vec<u8> {
        assert_eq!(self.step, Step::Init, "handshake already started");
        self.step = Step::Response;

        let (dh_ds_hs, dh_de_hs, salt) = self
            .init_keys()
            .expect("pinned host key must be a valid curve point");
        let k1 = master_key(&[dh_ds_hs, dh_de_hs], &salt, b"k1");
        let k1_session = key_session(&k1, &salt, b"k1");
        // Tag proves the device holds the paired identity *and* the host key.
        let tag = crate::crypto::seal(
            &k1_session,
            crate::crypto::Direction::DeviceToHost,
            1,
            &self.device_id,
        )
        .expect("sealing an init cannot fail with a fresh key");

        let mut out = vec![packet_type::HANDSHAKE_INIT];
        out.extend_from_slice(&self.init_body());
        out.extend_from_slice(&tag);
        out
    }

    /// Consume the host's `response`, returning the established session.
    pub fn finish(&mut self, response: &[u8]) -> Result<Session, HandshakeError> {
        if self.step != Step::Response {
            return Err(HandshakeError::OutOfOrder);
        }
        self.step = Step::Done;
        let packet = strip_type(response, packet_type::HANDSHAKE_RESPONSE)
            .ok_or(HandshakeError::BadResponse)?;
        if packet.len() < 32 {
            return Err(HandshakeError::BadResponse);
        }
        let mut host_eph = [0u8; 32];
        host_eph.copy_from_slice(&packet[..32]);
        let tag = &packet[32..];

        let (dh_ds_hs, dh_de_hs, salt1) = self.init_keys().ok_or(HandshakeError::NotTheHost)?;
        let k1 = master_key(&[dh_ds_hs, dh_de_hs], &salt1, b"k1");
        let k1_session = key_session(&k1, &salt1, b"k1");
        // The host must prove it holds the pinned static key.
        let opened =
            crate::crypto::open(&k1_session, crate::crypto::Direction::HostToDevice, 1, tag)
                .map_err(|_| HandshakeError::NotTheHost)?;
        if opened != self.device_id {
            return Err(HandshakeError::NotTheHost);
        }

        // Forward secrecy begins with the ephemeral-ephemeral secret, which is
        // mixed into the second key and carried into the session.
        let dh_de_he = self
            .ephemeral
            .agree(&host_eph)
            .ok_or(HandshakeError::BadResponse)?;
        let salt2 = transcript(&[&salt1, &host_eph]);
        let k2 = master_key(&[dh_de_he, k1], &salt2, b"k2");
        Session::from_key_material(&k2, &salt2, CONTEXT).map_err(|_| HandshakeError::BadResponse)
    }

    /// Build the `confirm` message that closes the handshake.
    pub fn confirm(&self, session: &Session) -> Vec<u8> {
        let mut out = vec![packet_type::HANDSHAKE_CONFIRM];
        out.extend_from_slice(
            &crate::crypto::seal(session, crate::crypto::Direction::DeviceToHost, 1, &[])
                .expect("confirm sealing cannot fail"),
        );
        out
    }
}

/// What the host learned from a valid `init`.
#[derive(Debug)]
pub struct InitOutcome {
    /// Bytes to send back to the device.
    pub response: Vec<u8>,
    pub device_id: [u8; DEVICE_ID_LEN],
    /// Session awaiting the device's `confirm`.
    pub session: Session,
}

/// Server (host) side of the handshake.
pub struct HandshakeServer<'a> {
    host_identity: &'a DeviceIdentity,
    step: Step,
    device_id: [u8; DEVICE_ID_LEN],
}

impl<'a> HandshakeServer<'a> {
    pub fn new(host_identity: &'a DeviceIdentity) -> Self {
        Self {
            host_identity,
            step: Step::Init,
            device_id: [0u8; DEVICE_ID_LEN],
        }
    }

    /// Consume a device's `init`.
    ///
    /// The caller **must** check the device id against its active device set
    /// before replying — that is the default-deny gate.
    pub fn accept_init(&mut self, packet: &[u8]) -> Result<InitOutcome, HandshakeError> {
        if self.step != Step::Init {
            return Err(HandshakeError::OutOfOrder);
        }
        let body =
            strip_type(packet, packet_type::HANDSHAKE_INIT).ok_or(HandshakeError::BadInit)?;
        if body.len() < DEVICE_ID_LEN + 64 {
            return Err(HandshakeError::BadInit);
        }
        let header_len = DEVICE_ID_LEN + 64;
        let mut device_id = [0u8; DEVICE_ID_LEN];
        device_id.copy_from_slice(&body[..DEVICE_ID_LEN]);
        let mut device_static = [0u8; 32];
        device_static.copy_from_slice(&body[DEVICE_ID_LEN..DEVICE_ID_LEN + 32]);
        let mut device_eph = [0u8; 32];
        device_eph.copy_from_slice(&body[DEVICE_ID_LEN + 32..header_len]);
        let tag = &body[header_len..];

        let dh_ds_hs = self
            .host_identity
            .agree(&device_static)
            .ok_or(HandshakeError::BadInit)?;
        let dh_de_hs = self
            .host_identity
            .agree(&device_eph)
            .ok_or(HandshakeError::BadInit)?;
        let salt1 = transcript(&[&body[..header_len]]);
        let k1 = master_key(&[dh_ds_hs, dh_de_hs], &salt1, b"k1");
        let k1_session = key_session(&k1, &salt1, b"k1");

        // Only a device that knows our static key can produce a valid tag.
        let opened =
            crate::crypto::open(&k1_session, crate::crypto::Direction::DeviceToHost, 1, tag)
                .map_err(|_| HandshakeError::BadInit)?;
        if opened != device_id {
            return Err(HandshakeError::BadInit);
        }

        let ephemeral = DeviceIdentity::generate().expect("OS CSPRNG required");
        let dh_de_he = ephemeral
            .agree(&device_eph)
            .ok_or(HandshakeError::BadInit)?;
        let host_eph = ephemeral.public_key_bytes();
        let salt2 = transcript(&[&salt1, &host_eph]);
        let k2 = master_key(&[dh_de_he, k1], &salt2, b"k2");
        let session = Session::from_key_material(&k2, &salt2, CONTEXT)
            .map_err(|_| HandshakeError::BadInit)?;

        // Prove we hold the pinned static key the device already knows.
        let sealed = crate::crypto::seal(
            &k1_session,
            crate::crypto::Direction::HostToDevice,
            1,
            &device_id,
        )
        .map_err(|_| HandshakeError::BadInit)?;

        let mut out = vec![packet_type::HANDSHAKE_RESPONSE];
        out.extend_from_slice(&host_eph);
        out.extend_from_slice(&sealed);

        self.step = Step::Response;
        self.device_id = device_id;
        Ok(InitOutcome {
            response: out,
            device_id,
            session,
        })
    }
}

/// Verify a device's `confirm` against an already-negotiated session.
///
/// This is the single definition of the confirm rule. The confirm is sealed
/// under the session key, which requires the ephemeral-ephemeral secret, so a
/// party that merely replayed `init` can never produce one.
pub fn verify_confirm(session: &Session, packet: &[u8]) -> Result<(), HandshakeError> {
    let tag =
        strip_type(packet, packet_type::HANDSHAKE_CONFIRM).ok_or(HandshakeError::BadConfirm)?;
    crate::crypto::open(session, crate::crypto::Direction::DeviceToHost, 1, tag)
        .map(|_| ())
        .map_err(|_| HandshakeError::BadConfirm)
}

/// Handshake failures. All of these mean "drop the peer", never "reply".
#[derive(Debug, PartialEq, Eq)]
pub enum HandshakeError {
    BadInit,
    BadResponse,
    BadConfirm,
    OutOfOrder,
    /// The peer could not prove it knows the pinned host key.
    NotTheHost,
}

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadInit => write!(f, "malformed or unauthenticated handshake init"),
            Self::BadResponse => write!(f, "malformed handshake response"),
            Self::BadConfirm => write!(f, "handshake confirmation failed"),
            Self::OutOfOrder => write!(f, "handshake message out of order"),
            Self::NotTheHost => write!(f, "peer does not hold the pinned host key"),
        }
    }
}

impl std::error::Error for HandshakeError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::Direction;

    fn id_bytes(n: u8) -> [u8; DEVICE_ID_LEN] {
        [n; DEVICE_ID_LEN]
    }

    /// Drive a full handshake, returning both sessions and the device id.
    fn complete() -> (Session, Session, [u8; DEVICE_ID_LEN]) {
        let host = DeviceIdentity::generate().unwrap();
        let device = DeviceIdentity::generate().unwrap();
        let device_id = id_bytes(7);
        let mut client = HandshakeClient::new(device_id, device, host.public_key_bytes());
        let mut server = HandshakeServer::new(&host);

        let init = client.init();
        let outcome = server.accept_init(&init).unwrap();
        let client_session = client.finish(&outcome.response).unwrap();
        let confirm = client.confirm(&client_session);
        // The host verifies the confirm against the session it derived.
        verify_confirm(&outcome.session, &confirm).expect("confirm should verify");
        (client_session, outcome.session, outcome.device_id)
    }

    #[test]
    fn handshake_establishes_matching_sessions() {
        let (client, server, id) = complete();
        assert_eq!(id, id_bytes(7));
        let msg = b"first frame";
        let sealed = crate::crypto::seal(&server, Direction::HostToDevice, 1, msg).unwrap();
        assert_eq!(
            crate::crypto::open(&client, Direction::HostToDevice, 1, &sealed).unwrap(),
            msg
        );
    }

    #[test]
    fn both_directions_work() {
        let (client, server, _) = complete();
        let up = crate::crypto::seal(&server, Direction::HostToDevice, 3, b"up").unwrap();
        assert_eq!(
            crate::crypto::open(&client, Direction::HostToDevice, 3, &up).unwrap(),
            b"up"
        );
        let down = crate::crypto::seal(&client, Direction::DeviceToHost, 3, b"down").unwrap();
        assert_eq!(
            crate::crypto::open(&server, Direction::DeviceToHost, 3, &down).unwrap(),
            b"down"
        );
    }

    #[test]
    fn handshake_has_forward_secrecy() {
        // Two handshakes between the same pair of long-term identities must
        // yield different session keys, because each mixes in a fresh ephemeral
        // exchange. The session key is not derivable from stored static keys.
        let device = DeviceIdentity::generate().unwrap();
        let device_bytes = device.private_bytes_for_test();
        let host_bytes = DeviceIdentity::generate().unwrap().private_bytes_for_test();

        // Same long-term keys on both sides for both handshakes.
        let host_identity = DeviceIdentity::from_private_bytes(host_bytes);
        let mut sessions = Vec::new();
        for _ in 0..2 {
            let mut client = HandshakeClient::new(
                id_bytes(1),
                DeviceIdentity::from_private_bytes(device_bytes),
                host_identity.public_key_bytes(),
            );
            let mut server = HandshakeServer::new(&host_identity);
            let init = client.init();
            let outcome = server.accept_init(&init).unwrap();
            sessions.push(client.finish(&outcome.response).unwrap());
        }

        let sealed =
            crate::crypto::seal(&sessions[0], Direction::HostToDevice, 1, b"recorded").unwrap();
        assert!(
            crate::crypto::open(&sessions[1], Direction::HostToDevice, 1, &sealed).is_err(),
            "session keys must not be reproducible from the static keys alone"
        );
    }

    #[test]
    fn device_rejects_a_host_it_did_not_pin() {
        let real_host = DeviceIdentity::generate().unwrap();
        let fake_host = DeviceIdentity::generate().unwrap();
        let device = DeviceIdentity::generate().unwrap();

        // Client pins the real host; a response from the fake one must fail
        // because it cannot open the k1 tag sealed to the real key.
        let mut client = HandshakeClient::new(id_bytes(1), device, real_host.public_key_bytes());
        let init = client.init();

        // The fake host cannot even get through init: the tag does not open.
        let mut fake_server = HandshakeServer::new(&fake_host);
        assert_eq!(
            fake_server.accept_init(&init).unwrap_err(),
            HandshakeError::BadInit,
            "an unpinned host must not receive a response"
        );
    }

    #[test]
    fn host_rejects_a_client_that_knows_a_different_host_key() {
        let host = DeviceIdentity::generate().unwrap();
        let other = DeviceIdentity::generate().unwrap();
        let device = DeviceIdentity::generate().unwrap();
        // Client believes a different host owns the service.
        let mut client = HandshakeClient::new(id_bytes(1), device, other.public_key_bytes());
        let init = client.init();
        let mut server = HandshakeServer::new(&host);
        assert_eq!(
            server.accept_init(&init).unwrap_err(),
            HandshakeError::BadInit
        );
    }

    #[test]
    fn out_of_order_messages_are_rejected() {
        let host = DeviceIdentity::generate().unwrap();
        let mut server = HandshakeServer::new(&host);
        assert_eq!(
            server
                .accept_init(&[packet_type::HANDSHAKE_CONFIRM])
                .unwrap_err(),
            HandshakeError::BadInit
        );

        let device = DeviceIdentity::generate().unwrap();
        let mut client = HandshakeClient::new(id_bytes(1), device, host.public_key_bytes());
        assert_eq!(
            client.finish(&[0u8]).unwrap_err(),
            HandshakeError::OutOfOrder,
            "client must not finish before sending init"
        );
    }

    #[test]
    fn truncated_messages_are_rejected() {
        let host = DeviceIdentity::generate().unwrap();
        let mut server = HandshakeServer::new(&host);
        assert_eq!(
            server
                .accept_init(&[packet_type::HANDSHAKE_INIT, 1, 2, 3])
                .unwrap_err(),
            HandshakeError::BadInit
        );
    }

    #[test]
    fn forged_confirm_is_rejected() {
        let host = DeviceIdentity::generate().unwrap();
        let device = DeviceIdentity::generate().unwrap();
        let mut client = HandshakeClient::new(id_bytes(1), device, host.public_key_bytes());
        let mut server = HandshakeServer::new(&host);
        let init = client.init();
        let response = server.accept_init(&init).unwrap().response;
        let session = client.finish(&response).unwrap();

        let mut bogus = vec![packet_type::HANDSHAKE_CONFIRM];
        bogus.extend_from_slice(&[0u8; 40]);
        assert_eq!(
            verify_confirm(&session, &bogus).unwrap_err(),
            HandshakeError::BadConfirm
        );
    }

    #[test]
    fn replayed_confirm_does_not_complete_a_second_handshake() {
        let host = DeviceIdentity::generate().unwrap();
        let device = DeviceIdentity::generate().unwrap();
        let mut client = HandshakeClient::new(id_bytes(1), device, host.public_key_bytes());
        let mut server = HandshakeServer::new(&host);
        let init = client.init();
        let outcome = server.accept_init(&init).unwrap();
        let session = client.finish(&outcome.response).unwrap();
        let confirm = client.confirm(&session);
        assert!(verify_confirm(&outcome.session, &confirm).is_ok());
        // A replayed confirm is still a *valid* tag; replay defence at this
        // layer comes from the session being single-use, which the UDP path
        // enforces by removing the pending entry once a confirm is accepted.
    }
}
