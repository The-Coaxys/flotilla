//! Length-prefixed frames: `u32 BE len | u8 flags | u16 BE kind | payload`.
//!
//! `len` counts only the (possibly compressed) payload bytes. Payloads larger
//! than [`COMPRESS_THRESHOLD`] are zstd-compressed when that makes them smaller.
//! Decompression is bounded by the content size recorded in the zstd frame, so
//! a malicious peer cannot make us allocate more than [`MAX_FRAME_LEN`].
use bytes::{Buf, BufMut, Bytes, BytesMut};
use tokio_util::codec::{Decoder, Encoder};

pub const HEADER_LEN: usize = 7;
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;
pub const COMPRESS_THRESHOLD: usize = 1024;
pub const FLAG_ZSTD: u8 = 0b0000_0001;
const KNOWN_FLAGS: u8 = FLAG_ZSTD;
const ZSTD_LEVEL: i32 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub kind: u16,
    pub payload: Bytes,
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame too large: {0} bytes")]
    TooLarge(usize),
    #[error("unknown frame flags {0:#04x}")]
    UnknownFlags(u8),
    #[error("cannot decompress frame: {0}")]
    Decompress(String),
}

#[derive(Debug, Default, Clone, Copy)]
pub struct FrameCodec;

impl Encoder<Frame> for FrameCodec {
    type Error = FrameError;

    fn encode(&mut self, frame: Frame, dst: &mut BytesMut) -> Result<(), FrameError> {
        if frame.payload.len() > MAX_FRAME_LEN {
            return Err(FrameError::TooLarge(frame.payload.len()));
        }
        let (flags, body) = if frame.payload.len() > COMPRESS_THRESHOLD {
            let compressed = zstd::bulk::compress(&frame.payload, ZSTD_LEVEL)?;
            if compressed.len() < frame.payload.len() {
                (FLAG_ZSTD, Bytes::from(compressed))
            } else {
                (0, frame.payload)
            }
        } else {
            (0, frame.payload)
        };
        dst.reserve(HEADER_LEN + body.len());
        dst.put_u32(body.len() as u32);
        dst.put_u8(flags);
        dst.put_u16(frame.kind);
        dst.extend_from_slice(&body);
        Ok(())
    }
}

impl Decoder for FrameCodec {
    type Item = Frame;
    type Error = FrameError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Frame>, FrameError> {
        if src.len() < HEADER_LEN {
            return Ok(None);
        }
        let len = u32::from_be_bytes([src[0], src[1], src[2], src[3]]) as usize;
        if len > MAX_FRAME_LEN {
            return Err(FrameError::TooLarge(len));
        }
        let flags = src[4];
        if flags & !KNOWN_FLAGS != 0 {
            return Err(FrameError::UnknownFlags(flags));
        }
        if src.len() < HEADER_LEN + len {
            src.reserve(HEADER_LEN + len - src.len());
            return Ok(None);
        }
        let kind = u16::from_be_bytes([src[5], src[6]]);
        src.advance(HEADER_LEN);
        let body = src.split_to(len).freeze();
        let payload = if flags & FLAG_ZSTD != 0 {
            decompress_bounded(&body)?
        } else {
            body
        };
        Ok(Some(Frame { kind, payload }))
    }
}

