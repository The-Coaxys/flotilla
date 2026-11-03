//! Daemon binaries embedded at build time (see build.rs).
use std::borrow::Cow;

use flc_conn::bootstrap::DaemonArtifacts;

pub static FLOTILLAD_X86_64: &[u8] = include_bytes!(concat!(
    env!("OUT_DIR"),
    "/flotillad-x86_64-unknown-linux-musl"
));
pub static FLOTILLAD_AARCH64: &[u8] = include_bytes!(concat!(
    env!("OUT_DIR"),
    "/flotillad-aarch64-unknown-linux-musl"
));

/// The embedded daemons report the same version/build as this client (same CI run).
pub fn artifacts() -> DaemonArtifacts {
    DaemonArtifacts {
        x86_64: Cow::Borrowed(FLOTILLAD_X86_64),
        aarch64: Cow::Borrowed(FLOTILLAD_AARCH64),
        version: fl_proto::PRODUCT_VERSION.to_string(),
        build: fl_proto::build_id().to_string(),
    }
}

pub fn is_elf(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x7fELF")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifacts_report_this_build() {
        let a = artifacts();
        assert_eq!(a.version, fl_proto::PRODUCT_VERSION);
        assert_eq!(a.build, fl_proto::build_id());
        assert_eq!(a.x86_64.len(), FLOTILLAD_X86_64.len());
    }

    #[test]
    fn elf_magic() {
        assert!(is_elf(b"\x7fELF\x02\x01"));
        assert!(!is_elf(b""));
        assert!(!is_elf(b"MZ\x90\x00"));
    }
}
