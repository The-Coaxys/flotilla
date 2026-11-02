//! Flotilla wire protocol: length-prefixed frames, postcard messages, versions.
pub mod bootstrap;
pub mod frame;
pub mod msg;
pub mod version;

pub use frame::{
    COMPRESS_THRESHOLD, FLAG_ZSTD, Frame, FrameCodec, FrameError, HEADER_LEN, MAX_FRAME_LEN,
};
pub use version::{PRODUCT_VERSION, PROTO, ProtoVersion, build_id};
