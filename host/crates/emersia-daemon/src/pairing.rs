//! Pairing database and device lifecycle (ADR 0005).
//!
//! Trust anchor: a paired device's X25519 public key. The short pairing code is
//! a *consent* signal, not the secret — it is stored only as a salted hash,
//! expires, is single-use, and is rate-limited.
//!
//! Storage is a single JSON file under `$XDG_CONFIG_HOME/emersia/` (directory
//! `0700`, file `0600`). An unreadable or corrupt database is a hard error:
//! silently continuing with an empty trust store would mean "trust everything",
//! which is the exact failure mode ADR 0005 forbids.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::crypto::{parse_public_key_hex, PairingCode};

/// Environment variable holding the per-user config directory.
pub const CONFIG_DIR_ENV: &str = "XDG_CONFIG_HOME";

/// Subdirectory and file name of the pairing database.
pub const CONFIG_SUBDIR: &str = "emersia";
pub const PAIRING_FILE: &str = "pairing.json";

/// Schema version so a future format change can migrate rather than discard.
const SCHEMA_VERSION: u32 = 1;

/// How long a generated pairing code stays valid.
pub const CODE_TTL: Duration = Duration::from_secs(300);

/// Failed attempts before a pending code is burned.
const MAX_CODE_ATTEMPTS: u32 = 5;

/// Longest device display name we will store.
const MAX_NAME_LEN: usize = 64;

/// Errors from the pairing layer.
#[derive(Debug, PartialEq, Eq)]
pub enum PairingError {
    /// `XDG_CONFIG_HOME` is not set.
    NoConfigDir,
    /// Filesystem failure, with context.
    Io(String),
    /// The database exists but cannot be parsed — we refuse to continue.
    Corrupt(String),
    /// No code is pending, or it expired / was burned.
    NoPendingCode,
    /// The supplied code did not match the pending one.
    WrongCode,
    /// Too many failed attempts against the pending code.
    TooManyAttempts,
    /// A device id was not found, or is already revoked.
    NotFound(String),
    /// Input failed validation.
    Invalid(String),
}

impl std::fmt::Display for PairingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoConfigDir => write!(f, "{CONFIG_DIR_ENV} is not set"),
            Self::Io(m) => write!(f, "pairing database I/O failed: {m}"),
            Self::Corrupt(m) => write!(
                f,
                "pairing database is unreadable ({m}); refusing to start with an unknown trust store"
            ),
            Self::NoPendingCode => write!(f, "no pairing code is pending (expired or already used)"),
            Self::WrongCode => write!(f, "incorrect pairing code"),
            Self::TooManyAttempts => {
                write!(f, "too many incorrect code attempts; start a new pairing")
            }
            Self::NotFound(id) => write!(f, "no active device with id {id}"),
            Self::Invalid(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for PairingError {}

/// One paired (or revoked) device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    /// Hex-encoded X25519 public key — the trust anchor.
    pub public_key: String,
    /// Unix seconds.
    pub paired_at: u64,
    #[serde(default)]
    pub revoked: bool,
    /// Unix seconds when revoked, if applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<u64>,
}

/// A pairing code awaiting redemption.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingCode {
    /// Salted hash of the code; the code itself is never persisted.
    code_hash: String,
    salt: String,
    expires_at: u64,
    attempts: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DbFile {
    version: u32,
    #[serde(default)]
    devices: BTreeMap<String, Device>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending: Option<PendingCode>,
}

impl Default for DbFile {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            devices: BTreeMap::new(),
            pending: None,
        }
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Hash a pairing code with a random salt. Storing the hash (not the code)
/// means a leaked database does not hand over live pairing codes.
fn hash_code(code: &str, salt_hex: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(salt_hex.as_bytes());
    hasher.update(b"|");
    hasher.update(code.as_bytes());
    hex::encode(hasher.finalize())
}

