// Flotilla modification, licensed under Apache-2.0 OR MIT: exact parser checkpoints.

use alloc::string::String;
use alloc::vec::Vec;
use core::{fmt, str};

use serde::de::{Error, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::params::{Params, ParamsSnapshot};
use crate::{Parser, State, MAX_INTERMEDIATES, MAX_OSC_BYTES, MAX_OSC_PARAMS};

/// Complete incremental parser state, independent of any consumer callbacks.
///
/// Import with [`Parser::from_snapshot`] to validate all indices and resource
/// limits before processing more input. Unused fixed-buffer entries are zeroed
/// on export so checkpoint bytes do not depend on stale buffer contents.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParserSnapshot {
    state: State,
    intermediates: [u8; MAX_INTERMEDIATES],
    intermediate_idx: usize,
    params: ParamsSnapshot,
    param: u16,
    #[serde(deserialize_with = "deserialize_osc_bytes")]
    osc_raw: Vec<u8>,
    osc_params: [(usize, usize); MAX_OSC_PARAMS],
    osc_num_params: usize,
    ignoring: bool,
    partial_utf8: [u8; 4],
    partial_utf8_len: usize,
}

impl<const OSC_RAW_BUF_SIZE: usize> Parser<OSC_RAW_BUF_SIZE> {
    /// Export all state which can affect future input, including partial UTF-8,
    /// CSI/DCS parameters, and unfinished or discarded OSC strings.
    pub fn snapshot(&self) -> ParserSnapshot {
        let mut intermediates = [0; MAX_INTERMEDIATES];
        intermediates[..self.intermediate_idx]
            .copy_from_slice(&self.intermediates[..self.intermediate_idx]);
        let mut osc_params = [(0, 0); MAX_OSC_PARAMS];
        osc_params[..self.osc_num_params]
            .copy_from_slice(&self.osc_params[..self.osc_num_params]);
        let mut partial_utf8 = [0; 4];
        partial_utf8[..self.partial_utf8_len]
            .copy_from_slice(&self.partial_utf8[..self.partial_utf8_len]);
        ParserSnapshot {
            state: self.state,
            intermediates,
            intermediate_idx: self.intermediate_idx,
            params: self.params.snapshot(),
            param: self.param,
            osc_raw: self.osc_raw.to_vec(),
            osc_params,
            osc_num_params: self.osc_num_params,
            ignoring: self.ignoring,
            partial_utf8,
            partial_utf8_len: self.partial_utf8_len,
        }
    }

    /// Validate and restore a checkpoint without replaying consumer callbacks.
    pub fn from_snapshot(snapshot: ParserSnapshot) -> Result<Self, String> {
        let invalid = || String::from("invalid parser checkpoint");
        let ParserSnapshot {
            state, intermediates, intermediate_idx, params, param, osc_raw,
            osc_params, osc_num_params, ignoring, partial_utf8, partial_utf8_len,
        } = snapshot;

        if intermediate_idx > MAX_INTERMEDIATES
            || osc_num_params > MAX_OSC_PARAMS
            || osc_raw.len() > Self::osc_limit()
            || partial_utf8_len >= 4
        {
            return Err(invalid());
        }
        for (index, &byte) in intermediates[..intermediate_idx].iter().enumerate() {
            if !(0x20..=0x2f).contains(&byte)
                && !(index == 0 && (0x3c..=0x3f).contains(&byte))
            {
                return Err(invalid());
            }
        }
        if intermediates[intermediate_idx..].iter().any(|&byte| byte != 0)
            || partial_utf8[partial_utf8_len..].iter().any(|&byte| byte != 0)
            || osc_params[osc_num_params..].iter().any(|&range| range != (0, 0))
        {
            return Err(invalid());
        }

        let mut previous_end = 0;
        for &(start, end) in &osc_params[..osc_num_params] {
            if start != previous_end || end < start || end > osc_raw.len() {
                return Err(invalid());
            }
            previous_end = end;
        }
        if state != State::OscString && (!osc_raw.is_empty() || osc_num_params != 0) {
            return Err(invalid());
        }
        if osc_raw.iter().any(|&byte| byte < 0x20 || byte == b';') {
            return Err(invalid());
        }
        if partial_utf8_len != 0 {
            if state != State::Ground {
                return Err(invalid());
            }
            match str::from_utf8(&partial_utf8[..partial_utf8_len]) {
                Err(error) if error.valid_up_to() == 0 && error.error_len().is_none() => (),
                _ => return Err(invalid()),
            }
        }
        let params = Params::from_snapshot(params)?;

        #[cfg(not(feature = "std"))]
        let osc_raw = {
            let mut buffer = arrayvec::ArrayVec::new();
            // Capacity was checked above; no attacker-controlled indexing remains.
            buffer.try_extend_from_slice(&osc_raw).map_err(|_| invalid())?;
            buffer
        };
        Ok(Self {
            state, intermediates, intermediate_idx, params, param, osc_raw,
            osc_params, osc_num_params, ignoring, partial_utf8, partial_utf8_len,
        })
    }
}

