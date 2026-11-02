# Flotilla M0 — Foundations Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A Windows user installs `Flotilla-Setup-x64.exe`, adds their VPS (host, user, SSH key), trusts the host key, and the client installs, starts and enrolls the daemon over SSH. The title bar then shows `● Connected · QUIC · 42 ms`, and the client fails over to SSH and back without user action.

**Architecture:** `flotillad` is a static musl binary that runs on the VPS under `systemd --user`. It listens on a Unix socket (reached over SSH `direct-streamlocal`, or via `flotillad attach`) and on QUIC UDP 7443, which uses a self-signed certificate the client pins. Both carry the same framed protocol: `u32 len | u8 flags | u16 kind | postcard`, with zstd above 1 KiB. The native GPUI client (`flotilla.exe`) embeds the daemon binaries. It runs a connection supervisor that races QUIC against SSH, heartbeats the link and fails over.

**Tech Stack:** Rust 1.99.1 (edition 2024), tokio 1.54, russh 0.65, quinn 0.11 + rustls 0.23 (ring), rcgen 0.14, postcard 1.1, zstd 0.13, clap 4.6, gpui 0.2.2 (DirectX 11 on Windows), keyring 4.2, Inno Setup 6, GitHub Actions, cargo-zigbuild.

**Spec:** `docs/superpowers/specs/2026-11-02-flotilla-design.md` (sections 2, 3, 8, 9, 13 and the M0 row of section 14).

## Global Constraints

- Toolchain: `rust-toolchain.toml` pins `1.99.1`; `rust-version = "1.89"`; edition 2024; resolver 3.
- `Cargo.lock` is committed. **`libc` stays pinned to `0.2.188`** because gpui's `xattr 0.2.3` needs `libc::ENOATTR`. Never run a bare `cargo update`.
- **Do not add dependencies.** Every crate's `Cargo.toml` already lists what M0 needs. If you truly need another crate, stop and report back instead of adding it.
- Wire format: `u32 BE length | u8 flags | u16 BE kind | payload`. `FLAG_ZSTD = 0x01`. Compress only when the payload is **larger than 1024 bytes** and compression actually shrinks it. Maximum frame length is 16 MiB.
- ALPN `flotilla/1`, TLS server name `flotilla`, default QUIC bind `0.0.0.0:7443` (overridable with env `FLOTILLA_QUIC_BIND`).
- Heartbeat 2 s, link dead after 6 s of silence, QUIC re-probe every 30 s while on SSH, backoff 250 ms → 5 s with ±20 % jitter, connect timeout 10 s.
- Remote layout: `$FLOTILLA_HOME` (default `$HOME/.flotilla`) with `bin/flotillad`, `run/daemon.sock`, `run/daemon.json`, `state/quic-cert.der`, `state/quic-key.der`, `state/devices.json`, `logs/`. Directories are `0700`; secrets and the socket are `0600`.
- Device tokens are 32 random bytes as 64 hex chars. The daemon stores only their SHA-256. The client stores them in Windows Credential Manager (service `Flotilla`, user `device-token:<host_id>`).
- Design tokens (spec §8.2):
  - Colours: chrome `#27262B`, surface `#1D1E21`, raised `#25262B`, hover `#2A2B30`, selected row `#2D2E33` with border `rgba(255,255,255,0.08)`, control `#35363B`, text `#DFE0E5` / `#838285` / `#6C6D72`, accent `#3D8BFD`, add/success `#5DBB63`, delete/danger `#E5534B`.
  - Radii 10/8/6 px. Inter 13/11 px; JetBrains Mono 12.5 px.
- Keyboard (spec §8.3): plain `Ctrl+letter` is reserved for the terminal (M1+). App actions use `Ctrl+Shift` or `Alt`. Text fields use the standard `ctrl-a/c/v/x`.
- Exact status copy: `Connected · QUIC · 42 ms` (U+00B7 middle dots, spaces around them, `ms` lowercase).
- Commit messages are conventional (`feat(fl-proto): …`) and end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- **Deviations from spec §14 for M0, agreed by the plan author:**
  - `fl-prochost` moves to M1, since the M0 exit doesn't exercise PTYs.
  - minisign moves to M7 auto-update; M0 verifies uploads with SHA-256.
  - Devices are stored in `devices.json` with an atomic rename and file locks instead of SQLite.
  - There is no passphrase prompt: an encrypted key gives an actionable "use ssh-agent" error.
  - There is no custom app icon.

## Review Focus

These are the five failure modes the spec implies that are most likely to bite a real user. Each has a pinned test in the task that owns it.

1. **Passphrase-protected key file** → a clear `NeedsPassphrase` error within seconds, never a hang or a generic auth failure. Test: Task 7, `encrypted_key_needs_passphrase`.
2. **A firewall silently drops UDP 7443** → the client is `Connected · SSH` well before the QUIC connect timeout, and a QUIC link that goes silent fails over to SSH within `dead_after + heartbeat`. Tests: Task 5 `connect_times_out_against_a_black_hole`, Task 11 `falls_back_to_ssh_quickly_when_udp_is_blackholed` and `fails_over_to_ssh_when_quic_dies_then_upgrades_back`.
3. **A stale `daemon.sock` after a crash or reboot** → the next start replaces it. A second daemon on the same home refuses with `AlreadyRunning`. Tests: Task 8 `stale_socket_is_replaced`, `second_daemon_on_same_home_is_refused`.
4. **Concurrent `enroll` while the daemon verifies tokens** → readers never see a partial `devices.json` and no enrollment is lost. Test: Task 8 `concurrent_enrollments_are_all_kept_and_readers_never_see_partial_state`.
5. **Unsupported remote (macOS, riscv64) or a missing `sha256sum`** → a specific, readable error before anything is uploaded. Tests: Task 10 `parse_probe_*`.

