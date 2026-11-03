//! Offline cache under a caller-chosen per-host directory: the last
//! acknowledged layout with the layout operations not yet acknowledged, and
//! one mirror checkpoint per terminal. Files are replaced atomically (unique
//! temporary file, flush to disk, rename) and carry a SHA-256 trailer, so a
//! crash or a corrupt file yields "no cache", never a wrong state. Pending
//! terminal input is never written: stale keystrokes must not be replayed.
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use fl_model::layout::{Layout, LayoutOp};
use fl_model::{IncarnationId, LogCursor, OperationId, TerminalId};
use fl_term::{ENGINE_ID, EffectMode, Engine};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MAGIC: &[u8; 8] = b"FLCACHE1";
const KIND_WORKSPACE: u8 = 1;
const KIND_MIRROR: u8 = 2;
const DIGEST: usize = 32;
/// Same ceiling as the checkpoint codec; larger files are not read.
const MAX_FILE_BYTES: u64 = 512 * 1024 * 1024 + 4096;

/// A layout operation not yet acknowledged by the daemon. `base_rev` is set
/// once it was sent: its first retry must repeat the exact request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PersistedOp {
    pub operation: OperationId,
    pub base_rev: Option<u64>,
    pub op: LayoutOp,
}

#[derive(Serialize, Deserialize)]
struct WorkspaceFile {
    layout: Option<Layout>,
    pending: Vec<PersistedOp>,
}

#[derive(Serialize, Deserialize)]
struct MirrorFile {
    terminal: TerminalId,
    incarnation: IncarnationId,
    cursor: LogCursor,
    engine_id: String,
    checkpoint: Vec<u8>,
}

/// A restored mirror engine whose state is exactly `cursor`.
pub(crate) struct CachedMirror {
    pub incarnation: IncarnationId,
    pub cursor: LogCursor,
    pub engine: Engine,
}

#[derive(Debug, Clone)]
pub(crate) struct Cache {
    dir: PathBuf,
}

impl Cache {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn workspace_path(&self) -> PathBuf {
        self.dir.join("workspace.bin")
    }

    fn mirrors(&self) -> PathBuf {
        self.dir.join("terminals")
    }

    fn mirror_path(&self, terminal: TerminalId) -> PathBuf {
        self.mirrors().join(format!("{terminal}.bin"))
    }

    pub fn load_workspace(&self) -> Option<(Option<Layout>, Vec<PersistedOp>)> {
        let file: WorkspaceFile = read(&self.workspace_path(), KIND_WORKSPACE)?;
        Some((file.layout, file.pending))
    }

