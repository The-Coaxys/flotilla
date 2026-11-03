//! `flotilla --self-test=<report.json>`: headless checks CI runs on the release binary.
use std::path::Path;

use serde::Serialize;

use crate::secrets::SecretStore;

#[derive(Debug, Serialize)]
pub struct Report {
    pub version: String,
    pub build: String,
    pub daemon_x86_64_bytes: usize,
    pub daemon_aarch64_bytes: usize,
    pub daemon_x86_64_elf: bool,
    pub daemon_aarch64_elf: bool,
    pub hosts_path: Option<String>,
    pub secrets_roundtrip: bool,
    pub tokio_runtime: bool,
    pub errors: Vec<String>,
    pub ok: bool,
}

pub fn report() -> Report {
    let mut errors = Vec::new();
    let x86 = crate::embedded::FLOTILLAD_X86_64;
    let arm = crate::embedded::FLOTILLAD_AARCH64;
    let (x86_elf, arm_elf) = (crate::embedded::is_elf(x86), crate::embedded::is_elf(arm));
    if !x86_elf {
        errors.push("embedded x86_64 daemon is missing or not an ELF binary".to_string());
    }
    if !arm_elf {
        errors.push("embedded aarch64 daemon is missing or not an ELF binary".to_string());
    }
    let hosts_path = crate::hosts::HostStore::default_path().map(|p| p.display().to_string());
    if hosts_path.is_none() {
        errors.push("no per-user configuration directory".to_string());
    }
    let secrets_roundtrip = match secrets_roundtrip(crate::secrets::default_store().as_ref()) {
        Ok(()) => true,
        Err(e) => {
            errors.push(format!("secret store: {e:#}"));
            false
        }
    };
    let tokio_runtime = {
        let rt = crate::runtime::start();
        rt.block_on(async {
            tokio::spawn(async { 2 + 2 })
                .await
                .map(|v| v == 4)
                .unwrap_or(false)
        })
    };
    if !tokio_runtime {
        errors.push("Tokio runtime did not run a task".to_string());
    }
    let ok = errors.is_empty();
    Report {
        version: fl_proto::PRODUCT_VERSION.to_string(),
        build: fl_proto::build_id().to_string(),
        daemon_x86_64_bytes: x86.len(),
        daemon_aarch64_bytes: arm.len(),
        daemon_x86_64_elf: x86_elf,
        daemon_aarch64_elf: arm_elf,
        hosts_path,
        secrets_roundtrip,
        tokio_runtime,
        errors,
        ok,
    }
}

pub fn secrets_roundtrip(store: &dyn SecretStore) -> anyhow::Result<()> {
    let id = format!("selftest-{}", uuid::Uuid::new_v4());
    store.set(&id, "probe")?;
    let got = store.get(&id)?;
    store.delete(&id)?;
    anyhow::ensure!(got.as_deref() == Some("probe"), "read back {got:?}");
    anyhow::ensure!(
        store.get(&id)?.is_none(),
        "delete did not remove the secret"
    );
    Ok(())
}

/// Write the report; exit code 0 = ok, 1 = checks failed, 2 = cannot write report.
pub fn run(out: &Path) -> i32 {
    let report = report();
    let json = serde_json::to_string_pretty(&report).expect("report serializes");
    if let Err(e) = std::fs::write(out, &json) {
        eprintln!("flotilla: cannot write {}: {e}", out.display());
        return 2;
    }
    if report.ok { 0 } else { 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_roundtrip_with_file_store() {
        let dir = tempfile::tempdir().unwrap();
        secrets_roundtrip(&crate::secrets::FileStore::new(dir.path().to_path_buf())).unwrap();
    }

    #[test]
    fn report_flags_missing_embedded_daemons() {
        let r = report();
        assert!(r.tokio_runtime);
        if crate::embedded::FLOTILLAD_X86_64.is_empty() {
            assert!(!r.ok);
            assert!(r.errors.iter().any(|e| e.contains("x86_64")));
        }
    }

    #[test]
    fn run_writes_json_report() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("report.json");
        let code = run(&out);
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
        assert_eq!(v["ok"].as_bool().unwrap(), code == 0);
        assert_eq!(v["version"], fl_proto::PRODUCT_VERSION);
    }
}
