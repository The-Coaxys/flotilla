//! Layout of the daemon's state directory (`$FLOTILLA_HOME`, default `$HOME/.flotilla`).
use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::ServerError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlotillaHome {
    root: PathBuf,
}

impl FlotillaHome {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// `$FLOTILLA_HOME`, else `$HOME/.flotilla`.
    pub fn from_env() -> Result<Self, ServerError> {
        if let Some(v) = std::env::var_os("FLOTILLA_HOME").filter(|v| !v.is_empty()) {
            return Ok(Self::new(v));
        }
        let home = std::env::var_os("HOME")
            .filter(|v| !v.is_empty())
            .ok_or(ServerError::NoHome)?;
        Ok(Self::new(PathBuf::from(home).join(".flotilla")))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn bin_dir(&self) -> PathBuf {
        self.root.join("bin")
    }
    pub fn run_dir(&self) -> PathBuf {
        self.root.join("run")
    }
    pub fn state_dir(&self) -> PathBuf {
        self.root.join("state")
    }
    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }
    pub fn socket_path(&self) -> PathBuf {
        self.run_dir().join("daemon.sock")
    }
    pub fn runtime_info_path(&self) -> PathBuf {
        self.run_dir().join("daemon.json")
    }
    pub fn cert_path(&self) -> PathBuf {
        self.state_dir().join("quic-cert.der")
    }
    pub fn key_path(&self) -> PathBuf {
        self.state_dir().join("quic-key.der")
    }
    pub fn devices_path(&self) -> PathBuf {
        self.state_dir().join("devices.json")
    }

    /// Create all directories with mode 0700.
    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        for dir in [
            self.root.clone(),
            self.bin_dir(),
            self.run_dir(),
            self.state_dir(),
            self.logs_dir(),
        ] {
            std::fs::create_dir_all(&dir)?;
            std::fs::set_permissions(&dir, Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_matches_spec() {
        let h = FlotillaHome::new("/home/u/.flotilla");
        assert_eq!(
            h.socket_path(),
            PathBuf::from("/home/u/.flotilla/run/daemon.sock")
        );
        assert_eq!(
            h.runtime_info_path(),
            PathBuf::from("/home/u/.flotilla/run/daemon.json")
        );
        assert_eq!(
            h.cert_path(),
            PathBuf::from("/home/u/.flotilla/state/quic-cert.der")
        );
        assert_eq!(
            h.key_path(),
            PathBuf::from("/home/u/.flotilla/state/quic-key.der")
        );
        assert_eq!(
            h.devices_path(),
            PathBuf::from("/home/u/.flotilla/state/devices.json")
        );
        assert_eq!(h.bin_dir(), PathBuf::from("/home/u/.flotilla/bin"));
        assert_eq!(h.logs_dir(), PathBuf::from("/home/u/.flotilla/logs"));
    }

    #[test]
    fn ensure_dirs_creates_private_directories() {
        let dir = tempfile::tempdir().unwrap();
        let h = FlotillaHome::new(dir.path().join(".flotilla"));
        h.ensure_dirs().unwrap();
        for d in [
            h.root().to_path_buf(),
            h.bin_dir(),
            h.run_dir(),
            h.state_dir(),
            h.logs_dir(),
        ] {
            let mode = std::fs::metadata(&d).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{}", d.display());
        }
    }
}
