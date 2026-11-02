//! QUIC endpoints secured by a pinned self-signed certificate.
//!
//! The daemon generates one certificate on first start. The client learns its
//! SHA-256 over SSH during enrollment and pins it, so no CA is involved.
//!
//! Gotcha: a stream opened with `open_bi()` is invisible to the peer until the
//! opener writes to it. Clients always send `Hello` first, so `accept_ctl`
//! returns promptly.
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Once};
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use sha2::{Digest, Sha256};

pub const ALPN: &[u8] = b"flotilla/1";
pub const SERVER_NAME: &str = "flotilla";
const KEEP_ALIVE: Duration = Duration::from_secs(2);
const MAX_IDLE: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum QuicError {
    #[error("certificate generation failed: {0}")]
    Cert(#[from] rcgen::Error),
    #[error("TLS configuration error: {0}")]
    Tls(#[from] rustls::Error),
    #[error("QUIC configuration error: {0}")]
    Config(String),
    #[error("invalid certificate fingerprint (expected 64 hex characters)")]
    BadFingerprint,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("cannot start QUIC connection: {0}")]
    Connect(#[from] quinn::ConnectError),
    #[error("QUIC connection failed: {0}")]
    Connection(#[from] quinn::ConnectionError),
    #[error("QUIC handshake timed out")]
    Timeout,
}

/// Install ring as the process-wide rustls provider. Idempotent.
pub fn install_crypto_provider() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // Err means a provider is already installed, which is fine.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// The daemon's certificate and PKCS#8 private key, both DER.
#[derive(Clone)]
pub struct ServerIdentity {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
}

impl std::fmt::Debug for ServerIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerIdentity")
            .field("fingerprint", &fingerprint_hex(&self.fingerprint()))
            .finish_non_exhaustive()
    }
}

impl ServerIdentity {
    pub fn generate() -> Result<Self, QuicError> {
        let ck = rcgen::generate_simple_self_signed(vec![SERVER_NAME.to_string()])?;
        Ok(Self {
            cert_der: ck.cert.der().to_vec(),
            key_der: ck.signing_key.serialize_der(),
        })
    }

    pub fn fingerprint(&self) -> [u8; 32] {
        sha256(&self.cert_der)
    }
}

pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

pub fn fingerprint_hex(fp: &[u8; 32]) -> String {
    hex::encode(fp)
}

pub fn parse_fingerprint(s: &str) -> Result<[u8; 32], QuicError> {
    let bytes = hex::decode(s.trim()).map_err(|_| QuicError::BadFingerprint)?;
    bytes.try_into().map_err(|_| QuicError::BadFingerprint)
}

fn transport_config() -> quinn::TransportConfig {
    let mut t = quinn::TransportConfig::default();
    t.keep_alive_interval(Some(KEEP_ALIVE));
    t.max_idle_timeout(Some(
        MAX_IDLE.try_into().expect("30 s fits in a QUIC VarInt"),
    ));
    t
}

pub fn server_endpoint(
    identity: &ServerIdentity,
    bind: SocketAddr,
) -> Result<quinn::Endpoint, QuicError> {
    install_crypto_provider();
    let cert = CertificateDer::from(identity.cert_der.clone());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(identity.key_der.clone()));
    let mut tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls)
        .map_err(|e| QuicError::Config(e.to_string()))?;
    let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    cfg.transport_config(Arc::new(transport_config()));
    Ok(quinn::Endpoint::server(cfg, bind)?)
}

/// Connect to `addr`, accepting only a server certificate whose SHA-256 is
/// `fingerprint`. Fails with `Timeout` if the handshake takes longer than
/// `timeout` (for example, when UDP is silently dropped).
pub async fn connect(
    addr: SocketAddr,
    fingerprint: [u8; 32],
    timeout: Duration,
) -> Result<(quinn::Endpoint, quinn::Connection), QuicError> {
    install_crypto_provider();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(PinnedCert {
        fingerprint,
        provider,
    });
    let mut tls = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls)
        .map_err(|e| QuicError::Config(e.to_string()))?;
    let mut cfg = quinn::ClientConfig::new(Arc::new(crypto));
    cfg.transport_config(Arc::new(transport_config()));
    let bind: SocketAddr = if addr.is_ipv6() {
        (Ipv6Addr::UNSPECIFIED, 0).into()
    } else {
        (Ipv4Addr::UNSPECIFIED, 0).into()
    };
    let mut endpoint = quinn::Endpoint::client(bind)?;
    endpoint.set_default_client_config(cfg);
    let connecting = endpoint.connect(addr, SERVER_NAME)?;
    match tokio::time::timeout(timeout, connecting).await {
        Ok(Ok(conn)) => Ok((endpoint, conn)),
        Ok(Err(e)) => Err(e.into()),
        Err(_) => {
            endpoint.close(0u32.into(), b"timeout");
            Err(QuicError::Timeout)
        }
    }
}

