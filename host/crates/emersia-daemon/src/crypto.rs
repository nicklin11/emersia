//! Device identity and authenticated encryption primitives (ADR 0006).
//!
//! **Scope note:** this module composes audited RustCrypto primitives. It does
//! not invent any construction of its own, and it has not been independently
//! security-audited. See `docs/adr/0006-encrypted-channel.md` for the decision
//! record and what is still deliberately deferred.
//!
//! Construction:
//! - Device identity: X25519 static keypair; the public half is stored in the
//!   pairing database, the private half never leaves the device.
//! - Session keys: HKDF-SHA256 over the X25519 shared secret, bound to a
//!   protocol/session context string and a random salt, yielding separate
//!   send/receive keys so traffic cannot be reflected back at its sender.
//! - Record protection: ChaCha20-Poly1305 AEAD. Direction and sequence live in
//!   the authenticated-but-unencrypted AAD, so a replayed or reflected record
//!   fails authentication.

// `seal`/`open` and the key schedule are the M1.4 transport interface. They
// land with the identity work so both halves of ADR 0006 are reviewed in one
// place, before any bytes hit the wire. Until M1.4 calls them from production
// code, the dead-code lint would flag the whole sealing half as unused.
#![allow(dead_code)]

use std::fmt;
use std::path::Path;

use aead::{Aead, KeyInit};
use chacha20poly1305::ChaCha20Poly1305;
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey, StaticSecret};

/// Bytes in a pairing code (6 digits ≈ 20 bits — see `generate_pairing_code`).
const PAIRING_CODE_DIGITS: usize = 6;

/// Salt length for the session key schedule.
const SALT_LEN: usize = 16;

/// AAD length for a record: 1 direction byte + 8 sequence bytes.
pub const AAD_LEN: usize = 9;

/// An X25519 device identity. The secret is zeroized on drop by the crate.
pub struct DeviceIdentity {
    secret: StaticSecret,
}

/// Error type for the crypto layer.
#[derive(Debug)]
pub enum CryptoError {
    /// The OS random source failed.
    Random(&'static str),
    /// A hex field in stored data was malformed.
    Hex,
    /// A public key was not a valid curve point.
    BadKey,
    /// Key derivation output was the wrong length.
    KeyLength,
    /// AEAD open failed: wrong key, tampered ciphertext, or wrong AAD.
    Aead,
}

impl fmt::Display for CryptoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Random(what) => write!(f, "random source failed for {what}"),
            Self::Hex => write!(f, "malformed hex-encoded key material"),
            Self::BadKey => write!(f, "invalid X25519 public key"),
            Self::KeyLength => write!(f, "unexpected derived key length"),
            Self::Aead => write!(f, "authentication failed (wrong key or tampered data)"),
        }
    }
}

impl std::error::Error for CryptoError {}

fn random_bytes<const N: usize>(what: &'static str) -> Result<[u8; N], CryptoError> {
    let mut out = [0u8; N];
    getrandom::fill(&mut out).map_err(|_| CryptoError::Random(what))?;
    Ok(out)
}

impl DeviceIdentity {
    /// Generate a fresh device identity from the OS CSPRNG.
    pub fn generate() -> Result<Self, CryptoError> {
        let bytes: [u8; 32] = random_bytes("device identity")?;
        Ok(Self {
            secret: StaticSecret::from(bytes),
        })
    }

    /// Rebuild an identity from stored private-key bytes.
    pub fn from_private_bytes(bytes: [u8; 32]) -> Self {
        Self {
            secret: StaticSecret::from(bytes),
        }
    }

    /// Export the private scalar as hex.
    ///
    /// This is a **secret**. It exists for provisioning a device (the private
    /// half has to reach the headset somehow) and is only ever printed by
    /// `emersia-daemon keygen`. Nothing on the host should call it in a loop.
    pub fn private_key_hex(&self) -> String {
        hex::encode(self.secret.to_bytes())
    }

    /// Rebuild an identity from a hex private key.
    pub fn from_private_hex(hex_str: &str) -> Result<Self, CryptoError> {
        let bytes = hex::decode(hex_str).map_err(|_| CryptoError::Hex)?;
        let arr: [u8; 32] = bytes.try_into().map_err(|_| CryptoError::Hex)?;
        Ok(Self::from_private_bytes(arr))
    }

