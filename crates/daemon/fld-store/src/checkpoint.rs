//! Content-addressed checkpoints. A returned hash is safe to commit to SQLite.
//!
//! The owning Store serializes publication, database references and reclamation;
//! this module must not be opened concurrently for the same directory.
use std::ffi::CStr;
use std::fs::{File, Metadata, OpenOptions, Permissions};
use std::io::{self, Read, Write};
use std::ops::Deref;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path};

use sha2::{Digest, Sha256};

use crate::{BlobHash, Result, StoreError};

/// Maximum encoded checkpoint size, checked before allocating a read buffer.
pub const MAX_CHECKPOINT_BYTES: u64 = 512 * 1024 * 1024;
const READ_CHUNK_BYTES: usize = 64 * 1024;
const TEMP_PREFIX: &[u8] = b".checkpoint-";
const TEMP_SUFFIX: &[u8] = b".tmp";
const TEMP_NAME_BYTES: usize = TEMP_PREFIX.len() + 64 + TEMP_SUFFIX.len() + 1;

/// A private, descriptor-anchored `root/blobs/terminal` directory.
#[derive(Debug)]
pub struct CheckpointFiles {
    directory: File,
}

impl CheckpointFiles {
    pub fn open(root: &Path) -> Result<Self> {
        let root = open_root(root)?;
        let blobs = private_child_directory(&root, c"blobs")?;
        let directory = private_child_directory(&blobs, c"terminal")?;
        // Only this exact namespace is owned. Unknown files, including non-UTF8
        // names, belong to somebody else and must never be removed.
        visit_owned(&directory, |name, kind| {
            let _file = open_regular(&directory, name)?;
            if matches!(kind, OwnedName::Temporary) {
                unlink(&directory, name)?;
            }
            Ok(())
        })?;
        directory.sync_all()?;
        Ok(Self { directory })
    }

    /// Sync the complete file, publish without replacing an existing name, then
    /// sync its directory before returning a hash usable by the database.
    pub fn publish(&mut self, bytes: &[u8]) -> Result<BlobHash> {
        checked_len(bytes.len() as u64)?;
        let hash: BlobHash = Sha256::digest(bytes).into();
        let name = blob_name(hash);
        match open_regular(&self.directory, &name) {
            Ok(mut file) => {
                verify(&mut file, hash)?;
                // An existing name can be the result of an interrupted publish.
                // It must satisfy the same durability boundary as a new file.
                file.sync_all()?;
                self.directory.sync_all()?;
                return Ok(hash);
            }
            Err(StoreError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }

        let temporary_name = temporary_name(hash);
        let mut file = open_at(
            &self.directory,
            &temporary_name,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NONBLOCK,
        )?;
        let mut temporary = Temporary {
            directory: &self.directory,
            name: temporary_name,
            active: true,
        };
        make_private(&file, 0o600)?;
        file.write_all(bytes)?;
        file.sync_all()?;

        // linkat publishes a complete inode atomically and, unlike rename,
        // cannot silently overwrite corruption at an existing content hash.
        // Both names are in the same directory/filesystem.
        let linked = unsafe {
            libc::linkat(
                self.directory.as_raw_fd(),
                temporary.name.as_ptr(),
                self.directory.as_raw_fd(),
                name.as_ptr(),
                0,
            )
        };
        if linked != 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::AlreadyExists {
                return Err(error.into());
            }
            let mut existing = open_regular(&self.directory, &name)?;
            verify(&mut existing, hash)?;
            existing.sync_all()?;
        }
        temporary.remove()?;
        self.directory.sync_all()?;
        Ok(hash)
    }

    /// Read and authenticate a committed checkpoint; no invalid size is used
    /// for allocation, and a file that grows during the read is rejected.
    pub fn load(&self, hash: BlobHash) -> Result<Vec<u8>> {
        let mut file = open_regular(&self.directory, &blob_name(hash))?;
        let len = checked_len(file.metadata()?.len())?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(len).map_err(io::Error::other)?;
        bytes.resize(len, 0);
        let mut digest = Sha256::new();
        for chunk in bytes.chunks_mut(READ_CHUNK_BYTES) {
            file.read_exact(chunk)?;
            digest.update(chunk);
        }
        require_end(&mut file)?;
        if BlobHash::from(digest.finalize()) != hash {
            return Err(corrupt("checkpoint SHA-256 does not match its filename"));
        }
        Ok(bytes)
    }

