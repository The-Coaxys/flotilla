//! Flotilla wire protocol: length-prefixed frames, postcard messages, versions.
pub mod bootstrap;
pub mod bulk;
pub mod channel;
pub mod frame;
pub mod msg;
pub mod terminal;
pub mod validation;
pub mod version;

pub use bootstrap::{EnrollResponse, VersionInfo};
pub use bulk::{
    BulkChunk, BulkClientMsg, BulkError, BulkServerMsg, KIND_BULK_CLIENT, KIND_BULK_SERVER,
};
pub use channel::{
    CHANNEL_SCHEMA, ChannelGrant, ChannelHello, ChannelReady, ChannelRole, TERMINAL_PROTOCOL,
};
pub use frame::{
    COMPRESS_THRESHOLD, FLAG_ZSTD, Frame, FrameCodec, FrameError, HEADER_LEN, MAX_FRAME_LEN,
};
pub use msg::{
    Capabilities, ClientMsg, DaemonInfo, Hello, KIND_CLIENT, KIND_SERVER, MsgError, RejectReason,
    Request, Response, RpcError, RpcErrorCode, ServerMsg, Welcome, WireMessage,
};
pub use terminal::{
    InputAck, KIND_TERMINAL_CLIENT, KIND_TERMINAL_SERVER, RecordBatch, SnapshotOffer,
    TerminalClientMsg, TerminalError, TerminalServerMsg,
};
pub use validation::ValidationError;
pub use version::{PRODUCT_VERSION, PROTO, ProtoVersion, build_id};