    /// The public half, hex-encoded, for storage in the pairing database.
    pub fn public_key_hex(&self) -> String {
        hex::encode(self.public_key_bytes())
    }

    pub fn public_key_bytes(&self) -> [u8; 32] {
        public_from_secret(&self.secret).to_bytes()
    }

    /// Private scalar bytes, for tests that need two identities to share the
    /// same long-term key. Not reachable from production code paths.
    #[cfg(test)]
    pub fn private_bytes_for_test(&self) -> [u8; 32] {
        self.secret.to_bytes()
    }

    /// Agree on a shared secret with a peer's public key.
    ///
    /// Returns `None` for an all-zero result, which X25519 defines as a failed
    /// (small-order) exchange; treating it as a secret would be a real weakness.
    pub fn agree(&self, their_public: &[u8; 32]) -> Option<[u8; 32]> {
        let peer = PublicKey::from(*their_public);
        let shared = self.secret.diffie_hellman(&peer).to_bytes();
        if shared.iter().all(|b| *b == 0) {
            return None;
        }
        Some(shared)
    }
}

/// The daemon's own long-term identity, persisted next to the pairing
/// database.
///
/// The host needs a static key of its own before it can complete any
/// handshake (ADR 0006); without it a paired device has nobody to talk to.
pub struct HostIdentity {
    inner: DeviceIdentity,
}

impl HostIdentity {
    /// Load the host key from `path`, generating and persisting one on first
    /// run. The private key never leaves this process except into that file.
    pub fn load_or_create(path: &Path) -> Result<Self, CryptoError> {
        let inner = match std::fs::read_to_string(path) {
            Ok(text) => {
                let bytes = hex::decode(text.trim()).map_err(|_| CryptoError::Hex)?;
                let arr: [u8; 32] = bytes.try_into().map_err(|_| CryptoError::Hex)?;
                DeviceIdentity::from_private_bytes(arr)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let fresh = DeviceIdentity::generate()?;
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir).map_err(|_| CryptoError::Hex)?;
                    restrict(dir, 0o700)?;
                }
                // Write 0600 *before* the secret lands in the file.
                write_private(path, hex::encode(fresh.secret.to_bytes()).as_bytes())?;
                fresh
            }
            Err(_) => return Err(CryptoError::Hex),
        };
        Ok(Self { inner })
    }

    /// Hex-encoded host public key, safe to show to a user.
    pub fn public_key_hex(&self) -> String {
        self.inner.public_key_hex()
    }

    /// Consume the wrapper and take the identity.
    ///
    /// The streaming endpoint must use the *same* long-term key that devices
    /// pin; a second key would make every device reject the handshake.
    pub fn into_identity(self) -> DeviceIdentity {
        self.inner
    }
}

/// Set owner-only permissions on a path.
fn restrict(path: &Path, mode: u32) -> Result<(), CryptoError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|_| CryptoError::Hex)
}

/// Write a file that only the owner can read.
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), CryptoError> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|_| CryptoError::Hex)?;
    file.write_all(bytes).map_err(|_| CryptoError::Hex)?;
    file.sync_all().map_err(|_| CryptoError::Hex)
}

/// Compute the public key for a secret scalar.
fn public_from_secret(secret: &StaticSecret) -> PublicKey {
    // `PublicKey::from(&StaticSecret)` is the supported conversion.
    PublicKey::from(secret)
}

/// Direction of a protected record. Part of the AEAD associated data, so a
/// record cannot be reflected back to its sender.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    HostToDevice,
    DeviceToHost,
}

impl Direction {
    fn byte(self) -> u8 {
        match self {
            Self::HostToDevice => 0,
            Self::DeviceToHost => 1,
        }
    }

    fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::HostToDevice),
            1 => Some(Self::DeviceToHost),
            _ => None,
        }
    }
}

/// Symmetric session keys for one direction of a channel.
#[derive(Clone)]
pub struct SessionKeys {
    key: [u8; 32],
}

impl SessionKeys {
    fn from_bytes(bytes: [u8; 32]) -> Self {
        Self { key: bytes }
    }
}

