//! CLI failures. Refusals exit non-zero and carry the daemon's reason.
use fl_proto::{RpcError, RpcErrorCode};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct CliError {
    pub code: &'static str,
    pub message: String,
}

impl CliError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new("invalid_argument", message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new("not_found", message)
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new("unsupported", message)
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new("unavailable", message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new("internal", message)
    }

    pub fn rpc(error: RpcError) -> Self {
        Self {
            code: match error.code {
                RpcErrorCode::NotFound => "not_found",
                RpcErrorCode::InvalidArgument => "invalid_argument",
                RpcErrorCode::Unsupported => "unsupported",
                RpcErrorCode::Internal => "internal",
                RpcErrorCode::RevisionConflict => "revision_conflict",
                RpcErrorCode::WrongIncarnation => "wrong_incarnation",
                RpcErrorCode::ExpiredCapability => "expired_capability",
                RpcErrorCode::InvalidCapability => "invalid_capability",
                RpcErrorCode::NotOwner => "not_owner",
            },
            message: error.message,
        }
    }

    /// Print the reason. JSON mode stays machine-readable on stdout; text mode
    /// writes to stderr.
    pub fn print(&self, json: bool) {
        if json {
            let value = serde_json::json!({
                "error": { "code": self.code, "message": self.message },
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&value)
                    .unwrap_or_else(|_| "{\"error\":{\"code\":\"internal\"}}".into())
            );
        } else {
            eprintln!("fl: {}", self.message);
        }
    }
}

impl From<std::io::Error> for CliError {
    fn from(error: std::io::Error) -> Self {
        Self::unavailable(error.to_string())
    }
}

impl From<serde_json::Error> for CliError {
    fn from(error: serde_json::Error) -> Self {
        Self::invalid(format!("invalid JSON: {error}"))
    }
}

impl From<fl_transport::conn::ConnError> for CliError {
    fn from(error: fl_transport::conn::ConnError) -> Self {
        Self::unavailable(format!("daemon connection failed: {error}"))
    }
}