    /// Remove only unreferenced hash files and interrupted publication temps.
    /// Duplicate references protect the same shared blob. The returned count
    /// includes removed temporary files, but excludes all unrelated entries.
    pub fn retain_only(&mut self, hashes: &[BlobHash]) -> Result<usize> {
        let mut removed = 0;
        let result = visit_owned(&self.directory, |name, kind| {
            // Validate even protected entries: symlinks and foreign-owned files
            // are never followed, chmodded or unlinked.
            let _file = open_regular(&self.directory, name)?;
            let retain = matches!(kind, OwnedName::Blob(hash) if hashes.contains(&hash));
            if !retain {
                unlink(&self.directory, name)?;
                removed += 1;
            }
            Ok(())
        });
        // Persist a partial GC too, while preserving its original error.
        let synced = self.directory.sync_all().map_err(StoreError::from);
        result?;
        synced?;
        Ok(removed)
    }
}

fn checked_len(len: u64) -> Result<usize> {
    if len == 0 || len > MAX_CHECKPOINT_BYTES {
        return Err(corrupt(
            "checkpoint size must be between 1 byte and 512 MiB",
        ));
    }
    usize::try_from(len).map_err(|_| corrupt("checkpoint size exceeds addressable memory"))
}

fn verify(file: &mut File, expected: BlobHash) -> Result<()> {
    let mut remaining = checked_len(file.metadata()?.len())?;
    let mut digest = Sha256::new();
    let mut buffer = [0; READ_CHUNK_BYTES];
    while remaining != 0 {
        let len = remaining.min(buffer.len());
        file.read_exact(&mut buffer[..len])?;
        digest.update(&buffer[..len]);
        remaining -= len;
    }
    require_end(file)?;
    if BlobHash::from(digest.finalize()) != expected {
        return Err(corrupt(
            "existing checkpoint SHA-256 does not match its filename",
        ));
    }
    Ok(())
}

fn require_end(file: &mut File) -> Result<()> {
    if file.read(&mut [0])? != 0 {
        return Err(corrupt("checkpoint changed size while being read"));
    }
    Ok(())
}

struct Filename<const N: usize>([u8; N]);

impl<const N: usize> Deref for Filename<N> {
    type Target = CStr;

    fn deref(&self) -> &CStr {
        CStr::from_bytes_with_nul(&self.0).expect("fixed hexadecimal checkpoint filename")
    }
}

fn blob_name(hash: BlobHash) -> Filename<69> {
    let mut name = [0; 69];
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for (index, byte) in hash.into_iter().enumerate() {
        name[index * 2] = HEX[usize::from(byte >> 4)];
        name[index * 2 + 1] = HEX[usize::from(byte & 15)];
    }
    name[64..68].copy_from_slice(b".bin");
    Filename(name)
}

fn temporary_name(hash: BlobHash) -> Filename<TEMP_NAME_BYTES> {
    let blob = blob_name(hash);
    let mut name = [0; TEMP_NAME_BYTES];
    name[..TEMP_PREFIX.len()].copy_from_slice(TEMP_PREFIX);
    let suffix = TEMP_PREFIX.len() + 64;
    name[TEMP_PREFIX.len()..suffix].copy_from_slice(&blob.0[..64]);
    name[suffix..TEMP_NAME_BYTES - 1].copy_from_slice(TEMP_SUFFIX);
    Filename(name)
}

#[derive(Clone, Copy)]
enum OwnedName {
    Blob(BlobHash),
    Temporary,
}

fn owned_name(name: &[u8]) -> Option<OwnedName> {
    if let Some(digits) = name.strip_suffix(b".bin") {
        return decode_hash(digits).map(OwnedName::Blob);
    }
    let digits = name.strip_prefix(TEMP_PREFIX)?.strip_suffix(TEMP_SUFFIX)?;
    decode_hash(digits).map(|_| OwnedName::Temporary)
}

fn decode_hash(digits: &[u8]) -> Option<BlobHash> {
    if digits.len() != 64 {
        return None;
    }
    fn nibble(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            _ => None,
        }
    }
    let mut hash = [0; 32];
    for (byte, [high, low]) in hash.iter_mut().zip(digits.as_chunks::<2>().0) {
        *byte = (nibble(*high)? << 4) | nibble(*low)?;
    }
    Some(hash)
}

fn open_root(path: &Path) -> Result<File> {
    if path.as_os_str().is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty store root").into());
    }
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(if path.is_absolute() {
            Path::new("/")
        } else {
            Path::new(".")
        })?;
    // Walk instead of canonicalizing: canonicalize would accept symlinked
    // ancestors. Descriptor-relative operations remain anchored across renames.
    for component in path.components() {
        let component = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(name) => name,
            Component::ParentDir => std::ffi::OsStr::new(".."),
            Component::Prefix(_) => {
                return Err(
                    io::Error::new(io::ErrorKind::InvalidInput, "unsupported store root").into(),
                );
            }
        };
        let bytes = component.as_bytes();
        // Linux filesystem components are at most NAME_MAX (255) bytes.
        let mut name = [0; 256];
        if bytes.len() >= name.len() || bytes.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid store root component",
            )
            .into());
        }
        name[..bytes.len()].copy_from_slice(bytes);
        let name = CStr::from_bytes_with_nul(&name[..=bytes.len()])
            .expect("validated store root component");
        directory = child_directory(&directory, name)?;
    }
    require_owner(&directory.metadata()?)?;
    make_private(&directory, 0o700)?;
    directory.sync_all()?;
    Ok(directory)
}

