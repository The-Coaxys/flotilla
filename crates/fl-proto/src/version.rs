//! Protocol and product versions.
use serde::{Deserialize, Serialize};

/// Wire protocol version. Peers interoperate when the majors match and the
/// minors differ by at most one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProtoVersion {
    pub major: u16,
    pub minor: u16,
}

/// The protocol version this build speaks.
pub const PROTO: ProtoVersion = ProtoVersion { major: 0, minor: 1 };

/// Product version from Cargo, e.g. `0.1.0`.
pub const PRODUCT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Build identifier baked in at compile time from `FLOTILLA_BUILD_ID`.
/// CI sets it to the commit SHA; local builds report `"dev"`.
pub fn build_id() -> &'static str {
    option_env!("FLOTILLA_BUILD_ID").unwrap_or("dev")
}

impl ProtoVersion {
    pub fn is_compatible_with(self, other: ProtoVersion) -> bool {
        self.major == other.major && self.minor.abs_diff(other.minor) <= 1
    }
}

impl std::fmt::Display for ProtoVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn v(major: u16, minor: u16) -> ProtoVersion {
        ProtoVersion { major, minor }
    }

    #[test]
    fn same_major_adjacent_minor_is_compatible() {
        assert!(v(1, 4).is_compatible_with(v(1, 4)));
        assert!(v(1, 4).is_compatible_with(v(1, 5)));
        assert!(v(1, 5).is_compatible_with(v(1, 4)));
    }

    #[test]
    fn minor_gap_of_two_is_incompatible() {
        assert!(!v(1, 2).is_compatible_with(v(1, 4)));
    }

    #[test]
    fn different_major_is_incompatible() {
        assert!(!v(1, 0).is_compatible_with(v(2, 0)));
    }

    #[test]
    fn displays_as_dotted_pair() {
        assert_eq!(v(0, 1).to_string(), "0.1");
    }

    #[test]
    fn build_id_is_never_empty() {
        assert!(!build_id().is_empty());
    }
}
