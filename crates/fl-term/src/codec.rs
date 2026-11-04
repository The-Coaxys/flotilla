//! Bounded, wire-compatible Postcard snapshots.
//!
//! Postcard 1.1.3's slice deserializer lends string/byte data, but exposes map
//! lengths and (sometimes) sequence lengths as allocation hints. This adapter
//! reads those wire lengths itself, checks them before calling a visitor, and
//! never exposes a collection allocation hint. Every nested seed, option,
//! newtype and enum payload continues through this adapter.
//!
//! The work budget is conservative accounting, not an allocator sandbox: each
//! seed charges its inline result size plus a small per-value allowance, and
//! strings/byte blobs additionally charge their payload length. Every deserialize
//! entry also consumes a visit. This bounds wire-driven collection growth and
//! zero-byte values, including Vec<()>. It does not measure allocator rounding,
//! collection capacity slack, or allocations made independently by a custom
//! Deserialize implementation. Snapshot DTO validation still enforces semantic
//! limits after decoding; arbitrary third-party Deserialize code is not trusted.

use std::fmt;
use std::marker::PhantomData;
use std::mem::size_of;

use serde::de::{
    self, DeserializeOwned, DeserializeSeed, EnumAccess, MapAccess, SeqAccess, VariantAccess,
    Visitor,
};
use serde::{Deserialize, Serialize};

pub const MAX_SNAPSHOT_BYTES: usize = 512 * 1024 * 1024;
const MAX_DEPTH: usize = 128;
// A snapshot-local hyperlink table can span both maximum-sized grids.
const MAX_COLLECTION_LEN: usize = 4_194_304;
// Admit the complete aggregate hyperlink budget even as a single string/blob.
const MAX_BLOB_BYTES: usize = 16 * 1024 * 1024;
const MAX_DECODE_WORK: usize = 1024 * 1024 * 1024;
const MAX_VISITS: usize = 128 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub(crate) enum CodecError {
    #[error("Postcard snapshot: {0}")]
    Postcard(#[from] postcard::Error),
    #[error("snapshot resource limit exceeded: {0}")]
    Limit(&'static str),
    #[error("trailing bytes after snapshot")]
    TrailingBytes,
    #[error("invalid snapshot: {0}")]
    Invalid(String),
}

impl de::Error for CodecError {
    fn custom<T: fmt::Display>(message: T) -> Self {
        Self::Invalid(message.to_string())
    }
}

pub(crate) fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, CodecError> {
    let mut failure = None;
    let result = postcard::serialize_with_flavor(
        value,
        Output {
            bytes: Vec::new(),
            failure: &mut failure,
            limit: MAX_SNAPSHOT_BYTES,
        },
    );
    result.map_err(|error| match failure {
        Some(reason) => CodecError::Limit(reason),
        None => error.into(),
    })
}

// A single serialization pass, with a limit checked before any growth. Using
// try_reserve_exact only at geometric growth boundaries also makes allocation
// failure reportable without reserving the maximum snapshot up front.
struct Output<'a> {
    bytes: Vec<u8>,
    failure: &'a mut Option<&'static str>,
    limit: usize,
}

impl Output<'_> {
    fn reserve(&mut self, additional: usize) -> Result<(), postcard::Error> {
        let required = self.bytes.len().checked_add(additional);
        let Some(required) = required.filter(|required| *required <= self.limit) else {
            *self.failure = Some("encoded bytes");
            return Err(postcard::Error::SerializeBufferFull);
        };
        if required > self.bytes.capacity() {
            let target = required
                .checked_next_power_of_two()
                .unwrap_or(self.limit)
                .min(self.limit);
            if self
                .bytes
                .try_reserve_exact(target - self.bytes.len())
                .is_err()
            {
                *self.failure = Some("output allocation");
                return Err(postcard::Error::SerializeBufferFull);
            }
        }
        Ok(())
    }
}

