//! Debounced change signals for one worktree and its git directories.
//!
//! The worktree is watched directory by directory (non-recursively, so heavy
//! ignored directories such as `node_modules` cost nothing), the worktree's
//! git directory and the common git directory non-recursively (HEAD, index,
//! packed-refs) and `<common>/refs` recursively (branch updates). Signals
//! are debounced and coalesced; a periodic poll signal covers anything the
//! OS watcher misses.
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use notify::event::{
    AccessKind, AccessMode, CreateKind, EventKind, MetadataKind, ModifyKind, RemoveKind, RenameMode,
};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher as _};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::Result;

/// Directory names never watched (nor walked when copying includes).
pub const IGNORED_DIRS: &[&str] = &["node_modules", "target", ".venv", "dist", "build", ".next"];

/// Most directories one watcher registers; beyond it, polling covers the rest.
pub const MAX_WATCHED_DIRS: usize = 10_000;

/// Raw OS events buffered between the notify thread and the debounce task.
const EVENT_QUEUE: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchConfig {
    /// Quiet period after the last event before signalling.
    pub debounce: Duration,
    /// Longest delay between the first event of a burst and its signal.
    pub max_delay: Duration,
    /// Unconditional signal interval (fallback for missed events).
    pub poll: Duration,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            debounce: Duration::from_millis(150),
            max_delay: Duration::from_secs(1),
            poll: Duration::from_secs(10),
        }
    }
}

/// Watches one worktree. Dropping it stops every watch and the background
/// task.
pub struct Watcher {
    os: Arc<Mutex<Option<RecommendedWatcher>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.task.abort();
        // Dropping the OS watcher here, not when the aborted task is next
        // polled, stops event delivery immediately.
        drop(lock(&self.os).take());
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl std::fmt::Debug for Watcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watcher").finish_non_exhaustive()
    }
}

/// Paths, filters and the set of registered worktree directories.
struct Registry {
    worktree: PathBuf,
    git_dir: PathBuf,
    common_dir: PathBuf,
    objects_dir: PathBuf,
    dirs: HashSet<PathBuf>,
    cap_warned: bool,
    /// Registration failed for a reason more directories will not fix
    /// (e.g. the inotify watch limit); stop trying.
    exhausted: bool,
}

impl Registry {
    /// Whether an event at `path` should trigger a signal.
    fn relevant(&self, path: &Path) -> bool {
        if path.starts_with(&self.objects_dir) {
            return false;
        }
        if path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().ends_with(".lock"))
        {
            return false;
        }
        if path.starts_with(&self.git_dir) || path.starts_with(&self.common_dir) {
            return true;
        }
        match path.strip_prefix(&self.worktree) {
            Ok(relative) => !excluded(relative),
            Err(_) => false,
        }
    }

    /// Whether `dir` is a worktree directory that should be watched.
    fn watchable(&self, dir: &Path) -> bool {
        dir.strip_prefix(&self.worktree)
            .is_ok_and(|relative| !excluded(relative))
            && !dir.starts_with(&self.git_dir)
            && !dir.starts_with(&self.common_dir)
    }

    /// Registers `root` and every watchable directory below it.
    fn register_tree(&mut self, os: &mut RecommendedWatcher, root: &Path) {
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            if self.exhausted || !self.watchable(&dir) {
                continue;
            }
            if !self.dirs.contains(&dir) {
                if self.dirs.len() >= MAX_WATCHED_DIRS {
                    if !self.cap_warned {
                        self.cap_warned = true;
                        tracing::warn!(
                            worktree = %self.worktree.display(),
                            limit = MAX_WATCHED_DIRS,
                            "too many directories to watch; relying on polling for the rest"
                        );
                    }
                    return;
                }
                match os.watch(&dir, RecursiveMode::NonRecursive) {
                    Ok(()) => {
                        self.dirs.insert(dir.clone());
                    }
                    Err(e) => {
                        if matches!(e.kind, notify::ErrorKind::MaxFilesWatch) {
                            self.exhausted = true;
                            tracing::warn!(
                                worktree = %self.worktree.display(),
                                "OS file watch limit reached; relying on polling"
                            );
                            return;
                        }
                        // Vanished or unreadable: nothing to watch.
                        tracing::debug!(dir = %dir.display(), error = %e, "cannot watch directory");
                        continue;
                    }
                }
            }
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    stack.push(entry.path());
                }
            }
        }
    }

    /// Forgets `dir` and everything registered below it.
    fn forget_tree(&mut self, os: &mut RecommendedWatcher, dir: &Path) {
        let gone: Vec<PathBuf> = self
            .dirs
            .iter()
            .filter(|d| d.starts_with(dir))
            .cloned()
            .collect();
        for d in gone {
            // The kernel drops watches of deleted directories by itself;
            // this keeps notify's bookkeeping in step and covers moves.
            let _ = os.unwatch(&d);
            self.dirs.remove(&d);
        }
    }

    /// Updates registrations for directories created, moved or removed by
    /// `event`.
    fn track(&mut self, os: &mut RecommendedWatcher, event: &Event) {
        match event.kind {
            EventKind::Create(CreateKind::Folder | CreateKind::Any)
            | EventKind::Modify(ModifyKind::Name(RenameMode::To | RenameMode::Any)) => {
                for path in &event.paths {
                    if path.symlink_metadata().is_ok_and(|m| m.is_dir()) {
                        self.register_tree(os, path);
                    } else {
                        self.forget_tree(os, path);
                    }
                }
            }
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
                if let [from, to] = event.paths.as_slice() {
                    self.forget_tree(os, from);
                    if to.symlink_metadata().is_ok_and(|m| m.is_dir()) {
                        self.register_tree(os, to);
                    }
                }
            }
            EventKind::Remove(RemoveKind::Folder | RemoveKind::Any)
            | EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
                for path in &event.paths {
                    self.forget_tree(os, path);
                }
            }
            _ => {}
        }
    }
}