---

## Execution Model (read this first)

Up to three implementer agents work in parallel in the **same checkout** (`/srv/super-eng`, branch `main`). Rules:

- **Only touch the files your task lists.** Other agents are editing other crates at the same time.
- **Commit only your own paths:** `git add <explicit paths>`, never `git add -A` or `git add .`. If `git commit` fails with `index.lock` exists, wait 2 seconds and retry, up to 10 times. Do **not** delete the lock file.
- **Do not push.** The coordinator pushes.
- Builds share `target/`. If cargo prints `Blocking waiting for file lock`, just wait.
- Run only your crate's tests, e.g. `cargo test -p fl-proto`. The whole workspace may be mid-edit by others.
- If a step's code doesn't compile because a library API differs from what the plan shows, fix it with the smallest change that keeps the behaviour and the test intent. Note the deviation in your final report. Never weaken or delete a test to make it pass.
- Before reporting back, run `cargo fmt -p <crate>` and `cargo clippy -p <crate> --all-targets -- -D warnings`, and fix what they flag.

Lanes:

| Lane | Tasks (in order) | Waits for |
|------|------------------|-----------|
| A — protocol & daemon | 2 → 3 → 4 → 5 → 8 → 9 | — |
| B — SSH & connection | 6 → 7 → 10 → 11 | 10 needs Tasks 3 and 9; 11 needs Task 8 |
| C — UI, client & CI | 12 → 15 → 13 → 14 | 13 needs Task 10; 14 needs Task 11 |

## File Map

```
Cargo.toml, Cargo.lock, rust-toolchain.toml            workspace (Task 1, done)
crates/fl-proto/src/{lib,version,frame,msg,bootstrap}.rs   wire protocol (Tasks 2–3)
crates/fl-transport/src/conn.rs                        typed framed streams (Task 4)
crates/fl-transport/src/quic.rs                        QUIC + pinned certs (Task 5)
crates/fl-transport/src/ssh.rs                         SSH client (types: Task 1; session: Task 7)
crates/fl-transport/tests/ssh.rs                       SSH integration tests (Task 7)
crates/fl-testkit/src/{ssh_server,udp_relay}.rs        test SSH server + UDP black-hole relay (Task 6)
crates/daemon/fld-server/src/{lib,paths,identity,devices,session,listen}.rs   daemon core (Task 8)
crates/daemon/fld-server/tests/server.rs               daemon integration tests (Task 8)
crates/daemon/flotillad/src/main.rs                    daemon CLI (Task 9)
crates/daemon/flotillad/tests/cli.rs                   CLI tests (Task 9)
crates/daemon/flotillad/tests/bootstrap_e2e.rs         client-bootstrap end-to-end test (Task 10)
crates/client/flc-conn/src/profile.rs                  HostProfile (Task 1, done)
crates/client/flc-conn/src/bootstrap.rs                install/enroll over SSH (Task 10)
crates/client/flc-conn/src/supervisor.rs               QUIC/SSH supervisor (Task 11)
crates/client/flc-conn/tests/supervisor.rs             supervisor integration tests (Task 11)
crates/client/flc-ui/src/{theme,input}.rs + assets/fonts/   UI kit (Task 12)
crates/client/flotilla/build.rs                        embeds daemon binaries (Task 13)
crates/client/flotilla/src/{main,runtime,hosts,secrets,embedded,selftest,connect}.rs  client core (Task 13)
crates/client/flotilla/src/app/{mod,titlebar,sidebar,add_host,host_view}.rs   UI shell (Task 14)
.github/workflows/ci.yml, packaging/windows/flotilla.iss   CI, installer, releases (Task 15)
```

---

### Task 1: Workspace scaffold, repo, toolchain — DONE (coordinator)

**Files:** everything under `crates/*/Cargo.toml`, the stub `src` files, `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, and `crates/client/flc-conn/src/profile.rs`. `crates/fl-transport/src/ssh.rs` holds the shared types `SshTarget`, `SshAuth`, `HostKeyPolicy`.

**Interfaces produced:**
- `fl_transport::ssh::{SshTarget { host: String, port: u16, user: String }, SshAuth::{Agent { socket: Option<String> }, KeyFile { path: PathBuf }}, HostKeyPolicy::{TrustOnFirstUse, Expect(String)}}`, all `Clone + Serialize + Deserialize + PartialEq`.
- `flc_conn::profile::{HostProfile { id, name, ssh, auth, host_key: Option<String>, quic: Option<QuicEndpoint>, device_id: Option<String>, socket_path: Option<String> }, QuicEndpoint { port: u16, cert_sha256: String }}`.
- Private repo `The-Coaxys/flotilla`, branch `main`.

- [x] Workspace, manifests, stubs, lockfile with `libc 0.2.188`
- [x] `gh repo create The-Coaxys/flotilla --private --source . --remote origin --push`

---

### Task 2: fl-proto — versions and the frame codec

**Files:**
- Modify: `crates/fl-proto/src/lib.rs`
- Create (replace stub): `crates/fl-proto/src/version.rs`, `crates/fl-proto/src/frame.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `fl_proto::{ProtoVersion { major: u16, minor: u16 }, PROTO, PRODUCT_VERSION: &str, build_id() -> &'static str}`
  - `ProtoVersion::is_compatible_with(self, other) -> bool`
  - `fl_proto::{Frame { kind: u16, payload: Bytes }, FrameCodec, FrameError::{Io, TooLarge(usize), UnknownFlags(u8), Decompress(String)}, HEADER_LEN, MAX_FRAME_LEN, COMPRESS_THRESHOLD, FLAG_ZSTD}`
  - `FrameCodec` implements `tokio_util::codec::{Encoder<Frame>, Decoder<Item = Frame>}`.

