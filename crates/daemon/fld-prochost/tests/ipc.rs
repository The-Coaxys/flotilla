#![cfg(target_os = "linux")]
use fl_prochost_proto::{Request, Response};
use fld_prochost::{Client, client::ClientError};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_shutdown_flushes_its_ack_before_closing_connections() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().to_path_buf();
    let server = tokio::spawn(fld_prochost::serve(path));
    let client = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match Client::connect(root.path()).await {
                Ok(client) => break client,
                Err(ClientError::Io(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                Err(error) => panic!("process-host startup failed: {error}"),
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        client.request(&Request::ShutdownIfIdle).await.unwrap(),
        Response::Done
    );
    tokio::time::timeout(Duration::from_secs(3), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    client.shutdown().await;
}