/// A relative worktree path inside an ignored directory or a nested `.git`.
fn excluded(relative: &Path) -> bool {
    relative.components().any(|c| match c {
        Component::Normal(name) => {
            name == ".git" || IGNORED_DIRS.iter().any(|ignored| name == *ignored)
        }
        _ => false,
    })
}

/// Event kinds that never indicate a change: opens, reads and atime updates
/// (our own `git status` causes plenty of those).
fn is_noise(kind: &EventKind) -> bool {
    match kind {
        EventKind::Access(AccessKind::Close(AccessMode::Write)) => false,
        EventKind::Access(_) => true,
        EventKind::Modify(ModifyKind::Metadata(MetadataKind::AccessTime)) => true,
        _ => false,
    }
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

impl Watcher {
    /// Starts watching `worktree`, its `git_dir` and the repository's
    /// `common_dir`. Must be called inside a tokio runtime; it blocks while
    /// registering the worktree's directories. Signals arrive on the
    /// receiver (capacity 1, coalesced). When the OS watcher cannot start,
    /// a warning is logged and only the periodic poll signals.
    pub fn start(
        worktree: &Path,
        git_dir: &Path,
        common_dir: &Path,
        config: WatchConfig,
    ) -> Result<(Watcher, mpsc::Receiver<()>)> {
        let common_dir = canonical(common_dir);
        let mut registry = Registry {
            worktree: canonical(worktree),
            git_dir: canonical(git_dir),
            objects_dir: common_dir.join("objects"),
            common_dir,
            dirs: HashSet::new(),
            cap_warned: false,
            exhausted: false,
        };
        let (events_tx, events_rx) = mpsc::channel::<Event>(EVENT_QUEUE);
        let overflow = Arc::new(AtomicBool::new(false));
        let handler_overflow = Arc::clone(&overflow);
        let handler = move |result: notify::Result<Event>| match result {
            Ok(event) => {
                if is_noise(&event.kind) {
                    return;
                }
                // Never block notify's thread: a full queue becomes a rescan.
                if events_tx.try_send(event).is_err() {
                    handler_overflow.store(true, Ordering::Release);
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, "file watch error");
                handler_overflow.store(true, Ordering::Release);
            }
        };
        let os = match notify::recommended_watcher(handler) {
            Ok(mut os) => {
                registry.register_tree(&mut os, &registry.worktree.clone());
                let mut extra = vec![
                    (registry.git_dir.clone(), RecursiveMode::NonRecursive),
                    (registry.common_dir.clone(), RecursiveMode::NonRecursive),
                    (registry.common_dir.join("refs"), RecursiveMode::Recursive),
                ];
                extra.dedup_by(|a, b| a.0 == b.0);
                for (path, mode) in extra {
                    if let Err(e) = os.watch(&path, mode) {
                        tracing::warn!(path = %path.display(), error = %e, "cannot watch git directory");
                    }
                }
                Some(os)
            }
            Err(e) => {
                tracing::warn!(
                    worktree = %registry.worktree.display(),
                    error = %e,
                    "file watching unavailable; polling only"
                );
                None
            }
        };
        let os = Arc::new(Mutex::new(os));
        let (signal_tx, signal_rx) = mpsc::channel(1);
        let task = tokio::spawn(run(
            Arc::clone(&os),
            registry,
            events_rx,
            overflow,
            signal_tx,
            config,
        ));
        Ok((Watcher { os, task }, signal_rx))
    }
}

/// The debounce loop: owns the registry, reacts to OS events and emits
/// signals until the receiver is dropped or the watcher is aborted.
async fn run(
    os: Arc<Mutex<Option<RecommendedWatcher>>>,
    mut registry: Registry,
    mut events: mpsc::Receiver<Event>,
    overflow: Arc<AtomicBool>,
    signals: mpsc::Sender<()>,
    config: WatchConfig,
) {
    let mut poll = tokio::time::interval_at(Instant::now() + config.poll, config.poll);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // (first event of the burst, latest event)
    let mut pending: Option<(Instant, Instant)> = None;
    let emit = |signals: &mpsc::Sender<()>| match signals.try_send(()) {
        // A full channel already holds an undelivered signal.
        Ok(()) | Err(mpsc::error::TrySendError::Full(())) => true,
        Err(mpsc::error::TrySendError::Closed(())) => false,
    };
    loop {
        let deadline =
            pending.map(|(first, last)| (last + config.debounce).min(first + config.max_delay));
        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else { return };
                let mut relevant = event.paths.iter().any(|p| registry.relevant(p));
                if let Some(os) = lock(&os).as_mut() {
                    registry.track(os, &event);
                    if event.need_rescan() || overflow.swap(false, Ordering::AcqRel) {
                        let root = registry.worktree.clone();
                        registry.register_tree(os, &root);
                        relevant = true;
                    }
                }
                if relevant {
                    let now = Instant::now();
                    pending = Some(pending.map_or((now, now), |(first, _)| (first, now)));
                }
            }
            () = async {
                match deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            } => {
                pending = None;
                if !emit(&signals) {
                    return;
                }
            }
            _ = poll.tick() => {
                if !emit(&signals) {
                    return;
                }
            }
            () = signals.closed() => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> Registry {
        Registry {
            worktree: "/w".into(),
            git_dir: "/r/.git/worktrees/w".into(),
            common_dir: "/r/.git".into(),
            objects_dir: "/r/.git/objects".into(),
            dirs: HashSet::new(),
            cap_warned: false,
            exhausted: false,
        }
    }

    #[test]
    fn filters_event_paths() {
        let r = registry();
        assert!(r.relevant(Path::new("/w/src/main.rs")));
        assert!(r.relevant(Path::new("/w/.gitignore")));
        assert!(!r.relevant(Path::new("/w/node_modules/x/index.js")));
        assert!(!r.relevant(Path::new("/w/a/target/debug/x")));
        assert!(!r.relevant(Path::new("/w/vendor/.git/HEAD")));
        assert!(!r.relevant(Path::new("/w/src/file.lock")));
        assert!(r.relevant(Path::new("/r/.git/worktrees/w/HEAD")));
        assert!(r.relevant(Path::new("/r/.git/worktrees/w/index")));
        assert!(!r.relevant(Path::new("/r/.git/worktrees/w/index.lock")));
        assert!(r.relevant(Path::new("/r/.git/refs/heads/main")));
        assert!(r.relevant(Path::new("/r/.git/packed-refs")));
        assert!(!r.relevant(Path::new("/r/.git/objects/ab/cdef")));
        assert!(!r.relevant(Path::new("/elsewhere/file")));
    }

    #[test]
    fn primary_checkout_git_dir_is_relevant() {
        let r = Registry {
            worktree: "/w".into(),
            git_dir: "/w/.git".into(),
            common_dir: "/w/.git".into(),
            objects_dir: "/w/.git/objects".into(),
            ..registry()
        };
        assert!(r.relevant(Path::new("/w/.git/index")));
        assert!(!r.relevant(Path::new("/w/.git/objects/pack/x")));
        assert!(!r.watchable(Path::new("/w/.git")));
        assert!(r.watchable(Path::new("/w/src")));
        assert!(!r.watchable(Path::new("/w/build")));
    }

    #[test]
    fn access_events_are_noise() {
        assert!(is_noise(&EventKind::Access(AccessKind::Open(
            AccessMode::Any
        ))));
        assert!(!is_noise(&EventKind::Access(AccessKind::Close(
            AccessMode::Write
        ))));
        assert!(!is_noise(&EventKind::Modify(ModifyKind::Data(
            notify::event::DataChange::Any
        ))));
    }
}
