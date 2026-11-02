//! UDP relay between clients and one upstream server. `set_dropping(true)`
//! silently discards every datagram in both directions, which is how a firewall
//! that black-holes QUIC looks to both peers.
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::task::{JoinHandle, JoinSet};

pub struct UdpRelay {
    pub addr: SocketAddr,
    dropping: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

impl UdpRelay {
    pub async fn start(upstream: SocketAddr) -> std::io::Result<Self> {
        let front = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let addr = front.local_addr()?;
        let dropping = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(run(front, upstream, dropping.clone()));
        Ok(Self {
            addr,
            dropping,
            task,
        })
    }

    pub fn set_dropping(&self, drop_all: bool) {
        self.dropping.store(drop_all, Ordering::SeqCst);
    }
}

impl Drop for UdpRelay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn run(front: Arc<UdpSocket>, upstream: SocketAddr, dropping: Arc<AtomicBool>) {
    // One upstream-facing socket per client address, so replies can be routed back.
    let mut backs: HashMap<SocketAddr, Arc<UdpSocket>> = HashMap::new();
    // Dropping the JoinSet (when this task is aborted) aborts the reply pumps.
    let mut pumps = JoinSet::new();
    let mut buf = vec![0u8; 65_535];
    loop {
        let Ok((n, client)) = front.recv_from(&mut buf).await else {
            break;
        };
        if dropping.load(Ordering::SeqCst) {
            continue;
        }
        let back = match backs.get(&client) {
            Some(b) => b.clone(),
            None => {
                let Ok(b) = UdpSocket::bind("127.0.0.1:0").await else {
                    continue;
                };
                if b.connect(upstream).await.is_err() {
                    continue;
                }
                let b = Arc::new(b);
                let (b2, f2, d2) = (b.clone(), front.clone(), dropping.clone());
                pumps.spawn(async move {
                    let mut buf = vec![0u8; 65_535];
                    loop {
                        match b2.recv(&mut buf).await {
                            Ok(n) => {
                                if !d2.load(Ordering::SeqCst) {
                                    let _ = f2.send_to(&buf[..n], client).await;
                                }
                            }
                            // e.g. ICMP port unreachable on a connected socket
                            Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
                        }
                    }
                });
                backs.insert(client, b.clone());
                b
            }
        };
        let _ = back.send(&buf[..n]).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn echo_upstream() -> SocketAddr {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = sock.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            while let Ok((n, from)) = sock.recv_from(&mut buf).await {
                let _ = sock.send_to(&buf[..n], from).await;
            }
        });
        addr
    }

    async fn roundtrip(client: &UdpSocket, relay: SocketAddr, msg: &[u8]) -> Option<Vec<u8>> {
        client.send_to(msg, relay).await.unwrap();
        let mut buf = [0u8; 1500];
        match tokio::time::timeout(Duration::from_millis(300), client.recv_from(&mut buf)).await {
            Ok(Ok((n, _))) => Some(buf[..n].to_vec()),
            _ => None,
        }
    }

    #[tokio::test]
    async fn relays_until_dropping_then_recovers() {
        let relay = UdpRelay::start(echo_upstream().await).await.unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        assert_eq!(
            roundtrip(&client, relay.addr, b"one").await.as_deref(),
            Some(&b"one"[..])
        );
        relay.set_dropping(true);
        assert_eq!(roundtrip(&client, relay.addr, b"two").await, None);
        relay.set_dropping(false);
        assert_eq!(
            roundtrip(&client, relay.addr, b"three").await.as_deref(),
            Some(&b"three"[..])
        );
    }
}
