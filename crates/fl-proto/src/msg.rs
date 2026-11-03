//! Control-stream messages. Each direction has one enum; the frame `kind`
//! says which enum the payload holds. Postcard encodes enum variants by index,
//! so **only append** new variants and fields-with-defaults; never reorder.
use bytes::Bytes;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::{Frame, ProtoVersion};

pub const KIND_CLIENT: u16 = 1;
pub const KIND_SERVER: u16 = 2;

#[derive(Debug, thiserror::Error)]
pub enum MsgError {
    #[error("unexpected frame kind {got} (expected {expected})")]
    WrongKind { expected: u16, got: u16 },
    #[error("message codec error: {0}")]
    Codec(#[from] postcard::Error),
}

/// A message type that travels as one frame of a fixed kind.
pub trait WireMessage: Serialize + DeserializeOwned {
    const KIND: u16;

    fn to_frame(&self) -> Result<Frame, MsgError> {
        Ok(Frame {
            kind: Self::KIND,
            payload: Bytes::from(postcard::to_stdvec(self)?),
        })
    }

    fn from_frame(frame: &Frame) -> Result<Self, MsgError> {
        if frame.kind != Self::KIND {
            return Err(MsgError::WrongKind {
                expected: Self::KIND,
                got: frame.kind,
            });
        }
        Ok(postcard::from_bytes(&frame.payload)?)
    }
}

/// First message from a client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub client_version: String,
    pub proto: ProtoVersion,
    /// Required for QUIC peers; ignored on the local socket (SSH already authenticated).
    pub device_id: Option<String>,
    pub token: Option<String>,
    pub client_session_id: [u8; 16],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    pub daemon_version: String,
    pub build: String,
    pub proto: ProtoVersion,
    pub hostname: String,
    pub server_session_id: [u8; 16],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMsg {
    Hello(Hello),
    Ping { nonce: u64 },
    Rpc { id: u64, req: Request },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMsg {
    Welcome(Welcome),
    Rejected(RejectReason),
    Pong {
        nonce: u64,
    },
    RpcResult {
        id: u64,
        result: Result<Response, RpcError>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RejectReason {
    BadToken,
    ProtoMismatch { server: ProtoVersion },
    HelloTimeout,
    ExpectedHello,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    DaemonInfo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    DaemonInfo(DaemonInfo),
}

/// Also printed as JSON by `flotillad status --json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub version: String,
    pub build: String,
    pub hostname: String,
    pub pid: u32,
    pub uptime_secs: u64,
    pub quic_port: Option<u16>,
    pub socket_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: RpcErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RpcErrorCode {
    NotFound,
    InvalidArgument,
    Unsupported,
    Internal,
}

impl WireMessage for ClientMsg {
    const KIND: u16 = KIND_CLIENT;
}

impl WireMessage for ServerMsg {
    const KIND: u16 = KIND_SERVER;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PROTO;

    fn hello() -> Hello {
        Hello {
            client_version: "flotilla/0.1.0".into(),
            proto: PROTO,
            device_id: Some("dev".into()),
            token: Some("tok".into()),
            client_session_id: [9; 16],
        }
    }

    fn info() -> DaemonInfo {
        DaemonInfo {
            version: "0.1.0".into(),
            build: "dev".into(),
            hostname: "vps".into(),
            pid: 42,
            uptime_secs: 7,
            quic_port: Some(7443),
            socket_path: "/root/.flotilla/run/daemon.sock".into(),
        }
    }

    #[test]
    fn every_client_message_roundtrips() {
        for msg in [
            ClientMsg::Hello(hello()),
            ClientMsg::Ping { nonce: u64::MAX },
            ClientMsg::Rpc {
                id: 3,
                req: Request::DaemonInfo,
            },
        ] {
            let frame = msg.to_frame().unwrap();
            assert_eq!(frame.kind, KIND_CLIENT);
            assert_eq!(ClientMsg::from_frame(&frame).unwrap(), msg);
        }
    }

    #[test]
    fn every_server_message_roundtrips() {
        for msg in [
            ServerMsg::Welcome(Welcome {
                daemon_version: "0.1.0".into(),
                build: "dev".into(),
                proto: PROTO,
                hostname: "vps".into(),
                server_session_id: [1; 16],
            }),
            ServerMsg::Rejected(RejectReason::BadToken),
            ServerMsg::Rejected(RejectReason::ProtoMismatch { server: PROTO }),
            ServerMsg::Rejected(RejectReason::HelloTimeout),
            ServerMsg::Rejected(RejectReason::ExpectedHello),
            ServerMsg::Pong { nonce: 5 },
            ServerMsg::RpcResult {
                id: 1,
                result: Ok(Response::DaemonInfo(info())),
            },
            ServerMsg::RpcResult {
                id: 2,
                result: Err(RpcError {
                    code: RpcErrorCode::Unsupported,
                    message: "no".into(),
                }),
            },
        ] {
            let frame = msg.to_frame().unwrap();
            assert_eq!(frame.kind, KIND_SERVER);
            assert_eq!(ServerMsg::from_frame(&frame).unwrap(), msg);
        }
    }

    #[test]
    fn wrong_kind_is_reported() {
        let frame = ServerMsg::Pong { nonce: 1 }.to_frame().unwrap();
        assert!(matches!(
            ClientMsg::from_frame(&frame),
            Err(MsgError::WrongKind {
                expected: KIND_CLIENT,
                got: KIND_SERVER
            })
        ));
    }

    #[test]
    fn garbage_payload_is_a_codec_error() {
        let frame = Frame {
            kind: KIND_CLIENT,
            payload: Bytes::from_static(&[0xFF, 0xFF, 0xFF]),
        };
        assert!(matches!(
            ClientMsg::from_frame(&frame),
            Err(MsgError::Codec(_))
        ));
    }

    /// Pins the wire encoding: variant index 1 (`Ping`), then varint 7.
    #[test]
    fn ping_wire_bytes_are_stable() {
        assert_eq!(
            postcard::to_stdvec(&ClientMsg::Ping { nonce: 7 }).unwrap(),
            vec![1, 7]
        );
    }

    #[test]
    fn daemon_info_json_field_names_are_stable() {
        let json = serde_json::to_value(info()).unwrap();
        for key in [
            "version",
            "build",
            "hostname",
            "pid",
            "uptime_secs",
            "quic_port",
            "socket_path",
        ] {
            assert!(json.get(key).is_some(), "missing {key}");
        }
    }
}
