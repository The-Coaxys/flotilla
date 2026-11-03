//! Saved hosts in `<config dir>/Flotilla/hosts.json` (`%APPDATA%` on Windows).
//! A corrupt file is reported, never silently overwritten.
use std::path::PathBuf;

use anyhow::{Context, Result};
use flc_conn::profile::HostProfile;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
struct HostsFile {
    version: u32,
    hosts: Vec<HostProfile>,
}

#[derive(Debug, Clone)]
pub struct HostStore {
    path: PathBuf,
}

impl HostStore {
    pub fn default_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("Flotilla").join("hosts.json"))
    }

    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    #[cfg(test)]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub fn load(&self) -> Result<Vec<HostProfile>> {
        match std::fs::read(&self.path) {
            Ok(bytes) => {
                let file: HostsFile = serde_json::from_slice(&bytes).with_context(|| {
                    format!("{} is corrupt; fix or delete it", self.path.display())
                })?;
                Ok(file.hosts)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e).with_context(|| format!("cannot read {}", self.path.display())),
        }
    }

    pub fn save(&self, hosts: &[HostProfile]) -> Result<()> {
        let dir = self
            .path
            .parent()
            .context("hosts path has no parent directory")?;
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        let tmp = self.path.with_extension("json.tmp");
        let body = serde_json::to_vec_pretty(&HostsFile {
            version: 1,
            hosts: hosts.to_vec(),
        })?;
        std::fs::write(&tmp, body).with_context(|| format!("cannot write {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("cannot replace {}", self.path.display()))?;
        Ok(())
    }

    /// Insert or replace (by `id`), then save. Returns the new list.
    pub fn upsert(&self, host: HostProfile) -> Result<Vec<HostProfile>> {
        let mut hosts = self.load()?;
        match hosts.iter_mut().find(|h| h.id == host.id) {
            Some(existing) => *existing = host,
            None => hosts.push(host),
        }
        self.save(&hosts)?;
        Ok(hosts)
    }

    pub fn remove(&self, id: &str) -> Result<Vec<HostProfile>> {
        let mut hosts = self.load()?;
        hosts.retain(|h| h.id != id);
        self.save(&hosts)?;
        Ok(hosts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_transport::ssh::{SshAuth, SshTarget};

    fn host(id: &str, name: &str) -> HostProfile {
        HostProfile {
            id: id.into(),
            name: name.into(),
            ssh: SshTarget {
                host: "203.0.113.7".into(),
                port: 22,
                user: "root".into(),
            },
            auth: SshAuth::Agent { socket: None },
            host_key: Some("SHA256:abc".into()),
            quic: None,
            device_id: Some("dev".into()),
            socket_path: Some("/root/.flotilla/run/daemon.sock".into()),
            clipboard_consent: None,
        }
    }

    fn store() -> (tempfile::TempDir, HostStore) {
        let dir = tempfile::tempdir().unwrap();
        let s = HostStore::new(dir.path().join("Flotilla").join("hosts.json"));
        (dir, s)
    }

    #[test]
    fn missing_file_means_no_hosts() {
        let (_d, s) = store();
        assert!(s.load().unwrap().is_empty());
    }

    #[test]
    fn upsert_inserts_then_replaces() {
        let (_d, s) = store();
        s.upsert(host("1", "a")).unwrap();
        s.upsert(host("2", "b")).unwrap();
        let hosts = s.upsert(host("1", "renamed")).unwrap();
        assert_eq!(hosts.len(), 2);
        assert_eq!(s.load().unwrap()[0].name, "renamed");
    }

    #[test]
    fn remove_deletes_by_id() {
        let (_d, s) = store();
        s.upsert(host("1", "a")).unwrap();
        s.upsert(host("2", "b")).unwrap();
        let hosts = s.remove("1").unwrap();
        assert_eq!(
            hosts.iter().map(|h| h.id.as_str()).collect::<Vec<_>>(),
            vec!["2"]
        );
    }

    #[test]
    fn corrupt_file_is_an_error_and_left_untouched() {
        let (_d, s) = store();
        std::fs::create_dir_all(s.path().parent().unwrap()).unwrap();
        std::fs::write(s.path(), b"{broken").unwrap();
        let err = s.load().unwrap_err();
        assert!(format!("{err:#}").contains("corrupt"));
        assert!(s.upsert(host("1", "a")).is_err());
        assert_eq!(std::fs::read(s.path()).unwrap(), b"{broken");
    }
}