- [ ] **Step 1: Write `version.rs` with its tests**

```rust
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
```

- [ ] **Step 2: Write `frame.rs` with its tests**

```rust
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
    let out = zstd::bulk::decompress(body, size)
        .map_err(|e| FrameError::Decompress(e.to_string()))?;
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
            .encode(Frame { kind, payload: Bytes::from(payload) }, &mut buf)
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
        assert_eq!(frame, Frame { kind: 2, payload: Bytes::from_static(b"hello") });
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
        assert!(buf.len() < 1024, "64 KiB of 'a' should compress well, got {}", buf.len());
        let frame = FrameCodec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(frame.payload, payload);
    }

    #[test]
    fn incompressible_large_frame_stays_raw() {
        let payload = noise(4096);
        let mut buf = encode(1, payload.clone());
        assert_eq!(buf[4], 0);
        assert_eq!(FrameCodec.decode(&mut buf).unwrap().unwrap().payload, payload);
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
        assert!(matches!(FrameCodec.decode(&mut buf), Err(FrameError::TooLarge(_))));
    }

    #[test]
    fn unknown_flags_are_rejected() {
        let mut buf = header(0, 0x80, 1);
        assert!(matches!(FrameCodec.decode(&mut buf), Err(FrameError::UnknownFlags(0x80))));
    }

    #[test]
    fn zstd_bomb_is_rejected() {
        let bomb = zstd::bulk::compress(&vec![0u8; MAX_FRAME_LEN + 1], 3).unwrap();
        let mut buf = header(bomb.len() as u32, FLAG_ZSTD, 1);
        buf.extend_from_slice(&bomb);
        assert!(matches!(FrameCodec.decode(&mut buf), Err(FrameError::TooLarge(_))));
    }

    #[test]
    fn garbage_marked_compressed_is_an_error_not_a_panic() {
        let mut buf = header(4, FLAG_ZSTD, 1);
        buf.extend_from_slice(b"junk");
        assert!(matches!(FrameCodec.decode(&mut buf), Err(FrameError::Decompress(_))));
    }

    #[test]
    fn encoder_rejects_oversized_payload() {
        let mut buf = BytesMut::new();
        let r = FrameCodec.encode(
            Frame { kind: 1, payload: Bytes::from(vec![0u8; MAX_FRAME_LEN + 1]) },
            &mut buf,
        );
        assert!(matches!(r, Err(FrameError::TooLarge(_))));
    }
}
```

- [ ] **Step 3: Export from `lib.rs`**

Replace `crates/fl-proto/src/lib.rs` with:

```rust
//! Flotilla wire protocol: length-prefixed frames, postcard messages, versions.
pub mod bootstrap;
pub mod frame;
pub mod msg;
pub mod version;

pub use frame::{COMPRESS_THRESHOLD, FLAG_ZSTD, Frame, FrameCodec, FrameError, HEADER_LEN, MAX_FRAME_LEN};
pub use version::{PRODUCT_VERSION, PROTO, ProtoVersion, build_id};
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-proto`
Expected: all `version::tests::*` and `frame::tests::*` pass (16 tests).

- [ ] **Step 5: Commit**