/// Both directions of a session, derived once from a shared secret.
///
/// Deliberately has no `Debug` that could print key material.
#[derive(Clone)]
pub struct Session {
    host_to_device: SessionKeys,
    device_to_host: SessionKeys,
    /// Salt used for this session, echoed by the responder.
    pub salt: [u8; SALT_LEN],
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("keys", &"<redacted>")
            .finish()
    }
}

impl Session {
    /// Derive session keys from an X25519 shared secret.
    ///
    /// `context` binds the keys to this protocol/protocol version so a secret
    /// can never be replayed into a different construction.
    pub fn derive(shared: &[u8; 32], context: &[u8]) -> Result<Self, CryptoError> {
        let salt: [u8; SALT_LEN] = random_bytes("session salt")?;
        let mut h2d = [0u8; 32];
        let mut d2h = [0u8; 32];
        expand(shared, &salt, context, b"emersia/stream-key-v1", &mut h2d)?;
        expand(shared, &salt, context, b"emersia/input-key-v1", &mut d2h)?;
        Ok(Self {
            host_to_device: SessionKeys::from_bytes(h2d),
            device_to_host: SessionKeys::from_bytes(d2h),
            salt,
        })
    }

    /// Derive a session from arbitrary key material (the handshake path).
    ///
    /// `ikm` should already contain every Diffie-Hellman output the design
    /// calls for; including the ephemeral pair is what buys forward secrecy.
    pub fn from_key_material(ikm: &[u8], salt: &[u8], context: &[u8]) -> Result<Self, CryptoError> {
        let mut h2d = [0u8; 32];
        let mut d2h = [0u8; 32];
        expand(ikm, salt, context, b"emersia/stream-key-v1", &mut h2d)?;
        expand(ikm, salt, context, b"emersia/input-key-v1", &mut d2h)?;
        Ok(Self {
            host_to_device: SessionKeys::from_bytes(h2d),
            device_to_host: SessionKeys::from_bytes(d2h),
            salt: [0u8; SALT_LEN],
        })
    }

    /// Rebuild a session from a peer-provided salt (responder side).
    pub fn derive_with_salt(
        shared: &[u8; 32],
        salt: [u8; SALT_LEN],
        context: &[u8],
    ) -> Result<Self, CryptoError> {
        let mut h2d = [0u8; 32];
        let mut d2h = [0u8; 32];
        expand(shared, &salt, context, b"emersia/stream-key-v1", &mut h2d)?;
        expand(shared, &salt, context, b"emersia/input-key-v1", &mut d2h)?;
        Ok(Self {
            host_to_device: SessionKeys::from_bytes(h2d),
            device_to_host: SessionKeys::from_bytes(d2h),
            salt,
        })
    }

    fn keys_for(&self, dir: Direction) -> &SessionKeys {
        match dir {
            Direction::HostToDevice => &self.host_to_device,
            Direction::DeviceToHost => &self.device_to_host,
        }
    }
}

/// Hash of a running handshake transcript, used as the HKDF salt so each
/// message is bound to every message before it.
pub fn transcript(parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for part in parts {
        // Length-prefix each part so concatenation is unambiguous.
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    hasher.finalize().into()
}

/// HKDF-SHA256 extract-and-expand with a label as the `info` parameter.
fn expand(
    ikm: &[u8],
    salt: &[u8],
    context: &[u8],
    label: &[u8],
    out: &mut [u8; 32],
) -> Result<(), CryptoError> {
    // Bind the protocol context and the role label into one info string.
    let mut info = Vec::with_capacity(context.len() + label.len() + 1);
    info.extend_from_slice(context);
    info.push(0);
    info.extend_from_slice(label);

    let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(salt), ikm);
    hk.expand(&info, out).map_err(|_| CryptoError::KeyLength)
}

/// Build the authenticated-but-unencrypted header for a record.
pub fn build_aad(direction: Direction, sequence: u64) -> [u8; AAD_LEN] {
    let mut aad = [0u8; AAD_LEN];
    aad[0] = direction.byte();
    aad[1..].copy_from_slice(&sequence.to_be_bytes());
    aad
}

