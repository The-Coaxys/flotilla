//! Typed message streams over any byte stream (Unix socket, SSH channel,
//! QUIC stream, stdio). One `FrameReader`/`FrameWriter` pair per connection.
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{FramedRead, FramedWrite};

use fl_proto::{FrameCodec, FrameError, MsgError, WireMessage};

pub type BoxRead = Box<dyn AsyncRead + Send + Unpin>;
pub type BoxWrite = Box<dyn AsyncWrite + Send + Unpin>;

#[derive(Debug, thiserror::Error)]
pub enum ConnError {
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error(transparent)]
    Msg(#[from] MsgError),
    #[error("connection closed")]
    Closed,
}

pub struct FrameReader {
    inner: FramedRead<BoxRead, FrameCodec>,
}

pub struct FrameWriter {
    inner: FramedWrite<BoxWrite, FrameCodec>,
}

pub fn framed(
    read: impl AsyncRead + Send + Unpin + 'static,
    write: impl AsyncWrite + Send + Unpin + 'static,
) -> (FrameReader, FrameWriter) {
    (
        FrameReader {
            inner: FramedRead::new(Box::new(read), FrameCodec),
        },
        FrameWriter {
            inner: FramedWrite::new(Box::new(write), FrameCodec),
        },
    )
}

impl FrameReader {
    /// Next message. Cancel-safe: dropping the future loses no data.
    pub async fn recv<M: WireMessage>(&mut self) -> Result<M, ConnError> {
        match self.inner.next().await {
            Some(Ok(frame)) => Ok(M::from_frame(&frame)?),
            Some(Err(e)) => Err(e.into()),
            None => Err(ConnError::Closed),
        }
    }
}

impl FrameWriter {
    /// Encode, write and flush one message.
    pub async fn send<M: WireMessage>(&mut self, msg: &M) -> Result<(), ConnError> {
        self.inner.send(msg.to_frame()?).await?;
        Ok(())
    }

    /// Flush and shut down the write side (the peer then reads EOF).
    pub async fn shutdown(&mut self) -> Result<(), ConnError> {
        self.inner.close().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_proto::{ClientMsg, ServerMsg};

    fn pipe() -> ((FrameReader, FrameWriter), (FrameReader, FrameWriter)) {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let (ar, aw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);
        (framed(ar, aw), framed(br, bw))
    }

    #[tokio::test]
    async fn messages_flow_both_ways() {
        let ((mut ar, mut aw), (mut br, mut bw)) = pipe();
        aw.send(&ClientMsg::Ping { nonce: 1 }).await.unwrap();
        assert_eq!(
            br.recv::<ClientMsg>().await.unwrap(),
            ClientMsg::Ping { nonce: 1 }
        );
        bw.send(&ServerMsg::Pong { nonce: 1 }).await.unwrap();
        assert_eq!(
            ar.recv::<ServerMsg>().await.unwrap(),
            ServerMsg::Pong { nonce: 1 }
        );
    }

    /// A payload above the compression threshold, incompressible and far
    /// larger than the duplex buffer, must arrive intact.
    #[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    struct Blob {
        data: Vec<u8>,
    }

    impl WireMessage for Blob {
        const KIND: u16 = 0x7f01;
    }

    /// Deterministic bytes that zstd cannot shrink (xorshift64).
    fn noise(n: usize) -> Vec<u8> {
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    #[tokio::test]
    async fn large_message_survives_compression() {
        let ((_ar, mut aw), (mut br, _bw)) = pipe();
        let big = Blob {
            data: noise(200_000),
        };
        let send = tokio::spawn(async move { aw.send(&big).await.map(|_| big) });
        let got = tokio::time::timeout(std::time::Duration::from_secs(5), br.recv::<Blob>())
            .await
            .expect("frame did not arrive")
            .unwrap();
        assert_eq!(got, send.await.unwrap().unwrap());
    }

    /// Payload validation happens before a single byte is written, so a peer
    /// that rejects a message cannot leave the other side waiting for a frame.
    #[tokio::test]
    async fn invalid_payload_is_rejected_before_any_bytes_are_written() {
        let ((_ar, mut aw), (mut br, _bw)) = pipe();
        let too_long = ClientMsg::Hello(fl_proto::Hello {
            client_version: "x".repeat(fl_proto::validation::MAX_TEXT_BYTES + 1),
            proto: fl_proto::PROTO,
            device_id: None,
            token: None,
            client_session_id: [0; 16],
        });
        assert!(matches!(aw.send(&too_long).await, Err(ConnError::Msg(_))));
        aw.shutdown().await.unwrap();
        assert!(matches!(
            br.recv::<ClientMsg>().await,
            Err(ConnError::Closed)
        ));
    }

    #[tokio::test]
    async fn peer_shutdown_reads_as_closed() {
        let ((_ar, mut aw), (mut br, _bw)) = pipe();
        aw.shutdown().await.unwrap();
        assert!(matches!(
            br.recv::<ClientMsg>().await,
            Err(ConnError::Closed)
        ));
    }

    #[tokio::test]
    async fn wrong_direction_message_is_a_msg_error() {
        let ((_ar, mut aw), (mut br, _bw)) = pipe();
        aw.send(&ServerMsg::Pong { nonce: 9 }).await.unwrap();
        assert!(matches!(
            br.recv::<ClientMsg>().await,
            Err(ConnError::Msg(_))
        ));
    }
}
