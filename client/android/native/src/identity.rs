//! The headset's long-term identity, kept in app-private storage.
//!
//! Generated on first launch; only the public half ever leaves the device (it
//! is what the host pairs). The private key file is `0600` inside the app's
//! sandboxed files directory.

use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

use emersia_transport::crypto::{device_id_hex, DeviceIdentity};

const KEY_FILE: &str = "device.key";
/// Public half, for pairing tooling (`run-as … cat files/device.pub`), so
/// nothing outside the app ever needs to read the private key.
const PUB_FILE: &str = "device.pub";

/// Load the identity from `dir`, creating and persisting one if absent.
///
/// A present-but-unreadable key is an error, never silently replaced: a new
/// key would be a new, unpaired device.
pub fn load_or_create(dir: &Path) -> Result<DeviceIdentity, String> {
    let identity = load_or_generate(dir)?;
    let public = dir.join(PUB_FILE);
    let text = describe(&identity);
    if std::fs::read_to_string(&public).ok().as_deref() != Some(text.as_str()) {
        std::fs::write(&public, &text).map_err(|e| format!("{}: {e}", public.display()))?;
    }
    Ok(identity)
}

fn load_or_generate(dir: &Path) -> Result<DeviceIdentity, String> {
    let path = dir.join(KEY_FILE);
    match std::fs::read_to_string(&path) {
        Ok(hex) => DeviceIdentity::from_private_hex(hex.trim())
            .map_err(|e| format!("{} is corrupt: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let identity =
                DeviceIdentity::generate().map_err(|e| format!("generating identity: {e}"))?;
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            let tmp = dir.join(format!("{KEY_FILE}.tmp"));
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)
                .map_err(|e| format!("{}: {e}", tmp.display()))?;
            file.write_all(identity.private_key_hex().as_bytes())
                .and_then(|()| file.sync_all())
                .map_err(|e| format!("{}: {e}", tmp.display()))?;
            std::fs::rename(&tmp, &path).map_err(|e| format!("{}: {e}", path.display()))?;
            log::info!(
                "generated device identity {}",
                device_id_hex(&identity.public_key_hex())
            );
            Ok(identity)
        }
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// `"<public key hex> <device id hex>"`, for display and the adb pairing script.
pub fn describe(identity: &DeviceIdentity) -> String {
    let public = identity.public_key_hex();
    let id = device_id_hex(&public);
    format!("{public} {id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn identity_is_created_once_and_reloaded() {
        let dir = tempfile::tempdir().unwrap();
        let first = load_or_create(dir.path()).unwrap();
        let second = load_or_create(dir.path()).unwrap();
        assert_eq!(first.public_key_hex(), second.public_key_hex());
        let mode = std::fs::metadata(dir.path().join(KEY_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let public = std::fs::read_to_string(dir.path().join(PUB_FILE)).unwrap();
        assert_eq!(public, describe(&first));
    }

    #[test]
    fn corrupt_key_is_an_error_not_a_new_identity() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(KEY_FILE), "not hex").unwrap();
        assert!(load_or_create(dir.path()).is_err());
    }

    #[test]
    fn describe_has_key_and_id() {
        let dir = tempfile::tempdir().unwrap();
        let id = load_or_create(dir.path()).unwrap();
        let text = describe(&id);
        let parts: Vec<_> = text.split(' ').collect();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].len(), 64);
        assert_eq!(parts[1].len(), 16);
    }
}
