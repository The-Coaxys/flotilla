//! Staged replacement of the installed binary with rollback.
//!
//! Replacing a running executable cannot be atomic, so the update is: copy the
//! verified candidate next to the installed binary, rename the installed binary
//! to `flotilla.exe.previous`, rename the candidate into place, and only then
//! run the new build's self-check. A failure puts the previous build back.
//! The backup survives until the new build renders its first window frame.
//! An interrupted first launch rolls back on the following start.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Marker recording an in-flight replacement, next to the installed binary.
pub const PENDING_FILE: &str = "update-pending.json";
/// Suffix of the backup kept until the new build self-checks.
pub const BACKUP_SUFFIX: &str = ".previous";
/// Suffix of a candidate that failed its self-check, kept for diagnosis.
pub const FAILED_SUFFIX: &str = ".failed";
/// Suffix of the candidate while it is copied into the install directory.
pub const INCOMING_SUFFIX: &str = ".new";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub schema: u32,
    pub from_version: String,
    pub to_version: String,
    pub tag: String,
    /// File name of the executable inside the install directory.
    pub installed: String,
    /// File name of the backup kept until the new build renders.
    pub backup: String,
    /// A startup check passed, but no window frame has confirmed this build.
    #[serde(default)]
    pub launch_attempted: bool,
}

impl Pending {
    pub fn new(from: &str, to: &str, tag: &str, installed: &str) -> Self {
        Self {
            schema: 1,
            from_version: from.to_string(),
            to_version: to.to_string(),
            tag: tag.to_string(),
            installed: installed.to_string(),
            backup: backup_name(installed),
            launch_attempted: false,
        }
    }
}

/// A completed replacement: `installed` is the new build, `backup` the old one.
#[derive(Debug, Clone)]
pub struct Replacement {
    pub installed: PathBuf,
    pub backup: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("the new build failed its self-check; the previous build is back in place")]
    SelfCheck,
    #[error("staged update is not usable: {0}")]
    Staged(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// What happened when a pending update was examined at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recovery {
    /// No pending update.
    Nothing,
    /// The new build passed its check; backup remains until a window renders.
    AwaitingUi { version: String },
    /// The new build failed its self-check; the previous build was restored and started.
    RolledBack { version: String },
    /// A swap that never took effect (or a finished one) was cleaned up.
    Cleaned { version: String },
    /// The marker describes other versions than the running build; left alone.
    Unrelated { running: String, pending: String },
}

fn sibling(installed: &Path, suffix: &str) -> Result<PathBuf, InstallError> {
    let name = installed
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| InstallError::Staged(format!("{} has no file name", installed.display())))?;
    Ok(installed.with_file_name(format!("{name}{suffix}")))
}

pub fn backup_name(installed_file_name: &str) -> String {
    format!("{installed_file_name}{BACKUP_SUFFIX}")
}

/// Copy `candidate` into place next to `installed`, swap it in, then self-check
/// it. On a failed self-check the previous build is restored before returning.
pub fn replace(
    installed: &Path,
    candidate: &Path,
    self_check: impl Fn(&Path) -> bool,
) -> Result<Replacement, InstallError> {
    if !candidate.is_file() {
        return Err(InstallError::Staged(format!(
            "{} is missing",
            candidate.display()
        )));
    }
    if !installed.is_file() {
        return Err(InstallError::Staged(format!(
            "{} is not installed here",
            installed.display()
        )));
    }
    let incoming = sibling(installed, INCOMING_SUFFIX)?;
    let backup = sibling(installed, BACKUP_SUFFIX)?;
    let failed = sibling(installed, FAILED_SUFFIX)?;

    // Same directory as the installed binary, so both renames stay on one volume.
    let _ = std::fs::remove_file(&incoming);
    std::fs::copy(candidate, &incoming)?;
    if let Ok(file) = std::fs::OpenOptions::new().write(true).open(&incoming) {
        let _ = file.sync_all();
    }
    let _ = std::fs::remove_file(&backup);
    std::fs::rename(installed, &backup)?;
    if let Err(e) = std::fs::rename(&incoming, installed) {
        // Nothing was replaced: put the previous build back and report.
        let _ = std::fs::rename(&backup, installed);
        let _ = std::fs::remove_file(&incoming);
        return Err(e.into());
    }
    if self_check(installed) {
        return Ok(Replacement {
            installed: installed.to_path_buf(),
            backup,
        });
    }
    // Roll back the swap we just made.
    let _ = std::fs::remove_file(&failed);
    if std::fs::rename(installed, &failed).is_err() {
        // Cannot move it aside (still running, permissions): remove it so the
        // previous build can take the name back.
        std::fs::remove_file(installed)?;
    }
    std::fs::rename(&backup, installed)?;
    Err(InstallError::SelfCheck)
}