```bash
cargo fmt -p fl-proto && cargo clippy -p fl-proto --all-targets -- -D warnings
git add crates/fl-proto/src/lib.rs crates/fl-proto/src/version.rs crates/fl-proto/src/frame.rs
git commit -m "feat(fl-proto): protocol versions and bounded zstd frame codec

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 3: fl-proto — control messages and bootstrap documents

**Files:**
- Modify: `crates/fl-proto/src/lib.rs`
- Create (replace stub): `crates/fl-proto/src/msg.rs`, `crates/fl-proto/src/bootstrap.rs`

**Interfaces:**
- Consumes: `Frame`, `ProtoVersion`, `PROTO`, `PRODUCT_VERSION`, `build_id()` from Task 2.
- Produces (all re-exported at the crate root):
  - `KIND_CLIENT = 1`, `KIND_SERVER = 2`
  - `trait WireMessage: Serialize + DeserializeOwned { const KIND: u16; fn to_frame(&self) -> Result<Frame, MsgError>; fn from_frame(&Frame) -> Result<Self, MsgError> }`
  - `MsgError::{WrongKind { expected, got }, Codec(postcard::Error)}`
  - `Hello { client_version: String, proto: ProtoVersion, device_id: Option<String>, token: Option<String>, client_session_id: [u8; 16] }`
  - `Welcome { daemon_version: String, build: String, proto: ProtoVersion, hostname: String, server_session_id: [u8; 16] }`
  - `ClientMsg::{Hello(Hello), Ping { nonce: u64 }, Rpc { id: u64, req: Request }}`
  - `ServerMsg::{Welcome(Welcome), Rejected(RejectReason), Pong { nonce: u64 }, RpcResult { id: u64, result: Result<Response, RpcError> }}`
  - `RejectReason::{BadToken, ProtoMismatch { server: ProtoVersion }, HelloTimeout, ExpectedHello}`
  - `Request::DaemonInfo`; `Response::DaemonInfo(DaemonInfo)`
  - `DaemonInfo { version, build, hostname: String, pid: u32, uptime_secs: u64, quic_port: Option<u16>, socket_path: String }`
  - `RpcError { code: RpcErrorCode, message: String }`; `RpcErrorCode::{NotFound, InvalidArgument, Unsupported, Internal}`
  - `VersionInfo { version, build: String, proto: ProtoVersion }` with `VersionInfo::current()`
  - `EnrollResponse { device_id, token: String, quic_port: Option<u16>, cert_sha256: String, socket_path: String }`

- [ ] **Step 1: Write `msg.rs` with its tests**

```rust
//! Control-stream messages. Each direction has one enum; the frame `kind`
//! says which enum the payload holds. Postcard encodes enum variants by index,
//! so **only append** new variants and fields-with-defaults; never reorder.
use bytes::Bytes;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::{Frame, ProtoVersion};

pub const KIND_CLIENT: u16 = 1;
pub const KIND_SERVER: u16 = 2;

#[derive(Debug, thiserror::Error)]
pub enum MsgError {
    #[error("unexpected frame kind {got} (expected {expected})")]
    WrongKind { expected: u16, got: u16 },
    #[error("message codec error: {0}")]
    Codec(#[from] postcard::Error),
}

/// A message type that travels as one frame of a fixed kind.
pub trait WireMessage: Serialize + DeserializeOwned {
    const KIND: u16;

    fn to_frame(&self) -> Result<Frame, MsgError> {
        Ok(Frame { kind: Self::KIND, payload: Bytes::from(postcard::to_stdvec(self)?) })
    }

