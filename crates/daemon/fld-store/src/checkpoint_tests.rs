use super::*;
use std::ffi::{CString, OsStr, OsString};
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{FileTypeExt, symlink};
use std::path::PathBuf;
use tempfile::TempDir;

fn fixture() -> (TempDir, PathBuf, CheckpointFiles) {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("state");
    let files = CheckpointFiles::open(&root).unwrap();
    (directory, root, files)
}

fn terminal_directory(root: &Path) -> PathBuf {
    root.join("blobs/terminal")
}

fn blob_path(root: &Path, hash: BlobHash) -> PathBuf {
    terminal_directory(root).join(OsStr::from_bytes(blob_name(hash).to_bytes()))
}

fn temporary_path(root: &Path, hash: BlobHash) -> PathBuf {
    terminal_directory(root).join(OsStr::from_bytes(temporary_name(hash).to_bytes()))
}

fn write_synced(path: &Path, bytes: &[u8]) {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(bytes).unwrap();
    file.sync_all().unwrap();
}

#[test]
fn restart_preserves_exact_bytes_and_content_address() {
    let (_directory, root, mut files) = fixture();
    let first = files.publish(b"abc").unwrap();
    assert_eq!(
        first,
        [
            0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
            0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
            0xf2, 0x00, 0x15, 0xad,
        ]
    );
    let bytes: Vec<_> = (0..(READ_CHUNK_BYTES * 3 + 17))
        .map(|index| (index % 251) as u8)
        .collect();
    let second = files.publish(&bytes).unwrap();
    assert_ne!(first, second);
    assert_eq!(files.publish(b"abc").unwrap(), first);
    assert_eq!(fs::read_dir(terminal_directory(&root)).unwrap().count(), 2);
    assert!(!temporary_path(&root, first).exists());
    drop(files);

    let mut files = CheckpointFiles::open(&root).unwrap();
    assert_eq!(files.load(first).unwrap(), b"abc");
    assert_eq!(files.load(second).unwrap(), bytes);
    assert_eq!(files.publish(b"abc").unwrap(), first);
}

#[test]
fn interrupted_unpublished_temp_is_removed_without_inventing_a_checkpoint() {
    let (_directory, root, mut files) = fixture();
    let committed = files.publish(b"already committed").unwrap();
    let incomplete_hash: BlobHash = Sha256::digest(b"complete candidate").into();
    let temporary = temporary_path(&root, incomplete_hash);
    write_synced(&temporary, b"complete can");
    drop(files);

    let mut files = CheckpointFiles::open(&root).unwrap();
    assert!(!temporary.exists());
    assert!(files.load(incomplete_hash).is_err());
    assert_eq!(files.load(committed).unwrap(), b"already committed");
    assert_eq!(
        files.publish(b"complete candidate").unwrap(),
        incomplete_hash
    );
    assert_eq!(files.load(incomplete_hash).unwrap(), b"complete candidate");
}

#[test]
fn interrupted_link_publication_keeps_the_complete_blob_and_reclaims_the_temp() {
    let (_directory, root, files) = fixture();
    let bytes = b"complete and file-synced before atomic publication";
    let hash: BlobHash = Sha256::digest(bytes).into();
    let temporary = temporary_path(&root, hash);
    write_synced(&temporary, bytes);
    fs::hard_link(&temporary, blob_path(&root, hash)).unwrap();
    // Simulate process death between the atomic link and temp removal/directory
    // sync. Reopening establishes durability before a republished hash is usable.
    drop(files);

    let mut files = CheckpointFiles::open(&root).unwrap();
    assert!(!temporary.exists());
    assert_eq!(files.load(hash).unwrap(), bytes);
    let inode = fs::metadata(blob_path(&root, hash)).unwrap().ino();
    assert_eq!(files.publish(bytes).unwrap(), hash);
    assert_eq!(fs::metadata(blob_path(&root, hash)).unwrap().ino(), inode);
    assert_eq!(fs::read_dir(terminal_directory(&root)).unwrap().count(), 1);
}

#[test]
fn corrupt_existing_content_is_never_replaced_or_acknowledged() {
    let (_directory, root, mut files) = fixture();
    let bytes = b"original checkpoint";
    let hash = files.publish(bytes).unwrap();
    let path = blob_path(&root, hash);
    let mut corrupt_bytes = bytes.to_vec();
    corrupt_bytes[3] ^= 0x80;
    let mut file = OpenOptions::new().write(true).open(&path).unwrap();
    file.write_all(&corrupt_bytes).unwrap();
    file.sync_all().unwrap();
    drop(file);

    assert!(files.load(hash).is_err());
    assert!(files.publish(bytes).is_err());
    assert_eq!(fs::read(&path).unwrap(), corrupt_bytes);
    assert!(!temporary_path(&root, hash).exists());
    drop(files);
    let mut files = CheckpointFiles::open(&root).unwrap();
    assert!(files.load(hash).is_err());
    assert!(files.publish(bytes).is_err());
    assert_eq!(fs::read(path).unwrap(), corrupt_bytes);
}