/// Parse a record header, rejecting unknown directions.
pub fn parse_aad(aad: &[u8]) -> Result<(Direction, u64), CryptoError> {
    if aad.len() != AAD_LEN {
        return Err(CryptoError::Aead);
    }
    let dir = Direction::from_byte(aad[0]).ok_or(CryptoError::Aead)?;
    let mut seq = [0u8; 8];
    seq.copy_from_slice(&aad[1..]);
    Ok((dir, u64::from_be_bytes(seq)))
}

/// Bytes of nonce prefixed to every sealed record.
const NONCE_LEN: usize = 12;

/// Encrypt one record. Returns `nonce || ciphertext || tag`.
///
/// The nonce is transmitted in the clear because it is not secret; it must be
/// carried alongside the ciphertext or the record cannot be opened.
pub fn seal(
    session: &Session,
    direction: Direction,
    sequence: u64,
    plaintext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let cipher = ChaCha20Poly1305::new_from_slice(&session.keys_for(direction).key)
        .map_err(|_| CryptoError::KeyLength)?;
    let nonce_bytes: [u8; NONCE_LEN] = random_bytes("record nonce")?;
    let nonce =
        chacha20poly1305::Nonce::try_from(&nonce_bytes[..]).map_err(|_| CryptoError::KeyLength)?;
    let aad = build_aad(direction, sequence);
    let ciphertext = cipher
        .encrypt(
            &nonce,
            aead::Payload {
                msg: plaintext,
                aad: &aad,
            },
        )
        .map_err(|_| CryptoError::Aead)?;

    let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Decrypt and authenticate one record produced by [`seal`].
pub fn open(
    session: &Session,
    direction: Direction,
    sequence: u64,
    sealed: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    if sealed.len() < NONCE_LEN {
        return Err(CryptoError::Aead);
    }
    let (nonce_bytes, ciphertext) = sealed.split_at(NONCE_LEN);
    let cipher = ChaCha20Poly1305::new_from_slice(&session.keys_for(direction).key)
        .map_err(|_| CryptoError::KeyLength)?;
    let nonce =
        chacha20poly1305::Nonce::try_from(nonce_bytes).map_err(|_| CryptoError::KeyLength)?;
    let aad = build_aad(direction, sequence);
    cipher
        .decrypt(
            &nonce,
            aead::Payload {
                msg: ciphertext,
                aad: &aad,
            },
        )
        .map_err(|_| CryptoError::Aead)
}

/// A human-transcribable pairing code.
///
/// Six digits is deliberately *not* treated as the security boundary: it is a
/// consent/confirmation signal. The actual trust anchor is the device's X25519
/// public key, which must be presented when the code is redeemed. Rate limiting
/// lives in the pairing store, not here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingCode(String);

impl PairingCode {
    pub fn generate() -> Result<Self, CryptoError> {
        let bytes: [u8; PAIRING_CODE_DIGITS] = random_bytes("pairing code")?;
        // Map bytes onto 0..=9 without modulo bias.
        let mut code = String::with_capacity(PAIRING_CODE_DIGITS);
        for b in bytes {
            // 252 = 9*28 is the largest multiple of 10 that fits a byte.
            let digit = if b >= 252 { b % 10 } else { (b / 25) % 10 };
            code.push((b'0' + digit) as char);
        }
        Ok(Self(code))
    }

    /// Parse and normalise user input (accepts spaces/dashes, case-insensitive).
    pub fn parse(input: &str) -> Result<Self, CryptoError> {
        let cleaned: String = input
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .map(|c| c.to_ascii_uppercase())
            .collect();
        if cleaned.len() != PAIRING_CODE_DIGITS || !cleaned.chars().all(|c| c.is_ascii_digit()) {
            return Err(CryptoError::Hex);
        }
        Ok(Self(cleaned))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PairingCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Group as XXX-XXX for readability on a headset or terminal.
        let (a, b) = self.0.split_at(PAIRING_CODE_DIGITS / 2);
        write!(f, "{a}-{b}")
    }
}

/// Decode a hex-encoded public key.
pub fn parse_public_key_hex(hex_str: &str) -> Result<[u8; 32], CryptoError> {
    let bytes = hex::decode(hex_str).map_err(|_| CryptoError::Hex)?;
    let arr: [u8; 32] = bytes.try_into().map_err(|_| CryptoError::Hex)?;
    Ok(arr)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONTEXT: &[u8] = b"emersia/v1";

    fn pair() -> (DeviceIdentity, DeviceIdentity) {
        (
            DeviceIdentity::generate().unwrap(),
            DeviceIdentity::generate().unwrap(),
        )
    }

    #[test]
    fn agreement_is_symmetric() {
        let (host, device) = pair();
        let host_pub = host.public_key_bytes();
        let device_pub = device.public_key_bytes();
        let a = host.agree(&device_pub).unwrap();
        let b = device.agree(&host_pub).unwrap();
        assert_eq!(a, b, "both sides derive the same secret");
    }

    #[test]
    fn distinct_identities_derive_distinct_secrets() {
        let (a1, _) = pair();
        let (_, b1) = pair();
        let (_, b2) = pair();
        let a1_pub = a1.public_key_bytes();
        assert_ne!(
            b1.agree(&a1_pub).unwrap(),
            b2.agree(&a1_pub).unwrap(),
            "different device keys must not agree to the same secret"
        );
    }

    #[test]
    fn all_zero_public_key_is_rejected() {
        let (host, _) = pair();
        assert!(
            host.agree(&[0u8; 32]).is_none(),
            "small-order point must not yield a usable secret"
        );
    }

    #[test]
    fn seal_open_roundtrip() {
        let (host, device) = pair();
        let shared = host.agree(&device.public_key_bytes()).unwrap();
        let session = Session::derive(&shared, CONTEXT).unwrap();
        let msg = b"the quick brown fox jumps over the lazy dog";
        let ct = seal(&session, Direction::HostToDevice, 7, msg).unwrap();
        let pt = open(&session, Direction::HostToDevice, 7, &ct).unwrap();
        assert_eq!(pt, msg);
    }

    #[test]
    fn ciphertext_is_not_plaintext() {
        let (host, device) = pair();
        let session =
            Session::derive(&host.agree(&device.public_key_bytes()).unwrap(), CONTEXT).unwrap();
        let ct = seal(&session, Direction::HostToDevice, 1, b"secret").unwrap();
        assert!(!ct.windows(6).any(|w| w == b"secret"));
    }

    #[test]
    fn wrong_direction_fails_authentication() {
        let (host, device) = pair();
        let session =
            Session::derive(&host.agree(&device.public_key_bytes()).unwrap(), CONTEXT).unwrap();
        let ct = seal(&session, Direction::HostToDevice, 1, b"payload").unwrap();
        // Reflected back at the sender: must not open in the other direction.
        assert!(open(&session, Direction::DeviceToHost, 1, &ct).is_err());
    }

    #[test]
    fn wrong_sequence_fails_authentication() {
        let (host, device) = pair();
        let session =
            Session::derive(&host.agree(&device.public_key_bytes()).unwrap(), CONTEXT).unwrap();
        let ct = seal(&session, Direction::HostToDevice, 1, b"payload").unwrap();
        assert!(open(&session, Direction::HostToDevice, 2, &ct).is_err());
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let (host, device) = pair();
        let session =
            Session::derive(&host.agree(&device.public_key_bytes()).unwrap(), CONTEXT).unwrap();
        let mut ct = seal(&session, Direction::HostToDevice, 1, b"payload").unwrap();
        let last = ct.len() - 1;
        ct[last] ^= 0x01;
        assert!(open(&session, Direction::HostToDevice, 1, &ct).is_err());
    }

    #[test]
    fn tampered_plaintext_is_rejected() {
        let (host, device) = pair();
        let session =
            Session::derive(&host.agree(&device.public_key_bytes()).unwrap(), CONTEXT).unwrap();
        let mut ct = seal(&session, Direction::HostToDevice, 1, b"payload").unwrap();
        ct[0] ^= 0xff;
        assert!(open(&session, Direction::HostToDevice, 1, &ct).is_err());
    }

    #[test]
    fn wrong_device_key_cannot_open() {
        let (host, device) = pair();
        let (_, attacker) = pair();
        let victim =
            Session::derive(&host.agree(&device.public_key_bytes()).unwrap(), CONTEXT).unwrap();
        let attacker_session =
            Session::derive(&attacker.agree(&host.public_key_bytes()).unwrap(), CONTEXT).unwrap();
        let ct = seal(&victim, Direction::HostToDevice, 1, b"payload").unwrap();
        assert!(open(&attacker_session, Direction::HostToDevice, 1, &ct).is_err());
    }

    #[test]
    fn separate_direction_keys_are_independent() {
        let (host, device) = pair();
        let session =
            Session::derive(&host.agree(&device.public_key_bytes()).unwrap(), CONTEXT).unwrap();
        assert_ne!(
            session.keys_for(Direction::HostToDevice).key,
            session.keys_for(Direction::DeviceToHost).key,
            "directions must not share a key"
        );
    }

    #[test]
    fn context_binds_the_key_schedule() {
        let (host, device) = pair();
        let shared = host.agree(&device.public_key_bytes()).unwrap();
        let a = Session::derive(&shared, b"emersia/v1").unwrap();
        let b = Session::derive(&shared, b"emersia/v2").unwrap();
        let ct = seal(&a, Direction::HostToDevice, 1, b"payload").unwrap();
        assert!(
            open(&b, Direction::HostToDevice, 1, &ct).is_err(),
            "keys must not transfer across protocol versions"
        );
    }

    #[test]
    fn responder_derives_the_same_keys() {
        let (host, device) = pair();
        let host_pub = host.public_key_bytes();
        let device_pub = device.public_key_bytes();
        let a = host.agree(&device_pub).unwrap();
        let b = device.agree(&host_pub).unwrap();
        // Host picks the salt and sends it; device rebuilds with it.
        let host_session = Session::derive(&a, CONTEXT).unwrap();
        let device_session = Session::derive_with_salt(&b, host_session.salt, CONTEXT).unwrap();
        let ct = seal(&host_session, Direction::HostToDevice, 42, b"hello headset").unwrap();
        assert_eq!(
            open(&device_session, Direction::HostToDevice, 42, &ct).unwrap(),
            b"hello headset"
        );
    }

    #[test]
    fn nonces_differ_between_records() {
        let (host, device) = pair();
        let session =
            Session::derive(&host.agree(&device.public_key_bytes()).unwrap(), CONTEXT).unwrap();
        let a = seal(&session, Direction::HostToDevice, 1, b"same").unwrap();
        let b = seal(&session, Direction::HostToDevice, 1, b"same").unwrap();
        assert_ne!(a, b, "fresh nonce per record is required");
    }

    #[test]
    fn pairing_codes_are_six_digits() {
        let code = PairingCode::generate().unwrap();
        assert_eq!(code.as_str().len(), 6);
        assert!(code.as_str().chars().all(|c| c.is_ascii_digit()));
        assert_eq!(code.to_string().len(), 7, "formatted as XXX-XXX");
    }

    #[test]
    fn pairing_code_input_is_normalised() {
        assert_eq!(
            PairingCode::parse("123-456").unwrap().as_str(),
            "123456",
            "user typing the displayed form"
        );
        assert_eq!(PairingCode::parse(" 123 456 ").unwrap().as_str(), "123456");
    }

    #[test]
    fn pairing_code_rejects_bad_input() {
        for bad in ["", "12345", "1234567", "abcdef", "12 34 56x", "!!!!!!"] {
            assert!(
                PairingCode::parse(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn public_key_hex_roundtrips() {
        let id = DeviceIdentity::generate().unwrap();
        let hex_str = id.public_key_hex();
        let bytes = parse_public_key_hex(&hex_str).unwrap();
        assert_eq!(bytes, id.public_key_bytes());
    }

    #[test]
    fn malformed_hex_is_rejected() {
        assert!(parse_public_key_hex("not-hex").is_err());
        assert!(parse_public_key_hex("abcd").is_err(), "wrong length");
    }

    #[test]
    fn aad_roundtrips_and_rejects_bad_headers() {
        let aad = build_aad(Direction::DeviceToHost, 99);
        assert_eq!(parse_aad(&aad).unwrap(), (Direction::DeviceToHost, 99));
        assert!(parse_aad(&[0u8; 3]).is_err());
        let mut bad = build_aad(Direction::HostToDevice, 1);
        bad[0] = 7;
        assert!(parse_aad(&bad).is_err(), "unknown direction rejected");
    }
}