    pub fn save_workspace(
        &self,
        layout: Option<&Layout>,
        pending: &[PersistedOp],
    ) -> std::io::Result<()> {
        #[derive(Serialize)]
        struct Borrowed<'a> {
            layout: Option<&'a Layout>,
            pending: &'a [PersistedOp],
        }
        write(
            &self.workspace_path(),
            KIND_WORKSPACE,
            &Borrowed { layout, pending },
        )
    }

    /// The cached mirror of `terminal`, if present, intact and restorable by
    /// this build's engine.
    pub fn load_mirror(&self, terminal: TerminalId) -> Option<CachedMirror> {
        let file: MirrorFile = read(&self.mirror_path(terminal), KIND_MIRROR)?;
        if file.terminal != terminal || file.engine_id != ENGINE_ID {
            return None;
        }
        let engine = Engine::restore(&file.checkpoint, EffectMode::Mirror).ok()?;
        if engine.offset() != file.cursor.next_byte {
            return None;
        }
        Some(CachedMirror {
            incarnation: file.incarnation,
            cursor: file.cursor,
            engine,
        })
    }

    /// The incarnation of a cached mirror, without restoring its engine.
    pub fn save_mirror(
        &self,
        terminal: TerminalId,
        incarnation: IncarnationId,
        cursor: LogCursor,
        checkpoint: Vec<u8>,
    ) -> std::io::Result<()> {
        write(
            &self.mirror_path(terminal),
            KIND_MIRROR,
            &MirrorFile {
                terminal,
                incarnation,
                cursor,
                engine_id: ENGINE_ID.into(),
                checkpoint,
            },
        )
    }

    pub fn remove_mirror(&self, terminal: TerminalId) {
        let _ = std::fs::remove_file(self.mirror_path(terminal));
    }

    /// Delete mirrors of terminals that are no longer part of the layout.
    pub fn retain_mirrors(&self, keep: impl Fn(TerminalId) -> bool) {
        let Ok(entries) = std::fs::read_dir(self.mirrors()) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(terminal) = name
                .to_str()
                .and_then(|name| name.strip_suffix(".bin"))
                .and_then(|id| id.parse::<TerminalId>().ok())
            else {
                continue;
            };
            if !keep(terminal) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

fn read<T: DeserializeOwned>(path: &Path, kind: u8) -> Option<T> {
    let length = std::fs::metadata(path).ok()?.len();
    if length > MAX_FILE_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let body_len = bytes.len().checked_sub(DIGEST)?;
    let (body, digest) = bytes.split_at(body_len);
    if body.len() < MAGIC.len() + 1
        || &body[..MAGIC.len()] != MAGIC
        || body[MAGIC.len()] != kind
        || Sha256::digest(body).as_slice() != digest
    {
        tracing::warn!("ignoring invalid cache file {}", path.display());
        return None;
    }
    let (value, rest) = postcard::take_from_bytes(&body[MAGIC.len() + 1..]).ok()?;
    rest.is_empty().then_some(value)
}

fn write<T: Serialize>(path: &Path, kind: u8, value: &T) -> std::io::Result<()> {
    let mut bytes = Vec::with_capacity(4096);
    bytes.extend_from_slice(MAGIC);
    bytes.push(kind);
    let mut bytes = postcard::to_extend(value, bytes).map_err(std::io::Error::other)?;
    let digest = Sha256::digest(&bytes);
    bytes.extend_from_slice(&digest);
    write_atomic(path, &bytes)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("cache path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let temporary = path.with_extension(format!("tmp-{}-{unique}", std::process::id()));
    let result = (|| {
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
        return result;
    }
    // Make the rename itself durable where directories can be synced.
    #[cfg(unix)]
    if let Ok(directory) = std::fs::File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_model::layout::LayoutOp;
    use fl_model::{PaneId, Size, TabId, ViewId, WorkspaceId};

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "flc-sync-cache-{}-{}",
            std::process::id(),
            TerminalId::new()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn workspace_and_mirror_round_trip_and_reject_corruption() {
        let dir = scratch();
        let cache = Cache::new(dir.clone());
        assert!(cache.load_workspace().is_none());
        let view = ViewId::new();
        let mut layout = Layout::new(WorkspaceId::new(), view);
        let op = LayoutOp::NewTab {
            view,
            tab: TabId::new(),
            pane: PaneId::new(),
            terminal: TerminalId::new(),
            name: None,
            size: Size::new(80, 24),
        };
        layout.apply(op.clone()).unwrap();
        let pending = vec![PersistedOp {
            operation: OperationId::new(),
            base_rev: Some(1),
            op,
        }];
        cache.save_workspace(Some(&layout), &pending).unwrap();
        assert_eq!(cache.load_workspace(), Some((Some(layout), pending)));

        let terminal = TerminalId::new();
        let incarnation = IncarnationId::new();
        let mut engine = Engine::new(Size::new(20, 4), EffectMode::Mirror).unwrap();
        engine.feed(0, b"cached \xe7\x95").unwrap();
        let cursor = LogCursor {
            next_record: 3,
            next_byte: engine.offset(),
        };
        cache
            .save_mirror(terminal, incarnation, cursor, engine.checkpoint().unwrap())
            .unwrap();
        let cached = cache.load_mirror(terminal).unwrap();
        assert_eq!((cached.incarnation, cached.cursor), (incarnation, cursor));
        assert_eq!(
            cached.engine.checkpoint().unwrap(),
            engine.checkpoint().unwrap()
        );

        // Flip one byte: the digest rejects the file.
        let path = cache.mirror_path(terminal);
        let mut bytes = std::fs::read(&path).unwrap();
        let middle = bytes.len() / 2;
        bytes[middle] ^= 1;
        std::fs::write(&path, bytes).unwrap();
        assert!(cache.load_mirror(terminal).is_none());

        cache.retain_mirrors(|_| false);
        assert!(!path.exists());
        assert!(
            std::fs::read_dir(&dir)
                .unwrap()
                .flatten()
                .all(|e| !e.file_name().to_string_lossy().contains("tmp"))
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
