//! Long-lived Ed25519 identity, loaded from disk or generated on first run.

use std::fs;
use std::io;
use std::path::Path;

use ed25519_dalek::SigningKey;
use rand_core::OsRng;

use crate::packet::{fingerprint, fingerprint_str, FPR_LEN, PUBKEY_LEN};

const SEED_LEN: usize = 32;
const KEY_FILENAME: &str = "identity.key";

#[derive(Debug)]
pub enum IdentityError {
    Io(io::Error),
    BadLength(usize),
}

impl From<io::Error> for IdentityError {
    fn from(e: io::Error) -> Self { Self::Io(e) }
}

pub struct Identity {
    pub signing: SigningKey,
    pub pubkey: [u8; PUBKEY_LEN],
    pub fingerprint: [u8; FPR_LEN],
}

impl Identity {
    #[must_use]
    pub fn fingerprint_str(&self) -> String { fingerprint_str(&self.fingerprint) }
}

/// Load the identity from `state_dir/identity.key`, or generate and persist a new one.
///
/// # Errors
///
/// Returns [`IdentityError::Io`] on any filesystem error, or
/// [`IdentityError::BadLength`] if an existing key file has the wrong byte count.
pub fn load_or_generate(state_dir: &Path) -> Result<Identity, IdentityError> {
    fs::create_dir_all(state_dir)?;
    let path = state_dir.join(KEY_FILENAME);
    let seed = if path.exists() {
        let bytes = fs::read(&path)?;
        if bytes.len() != SEED_LEN {
            return Err(IdentityError::BadLength(bytes.len()));
        }
        let mut s = [0_u8; SEED_LEN];
        s.copy_from_slice(&bytes);
        s
    } else {
        let signing = SigningKey::generate(&mut OsRng);
        let s = signing.to_bytes();
        write_secret_file(&path, &s)?;
        s
    };
    let signing = SigningKey::from_bytes(&seed);
    let pubkey = signing.verifying_key().to_bytes();
    Ok(Identity {
        signing,
        pubkey,
        fingerprint: fingerprint(&pubkey),
    })
}

#[cfg(unix)]
fn write_secret_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    use std::io::Write;
    f.write_all(bytes)?;
    Ok(())
}

#[cfg(not(unix))]
fn write_secret_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    // On Windows, %LOCALAPPDATA% inherits a per-user ACL from the profile
    // directory. We don't apply additional ACLs here — the file is
    // unreadable to other users by default.
    fs::write(path, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn first_call_generates_file() {
        let dir = tempdir().unwrap();
        let id = load_or_generate(dir.path()).unwrap();
        let key_path = dir.path().join(KEY_FILENAME);
        assert!(key_path.exists());
        assert_eq!(fs::metadata(&key_path).unwrap().len(), SEED_LEN as u64);
        assert_eq!(id.pubkey.len(), PUBKEY_LEN);
    }

    #[test]
    fn second_call_returns_same_key() {
        let dir = tempdir().unwrap();
        let a = load_or_generate(dir.path()).unwrap();
        let b = load_or_generate(dir.path()).unwrap();
        assert_eq!(a.pubkey, b.pubkey);
        assert_eq!(a.fingerprint, b.fingerprint);
    }

    #[cfg(unix)]
    #[test]
    fn file_is_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        load_or_generate(dir.path()).unwrap();
        let perms = fs::metadata(dir.path().join(KEY_FILENAME)).unwrap().permissions();
        assert_eq!(perms.mode() & 0o777, 0o600);
    }

    #[test]
    fn corrupt_file_rejected() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join(KEY_FILENAME), b"too short").unwrap();
        assert!(matches!(load_or_generate(dir.path()), Err(IdentityError::BadLength(_))));
    }
}
