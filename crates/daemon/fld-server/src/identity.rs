//! The daemon's QUIC certificate: generated once, then reused so pinned
//! fingerprints stay valid across restarts and upgrades.
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use fl_transport::quic::ServerIdentity;

use crate::{FlotillaHome, ServerError};

/// Requires `home.ensure_dirs()` to have run.
pub fn load_or_create(home: &FlotillaHome) -> Result<ServerIdentity, ServerError> {
    let (cert_path, key_path) = (home.cert_path(), home.key_path());
    if cert_path.exists() && key_path.exists() {
        let cert_der = std::fs::read(&cert_path)?;
        let key_der = std::fs::read(&key_path)?;
        if cert_der.is_empty() || key_der.is_empty() {
            return Err(ServerError::Corrupt {
                path: cert_path,
                reason: "empty certificate or key file".into(),
            });
        }
        return Ok(ServerIdentity { cert_der, key_der });
    }
    let identity = ServerIdentity::generate()?;
    write_private(&key_path, &identity.key_der)?;
    write_private(&cert_path, &identity.cert_der)?;
    Ok(identity)
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn home() -> (tempfile::TempDir, FlotillaHome) {
        let dir = tempfile::tempdir().unwrap();
        let h = FlotillaHome::new(dir.path().join(".flotilla"));
        h.ensure_dirs().unwrap();
        (dir, h)
    }

    #[test]
    fn creates_once_then_reuses() {
        let (_d, h) = home();
        let a = load_or_create(&h).unwrap();
        let b = load_or_create(&h).unwrap();
        assert_eq!(a.fingerprint(), b.fingerprint());
    }

    #[test]
    fn key_and_cert_are_private() {
        let (_d, h) = home();
        load_or_create(&h).unwrap();
        for p in [h.key_path(), h.cert_path()] {
            assert_eq!(
                std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn empty_files_are_corrupt() {
        let (_d, h) = home();
        std::fs::write(h.cert_path(), b"").unwrap();
        std::fs::write(h.key_path(), b"").unwrap();
        assert!(matches!(
            load_or_create(&h),
            Err(ServerError::Corrupt { .. })
        ));
    }
}
