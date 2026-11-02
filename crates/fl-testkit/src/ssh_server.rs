//! In-process SSH server for tests. Runs `exec` commands through `sh -c` with
//! `HOME` set to a test directory, and bridges `direct-streamlocal` channels to
//! Unix sockets, as an OpenSSH server would.
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::keys::{Algorithm, HashAlg, PrivateKey, PublicKey};
use russh::server::{
    Auth, ChannelOpenHandle, Handle, Msg, Server as _, Session, VerifiedPublicKey,
};
use russh::{Channel, ChannelId, ChannelOpenFailure};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UnixStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

#[derive(Clone, Default)]
pub struct TestSshOptions {
    /// When set, only this public key may authenticate.
    pub allowed_key: Option<PublicKey>,
    /// Extra environment variables for `exec` commands.
    pub env: Vec<(String, String)>,
}

pub struct TestSshServer {
    pub addr: SocketAddr,
    /// `SHA256:...` fingerprint of the server host key.
    pub host_key_fingerprint: String,
    pub home: PathBuf,
    sessions: Arc<Mutex<Vec<Handle>>>,
    accept_task: JoinHandle<()>,
}

impl TestSshServer {
    pub async fn start(home: impl Into<PathBuf>) -> std::io::Result<Self> {
        Self::start_with(home, TestSshOptions::default()).await
    }

    pub async fn start_with(
        home: impl Into<PathBuf>,
        opts: TestSshOptions,
    ) -> std::io::Result<Self> {
        let home = home.into();
        let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)
            .map_err(std::io::Error::other)?;
        let host_key_fingerprint = host_key
            .public_key()
            .fingerprint(HashAlg::Sha256)
            .to_string();
        let config = Arc::new(russh::server::Config {
            keys: vec![host_key],
            auth_rejection_time: Duration::from_millis(10),
            auth_rejection_time_initial: Some(Duration::ZERO),
            inactivity_timeout: None,
            ..Default::default()
        });
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let sessions = Arc::new(Mutex::new(Vec::new()));
        let mut factory = ServerFactory {
            home: home.clone(),
            opts: Arc::new(opts),
            sessions: sessions.clone(),
        };
        let accept_task = tokio::spawn(async move {
            let _ = factory.run_on_socket(config, &listener).await;
        });
        Ok(Self {
            addr,
            host_key_fingerprint,
            home,
            sessions,
            accept_task,
        })
    }

    /// Forcibly disconnect every live SSH session (simulates a dead SSH path).
    pub async fn kill_sessions(&self) {
        let handles: Vec<Handle> = std::mem::take(&mut *self.sessions.lock().unwrap());
        for h in handles {
            let _ = h
                .disconnect(
                    russh::Disconnect::ByApplication,
                    "killed by test".into(),
                    "en".into(),
                )
                .await;
        }
    }
}

impl Drop for TestSshServer {
    fn drop(&mut self) {
        self.accept_task.abort();
    }
}

#[derive(Clone)]
struct ServerFactory {
    home: PathBuf,
    opts: Arc<TestSshOptions>,
    sessions: Arc<Mutex<Vec<Handle>>>,
}

impl russh::server::Server for ServerFactory {
    type Handler = SessionHandler;

    fn new_client(&mut self, _peer: Option<SocketAddr>) -> SessionHandler {
        SessionHandler {
            home: self.home.clone(),
            opts: self.opts.clone(),
            sessions: self.sessions.clone(),
            stdin: HashMap::new(),
            registered: false,
        }
    }
}

struct SessionHandler {
    home: PathBuf,
    opts: Arc<TestSshOptions>,
    sessions: Arc<Mutex<Vec<Handle>>>,
    /// Per-exec-channel stdin feeders. Dropping the sender closes the child's stdin.
    stdin: HashMap<ChannelId, mpsc::UnboundedSender<Vec<u8>>>,
    registered: bool,
}

impl SessionHandler {
    fn register(&mut self, session: &Session) {
        if !self.registered {
            self.sessions.lock().unwrap().push(session.handle());
            self.registered = true;
        }
    }
}

impl russh::server::Handler for SessionHandler {
    type Error = russh::Error;

    async fn auth_publickey(
        &mut self,
        _user: &str,
        key: VerifiedPublicKey<'_>,
    ) -> Result<Auth, Self::Error> {
        let ok = match &self.opts.allowed_key {
            None => true,
            Some(allowed) => allowed.key_data() == key.public_key.key_data(),
        };
        Ok(if ok { Auth::Accept } else { Auth::reject() })
    }

    async fn channel_open_session(
        &mut self,
        _channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.register(session);
        reply.accept().await;
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let command = String::from_utf8_lossy(data).into_owned();
        let (tx, rx) = mpsc::unbounded_channel();
        self.stdin.insert(channel, tx);
        session.channel_success(channel)?;
        tokio::spawn(run_exec(
            session.handle(),
            channel,
            command,
            self.home.clone(),
            self.opts.env.clone(),
            rx,
        ));
        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if let Some(tx) = self.stdin.get(&channel) {
            let _ = tx.send(data.to_vec());
        }
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.stdin.remove(&channel);
        Ok(())
    }

    async fn channel_close(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.stdin.remove(&channel);
        Ok(())
    }

    async fn channel_open_direct_streamlocal(
        &mut self,
        channel: Channel<Msg>,
        socket_path: &str,
        reply: ChannelOpenHandle,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.register(session);
        match UnixStream::connect(socket_path).await {
            Ok(mut unix) => {
                reply.accept().await;
                tokio::spawn(async move {
                    let mut stream = channel.into_stream();
                    let _ = tokio::io::copy_bidirectional(&mut stream, &mut unix).await;
                });
            }
            Err(_) => reply.reject(ChannelOpenFailure::ConnectFailed).await,
        }
        Ok(())
    }
}

async fn run_exec(
    handle: Handle,
    channel: ChannelId,
    command: String,
    home: PathBuf,
    env: Vec<(String, String)>,
    mut stdin_rx: mpsc::UnboundedReceiver<Vec<u8>>,
) {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(&command)
        .current_dir(&home)
        .env("HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (k, v) in &env {
        cmd.env(k, v);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = handle
                .extended_data(channel, 1, format!("spawn failed: {e}\n").into_bytes())
                .await;
            let _ = handle.exit_status_request(channel, 127).await;
            let _ = handle.eof(channel).await;
            let _ = handle.close(channel).await;
            return;
        }
    };
    let mut child_stdin = child.stdin.take().expect("stdin is piped");
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");

    let stdin_task = tokio::spawn(async move {
        while let Some(chunk) = stdin_rx.recv().await {
            if child_stdin.write_all(&chunk).await.is_err() {
                break;
            }
        }
        // Sender dropped (channel EOF): dropping child_stdin closes the pipe.
    });
    let out_handle = handle.clone();
    let out_task = tokio::spawn(async move {
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            match stdout.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if out_handle.data(channel, buf[..n].to_vec()).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
    let err_handle = handle.clone();
    let err_task = tokio::spawn(async move {
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            match stderr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if err_handle
                        .extended_data(channel, 1, buf[..n].to_vec())
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });

    let status = child.wait().await;
    let _ = out_task.await;
    let _ = err_task.await;
    stdin_task.abort();
    let code = status.ok().and_then(|s| s.code()).unwrap_or(255) as u32;
    let _ = handle.exit_status_request(channel, code).await;
    let _ = handle.eof(channel).await;
    let _ = handle.close(channel).await;
}
