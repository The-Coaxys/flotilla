//! Reconnecting process-host access. Every request is safe to retry: reads are
//! cursor-addressed and writes carry stable, deduplicated write IDs.
use std::path::PathBuf;

use fl_prochost_proto::{Error as WireError, ErrorCode, Request, Response};
use fld_prochost::Client;
use fld_prochost::client::ClientError;

#[derive(Debug, Clone, thiserror::Error)]
pub(crate) enum ProchostError {
    #[error("process host: {0}")]
    Remote(WireError),
    #[error("process host unavailable: {0}")]
    Unavailable(String),
}

impl ProchostError {
    pub(crate) fn code(&self) -> Option<ErrorCode> {
        match self {
            Self::Remote(error) => Some(error.code),
            Self::Unavailable(_) => None,
        }
    }
}

pub(crate) struct Prochost {
    root: PathBuf,
    /// `None`: an externally managed host; never launch or stop it.
    executable: Option<PathBuf>,
    client: tokio::sync::Mutex<Option<Client>>,
}

impl Prochost {
    pub(crate) fn new(root: PathBuf, executable: Option<PathBuf>) -> Self {
        Self {
            root,
            executable,
            client: tokio::sync::Mutex::new(None),
        }
    }

    pub(crate) fn managed(&self) -> bool {
        self.executable.is_some()
    }

    pub(crate) async fn client(&self) -> Result<Client, ProchostError> {
        let mut cached = self.client.lock().await;
        if let Some(client) = cached.as_ref().filter(|client| !client.is_closed()) {
            return Ok(client.clone());
        }
        let client = match &self.executable {
            Some(executable) => fld_prochost::ensure_running(&self.root, executable)
                .await
                .map_err(|error| ProchostError::Unavailable(format!("{error:#}")))?,
            None => Client::connect(&self.root)
                .await
                .map_err(|error| ProchostError::Unavailable(error.to_string()))?,
        };
        *cached = Some(client.clone());
        Ok(client)
    }

    pub(crate) async fn request(&self, request: &Request) -> Result<Response, ProchostError> {
        let client = self.client().await?;
        match client.request(request).await {
            Ok(response) => Ok(response),
            Err(ClientError::Remote(error)) => Err(ProchostError::Remote(error)),
            Err(error) => Err(ProchostError::Unavailable(error.to_string())),
        }
    }

    /// Stop an idle host this daemon manages. Live PTYs always keep it running.
    pub(crate) async fn shutdown_if_idle(&self) {
        if !self.managed() {
            return;
        }
        let client = self.client.lock().await.take();
        if let Some(client) = client.filter(|client| !client.is_closed()) {
            match client.request(&Request::ShutdownIfIdle).await {
                Ok(_) => tracing::info!("idle process host stopped"),
                Err(error) => tracing::debug!("process host remains running: {error}"),
            }
            client.shutdown().await;
        }
    }
}
