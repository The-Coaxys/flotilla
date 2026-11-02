//! Device tokens. Windows: Credential Manager (service "Flotilla",
//! account "device-token:<host id>"). Elsewhere (dev builds, tests): one
//! private file per host.
use std::path::PathBuf;

use anyhow::{Result, ensure};

pub const SERVICE: &str = "Flotilla";

pub fn account(host_id: &str) -> String {
    format!("device-token:{host_id}")
}

pub trait SecretStore: Send + Sync {
    fn get(&self, host_id: &str) -> Result<Option<String>>;
    fn set(&self, host_id: &str, token: &str) -> Result<()>;
    fn delete(&self, host_id: &str) -> Result<()>;
}

#[cfg(windows)]
pub struct CredentialStore;

#[cfg(windows)]
impl SecretStore for CredentialStore {
    fn get(&self, host_id: &str) -> Result<Option<String>> {
        match keyring::Entry::new(SERVICE, &account(host_id))?.get_password() {
            Ok(token) => Ok(Some(token)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn set(&self, host_id: &str, token: &str) -> Result<()> {
        keyring::Entry::new(SERVICE, &account(host_id))?.set_password(token)?;
        Ok(())
    }

    fn delete(&self, host_id: &str) -> Result<()> {
        match keyring::Entry::new(SERVICE, &account(host_id))?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// One `<host id>.token` file per host, mode 0600 on Unix.
pub struct FileStore {
    dir: PathBuf,
}

impl FileStore {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn path(&self, host_id: &str) -> Result<PathBuf> {
        ensure!(
            !host_id.is_empty()
                && host_id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-'),
            "invalid host id {host_id:?}"
        );
        Ok(self.dir.join(format!("{host_id}.token")))
    }
}

impl SecretStore for FileStore {
    fn get(&self, host_id: &str) -> Result<Option<String>> {
        match std::fs::read_to_string(self.path(host_id)?) {
            Ok(token) => Ok(Some(token)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn set(&self, host_id: &str, token: &str) -> Result<()> {
        let path = self.path(host_id)?;
        std::fs::create_dir_all(&self.dir)?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        use std::io::Write;
        options.open(path)?.write_all(token.as_bytes())?;
        Ok(())
    }

    fn delete(&self, host_id: &str) -> Result<()> {
        match std::fs::remove_file(self.path(host_id)?) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

pub fn default_store() -> Box<dyn SecretStore> {
    #[cfg(windows)]
    {
        Box::new(CredentialStore)
    }
    #[cfg(not(windows))]
    {
        let dir = dirs::config_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("Flotilla")
            .join("secrets");
        Box::new(FileStore::new(dir))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_name_format() {
        assert_eq!(account("abc-1"), "device-token:abc-1");
    }

    #[test]
    fn file_store_roundtrip_and_delete() {
        let dir = tempfile::tempdir().unwrap();
        let s = FileStore::new(dir.path().join("secrets"));
        assert_eq!(s.get("host-1").unwrap(), None);
        s.set("host-1", "tok").unwrap();
        assert_eq!(s.get("host-1").unwrap().as_deref(), Some("tok"));
        s.set("host-1", "tok2").unwrap();
        assert_eq!(s.get("host-1").unwrap().as_deref(), Some("tok2"));
        s.delete("host-1").unwrap();
        s.delete("host-1").unwrap();
        assert_eq!(s.get("host-1").unwrap(), None);
    }

    #[test]
    fn file_store_rejects_path_tricks() {
        let dir = tempfile::tempdir().unwrap();
        let s = FileStore::new(dir.path().to_path_buf());
        assert!(s.set("../evil", "x").is_err());
        assert!(s.get("").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn file_store_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let s = FileStore::new(dir.path().to_path_buf());
        s.set("h", "tok").unwrap();
        let mode = std::fs::metadata(dir.path().join("h.token"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}
