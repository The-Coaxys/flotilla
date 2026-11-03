//! Flotilla wire protocol: length-prefixed frames, postcard messages, versions.
pub mod bootstrap;
pub mod frame;
pub mod msg;
pub mod version;

pub use bootstrap::{EnrollResponse, VersionInfo};
pub use frame::{
    COMPRESS_THRESHOLD, FLAG_ZSTD, Frame, FrameCodec, FrameError, HEADER_LEN, MAX_FRAME_LEN,
};
pub use msg::{
    ClientMsg, DaemonInfo, Hello, KIND_CLIENT, KIND_SERVER, MsgError, RejectReason, Request,
    Response, RpcError, RpcErrorCode, ServerMsg, Welcome, WireMessage,
};
pub use version::{PRODUCT_VERSION, PROTO, ProtoVersion, build_id};
