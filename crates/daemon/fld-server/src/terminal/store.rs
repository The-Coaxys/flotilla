//! SQLite and checkpoint-file work runs on one dedicated thread that exclusively
//! owns the store. An advisory lock keeps a restarting daemon from racing the
//! previous instance's final checkpoint/journal acknowledgment.
use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use fld_store::Store;
use tokio::sync::oneshot;

/// How long a new daemon waits for a previous instance to release the store.
const LOCK_WAIT: Duration = Duration::from_secs(15);

type Job = Box<dyn FnOnce(&mut Store) + Send>;

#[derive(Debug, thiserror::Error)]
#[error("terminal state store has stopped")]
pub struct StoreStopped;

#[derive(Clone)]
pub(crate) struct StoreHandle {
    jobs: mpsc::Sender<Job>,
}

impl StoreHandle {
    /// The thread exits, releasing the lock, once every handle is dropped.
    pub(crate) async fn open(root: PathBuf, lock: PathBuf) -> std::io::Result<Self> {
        let (jobs, queue) = mpsc::channel::<Job>();
        let (opened, result) = oneshot::channel();
        std::thread::Builder::new()
            .name("fld-store".into())
            .spawn(move || {
                let _lock = match acquire(&lock) {
                    Ok(lock) => lock,
                    Err(error) => {
                        let _ = opened.send(Err(error));
                        return;
                    }
                };
                let mut store = match Store::open(&root) {
                    Ok(store) => store,
                    Err(error) => {
                        let _ = opened.send(Err(std::io::Error::other(format!(
                            "opening terminal state {}: {error}",
                            root.display()
                        ))));
                        return;
                    }
                };
                if opened.send(Ok(())).is_err() {
                    return;
                }
                while let Ok(job) = queue.recv() {
                    job(&mut store);
                }
            })?;
        result
            .await
            .map_err(|_| std::io::Error::other("terminal state thread stopped while opening"))??;
        Ok(Self { jobs })
    }

    pub(crate) async fn call<T: Send + 'static>(
        &self,
        job: impl FnOnce(&mut Store) -> T + Send + 'static,
    ) -> Result<T, StoreStopped> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Box::new(move |store| {
                let _ = reply.send(job(store));
            }))
            .map_err(|_| StoreStopped)?;
        result.await.map_err(|_| StoreStopped)
    }

    /// Enqueue immediately, preserving order relative to later calls.
    pub(crate) fn submit(&self, job: impl FnOnce(&mut Store) + Send + 'static) {
        let _ = self.jobs.send(Box::new(job));
    }
}

fn acquire(path: &PathBuf) -> std::io::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(std::io::Error::other(format!(
                    "terminal state {} is still owned by another daemon",
                    path.display()
                )));
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error),
        }
    }
}
