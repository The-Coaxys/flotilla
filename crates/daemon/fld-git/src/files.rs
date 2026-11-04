//! Directory listings, file reads, atomic writes and diff sides.
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use fl_model::workspace::{DirEntry, DirListing, EntryKind, FileContent};
use sha2::{Digest, Sha256};

use crate::cmd::{self, line};
use crate::{GitError, Result};

/// A file is binary when its first this-many bytes contain a NUL.
pub const BINARY_SNIFF_BYTES: usize = 8192;

/// Mode of files created by [`write_file`].
pub const NEW_FILE_MODE: u32 = 0o644;

fn not_found_or_io(path: &Path, action: &str, e: std::io::Error) -> GitError {
    if e.kind() == std::io::ErrorKind::NotFound {
        GitError::NotFound(format!("{} does not exist", path.display()))
    } else {
        GitError::io(format!("cannot {action} {}", path.display()), e)
    }
}

/// Lists `path`: directories first, then everything else, each sorted
/// case-insensitively; at most `max_entries`. `ignored` reflects the
/// enclosing work tree's ignore rules (tracked files are never ignored).
pub fn list_dir(path: &Path, max_entries: usize) -> Result<DirListing> {
    let dir = std::fs::canonicalize(path).map_err(|e| not_found_or_io(path, "resolve", e))?;
    let meta = std::fs::metadata(&dir).map_err(|e| not_found_or_io(&dir, "inspect", e))?;
    if !meta.is_dir() {
        return Err(GitError::Invalid(format!(
            "{} is not a directory",
            dir.display()
        )));
    }
    let reader = std::fs::read_dir(&dir).map_err(|e| not_found_or_io(&dir, "list", e))?;
    let mut entries = Vec::new();
    for entry in reader {
        let entry = entry.map_err(|e| GitError::io(format!("cannot list {}", dir.display()), e))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        // A racing delete leaves the entry without metadata; it is skipped.
        let Ok(meta) = entry.path().symlink_metadata() else {
            continue;
        };
        let file_type = meta.file_type();
        let kind = if file_type.is_symlink() {
            EntryKind::Symlink
        } else if file_type.is_dir() {
            EntryKind::Dir
        } else if file_type.is_file() {
            EntryKind::File
        } else {
            EntryKind::Other
        };
        let modified_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        entries.push(DirEntry {
            name,
            kind,
            size: if kind == EntryKind::Dir {
                0
            } else {
                meta.len()
            },
            modified_ms,
            ignored: false,
        });
    }
    mark_ignored(&dir, &mut entries);
    entries.sort_by(|a, b| {
        let a_dir = a.kind == EntryKind::Dir;
        let b_dir = b.kind == EntryKind::Dir;
        b_dir
            .cmp(&a_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
    let truncated = entries.len() > max_entries;
    entries.truncate(max_entries);
    Ok(DirListing {
        path: dir.to_string_lossy().into_owned(),
        entries,
        truncated,
    })
}

/// Sets `ignored` from one batched `git check-ignore`; leaves everything
/// unignored outside a work tree or when git cannot tell.
fn mark_ignored(dir: &Path, entries: &mut [DirEntry]) {
    let inside = cmd::output(dir, &["rev-parse", "--is-inside-work-tree"])
        .map(|out| out.status.success() && line(&out.stdout) == "true")
        .unwrap_or(false);
    if !inside {
        return;
    }
    let mut input = Vec::new();
    for entry in entries.iter().filter(|e| e.name != ".git") {
        input.extend_from_slice(entry.name.as_bytes());
        input.push(0);
    }
    if input.is_empty() {
        return;
    }
    let args = ["check-ignore", "--stdin", "-z"];
    let out = match cmd::output_with_input(dir, &args, &input) {
        Ok(out) => out,
        Err(e) => {
            tracing::warn!(dir = %dir.display(), error = %e, "cannot check ignored files");
            return;
        }
    };
    // Exit 1 means nothing is ignored.
    if !matches!(out.status.code(), Some(0 | 1)) {
        tracing::warn!(
            dir = %dir.display(),
            error = %cmd::command_error(&args, &out),
            "cannot check ignored files"
        );
        return;
    }
    let ignored: HashSet<&[u8]> = out
        .stdout
        .split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .collect();
    for entry in entries.iter_mut() {
        entry.ignored = entry.name != ".git" && ignored.contains(entry.name.as_bytes());
    }
}

/// Streaming accumulator for a [`FileContent`].
struct ContentBuilder {
    hasher: Sha256,
    bytes: Vec<u8>,
    size: u64,
    binary: bool,
    max_bytes: usize,
}

impl ContentBuilder {
    fn new(max_bytes: usize) -> Self {
        Self {
            hasher: Sha256::new(),
            bytes: Vec::new(),
            size: 0,
            binary: false,
            max_bytes,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        self.hasher.update(chunk);
        let offset = usize::try_from(self.size).unwrap_or(usize::MAX);
        if offset < BINARY_SNIFF_BYTES {
            let sniff = &chunk[..chunk.len().min(BINARY_SNIFF_BYTES - offset)];
            self.binary |= sniff.contains(&0);
        }
        if self.bytes.len() < self.max_bytes {
            let take = chunk.len().min(self.max_bytes - self.bytes.len());
            self.bytes.extend_from_slice(&chunk[..take]);
        }
        self.size += chunk.len() as u64;
    }

    fn finish(self, path: String) -> FileContent {
        FileContent {
            path,
            truncated: self.size > self.bytes.len() as u64,
            bytes: self.bytes,
            sha256: self.hasher.finalize().into(),
            size: self.size,
            binary: self.binary,
        }
    }
}

fn read_stream(reader: &mut impl Read, mut sink: impl FnMut(&[u8])) -> std::io::Result<()> {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => sink(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

fn open_regular(path: &Path) -> Result<File> {
    let file = File::open(path).map_err(|e| not_found_or_io(path, "open", e))?;
    let meta = file
        .metadata()
        .map_err(|e| GitError::io(format!("cannot inspect {}", path.display()), e))?;
    if !meta.is_file() {
        return Err(GitError::Invalid(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    Ok(file)
}

fn sha256_file(path: &Path) -> Result<[u8; 32]> {
    let mut file = open_regular(path)?;
    let mut hasher = Sha256::new();
    read_stream(&mut file, |chunk| hasher.update(chunk))
        .map_err(|e| GitError::io(format!("cannot read {}", path.display()), e))?;
    Ok(hasher.finalize().into())
}

/// Reads a regular file (following symlinks): the first `max_bytes` bytes,
/// with size, binary flag and SHA-256 over the whole file.
pub fn read_file(path: &Path, max_bytes: usize) -> Result<FileContent> {
    let mut file = open_regular(path)?;
    let mut content = ContentBuilder::new(max_bytes);
    read_stream(&mut file, |chunk| content.push(chunk))
        .map_err(|e| GitError::io(format!("cannot read {}", path.display()), e))?;
    Ok(content.finish(path.to_string_lossy().into_owned()))
}

/// Follows `path` through symlinks (also dangling ones) to the file that
/// a write should replace.
fn resolve_write_target(path: &Path) -> Result<PathBuf> {
    let mut target = path.to_path_buf();
    for _ in 0..40 {
        match target.symlink_metadata() {
            Ok(meta) if meta.file_type().is_symlink() => {
                let link = std::fs::read_link(&target).map_err(|e| {
                    GitError::io(format!("cannot read link {}", target.display()), e)
                })?;
                target = match target.parent() {
                    Some(parent) => parent.join(link),
                    None => link,
                };
            }
            _ => return Ok(target),
        }
    }
    Err(GitError::Invalid(format!(
        "{} has too many levels of symbolic links",
        path.display()
    )))
}

/// Removes the temporary file unless the write completed.
struct TempGuard {
    path: Option<PathBuf>,
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn random_suffix() -> String {
    let mut random = [0u8; 6];
    if getrandom::fill(&mut random).is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        random.copy_from_slice(&nanos.to_le_bytes()[..6]);
    }
    random.iter().map(|b| format!("{b:02x}")).collect()
}

/// Atomically replaces `path` (the symlink target when `path` is a link)
/// with `bytes`. With `expected_sha256`, refuses with
/// [`GitError::Conflict`] unless the current file has that hash. Keeps the
/// existing file's mode; new files get [`NEW_FILE_MODE`]. Returns the
/// SHA-256 of `bytes`.
pub fn write_file(
    path: &Path,
    bytes: &[u8],
    expected_sha256: Option<[u8; 32]>,
) -> Result<[u8; 32]> {
    let target = resolve_write_target(path)?;
    let Some(name) = target.file_name() else {
        return Err(GitError::Invalid(format!(
            "{} is not a file path",
            path.display()
        )));
    };
    let parent = match target.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    if !parent.is_dir() {
        return Err(GitError::NotFound(format!(
            "directory {} does not exist",
            parent.display()
        )));
    }
    let existing = match std::fs::metadata(&target) {
        Ok(meta) if meta.is_file() => Some(meta),
        Ok(_) => {
            return Err(GitError::Invalid(format!(
                "{} is not a regular file",
                target.display()
            )));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(GitError::io(
                format!("cannot inspect {}", target.display()),
                e,
            ));
        }
    };
    if let Some(expected) = expected_sha256 {
        let current = match existing {
            Some(_) => match sha256_file(&target) {
                Ok(sha) => Some(sha),
                Err(GitError::NotFound(_)) => None,
                Err(e) => return Err(e),
            },
            None => None,
        };
        if current != Some(expected) {
            return Err(GitError::Conflict { current });
        }
    }
    let mode = existing.map_or(NEW_FILE_MODE, |m| m.permissions().mode() & 0o7777);

    let temp_path = parent.join(format!(
        ".{}.flotilla-{}",
        name.to_string_lossy(),
        random_suffix()
    ));
    let mut temp = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp_path)
        .map_err(|e| GitError::io(format!("cannot create {}", temp_path.display()), e))?;
    let mut guard = TempGuard {
        path: Some(temp_path.clone()),
    };
    let io = |e| GitError::io(format!("cannot write {}", temp_path.display()), e);
    temp.write_all(bytes).map_err(io)?;
    temp.set_permissions(std::fs::Permissions::from_mode(mode))
        .map_err(io)?;
    temp.sync_all().map_err(io)?;
    drop(temp);
    std::fs::rename(&temp_path, &target)
        .map_err(|e| GitError::io(format!("cannot replace {}", target.display()), e))?;
    guard.path = None;
    // The rename is durable once the directory is synced.
    File::open(&parent)
        .and_then(|dir| dir.sync_all())
        .map_err(|e| GitError::io(format!("cannot sync {}", parent.display()), e))?;
    Ok(Sha256::digest(bytes).into())
}

/// Rejects absolute paths and paths that leave the worktree via `..`.
fn check_relative(relative: &str) -> Result<()> {
    let path = Path::new(relative);
    let escapes = relative.is_empty()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir));
    if escapes {
        Err(GitError::Invalid(format!(
            "`{relative}` is not a path inside the worktree"
        )))
    } else {
        Ok(())
    }
}

/// Both sides of a changed file: `old` is the blob at `base` (at
/// `old_relative` for renames), `None` when absent there or without a base;
/// `new` is the working-tree file, `None` when it does not exist. Both
/// carry `relative` as their path.
pub fn diff_sides(
    worktree: &Path,
    base: Option<&str>,
    relative: &str,
    old_relative: Option<&str>,
    max_bytes: usize,
) -> Result<(Option<FileContent>, Option<FileContent>)> {
    check_relative(relative)?;
    if let Some(old) = old_relative {
        check_relative(old)?;
    }
    let old = match base {
        Some(base) => {
            if base.is_empty() || cmd::is_option_like(base) {
                return Err(GitError::Invalid(format!(
                    "`{base}` is not a valid revision"
                )));
            }
            // `./` makes the path relative to the worktree, not the
            // repository root.
            let spec = format!("{base}:./{}", old_relative.unwrap_or(relative));
            let kind = cmd::output(worktree, &["cat-file", "-t", spec.as_str()])?;
            if kind.status.success() && line(&kind.stdout) == "blob" {
                let mut content = ContentBuilder::new(max_bytes);
                cmd::stream(worktree, &["cat-file", "blob", spec.as_str()], |chunk| {
                    content.push(chunk)
                })?;
                Some(content.finish(relative.to_owned()))
            } else {
                None
            }
        }
        None => None,
    };
    let new = match read_file(&worktree.join(relative), max_bytes) {
        Ok(mut content) => {
            content.path = relative.to_owned();
            Some(content)
        }
        // Deleted, or no longer a file (a directory or submodule).
        Err(GitError::NotFound(_) | GitError::Invalid(_)) => None,
        Err(e) => return Err(e),
    };
    Ok((old, new))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_escaping_paths() {
        for bad in ["", "/etc/passwd", "../x", "a/../../x", "a/.."] {
            assert!(
                matches!(check_relative(bad), Err(GitError::Invalid(_))),
                "{bad}"
            );
        }
        for good in ["a", "a/b.rs", "./a", "dir/.hidden"] {
            check_relative(good).unwrap();
        }
    }

    #[test]
    fn content_builder_sniffs_across_chunks() {
        let mut c = ContentBuilder::new(4);
        c.push(&[b'a'; BINARY_SNIFF_BYTES - 1]);
        c.push(b"b\0");
        let content = c.finish("x".into());
        assert!(!content.binary, "NUL after the first 8 KiB is not binary");
        assert_eq!(content.bytes, b"aaaa");
        assert!(content.truncated);
        assert_eq!(content.size, BINARY_SNIFF_BYTES as u64 + 1);

        let mut c = ContentBuilder::new(100);
        c.push(&[b'a'; BINARY_SNIFF_BYTES - 1]);
        c.push(b"\0");
        assert!(c.finish("x".into()).binary);
    }
}