fn child_directory(parent: &File, name: &CStr) -> Result<File> {
    match open_at(parent, name, libc::O_RDONLY | libc::O_DIRECTORY) {
        Ok(directory) => {
            parent.sync_all()?;
            Ok(directory)
        }
        Err(StoreError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            let created = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) };
            if created != 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::AlreadyExists {
                    return Err(error.into());
                }
            }
            let directory = open_at(parent, name, libc::O_RDONLY | libc::O_DIRECTORY)?;
            require_owner(&directory.metadata()?)?;
            make_private(&directory, 0o700)?;
            directory.sync_all()?;
            parent.sync_all()?;
            Ok(directory)
        }
        Err(error) => Err(error),
    }
}

fn private_child_directory(parent: &File, name: &CStr) -> Result<File> {
    let directory = child_directory(parent, name)?;
    require_owner(&directory.metadata()?)?;
    make_private(&directory, 0o700)?;
    directory.sync_all()?;
    Ok(directory)
}

fn open_at(parent: &File, name: &CStr, flags: libc::c_int) -> Result<File> {
    // All names are individual components, never caller-controlled blob paths.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600 as libc::mode_t,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: openat returned a new descriptor which this File exclusively owns.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn open_regular(directory: &File, name: &CStr) -> Result<File> {
    // NONBLOCK prevents an unexpected FIFO from hanging before type validation.
    let file = open_at(directory, name, libc::O_RDONLY | libc::O_NONBLOCK)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(corrupt("checkpoint entry is not a regular file"));
    }
    require_owner(&metadata)?;
    make_private(&file, 0o600)?;
    Ok(file)
}

fn require_owner(metadata: &Metadata) -> Result<()> {
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(corrupt("checkpoint path is not owned by the current user"));
    }
    Ok(())
}

fn make_private(file: &File, mode: u32) -> Result<()> {
    if file.metadata()?.permissions().mode() & 0o7777 != mode {
        file.set_permissions(Permissions::from_mode(mode))?;
        file.sync_all()?;
    }
    Ok(())
}

fn unlink(directory: &File, name: &CStr) -> Result<()> {
    if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

struct Temporary<'a> {
    directory: &'a File,
    name: Filename<TEMP_NAME_BYTES>,
    active: bool,
}

impl Temporary<'_> {
    fn remove(&mut self) -> Result<()> {
        unlink(self.directory, &self.name)?;
        self.active = false;
        Ok(())
    }
}

impl Drop for Temporary<'_> {
    fn drop(&mut self) {
        if self.active {
            // Failure already prevents the caller from acknowledging a journal.
            // If cleanup also fails, open/retain_only handles the interrupted temp.
            let _ = unlink(self.directory, &self.name);
        }
    }
}

struct DirectoryStream(*mut libc::DIR);

impl Drop for DirectoryStream {
    fn drop(&mut self) {
        unsafe { libc::closedir(self.0) };
    }
}

fn visit_owned(
    directory: &File,
    mut visit: impl FnMut(&CStr, OwnedName) -> Result<()>,
) -> Result<()> {
    // A fresh open description gives each scan an independent directory offset.
    let fd = open_at(directory, c".", libc::O_RDONLY | libc::O_DIRECTORY)?.into_raw_fd();
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        let error = io::Error::last_os_error();
        drop(unsafe { File::from_raw_fd(fd) });
        return Err(error.into());
    }
    let stream = DirectoryStream(stream);
    loop {
        // readdir reports both EOF and errors as NULL; errno disambiguates them.
        unsafe { *libc::__errno_location() = 0 };
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(0) {
                Ok(())
            } else {
                Err(error.into())
            };
        }
        // SAFETY: readdir's NUL-terminated name lives until the next call on this
        // stream, and the callback completes before that next call.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if let Some(kind) = owned_name(name.to_bytes()) {
            visit(name, kind)?;
        }
    }
}

fn corrupt(message: &str) -> StoreError {
    StoreError::Corrupt(message.to_owned())
}

#[cfg(test)]
#[path = "checkpoint_tests.rs"]
mod tests;