/// Delete the backup after the new build proved itself.
pub fn commit(replacement: &Replacement) -> Result<(), InstallError> {
    match std::fs::remove_file(&replacement.backup) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Put the backup back and remove the failed build.
pub fn rollback(replacement: &Replacement) -> Result<(), InstallError> {
    let failed = sibling(&replacement.installed, FAILED_SUFFIX)?;
    let _ = std::fs::remove_file(&failed);
    let _ = std::fs::rename(&replacement.installed, &failed);
    std::fs::rename(&replacement.backup, &replacement.installed)?;
    Ok(())
}

pub fn pending_path(dir: &Path) -> PathBuf {
    dir.join(PENDING_FILE)
}

/// Write the marker atomically: a half-written marker must never be read as a
/// mandate to replace something.
pub fn write_pending(dir: &Path, pending: &Pending) -> Result<(), InstallError> {
    let path = pending_path(dir);
    let tmp = path.with_extension("json.tmp");
    let bytes =
        serde_json::to_vec_pretty(pending).map_err(|e| InstallError::Staged(e.to_string()))?;
    std::fs::write(&tmp, &bytes)?;
    if let Ok(file) = std::fs::OpenOptions::new().write(true).open(&tmp) {
        let _ = file.sync_all();
    }
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Read the marker. A corrupt marker is reported, never silently deleted.
pub fn read_pending(dir: &Path) -> Result<Option<Pending>, InstallError> {
    let path = pending_path(dir);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let pending: Pending = serde_json::from_slice(&bytes).map_err(|e| {
        InstallError::Staged(format!(
            "{} is corrupt ({e}); fix or delete it",
            path.display()
        ))
    })?;
    Ok(Some(pending))
}

pub fn clear_pending(dir: &Path) -> Result<(), InstallError> {
    match std::fs::remove_file(pending_path(dir)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Startup recovery: confirm a healthy new build, or restore the previous one.
///
/// `self_check` runs the installed build (`--self-test`) and `start` launches a
/// path; both are injected so the whole flow is testable without processes.
pub fn recover(
    dir: &Path,
    running_version: &str,
    self_check: impl Fn(&Path) -> bool,
    start: impl Fn(&Path) -> bool,
) -> Result<Recovery, InstallError> {
    let Some(mut pending) = read_pending(dir)? else {
        return Ok(Recovery::Nothing);
    };
    let installed = dir.join(&pending.installed);
    let backup = dir.join(&pending.backup);

    if running_version == pending.to_version {
        if !pending.launch_attempted && installed.is_file() && self_check(&installed) {
            pending.launch_attempted = true;
            write_pending(dir, &pending)?;
            return Ok(Recovery::AwaitingUi {
                version: pending.to_version,
            });
        }
        if backup.is_file() {
            let failed =
                installed.with_file_name(format!("{}{}", pending.installed, FAILED_SUFFIX));
            let _ = std::fs::remove_file(&failed);
            let _ = std::fs::rename(&installed, &failed);
            std::fs::rename(&backup, &installed)?;
            clear_pending(dir)?;
            let _ = start(&installed);
            return Ok(Recovery::RolledBack {
                version: pending.from_version,
            });
        }
        clear_pending(dir)?;
        return Ok(Recovery::RolledBack {
            version: pending.from_version,
        });
    }

    if running_version == pending.from_version {
        // The replacement never took effect for this build: drop the leftovers.
        let _ = std::fs::remove_file(&backup);
        clear_pending(dir)?;
        return Ok(Recovery::Cleaned {
            version: pending.from_version,
        });
    }

    Ok(Recovery::Unrelated {
        running: running_version.to_string(),
        pending: pending.to_version,
    })
}

/// Called only after the main window's first frame. An unrelated executable
/// cannot confirm (and discard the backup for) a pending replacement.
pub fn confirm_ui_ready(dir: &Path, running_version: &str) -> Result<bool, InstallError> {
    let Some(pending) = read_pending(dir)? else {
        return Ok(false);
    };
    if pending.to_version != running_version || !pending.launch_attempted {
        return Ok(false);
    }
    commit(&Replacement {
        installed: dir.join(&pending.installed),
        backup: dir.join(&pending.backup),
    })?;
    clear_pending(dir)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let installed = dir.path().join("flotilla.exe");
        std::fs::write(&installed, b"old build").unwrap();
        (dir, installed)
    }

    fn candidate(dir: &Path) -> PathBuf {
        let path = dir.join("staged").join("flotilla-x64.exe");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"new build").unwrap();
        path
    }

    #[test]
    fn healthy_start_keeps_backup_until_the_first_window_frame() {
        let (dir, installed) = setup();
        let staged = candidate(dir.path());
        let replacement = replace(&installed, &staged, |_| true).unwrap();
        write_pending(
            dir.path(),
            &Pending::new("0.4.0", "0.5.0", "v0.5.0", "flotilla.exe"),
        )
        .unwrap();
        recover(dir.path(), "0.5.0", |_| true, |_| true).unwrap();
        assert_eq!(std::fs::read(&replacement.backup).unwrap(), b"old build");
        assert_eq!(std::fs::read(&installed).unwrap(), b"new build");
        assert!(!confirm_ui_ready(dir.path(), "0.4.0").unwrap());
        assert_eq!(std::fs::read(&replacement.backup).unwrap(), b"old build");
        assert!(confirm_ui_ready(dir.path(), "0.5.0").unwrap());
        assert!(!replacement.backup.exists());
        assert!(read_pending(dir.path()).unwrap().is_none());
    }

    #[test]
    fn interrupted_first_launch_rolls_back_even_if_the_self_check_passes() {
        let (dir, installed) = setup();
        let staged = candidate(dir.path());
        replace(&installed, &staged, |_| true).unwrap();
        write_pending(
            dir.path(),
            &Pending::new("0.4.0", "0.5.0", "v0.5.0", "flotilla.exe"),
        )
        .unwrap();
        recover(dir.path(), "0.5.0", |_| true, |_| true).unwrap();
        let restored = std::cell::Cell::new(false);
        let state = recover(
            dir.path(),
            "0.5.0",
            |_| true,
            |path| {
                restored.set(std::fs::read(path).unwrap() == b"old build");
                true
            },
        )
        .unwrap();
        assert_eq!(
            state,
            Recovery::RolledBack {
                version: "0.4.0".into()
            }
        );
        assert!(restored.get());
        assert_eq!(std::fs::read(&installed).unwrap(), b"old build");
        assert_eq!(
            std::fs::read(sibling(&installed, FAILED_SUFFIX).unwrap()).unwrap(),
            b"new build"
        );
    }

    #[test]
    fn a_healthy_replacement_keeps_the_backup_until_committed() {
        let (dir, installed) = setup();
        let staged = candidate(dir.path());
        let replacement = replace(&installed, &staged, |path| {
            std::fs::read(path).unwrap() == b"new build"
        })
        .expect("replacement");
        assert_eq!(std::fs::read(&installed).unwrap(), b"new build");
        assert_eq!(std::fs::read(&replacement.backup).unwrap(), b"old build");
        commit(&replacement).unwrap();
        assert!(!replacement.backup.exists());
        assert_eq!(std::fs::read(&installed).unwrap(), b"new build");
    }

    #[test]
    fn a_failing_self_check_rolls_back_the_staged_replacement() {
        let (dir, installed) = setup();
        let staged = candidate(dir.path());
        let err = replace(&installed, &staged, |_| false).unwrap_err();
        assert!(matches!(err, InstallError::SelfCheck));
        // The old binary is runnable again, byte for byte.
        assert_eq!(std::fs::read(&installed).unwrap(), b"old build");
        assert!(!sibling(&installed, BACKUP_SUFFIX).unwrap().exists());
        // The refused build is kept aside, not installed.
        assert_eq!(
            std::fs::read(sibling(&installed, FAILED_SUFFIX).unwrap()).unwrap(),
            b"new build"
        );
    }

    #[test]
    fn a_missing_candidate_is_refused_and_touches_nothing() {
        let (dir, installed) = setup();
        let missing = dir.path().join("staged").join("nope.exe");
        let err = replace(&installed, &missing, |_| true).unwrap_err();
        assert!(matches!(err, InstallError::Staged(_)));
        assert_eq!(std::fs::read(&installed).unwrap(), b"old build");
    }

    #[test]
    fn recover_rolls_back_when_the_new_build_fails_its_self_check() {
        let (dir, installed) = setup();
        let staged = candidate(dir.path());
        let replacement = replace(&installed, &staged, |_| true).unwrap();
        write_pending(
            dir.path(),
            &Pending::new("0.4.0", "0.5.0", "v0.5.0", "flotilla.exe"),
        )
        .unwrap();
        let started = std::cell::RefCell::new(Vec::new());

        let recovery = recover(
            dir.path(),
            "0.5.0",
            |_| false,
            |path| {
                started.borrow_mut().push(path.to_path_buf());
                true
            },
        )
        .unwrap();
        assert_eq!(
            recovery,
            Recovery::RolledBack {
                version: "0.4.0".into()
            }
        );
        assert_eq!(std::fs::read(&installed).unwrap(), b"old build");
        assert!(!replacement.backup.exists());
        assert_eq!(
            started.borrow().as_slice(),
            std::slice::from_ref(&installed)
        );
        assert!(read_pending(dir.path()).unwrap().is_none());
    }

    #[test]
    fn recover_cleans_up_a_swap_that_never_happened() {
        let (dir, installed) = setup();
        std::fs::write(sibling(&installed, BACKUP_SUFFIX).unwrap(), b"old build").unwrap();
        write_pending(
            dir.path(),
            &Pending::new("0.4.0", "0.5.0", "v0.5.0", "flotilla.exe"),
        )
        .unwrap();
        let recovery = recover(dir.path(), "0.4.0", |_| true, |_| true).unwrap();
        assert_eq!(
            recovery,
            Recovery::Cleaned {
                version: "0.4.0".into()
            }
        );
        assert!(!sibling(&installed, BACKUP_SUFFIX).unwrap().exists());
        assert!(read_pending(dir.path()).unwrap().is_none());
        assert_eq!(std::fs::read(&installed).unwrap(), b"old build");
    }

    #[test]
    fn recover_leaves_an_unrelated_marker_alone() {
        let (dir, installed) = setup();
        write_pending(
            dir.path(),
            &Pending::new("0.3.0", "0.4.0", "v0.4.0", "flotilla.exe"),
        )
        .unwrap();
        let recovery = recover(dir.path(), "0.9.0", |_| true, |_| true).unwrap();
        assert!(matches!(recovery, Recovery::Unrelated { .. }));
        assert!(read_pending(dir.path()).unwrap().is_some());
        assert_eq!(std::fs::read(&installed).unwrap(), b"old build");
    }

    #[test]
    fn a_corrupt_marker_is_reported_not_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let path = pending_path(dir.path());
        std::fs::write(&path, b"{ not json").unwrap();
        let err = read_pending(dir.path()).unwrap_err();
        assert!(matches!(err, InstallError::Staged(_)));
        assert!(
            path.exists(),
            "the corrupt marker stays for the user to inspect"
        );
    }

    #[test]
    fn no_marker_means_nothing_to_do() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            recover(dir.path(), "0.4.0", |_| true, |_| true).unwrap(),
            Recovery::Nothing
        );
    }
}