#[test]
fn missing_empty_and_sparse_oversized_committed_blobs_fail_closed() {
    let (_directory, root, mut files) = fixture();
    assert!(files.publish(b"").is_err());
    assert_eq!(fs::read_dir(terminal_directory(&root)).unwrap().count(), 0);

    let bytes = b"the database references these exact bytes";
    let hash = files.publish(bytes).unwrap();
    let path = blob_path(&root, hash);
    fs::remove_file(&path).unwrap();
    assert!(
        matches!(files.load(hash), Err(StoreError::Io(error)) if error.kind() == io::ErrorKind::NotFound)
    );

    write_synced(&path, b"");
    assert!(files.load(hash).is_err());
    assert!(files.publish(bytes).is_err());
    assert_eq!(fs::metadata(&path).unwrap().len(), 0);

    let file = OpenOptions::new().write(true).open(&path).unwrap();
    file.set_len(MAX_CHECKPOINT_BYTES + 1).unwrap();
    file.sync_all().unwrap();
    // The oversized sparse length must be rejected before allocating its bytes.
    assert!(files.load(hash).is_err());
    assert!(files.publish(bytes).is_err());
    assert_eq!(fs::metadata(&path).unwrap().len(), MAX_CHECKPOINT_BYTES + 1);
    drop(files);
    let files = CheckpointFiles::open(&root).unwrap();
    assert!(files.load(hash).is_err());
}

#[test]
fn shared_references_protect_content_and_orphan_gc_survives_restart() {
    let (_directory, root, mut files) = fixture();
    let shared = files
        .publish(b"shared by two terminal checkpoints")
        .unwrap();
    assert_eq!(
        files
            .publish(b"shared by two terminal checkpoints")
            .unwrap(),
        shared
    );
    let orphan = files
        .publish(b"published before its database transaction crashed")
        .unwrap();
    let previous = files.publish(b"superseded checkpoint").unwrap();
    let interrupted: BlobHash = Sha256::digest(b"interrupted publication").into();
    write_synced(&temporary_path(&root, interrupted), b"interrupted");

    assert_eq!(files.retain_only(&[shared, shared]).unwrap(), 3);
    assert_eq!(
        files.load(shared).unwrap(),
        b"shared by two terminal checkpoints"
    );
    assert!(files.load(orphan).is_err());
    assert!(files.load(previous).is_err());
    assert!(!temporary_path(&root, interrupted).exists());
    // Removing just one database reference must not unlink the shared content.
    assert_eq!(files.retain_only(&[shared]).unwrap(), 0);
    drop(files);

    let mut files = CheckpointFiles::open(&root).unwrap();
    assert_eq!(
        files.load(shared).unwrap(),
        b"shared by two terminal checkpoints"
    );
    assert_eq!(files.retain_only(&[shared]).unwrap(), 0);
    assert_eq!(files.retain_only(&[]).unwrap(), 1);
    drop(files);
    let files = CheckpointFiles::open(&root).unwrap();
    assert!(files.load(shared).is_err());
    assert_eq!(fs::read_dir(terminal_directory(&root)).unwrap().count(), 0);
}

#[test]
fn cleanup_leaves_unrelated_names_directories_and_symlinks_untouched() {
    let (_directory, root, mut files) = fixture();
    let terminal = terminal_directory(&root);
    let unrelated = [
        OsString::from("notes.txt"),
        OsString::from(format!("{}.bin", "A".repeat(64))),
        OsString::from("12.bin"),
        OsString::from(".checkpoint-not-a-hash.tmp"),
        OsString::from_vec(b"unrelated-\xff.bin".to_vec()),
    ];
    for name in &unrelated {
        write_synced(&terminal.join(name), b"not owned by checkpoint storage");
    }
    let subdirectory = terminal.join("unrelated-directory");
    fs::create_dir(&subdirectory).unwrap();
    write_synced(&subdirectory.join("keep"), b"nested file");
    let outside = root.join("outside");
    write_synced(&outside, b"outside data");
    let unrelated_symlink = terminal.join("unrelated-link");
    symlink(&outside, &unrelated_symlink).unwrap();
    let orphan = files.publish(b"owned orphan").unwrap();

    assert_eq!(files.retain_only(&[]).unwrap(), 1);
    assert!(!blob_path(&root, orphan).exists());
    drop(files);
    let mut files = CheckpointFiles::open(&root).unwrap();
    assert_eq!(files.retain_only(&[]).unwrap(), 0);
    for name in &unrelated {
        assert_eq!(
            fs::read(terminal.join(name)).unwrap(),
            b"not owned by checkpoint storage"
        );
    }
    assert_eq!(fs::read(subdirectory.join("keep")).unwrap(), b"nested file");
    assert!(
        fs::symlink_metadata(unrelated_symlink)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(outside).unwrap(), b"outside data");
}

