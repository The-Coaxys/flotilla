//! Short-lived channel credentials. Validation here is structural only: the
//! server must authenticate the session, role, expiry and one-use ticket.
use crate::validation::{MAX_CHANNEL_LIFETIME_MS, ValidationError, schema};
use serde::{Deserialize, Serialize};
use std::fmt;

pub const CHANNEL_SCHEMA: u16 = 1;
pub const TERMINAL_PROTOCOL: u16 = 1;

/// Postcard discriminants are 0, 1, 2. Append only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChannelRole {
    TerminalHot,
    TerminalBackground,
    Bulk,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelGrant {
    pub server_session_id: [u8; 16],
    pub ticket: [u8; 32],
    pub role: ChannelRole,
    /// Relative validity measured by the issuing server, never a client clock.
    pub expires_in_ms: u32,
}

impl ChannelGrant {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.expires_in_ms == 0 || self.expires_in_ms > MAX_CHANNEL_LIFETIME_MS {
            return Err(ValidationError::Invalid("channel lifetime"));
        }
        Ok(())
    }

    pub fn hello(&self) -> ChannelHello {
        ChannelHello {
            schema: CHANNEL_SCHEMA,
            server_session_id: self.server_session_id,
            ticket: self.ticket,
            role: self.role,
        }
    }
}

impl fmt::Debug for ChannelGrant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChannelGrant")
            .field("server_session_id", &self.server_session_id)
            .field("ticket", &"[REDACTED]")
            .field("role", &self.role)
            .field("expires_in_ms", &self.expires_in_ms)
            .finish()
    }
}

/// The first root-kind message on a new logical stream. No terminal/bulk
/// message is accepted until the server has returned `ChannelReady`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelHello {
    pub schema: u16,
    pub server_session_id: [u8; 16],
    pub ticket: [u8; 32],
    pub role: ChannelRole,
}

impl ChannelHello {
    pub fn validate(&self) -> Result<(), ValidationError> {
        schema("channel schema", self.schema, CHANNEL_SCHEMA)
    }
}

impl fmt::Debug for ChannelHello {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChannelHello")
            .field("schema", &self.schema)
            .field("server_session_id", &self.server_session_id)
            .field("ticket", &"[REDACTED]")
            .field("role", &self.role)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelReady {
    pub server_session_id: [u8; 16],
    pub role: ChannelRole,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ClientMsg, Frame, KIND_CLIENT, MsgError, WireMessage};
    use bytes::Bytes;

    fn grant() -> ChannelGrant {
        ChannelGrant {
            server_session_id: [1; 16],
            ticket: [237; 32],
            role: ChannelRole::TerminalHot,
            expires_in_ms: MAX_CHANNEL_LIFETIME_MS,
        }
    }

    #[test]
    fn ticket_debug_is_redacted_even_in_nested_messages() {
        let value = grant();
        let grant_debug = format!("{value:?}");
        let hello_debug = format!("{:?}", ClientMsg::BindChannel(value.hello()));
        for debug in [grant_debug, hello_debug] {
            assert!(debug.contains("[REDACTED]"));
            assert!(!debug.contains("237"));
        }
    }

    #[test]
    fn unsupported_channel_schema_rejected_at_receive_boundary() {
        let mut hello = grant().hello();
        hello.schema = CHANNEL_SCHEMA + 1;
        let frame = Frame {
            kind: KIND_CLIENT,
            payload: postcard::to_stdvec(&ClientMsg::BindChannel(hello))
                .unwrap()
                .into(),
        };
        assert!(matches!(
            ClientMsg::from_frame(&frame),
            Err(MsgError::Validation(ValidationError::Schema {
                field: "channel schema",
                ..
            }))
        ));
    }

    #[test]
    fn unknown_role_discriminant_is_not_accepted() {
        // OpenChannel is Request index 6, ChannelRole has only indices 0..=2.
        let frame = Frame {
            kind: KIND_CLIENT,
            payload: Bytes::from_static(&[2, 0, 6, 3]),
        };
        assert!(matches!(
            ClientMsg::from_frame(&frame),
            Err(MsgError::Codec(_))
        ));
    }

    #[test]
    fn grant_lifetime_is_bounded_and_nonzero() {
        let mut value = grant();
        assert!(value.validate().is_ok());
        for lifetime in [0, MAX_CHANNEL_LIFETIME_MS + 1] {
            value.expires_in_ms = lifetime;
            assert!(value.validate().is_err());
        }
    }
}
