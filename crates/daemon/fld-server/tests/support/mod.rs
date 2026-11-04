//! A real process host for daemon tests, run in-process on the test's home.
use std::path::Path;
use std::time::Duration;

use fld_prochost::Client;
use fld_prochost::client::ClientError;

/// Start `fld_prochost::serve` for `root` unless one already answers there.
/// The host is independent of every daemon started afterwards.
pub async fn ensure_prochost(root: &Path) -> Client {
    if let Ok(client) = Client::connect(root).await {
        return client;
    }
    std::fs::create_dir_all(root.join("run")).unwrap();
    tokio::spawn(fld_prochost::serve(root.to_path_buf()));
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match Client::connect(root).await {
                Ok(client) => break client,
                Err(ClientError::Io(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                Err(error) => panic!("process-host startup failed: {error}"),
            }
        }
    })
    .await
    .expect("process host starts")
}