impl postcard::ser_flavors::Flavor for Output<'_> {
    type Output = Vec<u8>;

    fn try_push(&mut self, byte: u8) -> Result<(), postcard::Error> {
        self.reserve(1)?;
        self.bytes.push(byte);
        Ok(())
    }

    fn try_extend(&mut self, bytes: &[u8]) -> Result<(), postcard::Error> {
        self.reserve(bytes.len())?;
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    fn finalize(self) -> Result<Self::Output, postcard::Error> {
        Ok(self.bytes)
    }
}

pub(crate) fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, CodecError> {
    if bytes.len() > MAX_SNAPSHOT_BYTES {
        return Err(CodecError::Limit("encoded bytes"));
    }
    let mut decoder = Decoder {
        inner: postcard::Deserializer::from_bytes(bytes),
        depth: 0,
        work: MAX_DECODE_WORK,
        visits: MAX_VISITS,
    };
    let value = decoder.seed(PhantomData::<T>)?;
    if !decoder.inner.finalize()?.is_empty() {
        return Err(CodecError::TrailingBytes);
    }
    Ok(value)
}

struct Decoder<'de> {
    inner: postcard::Deserializer<'de, postcard::de_flavors::Slice<'de>>,
    depth: usize,
    work: usize,
    visits: usize,
}

impl<'de> Decoder<'de> {
    fn spend(&mut self, bytes: usize) -> Result<(), CodecError> {
        self.work = self
            .work
            .checked_sub(bytes)
            .ok_or(CodecError::Limit("decoded work"))?;
        Ok(())
    }

    fn visit(&mut self) -> Result<(), CodecError> {
        self.visits = self
            .visits
            .checked_sub(1)
            .ok_or(CodecError::Limit("decoded visits"))?;
        Ok(())
    }

    fn seed<S: DeserializeSeed<'de>>(&mut self, seed: S) -> Result<S::Value, CodecError> {
        self.spend(size_of::<S::Value>().saturating_add(8))?;
        seed.deserialize(self)
    }

    fn nested<T>(
        &mut self,
        operation: impl FnOnce(&mut Self) -> Result<T, CodecError>,
    ) -> Result<T, CodecError> {
        self.visit()?;
        if self.depth == MAX_DEPTH {
            return Err(CodecError::Limit("nesting depth"));
        }
        self.depth += 1;
        let result = operation(self);
        self.depth -= 1;
        result
    }

    fn length(&mut self) -> Result<usize, CodecError> {
        // Match the installed Postcard usize-varint width, including overflow.
        #[cfg(target_pointer_width = "64")]
        let length = u64::deserialize(&mut self.inner)? as usize;
        #[cfg(target_pointer_width = "32")]
        let length = u32::deserialize(&mut self.inner)? as usize;
        #[cfg(target_pointer_width = "16")]
        let length = u16::deserialize(&mut self.inner)? as usize;
        self.collection(length)?;
        Ok(length)
    }

    fn collection(&self, length: usize) -> Result<(), CodecError> {
        if length > MAX_COLLECTION_LEN {
            return Err(CodecError::Limit("collection length"));
        }
        // Every member costs at least the seed allowance, even zero-sized ones.
        // Check the lower bound before the visitor can allocate or loop.
        if length > self.work / 8 || length > self.visits {
            return Err(CodecError::Limit("collection work"));
        }
        Ok(())
    }

    fn blob(&mut self) -> Result<&'de [u8], CodecError> {
        // The slice flavor returns this borrow without allocating, including for
        // deserialize_byte_buf. Check before any consumer visitor sees it.
        let bytes = de::Deserializer::deserialize_bytes(&mut self.inner, BorrowBytes)?;
        if bytes.len() > MAX_BLOB_BYTES {
            return Err(CodecError::Limit("string or byte length"));
        }
        self.spend(bytes.len())?;
        Ok(bytes)
    }

    fn sequence<V: Visitor<'de>>(
        &mut self,
        length: usize,
        visitor: V,
    ) -> Result<V::Value, CodecError> {
        self.collection(length)?;
        visitor.visit_seq(Sequence {
            decoder: self,
            remaining: length,
        })
    }
}

struct BorrowBytes;