/// Default database location: `$XDG_CONFIG_HOME/emersia/pairing.json`.
pub fn default_db_path() -> Result<PathBuf, PairingError> {
    let base = std::env::var_os(CONFIG_DIR_ENV).ok_or(PairingError::NoConfigDir)?;
    Ok(Path::new(&base).join(CONFIG_SUBDIR).join(PAIRING_FILE))
}

/// The pairing store.
#[derive(Debug)]
pub struct PairingDb {
    path: PathBuf,
    db: DbFile,
}

impl PairingDb {
    /// Load the database, creating an empty one if absent.
    ///
    /// Fails closed on a corrupt file rather than resetting trust.
    pub fn load(path: &Path) -> Result<Self, PairingError> {
        let db = match std::fs::read(path) {
            Ok(bytes) => {
                let parsed: DbFile = serde_json::from_slice(&bytes)
                    .map_err(|e| PairingError::Corrupt(e.to_string()))?;
                if parsed.version != SCHEMA_VERSION {
                    return Err(PairingError::Corrupt(format!(
                        "unsupported schema version {} (expected {SCHEMA_VERSION})",
                        parsed.version
                    )));
                }
                parsed
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => DbFile::default(),
            Err(e) => return Err(PairingError::Io(e.to_string())),
        };
        Ok(Self {
            path: path.to_path_buf(),
            db,
        })
    }

    /// Persist atomically: write a temp file, fsync, then rename over the
    /// target, so a crash mid-write cannot truncate the trust store.
    fn save(&self) -> Result<(), PairingError> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| PairingError::Io(e.to_string()))?;
            let mode = std::fs::Permissions::from_mode(0o700);
            std::fs::set_permissions(dir, mode).map_err(|e| PairingError::Io(e.to_string()))?;
        }

        let json =
            serde_json::to_vec_pretty(&self.db).map_err(|e| PairingError::Io(e.to_string()))?;
        let tmp = self.path.with_extension("json.tmp");
        {
            let mut file =
                std::fs::File::create(&tmp).map_err(|e| PairingError::Io(e.to_string()))?;
            file.write_all(&json)
                .map_err(|e| PairingError::Io(e.to_string()))?;
            file.sync_all()
                .map_err(|e| PairingError::Io(e.to_string()))?;
        }
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| PairingError::Io(e.to_string()))?;
        std::fs::rename(&tmp, &self.path).map_err(|e| PairingError::Io(e.to_string()))?;
        Ok(())
    }

    /// Generate a new pairing code, replacing any pending one.
    ///
    /// Returns the code for display; only its salted hash is stored.
    pub fn new_code(&mut self) -> Result<PairingCode, PairingError> {
        let code = PairingCode::generate().map_err(|e| PairingError::Invalid(e.to_string()))?;
        let mut salt = [0u8; 16];
        getrandom::fill(&mut salt).map_err(|_| PairingError::Invalid("no entropy".into()))?;
        let salt_hex = hex::encode(salt);
        self.db.pending = Some(PendingCode {
            code_hash: hash_code(code.as_str(), &salt_hex),
            salt: salt_hex,
            expires_at: now_unix() + CODE_TTL.as_secs(),
            attempts: 0,
        });
        self.save()?;
        Ok(code)
    }

    /// Redeem a pairing code and register a device identity.
    pub fn accept_code(
        &mut self,
        code_input: &str,
        device_name: &str,
        public_key_hex: &str,
    ) -> Result<Device, PairingError> {
        let code = PairingCode::parse(code_input)
            .map_err(|e| PairingError::Invalid(format!("pairing code: {e}")))?;
        // Validate the key before touching state.
        parse_public_key_hex(public_key_hex)
            .map_err(|e| PairingError::Invalid(format!("public key: {e}")))?;
        let name = device_name.trim();
        if name.is_empty() {
            return Err(PairingError::Invalid(
                "device name must not be empty".into(),
            ));
        }
        if name.chars().count() > MAX_NAME_LEN {
            return Err(PairingError::Invalid(format!(
                "device name must be at most {MAX_NAME_LEN} characters"
            )));
        }

        let now = now_unix();
        let pending = self.db.pending.clone().ok_or(PairingError::NoPendingCode)?;
        if now > pending.expires_at {
            self.db.pending = None;
            self.save()?;
            return Err(PairingError::NoPendingCode);
        }
        if pending.attempts >= MAX_CODE_ATTEMPTS {
            self.db.pending = None;
            self.save()?;
            return Err(PairingError::TooManyAttempts);
        }

        if hash_code(code.as_str(), &pending.salt) != pending.code_hash {
            // Count the failure and persist before returning, so a crash cannot
            // reset the attempt counter and reopen brute-force.
            if let Some(p) = self.db.pending.as_mut() {
                p.attempts += 1;
                let burned = p.attempts >= MAX_CODE_ATTEMPTS;
                if burned {
                    self.db.pending = None;
                }
            }
            self.save()?;
            return if self.db.pending.is_none() {
                Err(PairingError::TooManyAttempts)
            } else {
                Err(PairingError::WrongCode)
            };
        }

        let id = derive_device_id(public_key_hex);
        let device = Device {
            id: id.clone(),
            name: name.to_string(),
            public_key: public_key_hex.to_string(),
            paired_at: now,
            revoked: false,
            revoked_at: None,
        };
        self.db.devices.insert(id, device.clone());
        // Single-use: the code is consumed on success.
        self.db.pending = None;
        self.save()?;
        Ok(device)
    }

    /// All devices, paired and revoked.
    pub fn devices(&self) -> Vec<Device> {
        self.db.devices.values().cloned().collect()
    }

    /// Devices that are paired and not revoked.
    pub fn active_devices(&self) -> Vec<Device> {
        self.db
            .devices
            .values()
            .filter(|d| !d.revoked)
            .cloned()
            .collect()
    }

    /// Revoke a device by id. Returns the revoked record.
    pub fn revoke(&mut self, id: &str) -> Result<Device, PairingError> {
        let now = now_unix();
        let device = self
            .db
            .devices
            .get_mut(id)
            .ok_or_else(|| PairingError::NotFound(id.to_string()))?;
        if device.revoked {
            return Err(PairingError::NotFound(id.to_string()));
        }
        device.revoked = true;
        device.revoked_at = Some(now);
        let revoked = device.clone();
        self.save()?;
        Ok(revoked)
    }

    #[cfg(test)]
    /// Look up one active (non-revoked) device.
    pub fn active_device(&self, id: &str) -> Option<&Device> {
        self.db.devices.get(id).filter(|d| !d.revoked)
    }

    #[cfg(test)]
    /// Test seam: is a code currently pending and unexpired?
    pub fn has_pending_code(&self) -> bool {
        self.db
            .pending
            .as_ref()
            .is_some_and(|p| now_unix() <= p.expires_at)
    }

    #[cfg(test)]
    /// Test seam: expire the pending code.
    pub fn expire_pending(&mut self) {
        if let Some(p) = self.db.pending.as_mut() {
            p.expires_at = 0;
        }
    }
}