/// Open the control stream (client side).
pub async fn open_ctl(
    conn: &quinn::Connection,
) -> Result<(quinn::SendStream, quinn::RecvStream), QuicError> {
    Ok(conn.open_bi().await?)
}

/// Accept the control stream (server side).
pub async fn accept_ctl(
    conn: &quinn::Connection,
) -> Result<(quinn::SendStream, quinn::RecvStream), QuicError> {
    Ok(conn.accept_bi().await?)
}

#[derive(Debug)]
struct PinnedCert {
    fingerprint: [u8; 32],
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for PinnedCert {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if sha256(end_entity.as_ref()) == self.fingerprint {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "certificate fingerprint mismatch".into(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Echo server: every accepted bidirectional stream is echoed back.
    async fn echo_server() -> (quinn::Endpoint, SocketAddr, [u8; 32]) {
        let identity = ServerIdentity::generate().unwrap();
        let endpoint = server_endpoint(&identity, "127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = endpoint.local_addr().unwrap();
        let server = endpoint.clone();
        tokio::spawn(async move {
            while let Some(incoming) = server.accept().await {
                tokio::spawn(async move {
                    let Ok(conn) = incoming.await else { return };
                    while let Ok((mut send, mut recv)) = accept_ctl(&conn).await {
                        let data = recv.read_to_end(1 << 20).await.unwrap_or_default();
                        let _ = send.write_all(&data).await;
                        let _ = send.finish();
                    }
                });
            }
        });
        (endpoint, addr, identity.fingerprint())
    }

    #[tokio::test]
    async fn pinned_fingerprint_connects_and_echoes() {
        let (_server, addr, fp) = echo_server().await;
        let (_ep, conn) = connect(addr, fp, Duration::from_secs(5)).await.unwrap();
        let (mut send, mut recv) = open_ctl(&conn).await.unwrap();
        send.write_all(b"ping").await.unwrap();
        send.finish().unwrap();
        assert_eq!(recv.read_to_end(1024).await.unwrap(), b"ping");
        assert!(conn.rtt() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn wrong_fingerprint_is_rejected() {
        let (_server, addr, _fp) = echo_server().await;
        let err = connect(addr, [0u8; 32], Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(matches!(err, QuicError::Connection(_)), "got {err:?}");
    }

    /// Review Focus #2: a bound socket that never answers behaves like a
    /// firewall silently dropping UDP. We must give up at the deadline.
    #[tokio::test]
    async fn connect_times_out_against_a_black_hole() {
        let hole = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = hole.local_addr().unwrap();
        let started = std::time::Instant::now();
        let err = connect(addr, [0u8; 32], Duration::from_millis(300))
            .await
            .unwrap_err();
        assert!(matches!(err, QuicError::Timeout), "got {err:?}");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn fingerprint_hex_roundtrip_and_validation() {
        let fp = sha256(b"cert");
        let text = fingerprint_hex(&fp);
        assert_eq!(text.len(), 64);
        assert_eq!(parse_fingerprint(&text).unwrap(), fp);
        assert_eq!(parse_fingerprint(&format!("  {text}\n")).unwrap(), fp);
        assert!(parse_fingerprint("abc").is_err());
        assert!(parse_fingerprint(&"zz".repeat(32)).is_err());
        assert!(parse_fingerprint(&"00".repeat(31)).is_err());
    }

    #[test]
    fn identity_fingerprint_is_sha256_of_der_and_debug_hides_key() {
        let id = ServerIdentity::generate().unwrap();
        assert_eq!(id.fingerprint(), sha256(&id.cert_der));
        let dbg = format!("{id:?}");
        assert!(dbg.contains(&fingerprint_hex(&id.fingerprint())));
        assert!(!dbg.contains("key_der"));
    }
}
