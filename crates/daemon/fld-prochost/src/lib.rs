//! Persistent PTY ownership is independent of daemon and transport sessions.
#![cfg(target_os = "linux")]
pub mod client;
pub mod journal;
pub mod process;
mod server;

pub use client::{Client, ensure_running};
pub use server::serve;

/// Shared kernel clock includes suspend time; daemon and prochost can enforce
/// the same input deadline without wall-clock adjustment or process epochs.
pub fn clock_ns() -> std::io::Result<u64> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `time` is valid writable timespec storage.
    if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut time) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let invalid = || std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid boot clock");
    u64::try_from(time.tv_sec)
        .ok()
        .and_then(|seconds| seconds.checked_mul(1_000_000_000))
        .and_then(|ns| ns.checked_add(u64::try_from(time.tv_nsec).ok()?))
        .ok_or_else(invalid)
}