impl<'de> Visitor<'de> for BorrowBytes {
    type Value = &'de [u8];

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a borrowed Postcard byte slice")
    }

    fn visit_borrowed_bytes<E: de::Error>(self, bytes: &'de [u8]) -> Result<Self::Value, E> {
        Ok(bytes)
    }
}

macro_rules! primitive {
    ($($method:ident),* $(,)?) => {$(
        fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CodecError> {
            self.visit()?;
            de::Deserializer::$method(&mut self.inner, visitor).map_err(Into::into)
        }
    )*};
}

impl<'de> de::Deserializer<'de> for &mut Decoder<'de> {
    type Error = CodecError;

    fn is_human_readable(&self) -> bool {
        false
    }

    primitive!(
        deserialize_any,
        deserialize_bool,
        deserialize_i8,
        deserialize_i16,
        deserialize_i32,
        deserialize_i64,
        deserialize_i128,
        deserialize_u8,
        deserialize_u16,
        deserialize_u32,
        deserialize_u64,
        deserialize_u128,
        deserialize_f32,
        deserialize_f64,
        deserialize_char,
        deserialize_unit,
        deserialize_identifier,
        deserialize_ignored_any,
    );

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CodecError> {
        self.visit()?;
        let bytes = self.blob()?;
        let text = std::str::from_utf8(bytes).map_err(|_| postcard::Error::DeserializeBadUtf8)?;
        visitor.visit_borrowed_str(text)
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CodecError> {
        self.visit()?;
        let bytes = self.blob()?;
        let text = std::str::from_utf8(bytes).map_err(|_| postcard::Error::DeserializeBadUtf8)?;
        visitor.visit_str(text)
    }

    fn deserialize_bytes<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CodecError> {
        self.visit()?;
        visitor.visit_borrowed_bytes(self.blob()?)
    }

    fn deserialize_byte_buf<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CodecError> {
        self.visit()?;
        visitor.visit_bytes(self.blob()?)
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CodecError> {
        self.nested(|decoder| match u8::deserialize(&mut decoder.inner)? {
            0 => visitor.visit_none(),
            1 => visitor.visit_some(decoder),
            _ => Err(postcard::Error::DeserializeBadOption.into()),
        })
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, CodecError> {
        self.visit()?;
        visitor.visit_unit()
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, CodecError> {
        self.nested(|decoder| visitor.visit_newtype_struct(decoder))
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CodecError> {
        self.nested(|decoder| {
            let length = decoder.length()?;
            decoder.sequence(length, visitor)
        })
    }

    fn deserialize_tuple<V: Visitor<'de>>(
        self,
        length: usize,
        visitor: V,
    ) -> Result<V::Value, CodecError> {
        self.nested(|decoder| decoder.sequence(length, visitor))
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        length: usize,
        visitor: V,
    ) -> Result<V::Value, CodecError> {
        self.deserialize_tuple(length, visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CodecError> {
        self.nested(|decoder| {
            let length = decoder.length()?;
            // Maps have both a key and a value for each declared entry.
            if length > decoder.work / 16 || length > decoder.visits / 2 {
                return Err(CodecError::Limit("collection work"));
            }
            visitor.visit_map(Mapping {
                decoder,
                remaining: length,
            })
        })
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, CodecError> {
        self.deserialize_tuple(fields.len(), visitor)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, CodecError> {
        self.nested(|decoder| visitor.visit_enum(Enumeration { decoder }))
    }
}

struct Sequence<'a, 'de> {
    decoder: &'a mut Decoder<'de>,
    remaining: usize,
}

impl<'de> SeqAccess<'de> for Sequence<'_, 'de> {
    type Error = CodecError;

    fn next_element_seed<S: DeserializeSeed<'de>>(
        &mut self,
        seed: S,
    ) -> Result<Option<S::Value>, CodecError> {
        if self.remaining == 0 {
            return Ok(None);
        }
        self.remaining -= 1;
        self.decoder.seed(seed).map(Some)
    }

    // Deliberately no size_hint: no attacker-controlled preallocation.
}

struct Mapping<'a, 'de> {
    decoder: &'a mut Decoder<'de>,
    remaining: usize,
}

impl<'de> MapAccess<'de> for Mapping<'_, 'de> {
    type Error = CodecError;

    fn next_key_seed<S: DeserializeSeed<'de>>(
        &mut self,
        seed: S,
    ) -> Result<Option<S::Value>, CodecError> {
        if self.remaining == 0 {
            return Ok(None);
        }
        self.remaining -= 1;
        self.decoder.seed(seed).map(Some)
    }

    fn next_value_seed<S: DeserializeSeed<'de>>(
        &mut self,
        seed: S,
    ) -> Result<S::Value, CodecError> {
        self.decoder.seed(seed)
    }
}

struct Enumeration<'a, 'de> {
    decoder: &'a mut Decoder<'de>,
}

impl<'a, 'de> EnumAccess<'de> for Enumeration<'a, 'de> {
    type Error = CodecError;
    type Variant = Self;

    fn variant_seed<S: DeserializeSeed<'de>>(
        self,
        seed: S,
    ) -> Result<(S::Value, Self), CodecError> {
        let index = u32::deserialize(&mut self.decoder.inner)?;
        self.decoder
            .spend(size_of::<S::Value>().saturating_add(8))?;
        self.decoder.visit()?;
        let value = seed.deserialize(de::value::U32Deserializer::<CodecError>::new(index))?;
        Ok((value, self))
    }
}

impl<'de> VariantAccess<'de> for Enumeration<'_, 'de> {
    type Error = CodecError;

    fn unit_variant(self) -> Result<(), CodecError> {
        Ok(())
    }

    fn newtype_variant_seed<S: DeserializeSeed<'de>>(
        self,
        seed: S,
    ) -> Result<S::Value, CodecError> {
        self.decoder.seed(seed)
    }

    fn tuple_variant<V: Visitor<'de>>(
        self,
        length: usize,
        visitor: V,
    ) -> Result<V::Value, CodecError> {
        de::Deserializer::deserialize_tuple(self.decoder, length, visitor)
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, CodecError> {
        de::Deserializer::deserialize_tuple(self.decoder, fields.len(), visitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Snapshot {
        grids: Vec<Vec<Cell>>,
        metadata: BTreeMap<String, Option<Payload>>,
        unfinished: Newtype,
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Newtype(Vec<u8>);

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Cell {
        character: char,
        flags: u16,
        color: Payload,
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    enum Payload {
        Unit,
        Newtype(Vec<String>),
        Tuple(u8, String),
        Struct { bytes: Vec<u8>, number: i64 },
    }

    #[test]
    fn nested_snapshot_is_exactly_postcard_compatible() {
        let value = Snapshot {
            grids: vec![vec![
                Cell {
                    character: '界',
                    flags: 0x101,
                    color: Payload::Unit,
                },
                Cell {
                    character: '\u{301}',
                    flags: 0,
                    color: Payload::Tuple(5, "blue".into()),
                },
                Cell {
                    character: '🦀',
                    flags: 7,
                    color: Payload::Struct {
                        bytes: vec![0, 255, 128],
                        number: -12_345,
                    },
                },
            ]],
            metadata: BTreeMap::from([
                (
                    "cwd".into(),
                    Some(Payload::Newtype(vec![
                        "/tmp/é".into(),
                        "https://example.test".into(),
                    ])),
                ),
                ("absent".into(), None),
            ]),
            unfinished: Newtype(vec![0x1b, b']', b'8', b';']),
        };
        let bytes = encode(&value).unwrap();
        assert_eq!(bytes, postcard::to_allocvec(&value).unwrap());
        assert_eq!(decode::<Snapshot>(&bytes).unwrap(), value);
        assert_eq!(postcard::from_bytes::<Snapshot>(&bytes).unwrap(), value);
    }

    fn length_wire(length: usize) -> Vec<u8> {
        // Postcard sequence/string/map lengths are unsigned usize varints.
        postcard::to_allocvec(&length).unwrap()
    }

    #[test]
    fn enormous_declared_lengths_fail_before_visitors_can_preallocate() {
        for length in [MAX_COLLECTION_LEN + 1, usize::MAX] {
            let bytes = length_wire(length);
            assert!(matches!(
                decode::<Vec<()>>(&bytes),
                Err(CodecError::Limit("collection length"))
            ));
            assert!(matches!(
                decode::<Vec<u8>>(&bytes),
                Err(CodecError::Limit("collection length"))
            ));
            assert!(matches!(
                decode::<BTreeMap<(), ()>>(&bytes),
                Err(CodecError::Limit("collection length"))
            ));
        }
    }

    #[test]
    fn nested_enum_payloads_cannot_bypass_length_checks() {
        // Newtype enum -> Vec<String>, struct enum -> Vec<u8>.
        let mut bytes = vec![1];
        bytes.extend(length_wire(usize::MAX));
        assert!(matches!(
            decode::<Payload>(&bytes),
            Err(CodecError::Limit("collection length"))
        ));
        let mut bytes = vec![3];
        bytes.extend(length_wire(usize::MAX));
        assert!(matches!(
            decode::<Payload>(&bytes),
            Err(CodecError::Limit("collection length"))
        ));
        let mut bytes = vec![1]; // Some(Newtype(Vec<u8>)).
        bytes.extend(length_wire(usize::MAX));
        assert!(matches!(
            decode::<Option<Newtype>>(&bytes),
            Err(CodecError::Limit("collection length"))
        ));
        #[derive(Deserialize)]
        enum TuplePayload {
            Tuple(u8, Vec<()>),
        }
        let mut bytes = vec![0, 42]; // Variant zero, then the first tuple field.
        bytes.extend(length_wire(usize::MAX));
        match decode::<TuplePayload>(&bytes) {
            Err(CodecError::Limit("collection length")) => {}
            Ok(TuplePayload::Tuple(tag, values)) => {
                panic!(
                    "hostile tuple accepted tag {tag} with {} items",
                    values.len()
                );
            }
            Err(error) => panic!("unexpected decode failure: {error:?}"),
        }
    }

    #[derive(Debug, Deserialize)]
    enum Recursive {
        End,
        More(Box<Recursive>),
    }

    #[test]
    fn recursive_enum_depth_is_bounded() {
        let mut wire = vec![1; MAX_DEPTH + 1];
        wire.push(0);
        assert!(matches!(
            decode::<Recursive>(&wire),
            Err(CodecError::Limit("nesting depth"))
        ));
        let mut allowed = vec![1; MAX_DEPTH - 1];
        allowed.push(0);
        let mut value = decode::<Recursive>(&allowed).unwrap();
        let mut count = 0;
        while let Recursive::More(next) = value {
            value = *next;
            count += 1;
        }
        assert_eq!(count, MAX_DEPTH - 1);
    }

    #[test]
    fn oversized_strings_fail_before_the_string_visitor_allocates() {
        let mut wire = length_wire(MAX_BLOB_BYTES + 1);
        wire.resize(wire.len() + MAX_BLOB_BYTES + 1, b'x');
        assert!(matches!(
            decode::<String>(&wire),
            Err(CodecError::Limit("string or byte length"))
        ));
        // Byte buffers use exactly the same wire prefix, but a different Serde
        // entry point; its consumer must not receive the oversized payload.
        assert!(matches!(
            decode::<ByteBuffer>(&wire),
            Err(CodecError::Limit("string or byte length"))
        ));
    }

    #[test]
    fn tails_and_truncated_payloads_are_rejected() {
        assert!(matches!(
            decode::<u8>(&[7, 8]),
            Err(CodecError::TrailingBytes)
        ));
        assert!(decode::<String>(&[10, b'a']).is_err());
        assert!(decode::<Vec<u8>>(&[2, 1]).is_err());
        assert!(decode::<Option<u8>>(&[2]).is_err());
        assert!(decode::<String>(&[1, 0xff]).is_err());
    }

    #[test]
    fn legitimate_parser_buffer_sizes_are_admitted() {
        for length in [1024 * 1024, 2 * 1024 * 1024] {
            let value = Newtype(vec![0x1b; length]);
            let bytes = encode(&value).unwrap();
            assert_eq!(decode::<Newtype>(&bytes).unwrap(), value);
        }
    }

    #[test]
    fn aggregate_work_and_zero_byte_visits_are_bounded() {
        let bytes = length_wire(4);
        let mut decoder = Decoder {
            inner: postcard::Deserializer::from_bytes(&bytes),
            depth: 0,
            work: 31,
            visits: MAX_VISITS,
        };
        assert!(matches!(
            Vec::<()>::deserialize(&mut decoder),
            Err(CodecError::Limit("collection work"))
        ));
        let mut decoder = Decoder {
            inner: postcard::Deserializer::from_bytes(&bytes),
            depth: 0,
            work: MAX_DECODE_WORK,
            visits: 4,
        };
        assert!(matches!(
            Vec::<()>::deserialize(&mut decoder),
            Err(CodecError::Limit("collection work"))
        ));
    }

    struct CheckHints;

    impl<'de> Deserialize<'de> for CheckHints {
        fn deserialize<D: de::Deserializer<'de>>(decoder: D) -> Result<Self, D::Error> {
            struct CheckVisitor;
            impl<'de> Visitor<'de> for CheckVisitor {
                type Value = CheckHints;
                fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    f.write_str("an unhinted sequence")
                }
                fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                    assert_eq!(seq.size_hint(), None);
                    while seq.next_element::<u8>()?.is_some() {}
                    Ok(CheckHints)
                }
            }
            decoder.deserialize_seq(CheckVisitor)
        }
    }

    #[derive(Debug, PartialEq)]
    struct ByteBuffer(Vec<u8>);

    impl<'de> Deserialize<'de> for ByteBuffer {
        fn deserialize<D: de::Deserializer<'de>>(decoder: D) -> Result<Self, D::Error> {
            struct BufferVisitor;
            impl<'de> Visitor<'de> for BufferVisitor {
                type Value = ByteBuffer;
                fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    f.write_str("a byte buffer")
                }
                fn visit_bytes<E: de::Error>(self, bytes: &[u8]) -> Result<Self::Value, E> {
                    Ok(ByteBuffer(bytes.to_vec()))
                }
            }
            decoder.deserialize_byte_buf(BufferVisitor)
        }
    }

    #[test]
    fn even_small_collection_allocation_hints_are_suppressed() {
        decode::<CheckHints>(&[3, 10, 20, 30]).unwrap();
    }

    #[test]
    fn string_and_byte_borrows_still_point_into_the_wire_buffer() {
        let wire = [3, b'a', b'b', b'c'];
        let mut decoder = Decoder {
            inner: postcard::Deserializer::from_bytes(&wire),
            depth: 0,
            work: MAX_DECODE_WORK,
            visits: MAX_VISITS,
        };
        let text = <&str>::deserialize(&mut decoder).unwrap();
        assert_eq!(text, "abc");
        assert_eq!(text.as_ptr(), wire[1..].as_ptr());
        let mut decoder = Decoder {
            inner: postcard::Deserializer::from_bytes(&wire),
            depth: 0,
            work: MAX_DECODE_WORK,
            visits: MAX_VISITS,
        };
        let bytes = <&[u8]>::deserialize(&mut decoder).unwrap();
        assert_eq!(bytes.as_ptr(), wire[1..].as_ptr());
    }

    #[test]
    fn encoder_checks_capacity_before_appending() {
        use postcard::ser_flavors::Flavor;
        let mut failure = None;
        let mut output = Output {
            bytes: Vec::new(),
            failure: &mut failure,
            limit: 3,
        };
        output.try_extend(&[1, 2, 3]).unwrap();
        assert!(output.try_push(4).is_err());
        assert_eq!(output.bytes, [1, 2, 3]);
        assert_eq!(failure, Some("encoded bytes"));
    }
}