    fn from_frame(frame: &Frame) -> Result<Self, MsgError> {
        if frame.kind != Self::KIND {
            return Err(MsgError::WrongKind { expected: Self::KIND, got: frame.kind });
        }
        Ok(postcard::from_bytes(&frame.payload)?)
    }
}

/// First message from a client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub client_version: String,
    pub proto: ProtoVersion,
    /// Required for QUIC peers; ignored on the local socket (SSH already authenticated).
    pub device_id: Option<String>,
    pub token: Option<String>,
    pub client_session_id: [u8; 16],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    pub daemon_version: String,
    pub build: String,
    pub proto: ProtoVersion,
    pub hostname: String,
    pub server_session_id: [u8; 16],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMsg {
    Hello(Hello),
    Ping { nonce: u64 },
    Rpc { id: u64, req: Request },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMsg {
    Welcome(Welcome),
    Rejected(RejectReason),
    Pong { nonce: u64 },
    RpcResult { id: u64, result: Result<Response, RpcError> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RejectReason {
    BadToken,
    ProtoMismatch { server: ProtoVersion },
    HelloTimeout,
    ExpectedHello,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    DaemonInfo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    DaemonInfo(DaemonInfo),
}

/// Also printed as JSON by `flotillad status --json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub version: String,
    pub build: String,
    pub hostname: String,
    pub pid: u32,
    pub uptime_secs: u64,
    pub quic_port: Option<u16>,
    pub socket_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: RpcErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RpcErrorCode {
    NotFound,
    InvalidArgument,
    Unsupported,
    Internal,
}

impl WireMessage for ClientMsg {
    const KIND: u16 = KIND_CLIENT;
}

impl WireMessage for ServerMsg {
    const KIND: u16 = KIND_SERVER;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PROTO;

    fn hello() -> Hello {
        Hello {
            client_version: "flotilla/0.1.0".into(),
            proto: PROTO,
            device_id: Some("dev".into()),
            token: Some("tok".into()),
            client_session_id: [9; 16],
        }
    }

    fn info() -> DaemonInfo {
        DaemonInfo {
            version: "0.1.0".into(),
            build: "dev".into(),
            hostname: "vps".into(),
            pid: 42,
            uptime_secs: 7,
            quic_port: Some(7443),
            socket_path: "/root/.flotilla/run/daemon.sock".into(),
        }
    }

    #[test]
    fn every_client_message_roundtrips() {
        for msg in [
            ClientMsg::Hello(hello()),
            ClientMsg::Ping { nonce: u64::MAX },
            ClientMsg::Rpc { id: 3, req: Request::DaemonInfo },
        ] {
            let frame = msg.to_frame().unwrap();
            assert_eq!(frame.kind, KIND_CLIENT);
            assert_eq!(ClientMsg::from_frame(&frame).unwrap(), msg);
        }
    }

    #[test]
    fn every_server_message_roundtrips() {
        for msg in [
            ServerMsg::Welcome(Welcome {
                daemon_version: "0.1.0".into(),
                build: "dev".into(),
                proto: PROTO,
                hostname: "vps".into(),
                server_session_id: [1; 16],
            }),
            ServerMsg::Rejected(RejectReason::BadToken),
            ServerMsg::Rejected(RejectReason::ProtoMismatch { server: PROTO }),
            ServerMsg::Rejected(RejectReason::HelloTimeout),
            ServerMsg::Rejected(RejectReason::ExpectedHello),
            ServerMsg::Pong { nonce: 5 },
            ServerMsg::RpcResult { id: 1, result: Ok(Response::DaemonInfo(info())) },
            ServerMsg::RpcResult {
                id: 2,
                result: Err(RpcError { code: RpcErrorCode::Unsupported, message: "no".into() }),
            },
        ] {
            let frame = msg.to_frame().unwrap();
            assert_eq!(frame.kind, KIND_SERVER);
            assert_eq!(ServerMsg::from_frame(&frame).unwrap(), msg);
        }
    }

    #[test]
    fn wrong_kind_is_reported() {
        let frame = ServerMsg::Pong { nonce: 1 }.to_frame().unwrap();
        assert!(matches!(
            ClientMsg::from_frame(&frame),
            Err(MsgError::WrongKind { expected: KIND_CLIENT, got: KIND_SERVER })
        ));
    }

    #[test]
    fn garbage_payload_is_a_codec_error() {
        let frame = Frame { kind: KIND_CLIENT, payload: Bytes::from_static(&[0xFF, 0xFF, 0xFF]) };
        assert!(matches!(ClientMsg::from_frame(&frame), Err(MsgError::Codec(_))));
    }

    /// Pins the wire encoding: variant index 1 (`Ping`), then varint 7.
    #[test]
    fn ping_wire_bytes_are_stable() {
        assert_eq!(postcard::to_stdvec(&ClientMsg::Ping { nonce: 7 }).unwrap(), vec![1, 7]);
    }

    #[test]
    fn daemon_info_json_field_names_are_stable() {
        let json = serde_json::to_value(info()).unwrap();
        for key in ["version", "build", "hostname", "pid", "uptime_secs", "quic_port", "socket_path"] {
            assert!(json.get(key).is_some(), "missing {key}");
        }
    }
}
```

- [ ] **Step 2: Write `bootstrap.rs` with its tests**

```rust
//! JSON documents printed by `flotillad` and parsed by the client during
//! install and enrollment. Field names are a cross-version contract.
use serde::{Deserialize, Serialize};

use crate::{PRODUCT_VERSION, PROTO, ProtoVersion, build_id};

/// Output of `flotillad version --json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionInfo {
    pub version: String,
    pub build: String,
    pub proto: ProtoVersion,
}

impl VersionInfo {
    pub fn current() -> Self {
        Self { version: PRODUCT_VERSION.to_string(), build: build_id().to_string(), proto: PROTO }
    }
}

/// Output of `flotillad enroll --device-name <name>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollResponse {
    pub device_id: String,
    /// 64 lowercase hex chars. Shown once; the daemon keeps only its SHA-256.
    pub token: String,
    /// `None` when the daemon runs without QUIC.
    pub quic_port: Option<u16>,
    /// Lowercase hex SHA-256 of the daemon's DER certificate.
    pub cert_sha256: String,
    pub socket_path: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_info_json_shape() {
        let json = serde_json::to_string(&VersionInfo::current()).unwrap();
        assert!(json.contains(r#""version":"0.1.0""#), "{json}");
        assert!(json.contains(r#""proto":{"major":0,"minor":1}"#), "{json}");
        let back: VersionInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back, VersionInfo::current());
    }

    #[test]
    fn enroll_response_roundtrips_with_null_port() {
        let r = EnrollResponse {
            device_id: "d".into(),
            token: "ab".repeat(32),
            quic_port: None,
            cert_sha256: "cd".repeat(32),
            socket_path: "/home/u/.flotilla/run/daemon.sock".into(),
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains(r#""quic_port":null"#));
        assert_eq!(serde_json::from_str::<EnrollResponse>(&json).unwrap(), r);
    }
}
```

- [ ] **Step 3: Export from `lib.rs`**

Append to `crates/fl-proto/src/lib.rs`:

```rust
pub use bootstrap::{EnrollResponse, VersionInfo};
pub use msg::{
    ClientMsg, DaemonInfo, Hello, KIND_CLIENT, KIND_SERVER, MsgError, RejectReason, Request, Response,
    RpcError, RpcErrorCode, ServerMsg, Welcome, WireMessage,
};
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-proto`
Expected: PASS for all tests, including `ping_wire_bytes_are_stable`.

- [ ] **Step 5: Commit**

```bash
cargo fmt -p fl-proto && cargo clippy -p fl-proto --all-targets -- -D warnings
git add crates/fl-proto/src/lib.rs crates/fl-proto/src/msg.rs crates/fl-proto/src/bootstrap.rs
git commit -m "feat(fl-proto): control messages, RPC envelope, bootstrap JSON documents

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 4: fl-transport — typed framed streams

**Files:**
- Create (replace stub): `crates/fl-transport/src/conn.rs`

**Interfaces:**
- Consumes: `fl_proto::{FrameCodec, FrameError, WireMessage, MsgError}`.
- Produces:
  - `fl_transport::conn::framed(read: impl AsyncRead + Send + Unpin + 'static, write: impl AsyncWrite + Send + Unpin + 'static) -> (FrameReader, FrameWriter)`
  - `FrameReader::recv::<M: WireMessage>(&mut self) -> Result<M, ConnError>`; it is cancel-safe and can be used inside `tokio::select!`.
  - `FrameWriter::send<M: WireMessage>(&mut self, &M) -> Result<(), ConnError>`, which flushes; `FrameWriter::shutdown(&mut self)`.
  - `ConnError::{Frame(FrameError), Msg(MsgError), Closed}`.

- [ ] **Step 1: Write `conn.rs` with its tests**

```rust
//! Typed message streams over any byte stream (Unix socket, SSH channel,
//! QUIC stream, stdio). One `FrameReader`/`FrameWriter` pair per connection.
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{FramedRead, FramedWrite};

use fl_proto::{FrameCodec, FrameError, MsgError, WireMessage};

pub type BoxRead = Box<dyn AsyncRead + Send + Unpin>;
pub type BoxWrite = Box<dyn AsyncWrite + Send + Unpin>;

#[derive(Debug, thiserror::Error)]
pub enum ConnError {
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error(transparent)]
    Msg(#[from] MsgError),
    #[error("connection closed")]
    Closed,
}

pub struct FrameReader {
    inner: FramedRead<BoxRead, FrameCodec>,
}

pub struct FrameWriter {
    inner: FramedWrite<BoxWrite, FrameCodec>,
}

pub fn framed(
    read: impl AsyncRead + Send + Unpin + 'static,
    write: impl AsyncWrite + Send + Unpin + 'static,
) -> (FrameReader, FrameWriter) {
    (
        FrameReader { inner: FramedRead::new(Box::new(read), FrameCodec) },
        FrameWriter { inner: FramedWrite::new(Box::new(write), FrameCodec) },
    )
}

impl FrameReader {
    /// Next message. Cancel-safe: dropping the future loses no data.
    pub async fn recv<M: WireMessage>(&mut self) -> Result<M, ConnError> {
        match self.inner.next().await {
            Some(Ok(frame)) => Ok(M::from_frame(&frame)?),
            Some(Err(e)) => Err(e.into()),
            None => Err(ConnError::Closed),
        }
    }
}

impl FrameWriter {
    /// Encode, write and flush one message.
    pub async fn send<M: WireMessage>(&mut self, msg: &M) -> Result<(), ConnError> {
        self.inner.send(msg.to_frame()?).await?;
        Ok(())
    }

    /// Flush and shut down the write side (the peer then reads EOF).
    pub async fn shutdown(&mut self) -> Result<(), ConnError> {
        self.inner.close().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_proto::{ClientMsg, ServerMsg};

    fn pipe() -> ((FrameReader, FrameWriter), (FrameReader, FrameWriter)) {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let (ar, aw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);
        (framed(ar, aw), framed(br, bw))
    }

    #[tokio::test]
    async fn messages_flow_both_ways() {
        let ((mut ar, mut aw), (mut br, mut bw)) = pipe();
        aw.send(&ClientMsg::Ping { nonce: 1 }).await.unwrap();
        assert_eq!(br.recv::<ClientMsg>().await.unwrap(), ClientMsg::Ping { nonce: 1 });
        bw.send(&ServerMsg::Pong { nonce: 1 }).await.unwrap();
        assert_eq!(ar.recv::<ServerMsg>().await.unwrap(), ServerMsg::Pong { nonce: 1 });
    }

    #[tokio::test]
    async fn large_message_survives_compression() {
        let ((_ar, mut aw), (mut br, _bw)) = pipe();
        let big = ClientMsg::Hello(fl_proto::Hello {
            client_version: "x".repeat(200_000),
            proto: fl_proto::PROTO,
            device_id: None,
            token: None,
            client_session_id: [0; 16],
        });
        let send = tokio::spawn(async move { aw.send(&big).await.map(|_| big) });
        let got = br.recv::<ClientMsg>().await.unwrap();
        assert_eq!(got, send.await.unwrap().unwrap());
    }

    #[tokio::test]
    async fn peer_shutdown_reads_as_closed() {
        let ((_ar, mut aw), (mut br, _bw)) = pipe();
        aw.shutdown().await.unwrap();
        assert!(matches!(br.recv::<ClientMsg>().await, Err(ConnError::Closed)));
    }

    #[tokio::test]
    async fn wrong_direction_message_is_a_msg_error() {
        let ((_ar, mut aw), (mut br, _bw)) = pipe();
        aw.send(&ServerMsg::Pong { nonce: 9 }).await.unwrap();
        assert!(matches!(br.recv::<ClientMsg>().await, Err(ConnError::Msg(_))));
    }
}
```

- [ ] **Step 2: Run the tests**

Run: `cargo test -p fl-transport --lib conn`
Expected: 4 tests pass. (`quic.rs` and `ssh.rs` are still stubs; that's fine.)

- [ ] **Step 3: Commit**

```bash
cargo fmt -p fl-transport && cargo clippy -p fl-transport --all-targets -- -D warnings
git add crates/fl-transport/src/conn.rs
git commit -m "feat(fl-transport): typed framed reader/writer over any byte stream

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 5: fl-transport — QUIC with a pinned certificate

**Files:**
- Create (replace stub): `crates/fl-transport/src/quic.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces (`fl_transport::quic::…`):
  - Constants `ALPN`, `SERVER_NAME`; `install_crypto_provider()`.
  - `ServerIdentity { cert_der: Vec<u8>, key_der: Vec<u8> }` with `generate()` and `fingerprint() -> [u8; 32]`.
  - `sha256(&[u8]) -> [u8; 32]`, `fingerprint_hex(&[u8; 32]) -> String`, `parse_fingerprint(&str) -> Result<[u8; 32], QuicError>`.
  - `server_endpoint(&ServerIdentity, SocketAddr) -> Result<quinn::Endpoint, QuicError>`.
  - `connect(SocketAddr, [u8; 32], Duration) -> Result<(quinn::Endpoint, quinn::Connection), QuicError>`. Keep the returned `Endpoint` alive as long as the connection.
  - `open_ctl(&Connection)` and `accept_ctl(&Connection)`, both `-> Result<(quinn::SendStream, quinn::RecvStream), QuicError>`.
  - `QuicError::{Cert, Tls, Config(String), BadFingerprint, Io, Connect, Connection, Timeout}`.

- [ ] **Step 1: Write `quic.rs` with its tests**

```rust
//! QUIC endpoints secured by a pinned self-signed certificate.
//!
//! The daemon generates one certificate on first start. The client learns its
//! SHA-256 over SSH during enrollment and pins it, so no CA is involved.
//!
//! Gotcha: a stream opened with `open_bi()` is invisible to the peer until the
//! opener writes to it. Clients always send `Hello` first, so `accept_ctl`
//! returns promptly.
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Once};
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use sha2::{Digest, Sha256};

pub const ALPN: &[u8] = b"flotilla/1";
pub const SERVER_NAME: &str = "flotilla";
const KEEP_ALIVE: Duration = Duration::from_secs(2);
const MAX_IDLE: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum QuicError {
    #[error("certificate generation failed: {0}")]
    Cert(#[from] rcgen::Error),
    #[error("TLS configuration error: {0}")]
    Tls(#[from] rustls::Error),
    #[error("QUIC configuration error: {0}")]
    Config(String),
    #[error("invalid certificate fingerprint (expected 64 hex characters)")]
    BadFingerprint,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("cannot start QUIC connection: {0}")]
    Connect(#[from] quinn::ConnectError),
    #[error("QUIC connection failed: {0}")]
    Connection(#[from] quinn::ConnectionError),
    #[error("QUIC handshake timed out")]
    Timeout,
}

/// Install ring as the process-wide rustls provider. Idempotent.
pub fn install_crypto_provider() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // Err means a provider is already installed, which is fine.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// The daemon's certificate and PKCS#8 private key, both DER.
#[derive(Clone)]
pub struct ServerIdentity {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
}

impl std::fmt::Debug for ServerIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerIdentity")
            .field("fingerprint", &fingerprint_hex(&self.fingerprint()))
            .finish_non_exhaustive()
    }
}

impl ServerIdentity {
    pub fn generate() -> Result<Self, QuicError> {
        let ck = rcgen::generate_simple_self_signed(vec![SERVER_NAME.to_string()])?;
        Ok(Self { cert_der: ck.cert.der().to_vec(), key_der: ck.signing_key.serialize_der() })
    }

    pub fn fingerprint(&self) -> [u8; 32] {
        sha256(&self.cert_der)
    }
}

pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

pub fn fingerprint_hex(fp: &[u8; 32]) -> String {
    hex::encode(fp)
}

pub fn parse_fingerprint(s: &str) -> Result<[u8; 32], QuicError> {
    let bytes = hex::decode(s.trim()).map_err(|_| QuicError::BadFingerprint)?;
    bytes.try_into().map_err(|_| QuicError::BadFingerprint)
}

fn transport_config() -> quinn::TransportConfig {
    let mut t = quinn::TransportConfig::default();
    t.keep_alive_interval(Some(KEEP_ALIVE));
    t.max_idle_timeout(Some(MAX_IDLE.try_into().expect("30 s fits in a QUIC VarInt")));
    t
}

pub fn server_endpoint(identity: &ServerIdentity, bind: SocketAddr) -> Result<quinn::Endpoint, QuicError> {
    install_crypto_provider();
    let cert = CertificateDer::from(identity.cert_der.clone());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(identity.key_der.clone()));
    let mut tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls)
        .map_err(|e| QuicError::Config(e.to_string()))?;
    let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    cfg.transport_config(Arc::new(transport_config()));
    Ok(quinn::Endpoint::server(cfg, bind)?)
}

/// Connect to `addr`, accepting only a server certificate whose SHA-256 is
/// `fingerprint`. Fails with `Timeout` if the handshake takes longer than
/// `timeout` (for example, when UDP is silently dropped).
pub async fn connect(
    addr: SocketAddr,
    fingerprint: [u8; 32],
    timeout: Duration,
) -> Result<(quinn::Endpoint, quinn::Connection), QuicError> {
    install_crypto_provider();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(PinnedCert { fingerprint, provider });
    let mut tls = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls)
        .map_err(|e| QuicError::Config(e.to_string()))?;
    let mut cfg = quinn::ClientConfig::new(Arc::new(crypto));
    cfg.transport_config(Arc::new(transport_config()));
    let bind: SocketAddr = if addr.is_ipv6() {
        (Ipv6Addr::UNSPECIFIED, 0).into()
    } else {
        (Ipv4Addr::UNSPECIFIED, 0).into()
    };
    let mut endpoint = quinn::Endpoint::client(bind)?;
    endpoint.set_default_client_config(cfg);
    let connecting = endpoint.connect(addr, SERVER_NAME)?;
    match tokio::time::timeout(timeout, connecting).await {
        Ok(Ok(conn)) => Ok((endpoint, conn)),
        Ok(Err(e)) => Err(e.into()),
        Err(_) => {
            endpoint.close(0u32.into(), b"timeout");
            Err(QuicError::Timeout)
        }
    }
}

/// Open the control stream (client side).
pub async fn open_ctl(conn: &quinn::Connection) -> Result<(quinn::SendStream, quinn::RecvStream), QuicError> {
    Ok(conn.open_bi().await?)
}

/// Accept the control stream (server side).
pub async fn accept_ctl(conn: &quinn::Connection) -> Result<(quinn::SendStream, quinn::RecvStream), QuicError> {
    Ok(conn.accept_bi().await?)
}

#[derive(Debug)]
struct PinnedCert {
    fingerprint: [u8; 32],
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for PinnedCert {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if sha256(end_entity.as_ref()) == self.fingerprint {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General("certificate fingerprint mismatch".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Echo server: every accepted bidirectional stream is echoed back.
    async fn echo_server() -> (quinn::Endpoint, SocketAddr, [u8; 32]) {
        let identity = ServerIdentity::generate().unwrap();
        let endpoint = server_endpoint(&identity, "127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = endpoint.local_addr().unwrap();
        let server = endpoint.clone();
        tokio::spawn(async move {
            while let Some(incoming) = server.accept().await {
                tokio::spawn(async move {
                    let Ok(conn) = incoming.await else { return };
                    while let Ok((mut send, mut recv)) = accept_ctl(&conn).await {
                        let data = recv.read_to_end(1 << 20).await.unwrap_or_default();
                        let _ = send.write_all(&data).await;
                        let _ = send.finish();
                    }
                });
            }
        });
        (endpoint, addr, identity.fingerprint())
    }

    #[tokio::test]
    async fn pinned_fingerprint_connects_and_echoes() {
        let (_server, addr, fp) = echo_server().await;
        let (_ep, conn) = connect(addr, fp, Duration::from_secs(5)).await.unwrap();
        let (mut send, mut recv) = open_ctl(&conn).await.unwrap();
        send.write_all(b"ping").await.unwrap();
        send.finish().unwrap();
        assert_eq!(recv.read_to_end(1024).await.unwrap(), b"ping");
        assert!(conn.rtt() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn wrong_fingerprint_is_rejected() {
        let (_server, addr, _fp) = echo_server().await;
        let err = connect(addr, [0u8; 32], Duration::from_secs(5)).await.unwrap_err();
        assert!(matches!(err, QuicError::Connection(_)), "got {err:?}");
    }

    /// Review Focus #2: a bound socket that never answers behaves like a
    /// firewall silently dropping UDP. We must give up at the deadline.
    #[tokio::test]
    async fn connect_times_out_against_a_black_hole() {
        let hole = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = hole.local_addr().unwrap();
        let started = std::time::Instant::now();
        let err = connect(addr, [0u8; 32], Duration::from_millis(300)).await.unwrap_err();
        assert!(matches!(err, QuicError::Timeout), "got {err:?}");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn fingerprint_hex_roundtrip_and_validation() {
        let fp = sha256(b"cert");
        let text = fingerprint_hex(&fp);
        assert_eq!(text.len(), 64);
        assert_eq!(parse_fingerprint(&text).unwrap(), fp);
        assert_eq!(parse_fingerprint(&format!("  {text}\n")).unwrap(), fp);
        assert!(parse_fingerprint("abc").is_err());
        assert!(parse_fingerprint(&"zz".repeat(32)).is_err());
        assert!(parse_fingerprint(&"00".repeat(31)).is_err());
    }

    #[test]
    fn identity_fingerprint_is_sha256_of_der_and_debug_hides_key() {
        let id = ServerIdentity::generate().unwrap();
        assert_eq!(id.fingerprint(), sha256(&id.cert_der));
        let dbg = format!("{id:?}");
        assert!(dbg.contains(&fingerprint_hex(&id.fingerprint())));
        assert!(!dbg.contains("key_der"));
    }
}
```

- [ ] **Step 2: Run the tests**

Run: `cargo test -p fl-transport --lib quic`
Expected: 5 tests pass. `connect_times_out_against_a_black_hole` finishes in about 0.3 s.

- [ ] **Step 3: Commit**

```bash
cargo fmt -p fl-transport && cargo clippy -p fl-transport --all-targets -- -D warnings
git add crates/fl-transport/src/quic.rs
git commit -m "feat(fl-transport): QUIC endpoints with pinned self-signed certificates

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```