#[test]
fn owned_blob_symlinks_are_rejected_without_touching_their_targets() {
    let (_directory, root, mut files) = fixture();
    let bytes = b"outside content must never be used as a checkpoint";
    let hash: BlobHash = Sha256::digest(bytes).into();
    let outside = root.join("outside");
    write_synced(&outside, bytes);
    fs::set_permissions(&outside, Permissions::from_mode(0o644)).unwrap();
    let link = blob_path(&root, hash);
    symlink(&outside, &link).unwrap();

    assert!(files.load(hash).is_err());
    assert!(files.publish(bytes).is_err());
    assert!(files.retain_only(&[]).is_err());
    drop(files);
    assert!(CheckpointFiles::open(&root).is_err());
    assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
    assert_eq!(fs::read(&outside).unwrap(), bytes);
    assert_eq!(
        fs::metadata(outside).unwrap().permissions().mode() & 0o777,
        0o644
    );
}

#[test]
fn owned_temporary_symlinks_are_not_removed_as_interrupted_publications() {
    let (_directory, root, mut files) = fixture();
    let outside = root.join("outside");
    write_synced(&outside, b"keep");
    let hash: BlobHash = Sha256::digest(b"incomplete").into();
    let link = temporary_path(&root, hash);
    symlink(&outside, &link).unwrap();

    assert!(files.retain_only(&[]).is_err());
    drop(files);
    assert!(CheckpointFiles::open(&root).is_err());
    assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
    assert_eq!(fs::read(outside).unwrap(), b"keep");
}

#[test]
fn root_ancestors_and_blob_directories_cannot_be_symlinks() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("target");
    fs::create_dir(&target).unwrap();
    let alias = directory.path().join("alias");
    symlink(&target, &alias).unwrap();
    assert!(CheckpointFiles::open(&alias).is_err());
    assert!(CheckpointFiles::open(&alias.join("state")).is_err());
    assert!(!target.join("state").exists());

    let root = directory.path().join("state");
    fs::create_dir(&root).unwrap();
    symlink(&target, root.join("blobs")).unwrap();
    assert!(CheckpointFiles::open(&root).is_err());
    fs::remove_file(root.join("blobs")).unwrap();
    fs::create_dir(root.join("blobs")).unwrap();
    symlink(&target, root.join("blobs/terminal")).unwrap();
    assert!(CheckpointFiles::open(&root).is_err());
    assert_eq!(fs::read_dir(target).unwrap().count(), 0);
}

#[test]
fn owned_nonregular_entries_are_rejected_and_not_unlinked() {
    let (_directory, root, mut files) = fixture();
    let bytes = b"not a directory or FIFO";
    let hash: BlobHash = Sha256::digest(bytes).into();
    let path = blob_path(&root, hash);
    fs::create_dir(&path).unwrap();
    assert!(files.load(hash).is_err());
    assert!(files.publish(bytes).is_err());
    assert!(files.retain_only(&[]).is_err());
    assert!(path.is_dir());
    fs::remove_dir(&path).unwrap();

    let name = CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    assert!(files.load(hash).is_err());
    assert!(files.publish(bytes).is_err());
    assert!(files.retain_only(&[]).is_err());
    assert!(fs::symlink_metadata(&path).unwrap().file_type().is_fifo());
    drop(files);
    assert!(CheckpointFiles::open(&root).is_err());
    assert!(path.exists());
}

#[test]
fn directories_and_files_are_private_on_creation_and_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("new-parent/nested/state");
    let mut files = CheckpointFiles::open(&root).unwrap();
    let hash = files.publish(b"private checkpoint").unwrap();
    let directories = [
        directory.path().join("new-parent"),
        directory.path().join("new-parent/nested"),
        root.clone(),
        root.join("blobs"),
        terminal_directory(&root),
    ];
    for path in &directories {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    let path = blob_path(&root, hash);
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    drop(files);
    for path in [&root, &root.join("blobs"), &terminal_directory(&root)] {
        fs::set_permissions(path, Permissions::from_mode(0o755)).unwrap();
    }
    fs::set_permissions(&path, Permissions::from_mode(0o644)).unwrap();
    let files = CheckpointFiles::open(&root).unwrap();
    assert_eq!(files.load(hash).unwrap(), b"private checkpoint");
    for path in [&root, &root.join("blobs"), &terminal_directory(&root)] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn open_directory_handles_keep_publication_and_gc_out_of_replaced_paths() {
    let (directory, root, mut files) = fixture();
    let original = files.publish(b"original directory orphan").unwrap();
    let moved = directory.path().join("moved-state");
    fs::rename(&root, &moved).unwrap();
    let replacement = terminal_directory(&root);
    fs::create_dir_all(&replacement).unwrap();
    let unrelated = blob_path(&root, original);
    write_synced(&unrelated, b"unrelated replacement path");

    assert_eq!(files.retain_only(&[]).unwrap(), 1);
    assert!(!blob_path(&moved, original).exists());
    assert_eq!(fs::read(&unrelated).unwrap(), b"unrelated replacement path");
    let retained = files.publish(b"anchored publication").unwrap();
    assert_eq!(files.load(retained).unwrap(), b"anchored publication");
    assert!(blob_path(&moved, retained).exists());
    assert!(!blob_path(&root, retained).exists());
    drop(files);
    let files = CheckpointFiles::open(&moved).unwrap();
    assert_eq!(files.load(retained).unwrap(), b"anchored publication");
    assert_eq!(fs::read(unrelated).unwrap(), b"unrelated replacement path");
}