fn deserialize_osc_bytes<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
    bounded_bytes::<D, MAX_OSC_BYTES>(deserializer)
}

/// Bound allocation during deserialization as well as when installing a snapshot.
/// Sequence hints are checked before reserving, and incremental formats cannot
/// exceed the cap by omitting or understating their hint.
pub(crate) fn bounded_bytes<'de, D: Deserializer<'de>, const LIMIT: usize>(
    deserializer: D,
) -> Result<Vec<u8>, D::Error> {
    struct BytesVisitor<const LIMIT: usize>;
    impl<'de, const LIMIT: usize> Visitor<'de> for BytesVisitor<LIMIT> {
        type Value = Vec<u8>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "at most {} checkpoint bytes", LIMIT)
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Self::Value, A::Error> {
            let hint = sequence.size_hint().unwrap_or(0);
            if hint > LIMIT {
                return Err(A::Error::custom("checkpoint byte buffer exceeds limit"));
            }
            let mut bytes = Vec::with_capacity(hint);
            while let Some(byte) = sequence.next_element()? {
                if bytes.len() == LIMIT {
                    return Err(A::Error::custom("checkpoint byte buffer exceeds limit"));
                }
                bytes.push(byte);
            }
            Ok(bytes)
        }
    }
    deserializer.deserialize_seq(BytesVisitor::<LIMIT>)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Sink;
    impl crate::Perform for Sink {}

    #[test]
    fn rejects_invalid_parser_indices_and_utf8() {
        let parser = Parser::new();
        let mut snapshot = parser.snapshot();
        snapshot.intermediate_idx = MAX_INTERMEDIATES + 1;
        assert!(Parser::<1024>::from_snapshot(snapshot).is_err());

        let mut snapshot = parser.snapshot();
        snapshot.osc_num_params = MAX_OSC_PARAMS + 1;
        assert!(Parser::<1024>::from_snapshot(snapshot).is_err());

        for bytes in [&b"x"[..], &b"\x80"[..], &b"\xf4\x90"[..], &b"\xe2\x82\xac"[..]] {
            let mut snapshot = parser.snapshot();
            snapshot.partial_utf8[..bytes.len()].copy_from_slice(bytes);
            snapshot.partial_utf8_len = bytes.len();
            assert!(Parser::<1024>::from_snapshot(snapshot).is_err());
        }
    }

    #[test]
    fn rejects_invalid_osc_ranges_and_overflow() {
        let mut parser = Parser::new();
        parser.advance(&mut Sink, b"\x1b]2;title");
        let mut snapshot = parser.snapshot();
        snapshot.osc_params[0].1 = snapshot.osc_raw.len() + 1;
        assert!(Parser::<1024>::from_snapshot(snapshot).is_err());

        let mut snapshot = parser.snapshot();
        snapshot.osc_raw = alloc::vec![b'x'; MAX_OSC_BYTES + 1];
        assert!(Parser::<1024>::from_snapshot(snapshot).is_err());
    }

    #[test]
    fn rejects_oversized_buffers_during_deserialization() {
        let mut parser = Parser::new();
        parser.advance(&mut Sink, b"\x1b]2;title");
        let mut value = serde_json::to_value(parser.snapshot()).unwrap();
        value["osc_raw"] = serde_json::json!(alloc::vec![b'x'; MAX_OSC_BYTES + 1]);
        assert!(serde_json::from_value::<ParserSnapshot>(value).is_err());
    }
}