fn decompress_bounded(body: &[u8]) -> Result<Bytes, FrameError> {
    let size = zstd::zstd_safe::get_frame_content_size(body)
        .map_err(|_| FrameError::Decompress("invalid zstd frame header".into()))?
        .ok_or_else(|| FrameError::Decompress("zstd frame has no content size".into()))?;
    let size = usize::try_from(size).map_err(|_| FrameError::TooLarge(usize::MAX))?;
    if size > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge(size));
    }
    let out =
        zstd::bulk::decompress(body, size).map_err(|e| FrameError::Decompress(e.to_string()))?;
    if out.len() != size {
        return Err(FrameError::Decompress("content size mismatch".into()));
    }
    Ok(Bytes::from(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(kind: u16, payload: Vec<u8>) -> BytesMut {
        let mut buf = BytesMut::new();
        FrameCodec
            .encode(
                Frame {
                    kind,
                    payload: Bytes::from(payload),
                },
                &mut buf,
            )
            .unwrap();
        buf
    }

    fn header(len: u32, flags: u8, kind: u16) -> BytesMut {
        let mut b = BytesMut::new();
        b.put_u32(len);
        b.put_u8(flags);
        b.put_u16(kind);
        b
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

    #[test]
    fn small_frame_roundtrips_uncompressed() {
        let mut buf = encode(2, b"hello".to_vec());
        assert_eq!(buf.len(), HEADER_LEN + 5);
        assert_eq!(buf[4], 0, "small payloads are never compressed");
        let frame = FrameCodec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(
            frame,
            Frame {
                kind: 2,
                payload: Bytes::from_static(b"hello")
            }
        );
        assert!(buf.is_empty());
    }

    #[test]
    fn exactly_threshold_sized_payload_is_not_compressed() {
        let buf = encode(1, vec![b'a'; COMPRESS_THRESHOLD]);
        assert_eq!(buf[4], 0);
    }

    #[test]
    fn large_compressible_frame_is_compressed_and_roundtrips() {
        let payload = vec![b'a'; 64 * 1024];
        let mut buf = encode(1, payload.clone());
        assert_eq!(buf[4], FLAG_ZSTD);
        assert!(
            buf.len() < 1024,
            "64 KiB of 'a' should compress well, got {}",
            buf.len()
        );
        let frame = FrameCodec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(frame.payload, payload);
    }

    #[test]
    fn incompressible_large_frame_stays_raw() {
        let payload = noise(4096);
        let mut buf = encode(1, payload.clone());
        assert_eq!(buf[4], 0);
        assert_eq!(
            FrameCodec.decode(&mut buf).unwrap().unwrap().payload,
            payload
        );
    }

    #[test]
    fn partial_input_waits_for_the_rest() {
        let full = encode(7, b"partial-frame".to_vec());
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&full[..3]);
        assert!(FrameCodec.decode(&mut buf).unwrap().is_none());
        buf.extend_from_slice(&full[3..full.len() - 1]);
        assert!(FrameCodec.decode(&mut buf).unwrap().is_none());
        buf.extend_from_slice(&full[full.len() - 1..]);
        let frame = FrameCodec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(frame.payload, Bytes::from_static(b"partial-frame"));
    }

    #[test]
    fn several_frames_in_one_buffer_decode_in_order() {
        let mut buf = BytesMut::new();
        for (kind, text) in [(1u16, "one"), (2, "two"), (3, "three")] {
            buf.extend_from_slice(&encode(kind, text.as_bytes().to_vec()));
        }
        for (kind, text) in [(1u16, "one"), (2, "two"), (3, "three")] {
            let f = FrameCodec.decode(&mut buf).unwrap().unwrap();
            assert_eq!((f.kind, &f.payload[..]), (kind, text.as_bytes()));
        }
        assert!(FrameCodec.decode(&mut buf).unwrap().is_none());
    }

    #[test]
    fn oversized_length_is_rejected_before_buffering() {
        let mut buf = header(MAX_FRAME_LEN as u32 + 1, 0, 1);
        assert!(matches!(
            FrameCodec.decode(&mut buf),
            Err(FrameError::TooLarge(_))
        ));
    }

    #[test]
    fn unknown_flags_are_rejected() {
        let mut buf = header(0, 0x80, 1);
        assert!(matches!(
            FrameCodec.decode(&mut buf),
            Err(FrameError::UnknownFlags(0x80))
        ));
    }

    #[test]
    fn zstd_bomb_is_rejected() {
        let bomb = zstd::bulk::compress(&vec![0u8; MAX_FRAME_LEN + 1], 3).unwrap();
        let mut buf = header(bomb.len() as u32, FLAG_ZSTD, 1);
        buf.extend_from_slice(&bomb);
        assert!(matches!(
            FrameCodec.decode(&mut buf),
            Err(FrameError::TooLarge(_))
        ));
    }

    #[test]
    fn garbage_marked_compressed_is_an_error_not_a_panic() {
        let mut buf = header(4, FLAG_ZSTD, 1);
        buf.extend_from_slice(b"junk");
        assert!(matches!(
            FrameCodec.decode(&mut buf),
            Err(FrameError::Decompress(_))
        ));
    }

    #[test]
    fn encoder_rejects_oversized_payload() {
        let mut buf = BytesMut::new();
        let r = FrameCodec.encode(
            Frame {
                kind: 1,
                payload: Bytes::from(vec![0u8; MAX_FRAME_LEN + 1]),
            },
            &mut buf,
        );
        assert!(matches!(r, Err(FrameError::TooLarge(_))));
    }
}