/// Stable device id derived from the public key: re-pairing the same identity
/// keeps the same id, so revocation cannot be dodged by re-pairing.
fn derive_device_id(public_key_hex: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"emersia/device-id/v1");
    hasher.update(public_key_hex.as_bytes());
    hex::encode(&hasher.finalize()[..8])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::DeviceIdentity;

    fn db(dir: &tempfile::TempDir) -> PairingDb {
        PairingDb::load(&dir.path().join("pairing.json")).unwrap()
    }

    fn new_identity() -> (DeviceIdentity, String) {
        let id = DeviceIdentity::generate().unwrap();
        let hex = id.public_key_hex();
        (id, hex)
    }

    #[test]
    fn missing_database_loads_empty() {
        let dir = tempfile::tempdir().unwrap();
        let d = db(&dir);
        assert!(d.devices().is_empty());
        assert!(!d.has_pending_code());
    }

    #[test]
    fn pairing_flow_registers_a_device() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = db(&dir);
        let code = d.new_code().unwrap();
        let (_, pubkey) = new_identity();
        let device = d.accept_code(code.as_str(), "Quest 3", &pubkey).unwrap();
        assert_eq!(device.name, "Quest 3");
        assert_eq!(device.public_key, pubkey);
        assert!(!device.revoked);
        assert_eq!(d.active_devices().len(), 1);
    }

    #[test]
    fn device_id_is_derived_from_the_public_key() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = db(&dir);
        let code = d.new_code().unwrap();
        let (_, pubkey) = new_identity();
        let a = d.accept_code(code.as_str(), "Quest", &pubkey).unwrap();
        // Re-pairing the same identity yields the same id.
        let code2 = d.new_code().unwrap();
        let b = d.accept_code(code2.as_str(), "Quest", &pubkey).unwrap();
        assert_eq!(a.id, b.id, "identity, not name, defines the device");
    }

    #[test]
    fn code_is_single_use() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = db(&dir);
        let code = d.new_code().unwrap();
        let (_, k1) = new_identity();
        let (_, k2) = new_identity();
        d.accept_code(code.as_str(), "first", &k1).unwrap();
        assert_eq!(
            d.accept_code(code.as_str(), "second", &k2).unwrap_err(),
            PairingError::NoPendingCode,
            "a redeemed code must not work twice"
        );
    }

    #[test]
    fn new_code_replaces_the_previous_one() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = db(&dir);
        let first = d.new_code().unwrap();
        let second = d.new_code().unwrap();
        let (_, pubkey) = new_identity();
        assert_eq!(
            d.accept_code(first.as_str(), "x", &pubkey).unwrap_err(),
            PairingError::WrongCode,
            "the superseded code must stop working"
        );
        assert!(d.accept_code(second.as_str(), "x", &pubkey).is_ok());
    }

    #[test]
    fn wrong_code_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = db(&dir);
        let _ = d.new_code().unwrap();
        let (_, pubkey) = new_identity();
        assert_eq!(
            d.accept_code("000000", "x", &pubkey).unwrap_err(),
            PairingError::WrongCode
        );
    }

    #[test]
    fn attempts_are_rate_limited() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = db(&dir);
        let code = d.new_code().unwrap();
        let (_, pubkey) = new_identity();
        // MAX_CODE_ATTEMPTS wrong guesses, then the code is burned.
        for i in 0..MAX_CODE_ATTEMPTS {
            let err = d.accept_code("000000", "x", &pubkey).unwrap_err();
            if i < MAX_CODE_ATTEMPTS - 1 {
                assert_eq!(err, PairingError::WrongCode);
            } else {
                assert_eq!(err, PairingError::TooManyAttempts);
            }
        }
        // Even the correct code is now dead: a burn must not be bypassable.
        assert_eq!(
            d.accept_code(code.as_str(), "x", &pubkey).unwrap_err(),
            PairingError::NoPendingCode
        );
    }

    #[test]
    fn attempt_counter_survives_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairing.json");
        let code;
        {
            let mut d = PairingDb::load(&path).unwrap();
            code = d.new_code().unwrap();
            let (_, pubkey) = new_identity();
            let _ = d.accept_code("000000", "x", &pubkey).unwrap_err();
        }
        let mut reloaded = PairingDb::load(&path).unwrap();
        let (_, pubkey) = new_identity();
        // Reloading must not reset the brute-force counter.
        for _ in 0..(MAX_CODE_ATTEMPTS - 1) {
            let _ = reloaded.accept_code("000000", "x", &pubkey);
        }
        assert_eq!(
            reloaded
                .accept_code(code.as_str(), "x", &pubkey)
                .unwrap_err(),
            PairingError::NoPendingCode
        );
    }

    #[test]
    fn expired_code_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = db(&dir);
        let code = d.new_code().unwrap();
        d.expire_pending();
        let (_, pubkey) = new_identity();
        assert_eq!(
            d.accept_code(code.as_str(), "x", &pubkey).unwrap_err(),
            PairingError::NoPendingCode
        );
    }

    #[test]
    fn revoke_removes_the_device_from_the_active_set() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = db(&dir);
        let code = d.new_code().unwrap();
        let (_, pubkey) = new_identity();
        let device = d.accept_code(code.as_str(), "Quest", &pubkey).unwrap();
        assert_eq!(d.active_devices().len(), 1);

        let revoked = d.revoke(&device.id).unwrap();
        assert!(revoked.revoked);
        assert!(revoked.revoked_at.is_some());
        assert!(
            d.active_devices().is_empty(),
            "revoked device is not active"
        );
        assert!(d.active_device(&device.id).is_none());
        // Still visible in the full list for audit.
        assert_eq!(d.devices().len(), 1);
    }

    #[test]
    fn revoking_twice_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = db(&dir);
        let code = d.new_code().unwrap();
        let (_, pubkey) = new_identity();
        let device = d.accept_code(code.as_str(), "Quest", &pubkey).unwrap();
        d.revoke(&device.id).unwrap();
        assert!(d.revoke(&device.id).is_err());
    }

    #[test]
    fn revoking_an_unknown_id_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = db(&dir);
        assert!(matches!(
            d.revoke("deadbeef").unwrap_err(),
            PairingError::NotFound(_)
        ));
    }

    #[test]
    fn state_survives_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairing.json");
        let id = {
            let mut d = PairingDb::load(&path).unwrap();
            let code = d.new_code().unwrap();
            let (_, pubkey) = new_identity();
            d.accept_code(code.as_str(), "Quest 3", &pubkey).unwrap().id
        };
        let reloaded = PairingDb::load(&path).unwrap();
        assert_eq!(reloaded.active_devices().len(), 1);
        assert_eq!(reloaded.active_devices()[0].id, id);
        assert_eq!(reloaded.active_devices()[0].name, "Quest 3");
    }

    #[test]
    fn database_file_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairing.json");
        let mut d = PairingDb::load(&path).unwrap();
        let _ = d.new_code().unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "trust store must be owner-only");
    }

    #[test]
    fn no_temp_file_is_left_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairing.json");
        let mut d = PairingDb::load(&path).unwrap();
        let _ = d.new_code().unwrap();
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn code_is_never_stored_in_plaintext() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairing.json");
        let mut d = PairingDb::load(&path).unwrap();
        let code = d.new_code().unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            !raw.contains(code.as_str()),
            "the live pairing code must not be on disk"
        );
    }

    #[test]
    fn corrupt_database_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairing.json");
        std::fs::write(&path, b"{ this is not json").unwrap();
        // Must NOT silently become an empty (trust-everything) store.
        assert!(matches!(
            PairingDb::load(&path).unwrap_err(),
            PairingError::Corrupt(_)
        ));
    }

    #[test]
    fn unknown_schema_version_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairing.json");
        std::fs::write(&path, br#"{"version":99,"devices":{}}"#).unwrap();
        assert!(matches!(
            PairingDb::load(&path).unwrap_err(),
            PairingError::Corrupt(_)
        ));
    }

    #[test]
    fn malformed_public_key_is_rejected_before_pairing() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = db(&dir);
        let code = d.new_code().unwrap();
        assert!(matches!(
            d.accept_code(code.as_str(), "x", "not-hex").unwrap_err(),
            PairingError::Invalid(_)
        ));
    }

    #[test]
    fn empty_and_long_names_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = db(&dir);
        let (_, pubkey) = new_identity();
        let code = d.new_code().unwrap();
        assert!(d.accept_code(code.as_str(), "   ", &pubkey).is_err());
        let long = "x".repeat(MAX_NAME_LEN + 1);
        let code2 = d.new_code().unwrap();
        assert!(d.accept_code(code2.as_str(), &long, &pubkey).is_err());
    }

    #[test]
    fn malformed_code_input_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = db(&dir);
        let _ = d.new_code().unwrap();
        let (_, pubkey) = new_identity();
        assert!(matches!(
            d.accept_code("12", "x", &pubkey).unwrap_err(),
            PairingError::Invalid(_)
        ));
    }
}
