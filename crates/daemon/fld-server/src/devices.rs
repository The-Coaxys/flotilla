//! Enrolled client devices. Tokens are random 32-byte secrets shown once;
//! only their SHA-256 is stored. Writes are atomic (temp file + rename), and
//! read-modify-write cycles hold an exclusive lock on `devices.lock`, so
//! concurrent `flotillad enroll` processes and the running daemon never
//! lose or half-read an entry.
use std::fs::File;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ServerError;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRecord {
    pub id: String,
    pub name: String,
    pub token_sha256: String,
    pub created_unix: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct DevicesFile {
    version: u32,
    devices: Vec<DeviceRecord>,
}

pub struct DeviceRegistry {
    path: PathBuf,
}

impl DeviceRegistry {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn lock(&self, exclusive: bool) -> Result<File, ServerError> {
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(self.path.with_extension("lock"))?;
        if exclusive {
            f.lock()?;
        } else {
            f.lock_shared()?;
        }
        Ok(f)
    }

    fn load(&self) -> Result<DevicesFile, ServerError> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| ServerError::Corrupt {
                path: self.path.clone(),
                reason: e.to_string(),
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DevicesFile {
                version: 1,
                devices: Vec::new(),
            }),
            Err(e) => Err(e.into()),
        }
    }

    fn store(&self, file: &DevicesFile) -> Result<(), ServerError> {
        crate::write_json_atomic(&self.path, file)?;
        Ok(())
    }

    /// Register a device; returns `(device_id, token)`. The token is not stored.
    pub fn enroll(&self, name: &str) -> Result<(String, String), ServerError> {
        let _lock = self.lock(true)?;
        let mut file = self.load()?;
        let id = uuid::Uuid::new_v4().to_string();
        let mut raw = [0u8; 32];
        getrandom::fill(&mut raw).map_err(|e| std::io::Error::other(e.to_string()))?;
        let token = hex::encode(raw);
        let created_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        file.version = 1;
        file.devices.push(DeviceRecord {
            id: id.clone(),
            name: name.to_string(),
            token_sha256: token_hash(&token),
            created_unix,
        });
        self.store(&file)?;
        Ok((id, token))
    }

    pub fn verify(&self, device_id: &str, token: &str) -> Result<bool, ServerError> {
        let _lock = self.lock(false)?;
        let file = self.load()?;
        let want = token_hash(token);
        Ok(file.devices.iter().any(|d| {
            d.id == device_id && constant_time_eq(d.token_sha256.as_bytes(), want.as_bytes())
        }))
    }

    pub fn list(&self) -> Result<Vec<DeviceRecord>, ServerError> {
        let _lock = self.lock(false)?;
        Ok(self.load()?.devices)
    }

    pub fn revoke(&self, device_id: &str) -> Result<bool, ServerError> {
        let _lock = self.lock(true)?;
        let mut file = self.load()?;
        let before = file.devices.len();
        file.devices.retain(|d| d.id != device_id);
        let removed = file.devices.len() != before;
        if removed {
            self.store(&file)?;
        }
        Ok(removed)
    }
}

fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn registry() -> (tempfile::TempDir, DeviceRegistry) {
        let dir = tempfile::tempdir().unwrap();
        let reg = DeviceRegistry::new(dir.path().join("devices.json"));
        (dir, reg)
    }

    #[test]
    fn enroll_then_verify() {
        let (_d, reg) = registry();
        let (id, token) = reg.enroll("laptop").unwrap();
        assert_eq!(token.len(), 64);
        assert!(reg.verify(&id, &token).unwrap());
        let list = reg.list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "laptop");
    }

    #[test]
    fn wrong_token_or_device_is_refused() {
        let (_d, reg) = registry();
        let (id, token) = reg.enroll("laptop").unwrap();
        assert!(!reg.verify(&id, &"0".repeat(64)).unwrap());
        assert!(!reg.verify("someone-else", &token).unwrap());
        assert!(!reg.verify(&id, "").unwrap());
    }

    #[test]
    fn verify_on_missing_file_is_false() {
        let (_d, reg) = registry();
        assert!(!reg.verify("x", "y").unwrap());
    }

    #[test]
    fn tokens_are_stored_only_as_hashes() {
        let (dir, reg) = registry();
        let (_id, token) = reg.enroll("laptop").unwrap();
        let raw = std::fs::read_to_string(dir.path().join("devices.json")).unwrap();
        assert!(!raw.contains(&token));
        assert!(raw.contains(&token_hash(&token)));
    }

    #[test]
    fn devices_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, reg) = registry();
        reg.enroll("laptop").unwrap();
        let mode = std::fs::metadata(dir.path().join("devices.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn revoke_removes_device() {
        let (_d, reg) = registry();
        let (id, token) = reg.enroll("laptop").unwrap();
        assert!(reg.revoke(&id).unwrap());
        assert!(!reg.revoke(&id).unwrap());
        assert!(!reg.verify(&id, &token).unwrap());
    }

    #[test]
    fn corrupt_file_is_reported_not_overwritten() {
        let (dir, reg) = registry();
        std::fs::write(dir.path().join("devices.json"), b"{not json").unwrap();
        assert!(matches!(
            reg.verify("a", "b"),
            Err(ServerError::Corrupt { .. })
        ));
        assert!(matches!(reg.enroll("x"), Err(ServerError::Corrupt { .. })));
        assert_eq!(
            std::fs::read(dir.path().join("devices.json")).unwrap(),
            b"{not json"
        );
    }

    /// Review Focus #4.
    #[test]
    fn concurrent_enrollments_are_all_kept_and_readers_never_see_partial_state() {
        let (_d, reg) = registry();
        let reg = Arc::new(reg);
        let stop = Arc::new(AtomicBool::new(false));
        let reader = {
            let (reg, stop) = (reg.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut reads = 0u32;
                while !stop.load(Ordering::SeqCst) {
                    reg.list()
                        .expect("a reader must never see a partial devices.json");
                    reads += 1;
                }
                reads
            })
        };
        let writers: Vec<_> = (0..20)
            .map(|i| {
                let reg = reg.clone();
                std::thread::spawn(move || reg.enroll(&format!("pc-{i}")).unwrap())
            })
            .collect();
        let creds: Vec<_> = writers.into_iter().map(|h| h.join().unwrap()).collect();
        stop.store(true, Ordering::SeqCst);
        assert!(reader.join().unwrap() > 0);
        assert_eq!(reg.list().unwrap().len(), 20);
        for (id, token) in &creds {
            assert!(reg.verify(id, token).unwrap());
        }
    }
}
