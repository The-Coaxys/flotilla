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

---

### Task 6: fl-testkit — in-process SSH server and UDP black-hole relay

**Files:**
- Create (replace stub): `crates/fl-testkit/src/ssh_server.rs`, `crates/fl-testkit/src/udp_relay.rs`
- (`crates/fl-testkit/src/lib.rs` already declares both modules behind `#![cfg(unix)]`. Don't change it.)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `fl_testkit::ssh_server::{TestSshServer, TestSshOptions}`
    - `TestSshServer::start(home: impl Into<PathBuf>) -> io::Result<TestSshServer>`
    - `TestSshServer::start_with(home, TestSshOptions) -> io::Result<TestSshServer>`
    - public fields `addr: SocketAddr`, `host_key_fingerprint: String` (`SHA256:…`), `home: PathBuf`
    - `async fn kill_sessions(&self)`: disconnects every live session.
  - `TestSshOptions { allowed_key: Option<russh::keys::PublicKey>, env: Vec<(String, String)> }` (`Default`). With `allowed_key: None`, any public key is accepted. `env` is applied to every `exec`.
  - Exec semantics: runs `sh -c <command>` with `HOME=<home>`, cwd `<home>`. Channel data goes to stdin; channel EOF closes stdin; stdout becomes channel data, stderr becomes extended data type 1; the exit status is reported, then EOF and close.
  - `direct-streamlocal@openssh.com`: connects to the requested Unix socket and bridges bytes. If the socket can't be connected, the channel open is **rejected** with `ConnectFailed` (as OpenSSH does).
  - `fl_testkit::udp_relay::UdpRelay`: `start(upstream: SocketAddr) -> io::Result<UdpRelay>`, public field `addr: SocketAddr`, `set_dropping(&self, bool)`. While dropping, every datagram in both directions is discarded silently.

- [ ] **Step 1: Write `udp_relay.rs` with its test**

```rust
//! UDP relay between clients and one upstream server. `set_dropping(true)`
//! silently discards every datagram in both directions, which is how a firewall
//! that black-holes QUIC looks to both peers.
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::task::{JoinHandle, JoinSet};

pub struct UdpRelay {
    pub addr: SocketAddr,
    dropping: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

impl UdpRelay {
    pub async fn start(upstream: SocketAddr) -> std::io::Result<Self> {
        let front = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let addr = front.local_addr()?;
        let dropping = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(run(front, upstream, dropping.clone()));
        Ok(Self { addr, dropping, task })
    }

    pub fn set_dropping(&self, drop_all: bool) {
        self.dropping.store(drop_all, Ordering::SeqCst);
    }
}

impl Drop for UdpRelay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn run(front: Arc<UdpSocket>, upstream: SocketAddr, dropping: Arc<AtomicBool>) {
    // One upstream-facing socket per client address, so replies can be routed back.
    let mut backs: HashMap<SocketAddr, Arc<UdpSocket>> = HashMap::new();
    // Dropping the JoinSet (when this task is aborted) aborts the reply pumps.
    let mut pumps = JoinSet::new();
    let mut buf = vec![0u8; 65_535];
    loop {
        let Ok((n, client)) = front.recv_from(&mut buf).await else { break };
        if dropping.load(Ordering::SeqCst) {
            continue;
        }
        let back = match backs.get(&client) {
            Some(b) => b.clone(),
            None => {
                let Ok(b) = UdpSocket::bind("127.0.0.1:0").await else { continue };
                if b.connect(upstream).await.is_err() {
                    continue;
                }
                let b = Arc::new(b);
                let (b2, f2, d2) = (b.clone(), front.clone(), dropping.clone());
                pumps.spawn(async move {
                    let mut buf = vec![0u8; 65_535];
                    loop {
                        match b2.recv(&mut buf).await {
                            Ok(n) => {
                                if !d2.load(Ordering::SeqCst) {
                                    let _ = f2.send_to(&buf[..n], client).await;
                                }
                            }
                            // e.g. ICMP port unreachable on a connected socket
                            Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
                        }
                    }
                });
                backs.insert(client, b.clone());
                b
            }
        };
        let _ = back.send(&buf[..n]).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn echo_upstream() -> SocketAddr {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = sock.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            while let Ok((n, from)) = sock.recv_from(&mut buf).await {
                let _ = sock.send_to(&buf[..n], from).await;
            }
        });
        addr
    }

    async fn roundtrip(client: &UdpSocket, relay: SocketAddr, msg: &[u8]) -> Option<Vec<u8>> {
        client.send_to(msg, relay).await.unwrap();
        let mut buf = [0u8; 1500];
        match tokio::time::timeout(Duration::from_millis(300), client.recv_from(&mut buf)).await {
            Ok(Ok((n, _))) => Some(buf[..n].to_vec()),
            _ => None,
        }
    }

    #[tokio::test]
    async fn relays_until_dropping_then_recovers() {
        let relay = UdpRelay::start(echo_upstream().await).await.unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        assert_eq!(roundtrip(&client, relay.addr, b"one").await.as_deref(), Some(&b"one"[..]));
        relay.set_dropping(true);
        assert_eq!(roundtrip(&client, relay.addr, b"two").await, None);
        relay.set_dropping(false);
        assert_eq!(roundtrip(&client, relay.addr, b"three").await.as_deref(), Some(&b"three"[..]));
    }
}
```

- [ ] **Step 2: Run the relay test**

Run: `cargo test -p fl-testkit udp_relay`
Expected: `relays_until_dropping_then_recovers` passes.

- [ ] **Step 3: Write `ssh_server.rs`**

```rust
//! In-process SSH server for tests. See the crate docs in the plan, Task 6.
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::keys::{Algorithm, HashAlg, PrivateKey, PublicKey};
use russh::server::{Auth, ChannelOpenHandle, Handle, Msg, Server as _, Session, VerifiedPublicKey};
use russh::{Channel, ChannelId, ChannelOpenFailure};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UnixStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

#[derive(Clone, Default)]
pub struct TestSshOptions {
    /// When set, only this public key may authenticate.
    pub allowed_key: Option<PublicKey>,
    /// Extra environment variables for `exec` commands.
    pub env: Vec<(String, String)>,
}

pub struct TestSshServer {
    pub addr: SocketAddr,
    /// `SHA256:...` fingerprint of the server host key.
    pub host_key_fingerprint: String,
    pub home: PathBuf,
    sessions: Arc<Mutex<Vec<Handle>>>,
    accept_task: JoinHandle<()>,
}

impl TestSshServer {
    pub async fn start(home: impl Into<PathBuf>) -> std::io::Result<Self> {
        Self::start_with(home, TestSshOptions::default()).await
    }

    pub async fn start_with(home: impl Into<PathBuf>, opts: TestSshOptions) -> std::io::Result<Self> {
        let home = home.into();
        let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).map_err(std::io::Error::other)?;
        let host_key_fingerprint = host_key.public_key().fingerprint(HashAlg::Sha256).to_string();
        let config = Arc::new(russh::server::Config {
            keys: vec![host_key],
            auth_rejection_time: Duration::from_millis(10),
            auth_rejection_time_initial: Some(Duration::ZERO),
            inactivity_timeout: None,
            ..Default::default()
        });
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let sessions = Arc::new(Mutex::new(Vec::new()));
        let mut factory = ServerFactory { home: home.clone(), opts: Arc::new(opts), sessions: sessions.clone() };
        let accept_task = tokio::spawn(async move {
            let _ = factory.run_on_socket(config, &listener).await;
        });
        Ok(Self { addr, host_key_fingerprint, home, sessions, accept_task })
    }

    /// Forcibly disconnect every live SSH session (simulates a dead SSH path).
    pub async fn kill_sessions(&self) {
        let handles: Vec<Handle> = std::mem::take(&mut *self.sessions.lock().unwrap());
        for h in handles {
            let _ = h
                .disconnect(russh::Disconnect::ByApplication, "killed by test".into(), "en".into())
                .await;
        }
    }
}

impl Drop for TestSshServer {
    fn drop(&mut self) {
        self.accept_task.abort();
    }
}

#[derive(Clone)]
struct ServerFactory {
    home: PathBuf,
    opts: Arc<TestSshOptions>,
    sessions: Arc<Mutex<Vec<Handle>>>,
}

impl russh::server::Server for ServerFactory {
    type Handler = SessionHandler;

    fn new_client(&mut self, _peer: Option<SocketAddr>) -> SessionHandler {
        SessionHandler {
            home: self.home.clone(),
            opts: self.opts.clone(),
            sessions: self.sessions.clone(),
            stdin: HashMap::new(),
            registered: false,
        }
    }
}

struct SessionHandler {
    home: PathBuf,
    opts: Arc<TestSshOptions>,
    sessions: Arc<Mutex<Vec<Handle>>>,
    /// Per-exec-channel stdin feeders. Dropping the sender closes the child's stdin.
    stdin: HashMap<ChannelId, mpsc::UnboundedSender<Vec<u8>>>,
    registered: bool,
}

impl SessionHandler {
    fn register(&mut self, session: &Session) {
        if !self.registered {
            self.sessions.lock().unwrap().push(session.handle());
            self.registered = true;
        }
    }
}

impl russh::server::Handler for SessionHandler {
    type Error = russh::Error;

    async fn auth_publickey(&mut self, _user: &str, key: VerifiedPublicKey<'_>) -> Result<Auth, Self::Error> {
        let ok = match &self.opts.allowed_key {
            None => true,
            Some(allowed) => allowed.key_data() == key.public_key.key_data(),
        };
        Ok(if ok { Auth::Accept } else { Auth::reject() })
    }

    async fn channel_open_session(
        &mut self,
        _channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.register(session);
        reply.accept().await;
        Ok(())
    }

    async fn exec_request(&mut self, channel: ChannelId, data: &[u8], session: &mut Session) -> Result<(), Self::Error> {
        let command = String::from_utf8_lossy(data).into_owned();
        let (tx, rx) = mpsc::unbounded_channel();
        self.stdin.insert(channel, tx);
        session.channel_success(channel)?;
        tokio::spawn(run_exec(session.handle(), channel, command, self.home.clone(), self.opts.env.clone(), rx));
        Ok(())
    }

    async fn data(&mut self, channel: ChannelId, data: &[u8], _session: &mut Session) -> Result<(), Self::Error> {
        if let Some(tx) = self.stdin.get(&channel) {
            let _ = tx.send(data.to_vec());
        }
        Ok(())
    }

    async fn channel_eof(&mut self, channel: ChannelId, _session: &mut Session) -> Result<(), Self::Error> {
        self.stdin.remove(&channel);
        Ok(())
    }

    async fn channel_close(&mut self, channel: ChannelId, _session: &mut Session) -> Result<(), Self::Error> {
        self.stdin.remove(&channel);
        Ok(())
    }

    async fn channel_open_direct_streamlocal(
        &mut self,
        channel: Channel<Msg>,
        socket_path: &str,
        reply: ChannelOpenHandle,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.register(session);
        match UnixStream::connect(socket_path).await {
            Ok(mut unix) => {
                reply.accept().await;
                tokio::spawn(async move {
                    let mut stream = channel.into_stream();
                    let _ = tokio::io::copy_bidirectional(&mut stream, &mut unix).await;
                });
            }
            Err(_) => reply.reject(ChannelOpenFailure::ConnectFailed).await,
        }
        Ok(())
    }
}

async fn run_exec(
    handle: Handle,
    channel: ChannelId,
    command: String,
    home: PathBuf,
    env: Vec<(String, String)>,
    mut stdin_rx: mpsc::UnboundedReceiver<Vec<u8>>,
) {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(&command)
        .current_dir(&home)
        .env("HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (k, v) in &env {
        cmd.env(k, v);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = handle.extended_data(channel, 1, format!("spawn failed: {e}\n").into_bytes()).await;
            let _ = handle.exit_status_request(channel, 127).await;
            let _ = handle.eof(channel).await;
            let _ = handle.close(channel).await;
            return;
        }
    };
    let mut child_stdin = child.stdin.take().expect("stdin is piped");
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");

    let stdin_task = tokio::spawn(async move {
        while let Some(chunk) = stdin_rx.recv().await {
            if child_stdin.write_all(&chunk).await.is_err() {
                break;
            }
        }
        // Sender dropped (channel EOF): dropping child_stdin closes the pipe.
    });
    let out_handle = handle.clone();
    let out_task = tokio::spawn(async move {
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            match stdout.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if out_handle.data(channel, buf[..n].to_vec()).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
    let err_handle = handle.clone();
    let err_task = tokio::spawn(async move {
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            match stderr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if err_handle.extended_data(channel, 1, buf[..n].to_vec()).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    let status = child.wait().await;
    let _ = out_task.await;
    let _ = err_task.await;
    stdin_task.abort();
    let code = status.ok().and_then(|s| s.code()).unwrap_or(255) as u32;
    let _ = handle.exit_status_request(channel, code).await;
    let _ = handle.eof(channel).await;
    let _ = handle.close(channel).await;
}
```

- [ ] **Step 4: Check it compiles**

Run: `cargo test -p fl-testkit`
Expected: compiles; the relay test passes. The SSH server is exercised by Task 7's integration tests.

- [ ] **Step 5: Commit**

```bash
cargo fmt -p fl-testkit && cargo clippy -p fl-testkit --all-targets -- -D warnings
git add crates/fl-testkit/src/ssh_server.rs crates/fl-testkit/src/udp_relay.rs
git commit -m "test(fl-testkit): in-process SSH server and UDP black-hole relay

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 7: fl-transport — SSH session (exec, Unix-socket channels, agent auth)

**Files:**
- Modify: `crates/fl-transport/src/ssh.rs`. **Keep** the existing `SshTarget`, `SshAuth`, `HostKeyPolicy` definitions unchanged at the top and append below them.
- Create: `crates/fl-transport/tests/ssh.rs`

**Interfaces:**
- Consumes: `fl_testkit::ssh_server::{TestSshServer, TestSshOptions}` (Task 6, test only).
- Produces (`fl_transport::ssh::…`):
  - `SshError::{Connect { addr, reason }, Timeout, HostKeyMismatch { expected, actual }, AuthFailed { user }, NoAgent(String), Key { path, reason }, NeedsPassphrase(PathBuf), SocketUnavailable { path, reason }, Command { cmd, code: Option<u32>, stderr }, Protocol(String)}`, plus `SshError::is_fatal(&self) -> bool`.
  - `ExecOutput { stdout: Vec<u8>, stderr: Vec<u8>, code: Option<u32> }` with `stdout_str()` and `stderr_str()`.
  - `load_key(&Path) -> Result<russh::keys::PrivateKey, SshError>`.
  - `SshSession`:
    - `SshSession::connect(&SshTarget, &SshAuth, &HostKeyPolicy, timeout: Duration) -> Result<SshSession, SshError>`
    - `host_key_fingerprint(&self) -> &str` (`SHA256:…`) and `target(&self) -> &SshTarget`
    - `exec(&self, cmd, stdin: Option<&[u8]>) -> Result<ExecOutput, SshError>`
    - `exec_checked(&self, cmd, stdin) -> Result<Vec<u8>, SshError>`: a non-zero exit becomes `Command`.
    - `open_unix_socket(&self, path: &str) -> Result<SshStream, SshError>`
    - `open_exec_stream(&self, cmd: &str) -> Result<SshStream, SshError>`
    - `is_closed(&self) -> bool` and `disconnect(&self)`
  - `pub type SshStream = russh::ChannelStream<russh::client::Msg>`. It implements `AsyncRead + AsyncWrite + Send + Unpin + 'static`.

- [ ] **Step 1: Write the failing integration tests** — `crates/fl-transport/tests/ssh.rs`

```rust
#![cfg(unix)]
//! SSH client tests against the in-process test server (fl-testkit).
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use fl_testkit::ssh_server::{TestSshOptions, TestSshServer};
use fl_transport::ssh::{HostKeyPolicy, SshAuth, SshError, SshSession, SshTarget, load_key};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const T: Duration = Duration::from_secs(10);

fn keygen(dir: &Path, name: &str, kind: &str, passphrase: &str) -> PathBuf {
    let path = dir.join(name);
    let status = Command::new("ssh-keygen")
        .args(["-q", "-t", kind, "-N", passphrase, "-C", "flotilla-test", "-f"])
        .arg(&path)
        .status()
        .expect("ssh-keygen must be installed (openssh-client)");
    assert!(status.success());
    path
}

struct Fx {
    dir: tempfile::TempDir,
    server: TestSshServer,
    key: PathBuf,
}

impl Fx {
    async fn new() -> Self {
        Self::with(TestSshOptions::default()).await
    }

    async fn with(opts: TestSshOptions) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let server = TestSshServer::start_with(dir.path(), opts).await.unwrap();
        let key = keygen(dir.path(), "id_ed25519", "ed25519", "");
        Self { dir, server, key }
    }

    fn target(&self) -> SshTarget {
        SshTarget { host: "127.0.0.1".into(), port: self.server.addr.port(), user: "tester".into() }
    }

    async fn connect(&self) -> SshSession {
        SshSession::connect(&self.target(), &SshAuth::KeyFile { path: self.key.clone() }, &HostKeyPolicy::TrustOnFirstUse, T)
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn tofu_reports_the_server_fingerprint() {
    let fx = Fx::new().await;
    let s = fx.connect().await;
    assert!(s.host_key_fingerprint().starts_with("SHA256:"));
    assert_eq!(s.host_key_fingerprint(), fx.server.host_key_fingerprint);
}

#[tokio::test]
async fn expected_fingerprint_connects() {
    let fx = Fx::new().await;
    let policy = HostKeyPolicy::Expect(fx.server.host_key_fingerprint.clone());
    SshSession::connect(&fx.target(), &SshAuth::KeyFile { path: fx.key.clone() }, &policy, T).await.unwrap();
}

#[tokio::test]
async fn host_key_mismatch_is_reported_and_fatal() {
    let fx = Fx::new().await;
    let policy = HostKeyPolicy::Expect("SHA256:not-the-key".into());
    let err = SshSession::connect(&fx.target(), &SshAuth::KeyFile { path: fx.key.clone() }, &policy, T)
        .await
        .err()
        .expect("must fail");
    match &err {
        SshError::HostKeyMismatch { expected, actual } => {
            assert_eq!(expected, "SHA256:not-the-key");
            assert_eq!(actual, &fx.server.host_key_fingerprint);
        }
        other => panic!("expected HostKeyMismatch, got {other:?}"),
    }
    assert!(err.is_fatal());
}

#[tokio::test]
async fn exec_captures_stdout_stderr_and_exit_code() {
    let fx = Fx::new().await;
    let out = fx.connect().await.exec("echo out; echo err >&2; exit 3", None).await.unwrap();
    assert_eq!(out.stdout_str(), "out\n");
    assert_eq!(out.stderr_str(), "err\n");
    assert_eq!(out.code, Some(3));
}

#[tokio::test]
async fn exec_runs_with_home_set() {
    let fx = Fx::new().await;
    let out = fx.connect().await.exec_checked("printf %s \"$HOME\"", None).await.unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), fx.dir.path().display().to_string());
}

#[tokio::test]
async fn exec_pipes_a_megabyte_through_stdin_and_back() {
    let fx = Fx::new().await;
    let input: Vec<u8> = (0..1_000_000u32).map(|i| (i % 251) as u8).collect();
    let out = fx.connect().await.exec("cat", Some(&input)).await.unwrap();
    assert_eq!(out.code, Some(0));
    assert!(out.stdout == input, "echoed {} bytes, expected {}", out.stdout.len(), input.len());
}

#[tokio::test]
async fn exec_checked_turns_failure_into_command_error() {
    let fx = Fx::new().await;
    let err = fx.connect().await.exec_checked("echo nope >&2; exit 7", None).await.unwrap_err();
    match err {
        SshError::Command { code, stderr, .. } => {
            assert_eq!(code, Some(7));
            assert_eq!(stderr, "nope");
        }
        other => panic!("expected Command, got {other:?}"),
    }
}

#[tokio::test]
async fn unix_socket_channel_bridges_bytes() {
    let fx = Fx::new().await;
    let sock = fx.dir.path().join("echo.sock");
    let listener = tokio::net::UnixListener::bind(&sock).unwrap();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let (mut r, mut w) = s.split();
        let _ = tokio::io::copy(&mut r, &mut w).await;
    });
    let session = fx.connect().await;
    let mut stream = session.open_unix_socket(sock.to_str().unwrap()).await.unwrap();
    stream.write_all(b"over-streamlocal").await.unwrap();
    let mut buf = [0u8; 16];
    stream.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"over-streamlocal");
}

#[tokio::test]
async fn missing_unix_socket_is_socket_unavailable() {
    let fx = Fx::new().await;
    let err = fx.connect().await.open_unix_socket("/nonexistent/flotilla.sock").await.unwrap_err();
    assert!(matches!(err, SshError::SocketUnavailable { .. }), "got {err:?}");
}

#[tokio::test]
async fn exec_stream_is_bidirectional() {
    let fx = Fx::new().await;
    let session = fx.connect().await;
    let mut stream = session.open_exec_stream("cat").await.unwrap();
    stream.write_all(b"hello\n").await.unwrap();
    let mut buf = [0u8; 6];
    stream.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"hello\n");
}

#[tokio::test]
async fn rsa_key_authenticates() {
    let fx = Fx::new().await;
    let rsa = keygen(fx.dir.path(), "id_rsa", "rsa", "");
    SshSession::connect(&fx.target(), &SshAuth::KeyFile { path: rsa }, &HostKeyPolicy::TrustOnFirstUse, T)
        .await
        .unwrap();
}

/// Review Focus #1: an encrypted key must produce an actionable error quickly.
#[tokio::test]
async fn encrypted_key_needs_passphrase() {
    let fx = Fx::new().await;
    let locked = keygen(fx.dir.path(), "id_locked", "ed25519", "correct horse");
    let started = Instant::now();
    let err = SshSession::connect(&fx.target(), &SshAuth::KeyFile { path: locked.clone() }, &HostKeyPolicy::TrustOnFirstUse, T)
        .await
        .err()
        .expect("must fail");
    assert!(started.elapsed() < Duration::from_secs(5));
    match &err {
        SshError::NeedsPassphrase(p) => assert_eq!(p, &locked),
        other => panic!("expected NeedsPassphrase, got {other:?}"),
    }
    assert!(err.to_string().contains("ssh-agent"));
    assert!(err.is_fatal());
}

#[tokio::test]
async fn missing_key_file_is_a_key_error() {
    let fx = Fx::new().await;
    let err = SshSession::connect(
        &fx.target(),
        &SshAuth::KeyFile { path: fx.dir.path().join("nope") },
        &HostKeyPolicy::TrustOnFirstUse,
        T,
    )
    .await
    .err()
    .expect("must fail");
    assert!(matches!(err, SshError::Key { .. }), "got {err:?}");
}

#[tokio::test]
async fn unauthorized_key_is_auth_failed() {
    let dir = tempfile::tempdir().unwrap();
    let other = keygen(dir.path(), "other", "ed25519", "");
    let allowed = load_key(&other).unwrap().public_key().clone();
    let fx = Fx::with(TestSshOptions { allowed_key: Some(allowed), ..Default::default() }).await;
    let err = SshSession::connect(&fx.target(), &SshAuth::KeyFile { path: fx.key.clone() }, &HostKeyPolicy::TrustOnFirstUse, T)
        .await
        .err()
        .expect("must fail");
    assert!(matches!(err, SshError::AuthFailed { .. }), "got {err:?}");
    assert!(err.is_fatal());
}

struct Agent {
    child: Child,
    sock: PathBuf,
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_agent(dir: &Path) -> Agent {
    let sock = dir.join("agent.sock");
    let child = Command::new("ssh-agent").arg("-D").arg("-a").arg(&sock).spawn().expect("ssh-agent");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !sock.exists() {
        assert!(Instant::now() < deadline, "ssh-agent did not create its socket");
        std::thread::sleep(Duration::from_millis(20));
    }
    Agent { child, sock }
}

#[tokio::test]
async fn agent_auth_with_explicit_socket() {
    let fx = Fx::new().await;
    let agent = start_agent(fx.dir.path());
    let added = Command::new("ssh-add").arg(&fx.key).env("SSH_AUTH_SOCK", &agent.sock).output().unwrap();
    assert!(added.status.success(), "{}", String::from_utf8_lossy(&added.stderr));
    let auth = SshAuth::Agent { socket: Some(agent.sock.display().to_string()) };
    SshSession::connect(&fx.target(), &auth, &HostKeyPolicy::TrustOnFirstUse, T).await.unwrap();
}

#[tokio::test]
async fn empty_agent_is_no_agent() {
    let fx = Fx::new().await;
    let agent = start_agent(fx.dir.path());
    let auth = SshAuth::Agent { socket: Some(agent.sock.display().to_string()) };
    let err = SshSession::connect(&fx.target(), &auth, &HostKeyPolicy::TrustOnFirstUse, T).await.err().expect("must fail");
    match err {
        SshError::NoAgent(msg) => assert!(msg.contains("no keys"), "{msg}"),
        other => panic!("expected NoAgent, got {other:?}"),
    }
}

#[tokio::test]
async fn silent_server_times_out() {
    // Accepts TCP (kernel backlog) but never speaks SSH.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = SshTarget { host: "127.0.0.1".into(), port: listener.local_addr().unwrap().port(), user: "x".into() };
    let dir = tempfile::tempdir().unwrap();
    let key = keygen(dir.path(), "k", "ed25519", "");
    let started = Instant::now();
    let err = SshSession::connect(&target, &SshAuth::KeyFile { path: key }, &HostKeyPolicy::TrustOnFirstUse, Duration::from_millis(300))
        .await
        .err()
        .expect("must fail");
    assert!(matches!(err, SshError::Timeout), "got {err:?}");
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(!err.is_fatal());
    drop(listener);
}

#[tokio::test]
async fn killed_session_reports_closed() {
    let fx = Fx::new().await;
    let s = fx.connect().await;
    // Open a channel so the server registers this session's handle.
    s.exec("true", None).await.unwrap();
    assert!(!s.is_closed());
    fx.server.kill_sessions().await;
    let deadline = Instant::now() + Duration::from_secs(3);
    while !s.is_closed() {
        assert!(Instant::now() < deadline, "session should notice the disconnect");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p fl-transport --test ssh`
Expected: compile errors such as `cannot find type SshSession in module fl_transport::ssh`.

- [ ] **Step 3: Implement — append to `crates/fl-transport/src/ssh.rs`**

Add these imports at the top of the file, merged with the existing `use` lines:

```rust
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::client::Handle;
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::{ChannelMsg, Disconnect};
```

Append:

```rust
/// A bidirectional byte stream over an SSH channel.
pub type SshStream = russh::ChannelStream<russh::client::Msg>;

#[derive(Debug, thiserror::Error)]
pub enum SshError {
    #[error("cannot connect to {addr}: {reason}")]
    Connect { addr: String, reason: String },
    #[error("SSH connection timed out")]
    Timeout,
    #[error("host key mismatch: expected {expected}, server presented {actual}")]
    HostKeyMismatch { expected: String, actual: String },
    #[error("authentication rejected for user {user}")]
    AuthFailed { user: String },
    #[error("SSH agent unavailable: {0}")]
    NoAgent(String),
    #[error("cannot load key {}: {reason}", .path.display())]
    Key { path: PathBuf, reason: String },
    #[error("key {} is passphrase-protected; load it into ssh-agent (ssh-add) and choose agent authentication", .0.display())]
    NeedsPassphrase(PathBuf),
    #[error("cannot open remote socket {path}: {reason}")]
    SocketUnavailable { path: String, reason: String },
    #[error("remote command `{cmd}` exited with {code:?}: {stderr}")]
    Command { cmd: String, code: Option<u32>, stderr: String },
    #[error("SSH protocol error: {0}")]
    Protocol(String),
}

impl SshError {
    /// Errors that retrying cannot fix without the user changing something.
    pub fn is_fatal(&self) -> bool {
        matches!(
            self,
            SshError::HostKeyMismatch { .. }
                | SshError::AuthFailed { .. }
                | SshError::NoAgent(_)
                | SshError::Key { .. }
                | SshError::NeedsPassphrase(_)
        )
    }
}

fn proto(e: russh::Error) -> SshError {
    SshError::Protocol(e.to_string())
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ExecOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub code: Option<u32>,
}

impl ExecOutput {
    pub fn stdout_str(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn stderr_str(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

struct ClientHandler {
    policy: HostKeyPolicy,
    seen: Arc<Mutex<Option<String>>>,
}

impl russh::client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        let PublicKeyOrCertificate::PublicKey { key, .. } = key else {
            // Host certificates are not supported in M0.
            return Ok(false);
        };
        let fp = key.fingerprint(HashAlg::Sha256).to_string();
        *self.seen.lock().unwrap() = Some(fp.clone());
        Ok(match &self.policy {
            HostKeyPolicy::TrustOnFirstUse => true,
            HostKeyPolicy::Expect(expected) => expected == &fp,
        })
    }
}

/// Load an unencrypted private key; encrypted keys map to `NeedsPassphrase`.
pub fn load_key(path: &Path) -> Result<russh::keys::PrivateKey, SshError> {
    match russh::keys::load_secret_key(path, None) {
        Ok(k) => Ok(k),
        Err(russh::keys::Error::KeyIsEncrypted) => Err(SshError::NeedsPassphrase(path.to_path_buf())),
        Err(e) => Err(SshError::Key { path: path.to_path_buf(), reason: e.to_string() }),
    }
}

pub struct SshSession {
    handle: Handle<ClientHandler>,
    host_key: String,
    target: SshTarget,
}

impl SshSession {
    pub async fn connect(
        target: &SshTarget,
        auth: &SshAuth,
        policy: &HostKeyPolicy,
        timeout: Duration,
    ) -> Result<Self, SshError> {
        // Load the key before touching the network: a bad key fails fast and offline.
        let key = match auth {
            SshAuth::KeyFile { path } => Some(load_key(path)?),
            SshAuth::Agent { .. } => None,
        };
        tokio::time::timeout(timeout, Self::connect_inner(target, auth, key, policy))
            .await
            .map_err(|_| SshError::Timeout)?
    }

    async fn connect_inner(
        target: &SshTarget,
        auth: &SshAuth,
        key: Option<russh::keys::PrivateKey>,
        policy: &HostKeyPolicy,
    ) -> Result<Self, SshError> {
        let seen = Arc::new(Mutex::new(None));
        let handler = ClientHandler { policy: policy.clone(), seen: seen.clone() };
        let config = Arc::new(russh::client::Config {
            keepalive_interval: Some(Duration::from_secs(5)),
            keepalive_max: 3,
            nodelay: true,
            inactivity_timeout: None,
            ..Default::default()
        });
        let mut handle = match russh::client::connect(config, (target.host.as_str(), target.port), handler).await {
            Ok(h) => h,
            Err(e) => {
                let presented = seen.lock().unwrap().clone();
                return Err(match (policy, presented) {
                    (HostKeyPolicy::Expect(expected), Some(actual)) if &actual != expected => {
                        SshError::HostKeyMismatch { expected: expected.clone(), actual }
                    }
                    _ => SshError::Connect { addr: format!("{}:{}", target.host, target.port), reason: e.to_string() },
                });
            }
        };
        let host_key = seen.lock().unwrap().clone().unwrap_or_default();
        match (auth, key) {
            (SshAuth::KeyFile { .. }, Some(key)) => {
                let hash = if matches!(key.algorithm(), russh::keys::Algorithm::Rsa { .. }) {
                    handle.best_supported_rsa_hash().await.map_err(proto)?.flatten()
                } else {
                    None
                };
                let res = handle
                    .authenticate_publickey(&target.user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
                    .await
                    .map_err(proto)?;
                if !res.success() {
                    return Err(SshError::AuthFailed { user: target.user.clone() });
                }
            }
            (SshAuth::Agent { socket }, _) => agent_auth(&mut handle, &target.user, socket.as_deref()).await?,
            (SshAuth::KeyFile { path }, None) => {
                return Err(SshError::Key { path: path.clone(), reason: "key not loaded".into() });
            }
        }
        Ok(Self { handle, host_key, target: target.clone() })
    }

    /// `SHA256:...` fingerprint of the server host key seen during this connection.
    pub fn host_key_fingerprint(&self) -> &str {
        &self.host_key
    }

    pub fn target(&self) -> &SshTarget {
        &self.target
    }

    /// Run `command` through the remote login shell, feeding `stdin` (then EOF).
    /// Output is collected until the channel closes.
    pub async fn exec(&self, command: &str, stdin: Option<&[u8]>) -> Result<ExecOutput, SshError> {
        let channel = self.handle.channel_open_session().await.map_err(proto)?;
        channel.exec(true, command).await.map_err(proto)?;
        let (mut read, write) = channel.split();
        let writer = async {
            if let Some(input) = stdin {
                write.data(input).await.map_err(proto)?;
            }
            write.eof().await.map_err(proto)?;
            Ok::<(), SshError>(())
        };
        let reader = async {
            let mut out = ExecOutput::default();
            while let Some(msg) = read.wait().await {
                match msg {
                    ChannelMsg::Data { data } => out.stdout.extend_from_slice(&data),
                    ChannelMsg::ExtendedData { data, ext: 1 } => out.stderr.extend_from_slice(&data),
                    ChannelMsg::ExitStatus { exit_status } => out.code = Some(exit_status),
                    _ => {}
                }
            }
            out
        };
        // Write and read concurrently so large stdin and large stdout cannot deadlock.
        let (written, out) = tokio::join!(writer, reader);
        written?;
        Ok(out)
    }

    /// Like `exec`, but a non-zero exit status is an error carrying stderr.
    pub async fn exec_checked(&self, command: &str, stdin: Option<&[u8]>) -> Result<Vec<u8>, SshError> {
        let out = self.exec(command, stdin).await?;
        if out.code == Some(0) {
            Ok(out.stdout)
        } else {
            Err(SshError::Command { cmd: command.to_string(), code: out.code, stderr: out.stderr_str().trim().to_string() })
        }
    }

    /// Open a `direct-streamlocal@openssh.com` channel to a Unix socket on the server.
    pub async fn open_unix_socket(&self, path: &str) -> Result<SshStream, SshError> {
        let channel = self
            .handle
            .channel_open_direct_streamlocal(path)
            .await
            .map_err(|e| SshError::SocketUnavailable { path: path.to_string(), reason: e.to_string() })?;
        Ok(channel.into_stream())
    }

    /// Start `command` and return its stdin/stdout as a byte stream (stderr is discarded).
    pub async fn open_exec_stream(&self, command: &str) -> Result<SshStream, SshError> {
        let channel = self.handle.channel_open_session().await.map_err(proto)?;
        channel.exec(true, command).await.map_err(proto)?;
        Ok(channel.into_stream())
    }

    pub fn is_closed(&self) -> bool {
        self.handle.is_closed()
    }

    pub async fn disconnect(&self) {
        let _ = self.handle.disconnect(Disconnect::ByApplication, "bye", "en").await;
    }
}

#[cfg(unix)]
async fn agent_auth(handle: &mut Handle<ClientHandler>, user: &str, socket: Option<&str>) -> Result<(), SshError> {
    use russh::keys::agent::client::AgentClient;
    let agent = match socket {
        Some(path) => AgentClient::connect_uds(path).await,
        None => AgentClient::connect_env().await,
    }
    .map_err(|e| SshError::NoAgent(e.to_string()))?;
    auth_with_agent(handle, user, agent).await
}

#[cfg(windows)]
async fn agent_auth(handle: &mut Handle<ClientHandler>, user: &str, socket: Option<&str>) -> Result<(), SshError> {
    use russh::keys::agent::client::AgentClient;
    if let Some(pipe) = socket {
        let agent = AgentClient::connect_named_pipe(pipe)
            .await
            .map_err(|e| SshError::NoAgent(format!("{pipe}: {e}")))?;
        return auth_with_agent(handle, user, agent).await;
    }
    match AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent").await {
        Ok(agent) => auth_with_agent(handle, user, agent).await,
        Err(pipe_err) => match AgentClient::connect_pageant().await {
            Ok(agent) => auth_with_agent(handle, user, agent).await,
            Err(pageant_err) => Err(SshError::NoAgent(format!(
                "OpenSSH agent: {pipe_err}; Pageant: {pageant_err}"
            ))),
        },
    }
}

async fn auth_with_agent<S>(
    handle: &mut Handle<ClientHandler>,
    user: &str,
    mut agent: russh::keys::agent::client::AgentClient<S>,
) -> Result<(), SshError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let identities = agent.request_identities().await.map_err(|e| SshError::NoAgent(e.to_string()))?;
    let mut tried = 0usize;
    for identity in identities {
        let russh::keys::agent::AgentIdentity::PublicKey { key, .. } = identity else { continue };
        tried += 1;
        let hash = if matches!(key.algorithm(), russh::keys::Algorithm::Rsa { .. }) {
            handle.best_supported_rsa_hash().await.map_err(proto)?.flatten()
        } else {
            None
        };
        let res = handle
            .authenticate_publickey_with(user, key, hash, &mut agent)
            .await
            .map_err(|e| SshError::Protocol(e.to_string()))?;
        if res.success() {
            return Ok(());
        }
    }
    if tried == 0 {
        return Err(SshError::NoAgent("the agent has no keys loaded (run ssh-add)".into()));
    }
    Err(SshError::AuthFailed { user: user.to_string() })
}
```

Note the `#[cfg(windows)]` branch can't be compiled on Linux. Keep it exactly as shown; Windows CI (Task 15) compiles it.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-transport --test ssh`
Expected: all 18 tests pass. If `cat` with 1 MB stalls, the concurrent read/write in `exec` is wrong; it must use `tokio::join!` as shown.

Then run: `cargo test -p fl-transport`. Expected: everything in the crate passes.

- [ ] **Step 5: Commit**

```bash
cargo fmt -p fl-transport && cargo clippy -p fl-transport --all-targets -- -D warnings
git add crates/fl-transport/src/ssh.rs crates/fl-transport/tests/ssh.rs
git commit -m "feat(fl-transport): SSH sessions with exec, unix-socket channels, agent auth

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 12: flc-ui — theme tokens, bundled fonts, themed text input

**Files:**
- Create (replace stub): `crates/client/flc-ui/src/theme.rs`, `crates/client/flc-ui/src/input.rs`
- Create: `crates/client/flc-ui/assets/fonts/{Inter-Regular.ttf, Inter-Medium.ttf, Inter-SemiBold.ttf, JetBrainsMono-Regular.ttf, JetBrainsMono-Medium.ttf, Inter-LICENSE.txt, JetBrainsMono-OFL.txt}`
- Create: `crates/client/flc-ui/NOTICE`
- Create: `scripts/vendor_text_input.py` (one-off, kept for reproducibility)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `flc_ui::theme::{Theme, ActiveTheme, init, FONTS}`.
  - `Theme::dark()` returns a `Theme` with the fields below (`Hsla`/`Pixels`/`SharedString`). It is a `gpui::Global`.
    - Colours: `chrome_bg, surface_bg, surface_raised, raised_hover, row_selected, row_selected_border, control_bg, border, text_primary, text_secondary, text_tertiary, accent, success, danger, warning`.
    - Sizes: `radius_lg, radius_md, radius_sm, text_size, text_size_sm, mono_size`.
    - Fonts: `font_ui, font_mono, font_terminal`.
  - `ActiveTheme::theme(&self) -> &Theme` is implemented for `gpui::App`, so it works on `Context<T>` too.
  - `init(cx: &mut App)` sets the global theme, registers the bundled fonts and binds the text-input keys.
  - `flc_ui::input::{TextInput, bind_keys}`:
    - `TextInput::new(cx: &mut Context<TextInput>, placeholder) -> TextInput`
    - `text(&self) -> &str`
    - `set_text(&mut self, text, cx)`
    - implements `Render + Focusable`

- [ ] **Step 1: Fetch the fonts (OFL) into the crate**

```bash
set -e
S=$(mktemp -d)
curl -fsSL -o "$S/inter.zip" https://github.com/rsms/inter/releases/download/v4.1/Inter-4.1.zip
curl -fsSL -o "$S/jbm.zip" https://github.com/JetBrains/JetBrainsMono/releases/download/v2.304/JetBrainsMono-2.304.zip
mkdir -p "$S/inter" "$S/jbm" crates/client/flc-ui/assets/fonts
unzip -q "$S/inter.zip" -d "$S/inter"
unzip -q "$S/jbm.zip" -d "$S/jbm"
find "$S/inter" -name 'Inter-Regular.ttf' -o -name 'Inter-Medium.ttf' -o -name 'Inter-SemiBold.ttf' | grep -v -i display
```

Copy the three static Inter TTFs (from the `extras/ttf/` folder, **not** "Inter Display") and `JetBrainsMono-Regular.ttf` and `JetBrainsMono-Medium.ttf` (from `fonts/ttf/`) into `crates/client/flc-ui/assets/fonts/`. Also copy Inter's `LICENSE.txt` as `Inter-LICENSE.txt` and JetBrains Mono's `OFL.txt` as `JetBrainsMono-OFL.txt`. Verify:

```bash
ls -l crates/client/flc-ui/assets/fonts
file crates/client/flc-ui/assets/fonts/*.ttf    # all "TrueType Font data"
```

- [ ] **Step 2: Write `theme.rs` with its tests**

```rust
//! Design tokens (spec §8.2) and font registration.
use std::borrow::Cow;

use gpui::{App, Global, Hsla, Pixels, SharedString, px, rgb, rgba};

/// Bundled fonts (SIL OFL 1.1; licenses in assets/fonts).
pub static FONTS: &[&[u8]] = &[
    include_bytes!("../assets/fonts/Inter-Regular.ttf"),
    include_bytes!("../assets/fonts/Inter-Medium.ttf"),
    include_bytes!("../assets/fonts/Inter-SemiBold.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMono-Medium.ttf"),
];

#[derive(Debug, Clone)]
pub struct Theme {
    pub chrome_bg: Hsla,
    pub surface_bg: Hsla,
    pub surface_raised: Hsla,
    pub raised_hover: Hsla,
    pub row_selected: Hsla,
    pub row_selected_border: Hsla,
    pub control_bg: Hsla,
    pub border: Hsla,
    pub text_primary: Hsla,
    pub text_secondary: Hsla,
    pub text_tertiary: Hsla,
    pub accent: Hsla,
    pub success: Hsla,
    pub danger: Hsla,
    pub warning: Hsla,
    pub radius_lg: Pixels,
    pub radius_md: Pixels,
    pub radius_sm: Pixels,
    pub font_ui: SharedString,
    pub font_mono: SharedString,
    pub font_terminal: SharedString,
    pub text_size: Pixels,
    pub text_size_sm: Pixels,
    pub mono_size: Pixels,
}

impl Theme {
    pub fn dark() -> Self {
        Self {
            chrome_bg: rgb(0x27262B).into(),
            surface_bg: rgb(0x1D1E21).into(),
            surface_raised: rgb(0x25262B).into(),
            raised_hover: rgb(0x2A2B30).into(),
            row_selected: rgb(0x2D2E33).into(),
            row_selected_border: rgba(0xFFFFFF14).into(),
            control_bg: rgb(0x35363B).into(),
            border: rgba(0xFFFFFF0F).into(),
            text_primary: rgb(0xDFE0E5).into(),
            text_secondary: rgb(0x838285).into(),
            text_tertiary: rgb(0x6C6D72).into(),
            accent: rgb(0x3D8BFD).into(),
            success: rgb(0x5DBB63).into(),
            danger: rgb(0xE5534B).into(),
            warning: rgb(0xD9A53C).into(),
            radius_lg: px(10.),
            radius_md: px(8.),
            radius_sm: px(6.),
            font_ui: "Inter".into(),
            font_mono: "JetBrains Mono".into(),
            font_terminal: "Cascadia Mono".into(),
            text_size: px(13.),
            text_size_sm: px(11.),
            mono_size: px(12.5),
        }
    }
}

impl Global for Theme {}

pub trait ActiveTheme {
    fn theme(&self) -> &Theme;
}

impl ActiveTheme for App {
    fn theme(&self) -> &Theme {
        self.global::<Theme>()
    }
}

/// Install the theme, bundled fonts and text-input key bindings.
pub fn init(cx: &mut App) {
    cx.set_global(Theme::dark());
    let fonts = FONTS.iter().map(|f| Cow::Borrowed(*f)).collect();
    if let Err(e) = cx.text_system().add_fonts(fonts) {
        eprintln!("flotilla: failed to register bundled fonts: {e:#}");
    }
    crate::input::bind_keys(cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_theme_matches_spec_tokens() {
        let t = Theme::dark();
        let c = |hex: u32| -> Hsla { rgb(hex).into() };
        assert_eq!(t.chrome_bg, c(0x27262B));
        assert_eq!(t.surface_bg, c(0x1D1E21));
        assert_eq!(t.surface_raised, c(0x25262B));
        assert_eq!(t.raised_hover, c(0x2A2B30));
        assert_eq!(t.row_selected, c(0x2D2E33));
        assert_eq!(t.control_bg, c(0x35363B));
        assert_eq!(t.text_primary, c(0xDFE0E5));
        assert_eq!(t.text_secondary, c(0x838285));
        assert_eq!(t.text_tertiary, c(0x6C6D72));
        assert_eq!(t.accent, c(0x3D8BFD));
        assert_eq!(t.success, c(0x5DBB63));
        assert_eq!(t.danger, c(0xE5534B));
        let border: Hsla = rgba(0xFFFFFF14).into();
        assert_eq!(t.row_selected_border, border);
        assert!((t.row_selected_border.a - 0.08).abs() < 0.01);
        assert_eq!((t.radius_lg, t.radius_md, t.radius_sm), (px(10.), px(8.), px(6.)));
        assert_eq!((t.text_size, t.text_size_sm, t.mono_size), (px(13.), px(11.), px(12.5)));
        assert_eq!(t.font_ui.as_ref(), "Inter");
        assert_eq!(t.font_mono.as_ref(), "JetBrains Mono");
    }

    #[test]
    fn bundled_fonts_are_truetype() {
        assert_eq!(FONTS.len(), 5);
        for f in FONTS {
            assert!(f.len() > 50_000, "font suspiciously small: {} bytes", f.len());
            assert!(f.starts_with(&[0, 1, 0, 0]) || f.starts_with(b"true"), "not a TrueType file");
        }
    }
}
```

- [ ] **Step 3: Run the theme tests**

Run: `cargo test -p flc-ui theme`
Expected: 2 tests pass. (The first gpui build takes a few minutes; `input.rs` is still a stub, which is fine.)

- [ ] **Step 4: Write the vendoring script** — `scripts/vendor_text_input.py`

The text input is gpui's own `examples/input.rs` (Apache-2.0), lines 1–607 (everything up to and including `impl Focusable for TextInput`), with exact, checked substitutions:

```python
#!/usr/bin/env python3
"""Vendor gpui 0.2.2 examples/input.rs (Apache-2.0) as flc_ui::input with Flotilla theming.

Usage: python3 scripts/vendor_text_input.py <path-to-gpui-0.2.2>/examples/input.rs crates/client/flc-ui/src/input.rs
Every substitution must match exactly once, or the script aborts.
"""
import sys

src_path, dst_path = sys.argv[1], sys.argv[2]
lines = open(src_path, encoding="utf-8").read().split("\n")
assert lines[602] == "impl Focusable for TextInput {", repr(lines[602])
assert lines[606] == "}", repr(lines[606])
text = "\n".join(lines[:607]) + "\n"


def sub(old, new):
    global text
    n = text.count(old)
    if n != 1:
        sys.exit(f"expected exactly one match, found {n}: {old!r}")
    text = text.replace(old, new)


sub("        Copy,\n        Quit,\n    ]", "        Copy,\n    ]")
sub("\nstruct TextInput {", "\npub struct TextInput {")
sub(
    "        let input = self.input.read(cx);\n        let content = input.content.clone();",
    "        let (placeholder_color, accent) = {\n"
    "            let t = cx.theme();\n"
    "            (t.text_tertiary, t.accent)\n"
    "        };\n"
    "        let input = self.input.read(cx);\n        let content = input.content.clone();",
)
sub("(input.placeholder.clone(), hsla(0., 0., 0., 0.2))", "(input.placeholder.clone(), placeholder_color)")
sub("                    gpui::blue(),\n", "                    accent,\n")
sub("                    rgba(0x3311ff30),\n", "                    accent.opacity(0.3),\n")
sub(
    "impl Render for TextInput {\n    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {\n        div()",
    "impl Render for TextInput {\n    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {\n"
    "        let theme = cx.theme().clone();\n"
    "        let focused = self.focus_handle.is_focused(window);\n"
    "        div()",
)
sub(
    "            .bg(rgb(0xeeeeee))\n            .line_height(px(30.))\n            .text_size(px(24.))",
    "            .w_full()\n            .line_height(px(20.))\n            .text_size(theme.text_size)\n"
    "            .text_color(theme.text_primary)\n            .font_family(theme.font_ui.clone())",
)
sub(
    "                    .h(px(30. + 4. * 2.))\n                    .w_full()\n                    .p(px(4.))\n                    .bg(white())",
    "                    .h(px(32.))\n                    .w_full()\n                    .px(px(10.))\n                    .py(px(6.))\n"
    "                    .bg(theme.control_bg)\n                    .rounded(theme.radius_sm)\n                    .border_1()\n"
    "                    .border_color(if focused { theme.accent } else { theme.border })",
)

header = (
    "//! Single-line text input.\n"
    "//!\n"
    "//! Vendored from gpui 0.2.2 `examples/input.rs` (Apache-2.0, Copyright Zed Industries)\n"
    "//! by `scripts/vendor_text_input.py`, with Flotilla theming and a public API.\n"
    "//! See crates/client/flc-ui/NOTICE.\n"
    "#![allow(clippy::all)]\n"
    "use crate::theme::ActiveTheme;\n"
)

footer = '''
impl TextInput {
    pub fn new(cx: &mut Context<Self>, placeholder: impl Into<SharedString>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content: "".into(),
            placeholder: placeholder.into(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            is_selecting: false,
        }
    }

    pub fn text(&self) -> &str {
        &self.content
    }

    pub fn set_text(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.content = text.into();
        let end = self.content.len();
        self.selected_range = end..end;
        self.selection_reversed = false;
        self.marked_range = None;
        cx.notify();
    }
}

/// Windows-style editing keys, active while a `TextInput` has focus.
pub fn bind_keys(cx: &mut App) {
    let ctx = Some("TextInput");
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, ctx),
        KeyBinding::new("delete", Delete, ctx),
        KeyBinding::new("left", Left, ctx),
        KeyBinding::new("right", Right, ctx),
        KeyBinding::new("shift-left", SelectLeft, ctx),
        KeyBinding::new("shift-right", SelectRight, ctx),
        KeyBinding::new("home", Home, ctx),
        KeyBinding::new("end", End, ctx),
        KeyBinding::new("ctrl-a", SelectAll, ctx),
        KeyBinding::new("ctrl-c", Copy, ctx),
        KeyBinding::new("ctrl-v", Paste, ctx),
        KeyBinding::new("ctrl-x", Cut, ctx),
    ]);
}
'''

open(dst_path, "w", encoding="utf-8").write(header + text + footer)
print(f"wrote {dst_path}")
```

`#![allow(clippy::all)]` is an inner attribute, so it must be at the very top of the module. That is why the header puts it right after the `//!` doc lines and before any item. It keeps upstream example code out of our clippy gate; rustc warnings (unused imports and so on) still apply.

- [ ] **Step 5: Generate `input.rs` and make it compile cleanly**

```bash
G=$(ls -d ~/.cargo/registry/src/index.crates.io-*/gpui-0.2.2 | head -1)
python3 scripts/vendor_text_input.py "$G/examples/input.rs" crates/client/flc-ui/src/input.rs
cargo check -p flc-ui 2>&1 | grep -E '^(warning|error)' | sort | uniq -c
```

Then fix the remaining **compiler warnings** by editing the generated `input.rs` by hand:
- Remove the now-unused imports from the `use gpui::{…}` list. Expect `Application`, `WindowBounds`, `WindowOptions`, `black`, `hsla`, `opaque_grey`, `rgb`, `rgba`, `white`, `yellow`, `Keystroke`, `MouseUpEvent` and possibly `size`; keep whatever the compiler says is used.
- If a method is reported as never used (e.g. `reset`), make it `pub` rather than deleting it.

Add a matching note to the script's docstring saying that the import list is pruned by hand afterwards. Then run:

```bash
cargo clippy -p flc-ui --all-targets -- -D warnings
```

Expected: no warnings.

- [ ] **Step 6: Write `NOTICE`**

`crates/client/flc-ui/NOTICE`:

```
flc-ui includes code derived from gpui (https://github.com/zed-industries/zed),
examples/input.rs at gpui 0.2.2, Copyright Zed Industries, Inc.,
licensed under the Apache License, Version 2.0
(http://www.apache.org/licenses/LICENSE-2.0). Changes: theming, public
constructor/accessors, Windows key bindings, removal of the demo app.

Bundled fonts:
- Inter 4.1, Copyright the Inter Project Authors, SIL Open Font License 1.1 (assets/fonts/Inter-LICENSE.txt)
- JetBrains Mono 2.304, Copyright JetBrains s.r.o., SIL Open Font License 1.1 (assets/fonts/JetBrainsMono-OFL.txt)
```

- [ ] **Step 7: Run all flc-ui checks**

Run: `cargo test -p flc-ui && cargo clippy -p flc-ui --all-targets -- -D warnings`
Expected: 2 tests pass; clippy is clean.

- [ ] **Step 8: Commit**

```bash
cargo fmt -p flc-ui
git add crates/client/flc-ui/src/theme.rs crates/client/flc-ui/src/input.rs crates/client/flc-ui/assets crates/client/flc-ui/NOTICE scripts/vendor_text_input.py
git commit -m "feat(flc-ui): design tokens, bundled Inter/JetBrains Mono, themed text input

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 15: CI, Windows installer, nightly and tagged releases

**Files:**
- Create: `.github/workflows/ci.yml`
- Create: `packaging/windows/flotilla.iss`

**Interfaces:**
- Consumes:
  - Crate names `flotillad`, `flotilla`, `flc-ui`.
  - The client's build-time env `FLOTILLA_EMBED_DAEMON_DIR`, which must contain `flotillad-x86_64-unknown-linux-musl` and `flotillad-aarch64-unknown-linux-musl` (Task 13).
  - `flotilla.exe --self-test=<path>`, which exits 0 on success (Task 13).
  - `FLOTILLA_BUILD_ID` (Task 2).
- Produces:
  - CI artifacts `daemon` and `flotilla-windows-x64`.
  - A GitHub release named `nightly` (prerelease, replaced on every push to `main`), or `vX.Y.Z` for `v*` tags. Assets: `Flotilla-Setup-x64.exe`, `flotilla-x64.exe`, `flotillad-x86_64-unknown-linux-musl`, `flotillad-aarch64-unknown-linux-musl`, `SHA256SUMS.txt`.

- [ ] **Step 1: Write `.github/workflows/ci.yml`**

```yaml
name: ci

on:
  push:
    branches: [main]
    tags: ["v*"]
  pull_request:
  workflow_dispatch:

permissions:
  contents: read

concurrency:
  group: ci-${{ github.ref }}
  cancel-in-progress: true

env:
  CARGO_TERM_COLOR: always
  RUST_BACKTRACE: "1"
  FLOTILLA_BUILD_ID: ${{ github.sha }}

jobs:
  daemon:
    name: daemon — test + static builds
    runs-on: ubuntu-latest
    timeout-minutes: 60
    steps:
      - uses: actions/checkout@v4
      - name: Install toolchain (rust-toolchain.toml)
        run: |
          rustup toolchain install
          rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
      - uses: Swatinem/rust-cache@v2
        with:
          key: daemon
      - name: Format
        run: cargo fmt --all --check
      - name: Clippy (non-UI crates)
        run: cargo clippy --locked --workspace --exclude flc-ui --exclude flotilla --all-targets -- -D warnings
      - name: Test (non-UI crates)
        run: cargo test --locked --workspace --exclude flc-ui --exclude flotilla
      - uses: mlugg/setup-zig@v2
        with:
          version: 0.14.1
      - uses: taiki-e/install-action@v2
        with:
          tool: cargo-zigbuild
      - name: Build static daemons (musl)
        run: |
          set -euo pipefail
          for t in x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do
            cargo zigbuild --locked --release -p flotillad --target "$t"
          done
          mkdir -p dist/daemon
          cp target/x86_64-unknown-linux-musl/release/flotillad dist/daemon/flotillad-x86_64-unknown-linux-musl
          cp target/aarch64-unknown-linux-musl/release/flotillad dist/daemon/flotillad-aarch64-unknown-linux-musl
          file dist/daemon/*
          ls -l dist/daemon
          ./dist/daemon/flotillad-x86_64-unknown-linux-musl version --json
      - uses: actions/upload-artifact@v4
        with:
          name: daemon
          path: dist/daemon/
          if-no-files-found: error

  ui-check:
    name: client — Linux type-check (headless gpui)
    runs-on: ubuntu-latest
    timeout-minutes: 60
    steps:
      - uses: actions/checkout@v4
      - name: Install toolchain (rust-toolchain.toml)
        run: rustup toolchain install
      - uses: Swatinem/rust-cache@v2
        with:
          key: ui-check
      - name: Clippy (UI crates)
        run: cargo clippy --locked -p flc-ui -p flotilla --all-targets -- -D warnings
      - name: Test (UI crates)
        run: cargo test --locked -p flc-ui -p flotilla

  windows:
    name: client — Windows build, self-test, installer
    needs: daemon
    runs-on: windows-latest
    timeout-minutes: 90
    steps:
      - uses: actions/checkout@v4
      - name: Install toolchain (rust-toolchain.toml)
        run: rustup toolchain install
      - uses: Swatinem/rust-cache@v2
        with:
          key: windows
      - uses: actions/download-artifact@v4
        with:
          name: daemon
          path: dist/daemon
      - name: Portable tests on Windows
        run: |
          cargo test --locked -p fl-proto -p fl-transport --lib
          cargo test --locked -p flotilla --bins
      - name: Build flotilla.exe (daemons embedded)
        shell: pwsh
        env:
          FLOTILLA_EMBED_DAEMON_DIR: ${{ github.workspace }}\dist\daemon
        run: cargo build --locked --release -p flotilla
      - name: Self-test release binary
        shell: pwsh
        run: |
          $p = Start-Process -FilePath target\release\flotilla.exe -ArgumentList '--self-test=selftest.json' -Wait -PassThru
          if (Test-Path selftest.json) { Get-Content selftest.json }
          if ($p.ExitCode -ne 0) { throw "self-test failed with exit code $($p.ExitCode)" }
      - name: Build installer (Inno Setup)
        shell: pwsh
        run: |
          $iscc = "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe"
          if (-not (Test-Path $iscc)) { choco install innosetup --no-progress -y | Out-Null }
          $meta = cargo metadata --locked --format-version 1 --no-deps | ConvertFrom-Json
          $ver = ($meta.packages | Where-Object { $_.name -eq 'flotilla' }).version
          New-Item -ItemType Directory -Force dist | Out-Null
          & $iscc "/DAppVersion=$ver" "/DSourceExe=$PWD\target\release\flotilla.exe" "/O$PWD\dist" packaging\windows\flotilla.iss
          if ($LASTEXITCODE -ne 0) { throw "ISCC failed with $LASTEXITCODE" }
          Copy-Item target\release\flotilla.exe dist\flotilla.exe
          Get-ChildItem dist
      - uses: actions/upload-artifact@v4
        with:
          name: flotilla-windows-x64
          path: |
            dist/Flotilla-Setup-x64.exe
            dist/flotilla.exe
          if-no-files-found: error

  release:
    name: publish release
    needs: [daemon, ui-check, windows]
    if: github.event_name == 'push' && (github.ref == 'refs/heads/main' || startsWith(github.ref, 'refs/tags/v'))
    runs-on: ubuntu-latest
    permissions:
      contents: write
    steps:
      - uses: actions/download-artifact@v4
        with:
          path: artifacts
      - name: Collect assets
        run: |
          set -euo pipefail
          mkdir -p upload
          cp artifacts/flotilla-windows-x64/Flotilla-Setup-x64.exe upload/
          cp artifacts/flotilla-windows-x64/flotilla.exe upload/flotilla-x64.exe
          cp artifacts/daemon/flotillad-* upload/
          (cd upload && sha256sum * > SHA256SUMS.txt)
          ls -l upload
      - name: Publish
        env:
          GH_TOKEN: ${{ github.token }}
          GH_REPO: ${{ github.repository }}
        run: |
          set -euo pipefail
          if [[ "$GITHUB_REF" == refs/tags/v* ]]; then
            gh release create "$GITHUB_REF_NAME" upload/* --title "Flotilla $GITHUB_REF_NAME" --generate-notes
          else
            gh release delete nightly --yes --cleanup-tag || true
            gh release create nightly upload/* --prerelease --target "$GITHUB_SHA" \
              --title "Flotilla nightly (${GITHUB_SHA::7})" \
              --notes "Automated build of ${GITHUB_SHA}. Install with Flotilla-Setup-x64.exe."
          fi
```

- [ ] **Step 2: Write `packaging/windows/flotilla.iss`**

```iss
; Flotilla per-user installer (no admin rights required).
; Built by CI:  ISCC /DAppVersion=0.1.0 /DSourceExe=<abs path>\flotilla.exe /O<outdir> flotilla.iss

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef SourceExe
  #define SourceExe "..\..\target\release\flotilla.exe"
#endif

[Setup]
AppId={{6B1F3C2E-8F4A-4C1D-9E7B-2A5D0C9F1E33}
AppName=Flotilla
AppVersion={#AppVersion}
AppVerName=Flotilla {#AppVersion}
AppPublisher=Coaxys
DefaultDirName={localappdata}\Programs\Flotilla
DisableDirPage=yes
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
OutputBaseFilename=Flotilla-Setup-x64
Compression=lzma2/ultra64
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
WizardStyle=modern
UninstallDisplayIcon={app}\flotilla.exe
UninstallDisplayName=Flotilla
CloseApplications=yes

[Tasks]
Name: "desktopicon"; Description: "Create a &desktop shortcut"; Flags: unchecked

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; DestName: "flotilla.exe"; Flags: ignoreversion

[Icons]
Name: "{userprograms}\Flotilla"; Filename: "{app}\flotilla.exe"; AppUserModelID: "Coaxys.Flotilla"
Name: "{userdesktop}\Flotilla"; Filename: "{app}\flotilla.exe"; AppUserModelID: "Coaxys.Flotilla"; Tasks: desktopicon

[Run]
Filename: "{app}\flotilla.exe"; Description: "Launch Flotilla"; Flags: nowait postinstall skipifsilent
```

- [ ] **Step 3: Validate the workflow syntax locally**

```bash
python3 - <<'PY'
import yaml, sys
doc = yaml.safe_load(open(".github/workflows/ci.yml"))
jobs = doc["jobs"]
assert set(jobs) == {"daemon", "ui-check", "windows", "release"}, jobs.keys()
assert jobs["windows"]["needs"] == "daemon"
assert jobs["release"]["permissions"]["contents"] == "write"
print("ci.yml OK:", ", ".join(jobs))
PY
```

Expected: `ci.yml OK: daemon, ui-check, windows, release`. If PyYAML is missing, use `python3 -c "import json,sys"` only to confirm Python works, and note in the report that YAML wasn't validated locally. The coordinator verifies by pushing.

- [ ] **Step 4: Commit** (the coordinator pushes and watches the CI run)

```bash
git add .github/workflows/ci.yml packaging/windows/flotilla.iss
git commit -m "ci: daemon tests + musl builds, Windows client build/self-test/installer, releases

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 8: fld-server — state home, device registry, sessions, listeners

**Files:**
- Create (replace stub): `crates/daemon/fld-server/src/lib.rs`
- Create: `crates/daemon/fld-server/src/{paths.rs, identity.rs, devices.rs, session.rs, listen.rs}`
- Create: `crates/daemon/fld-server/tests/server.rs`

**Interfaces:**
- Consumes:
  - `fl_proto::*` (Tasks 2–3).
  - `fl_transport::conn::{framed, FrameReader, FrameWriter, ConnError}` (Task 4).
  - `fl_transport::quic::{ServerIdentity, server_endpoint, accept_ctl, QuicError, fingerprint_hex}` (Task 5).
- Produces:
  - `fld_server::FlotillaHome`:
    - `new(root)`, `from_env()`
    - path accessors `root, bin_dir, run_dir, state_dir, logs_dir, socket_path, runtime_info_path, cert_path, key_path, devices_path`
    - `ensure_dirs()`
  - `fld_server::{DaemonConfig { home, quic_bind: Option<SocketAddr>, hello_timeout }, DaemonConfig::new(home)}`. The default `quic_bind` is `0.0.0.0:7443` and `hello_timeout` is 10 s.
  - `fld_server::start(DaemonConfig) -> Result<DaemonHandle, ServerError>`
  - `DaemonHandle`:
    - public fields `socket_path: PathBuf`, `quic_addr: Option<SocketAddr>`, `cert_sha256: [u8; 32]`
    - `home()`
    - `async shutdown(self)`: ends every session and removes the socket and `daemon.json`.
  - `fld_server::{RuntimeInfo { pid, quic_port, version, build, socket_path }, read_runtime_info(&FlotillaHome) -> Option<RuntimeInfo>}`
  - `fld_server::ServerError::{Io, AlreadyRunning(PathBuf), Corrupt { path, reason }, NoHome, Quic}`
  - `fld_server::identity::load_or_create(&FlotillaHome) -> Result<ServerIdentity, ServerError>`
  - `fld_server::devices::{DeviceRegistry, DeviceRecord}`:
    - `DeviceRegistry::new(path)`
    - `enroll(name) -> Result<(device_id, token), _>`
    - `verify(id, token) -> Result<bool, _>`
    - `list() -> Result<Vec<DeviceRecord>, _>`
    - `revoke(id) -> Result<bool, _>`

- [ ] **Step 1: Write `paths.rs`**

```rust
//! Layout of the daemon's state directory (`$FLOTILLA_HOME`, default `$HOME/.flotilla`).
use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::ServerError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlotillaHome {
    root: PathBuf,
}

impl FlotillaHome {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// `$FLOTILLA_HOME`, else `$HOME/.flotilla`.
    pub fn from_env() -> Result<Self, ServerError> {
        if let Some(v) = std::env::var_os("FLOTILLA_HOME").filter(|v| !v.is_empty()) {
            return Ok(Self::new(v));
        }
        let home = std::env::var_os("HOME").filter(|v| !v.is_empty()).ok_or(ServerError::NoHome)?;
        Ok(Self::new(PathBuf::from(home).join(".flotilla")))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn bin_dir(&self) -> PathBuf {
        self.root.join("bin")
    }
    pub fn run_dir(&self) -> PathBuf {
        self.root.join("run")
    }
    pub fn state_dir(&self) -> PathBuf {
        self.root.join("state")
    }
    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }
    pub fn socket_path(&self) -> PathBuf {
        self.run_dir().join("daemon.sock")
    }
    pub fn runtime_info_path(&self) -> PathBuf {
        self.run_dir().join("daemon.json")
    }
    pub fn cert_path(&self) -> PathBuf {
        self.state_dir().join("quic-cert.der")
    }
    pub fn key_path(&self) -> PathBuf {
        self.state_dir().join("quic-key.der")
    }
    pub fn devices_path(&self) -> PathBuf {
        self.state_dir().join("devices.json")
    }

    /// Create all directories with mode 0700.
    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        for dir in [self.root.clone(), self.bin_dir(), self.run_dir(), self.state_dir(), self.logs_dir()] {
            std::fs::create_dir_all(&dir)?;
            std::fs::set_permissions(&dir, Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_matches_spec() {
        let h = FlotillaHome::new("/home/u/.flotilla");
        assert_eq!(h.socket_path(), PathBuf::from("/home/u/.flotilla/run/daemon.sock"));
        assert_eq!(h.runtime_info_path(), PathBuf::from("/home/u/.flotilla/run/daemon.json"));
        assert_eq!(h.cert_path(), PathBuf::from("/home/u/.flotilla/state/quic-cert.der"));
        assert_eq!(h.key_path(), PathBuf::from("/home/u/.flotilla/state/quic-key.der"));
        assert_eq!(h.devices_path(), PathBuf::from("/home/u/.flotilla/state/devices.json"));
        assert_eq!(h.bin_dir(), PathBuf::from("/home/u/.flotilla/bin"));
        assert_eq!(h.logs_dir(), PathBuf::from("/home/u/.flotilla/logs"));
    }

    #[test]
    fn ensure_dirs_creates_private_directories() {
        let dir = tempfile::tempdir().unwrap();
        let h = FlotillaHome::new(dir.path().join(".flotilla"));
        h.ensure_dirs().unwrap();
        for d in [h.root().to_path_buf(), h.bin_dir(), h.run_dir(), h.state_dir(), h.logs_dir()] {
            let mode = std::fs::metadata(&d).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{}", d.display());
        }
    }
}
```

- [ ] **Step 2: Write `identity.rs`**

```rust
//! The daemon's QUIC certificate: generated once, then reused so pinned
//! fingerprints stay valid across restarts and upgrades.
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use fl_transport::quic::ServerIdentity;

use crate::{FlotillaHome, ServerError};

/// Requires `home.ensure_dirs()` to have run.
pub fn load_or_create(home: &FlotillaHome) -> Result<ServerIdentity, ServerError> {
    let (cert_path, key_path) = (home.cert_path(), home.key_path());
    if cert_path.exists() && key_path.exists() {
        let cert_der = std::fs::read(&cert_path)?;
        let key_der = std::fs::read(&key_path)?;
        if cert_der.is_empty() || key_der.is_empty() {
            return Err(ServerError::Corrupt { path: cert_path, reason: "empty certificate or key file".into() });
        }
        return Ok(ServerIdentity { cert_der, key_der });
    }
    let identity = ServerIdentity::generate()?;
    write_private(&key_path, &identity.key_der)?;
    write_private(&cert_path, &identity.cert_der)?;
    Ok(identity)
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    {
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn home() -> (tempfile::TempDir, FlotillaHome) {
        let dir = tempfile::tempdir().unwrap();
        let h = FlotillaHome::new(dir.path().join(".flotilla"));
        h.ensure_dirs().unwrap();
        (dir, h)
    }

    #[test]
    fn creates_once_then_reuses() {
        let (_d, h) = home();
        let a = load_or_create(&h).unwrap();
        let b = load_or_create(&h).unwrap();
        assert_eq!(a.fingerprint(), b.fingerprint());
    }

    #[test]
    fn key_and_cert_are_private() {
        let (_d, h) = home();
        load_or_create(&h).unwrap();
        for p in [h.key_path(), h.cert_path()] {
            assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn empty_files_are_corrupt() {
        let (_d, h) = home();
        std::fs::write(h.cert_path(), b"").unwrap();
        std::fs::write(h.key_path(), b"").unwrap();
        assert!(matches!(load_or_create(&h), Err(ServerError::Corrupt { .. })));
    }
}
```

- [ ] **Step 3: Write `devices.rs`**

```rust
//! Enrolled client devices. Tokens are random 32-byte secrets shown once;
//! only their SHA-256 is stored. Writes are atomic (temp file + rename), and
//! read-modify-write cycles hold an exclusive lock on `devices.lock`, so
//! concurrent `flotillad enroll` processes and the running daemon never
//! lose or half-read an entry.
use std::fs::File;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ServerError;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRecord {
    pub id: String,
    pub name: String,
    pub token_sha256: String,
    pub created_unix: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct DevicesFile {
    version: u32,
    devices: Vec<DeviceRecord>,
}

pub struct DeviceRegistry {
    path: PathBuf,
}

impl DeviceRegistry {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn lock(&self, exclusive: bool) -> Result<File, ServerError> {
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(self.path.with_extension("lock"))?;
        if exclusive {
            f.lock()?;
        } else {
            f.lock_shared()?;
        }
        Ok(f)
    }

    fn load(&self) -> Result<DevicesFile, ServerError> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| ServerError::Corrupt { path: self.path.clone(), reason: e.to_string() }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DevicesFile { version: 1, devices: Vec::new() }),
            Err(e) => Err(e.into()),
        }
    }

    fn store(&self, file: &DevicesFile) -> Result<(), ServerError> {
        crate::write_json_atomic(&self.path, file)?;
        Ok(())
    }

    /// Register a device; returns `(device_id, token)`. The token is not stored.
    pub fn enroll(&self, name: &str) -> Result<(String, String), ServerError> {
        let _lock = self.lock(true)?;
        let mut file = self.load()?;
        let id = uuid::Uuid::new_v4().to_string();
        let mut raw = [0u8; 32];
        getrandom::fill(&mut raw).map_err(|e| std::io::Error::other(e.to_string()))?;
        let token = hex::encode(raw);
        let created_unix = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        file.version = 1;
        file.devices.push(DeviceRecord { id: id.clone(), name: name.to_string(), token_sha256: token_hash(&token), created_unix });
        self.store(&file)?;
        Ok((id, token))
    }

    pub fn verify(&self, device_id: &str, token: &str) -> Result<bool, ServerError> {
        let _lock = self.lock(false)?;
        let file = self.load()?;
        let want = token_hash(token);
        Ok(file.devices.iter().any(|d| d.id == device_id && constant_time_eq(d.token_sha256.as_bytes(), want.as_bytes())))
    }

    pub fn list(&self) -> Result<Vec<DeviceRecord>, ServerError> {
        let _lock = self.lock(false)?;
        Ok(self.load()?.devices)
    }

    pub fn revoke(&self, device_id: &str) -> Result<bool, ServerError> {
        let _lock = self.lock(true)?;
        let mut file = self.load()?;
        let before = file.devices.len();
        file.devices.retain(|d| d.id != device_id);
        let removed = file.devices.len() != before;
        if removed {
            self.store(&file)?;
        }
        Ok(removed)
    }
}

fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn registry() -> (tempfile::TempDir, DeviceRegistry) {
        let dir = tempfile::tempdir().unwrap();
        let reg = DeviceRegistry::new(dir.path().join("devices.json"));
        (dir, reg)
    }

    #[test]
    fn enroll_then_verify() {
        let (_d, reg) = registry();
        let (id, token) = reg.enroll("laptop").unwrap();
        assert_eq!(token.len(), 64);
        assert!(reg.verify(&id, &token).unwrap());
        let list = reg.list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "laptop");
    }

    #[test]
    fn wrong_token_or_device_is_refused() {
        let (_d, reg) = registry();
        let (id, token) = reg.enroll("laptop").unwrap();
        assert!(!reg.verify(&id, &"0".repeat(64)).unwrap());
        assert!(!reg.verify("someone-else", &token).unwrap());
        assert!(!reg.verify(&id, "").unwrap());
    }

    #[test]
    fn verify_on_missing_file_is_false() {
        let (_d, reg) = registry();
        assert!(!reg.verify("x", "y").unwrap());
    }

    #[test]
    fn tokens_are_stored_only_as_hashes() {
        let (dir, reg) = registry();
        let (_id, token) = reg.enroll("laptop").unwrap();
        let raw = std::fs::read_to_string(dir.path().join("devices.json")).unwrap();
        assert!(!raw.contains(&token));
        assert!(raw.contains(&token_hash(&token)));
    }

    #[test]
    fn devices_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, reg) = registry();
        reg.enroll("laptop").unwrap();
        let mode = std::fs::metadata(dir.path().join("devices.json")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn revoke_removes_device() {
        let (_d, reg) = registry();
        let (id, token) = reg.enroll("laptop").unwrap();
        assert!(reg.revoke(&id).unwrap());
        assert!(!reg.revoke(&id).unwrap());
        assert!(!reg.verify(&id, &token).unwrap());
    }

    #[test]
    fn corrupt_file_is_reported_not_overwritten() {
        let (dir, reg) = registry();
        std::fs::write(dir.path().join("devices.json"), b"{not json").unwrap();
        assert!(matches!(reg.verify("a", "b"), Err(ServerError::Corrupt { .. })));
        assert!(matches!(reg.enroll("x"), Err(ServerError::Corrupt { .. })));
        assert_eq!(std::fs::read(dir.path().join("devices.json")).unwrap(), b"{not json");
    }

    /// Review Focus #4.
    #[test]
    fn concurrent_enrollments_are_all_kept_and_readers_never_see_partial_state() {
        let (_d, reg) = registry();
        let reg = Arc::new(reg);
        let stop = Arc::new(AtomicBool::new(false));
        let reader = {
            let (reg, stop) = (reg.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut reads = 0u32;
                while !stop.load(Ordering::SeqCst) {
                    reg.list().expect("a reader must never see a partial devices.json");
                    reads += 1;
                }
                reads
            })
        };
        let writers: Vec<_> = (0..20)
            .map(|i| {
                let reg = reg.clone();
                std::thread::spawn(move || reg.enroll(&format!("pc-{i}")).unwrap())
            })
            .collect();
        let creds: Vec<_> = writers.into_iter().map(|h| h.join().unwrap()).collect();
        stop.store(true, Ordering::SeqCst);
        assert!(reader.join().unwrap() > 0);
        assert_eq!(reg.list().unwrap().len(), 20);
        for (id, token) in &creds {
            assert!(reg.verify(id, token).unwrap());
        }
    }
}
```

- [ ] **Step 4: Write `session.rs`**

```rust
//! One control session: Hello → (auth) → Welcome, then Ping/Pong and RPCs.
use std::sync::Arc;
use std::time::{Duration, Instant};

use fl_proto::{
    ClientMsg, DaemonInfo, Hello, PRODUCT_VERSION, PROTO, RejectReason, Request, Response, RpcError, ServerMsg,
    Welcome, build_id,
};
use fl_transport::conn::{ConnError, FrameReader, FrameWriter};
use tokio_util::sync::CancellationToken;

use crate::devices::DeviceRegistry;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerKind {
    /// Unix socket. The peer came through SSH (already authenticated) or is local.
    Local,
    /// QUIC. The peer must present an enrolled device token.
    Quic,
}

pub struct InfoSource {
    pub started: Instant,
    pub hostname: String,
    pub quic_port: Option<u16>,
    pub socket_path: String,
}

pub struct SessionCtx {
    pub registry: Arc<DeviceRegistry>,
    pub hello_timeout: Duration,
    pub info: InfoSource,
    /// Cancelled when the daemon shuts down; every session then ends.
    pub shutdown: CancellationToken,
}

pub async fn serve_connection(
    ctx: Arc<SessionCtx>,
    mut reader: FrameReader,
    mut writer: FrameWriter,
    peer: PeerKind,
) -> Result<(), ConnError> {
    let first = tokio::select! {
        _ = ctx.shutdown.cancelled() => return Ok(()),
        r = tokio::time::timeout(ctx.hello_timeout, reader.recv::<ClientMsg>()) => r,
    };
    let hello = match first {
        Err(_elapsed) => return reject(&mut writer, RejectReason::HelloTimeout).await,
        Ok(Err(e)) => return Err(e),
        Ok(Ok(ClientMsg::Hello(h))) => h,
        Ok(Ok(_)) => return reject(&mut writer, RejectReason::ExpectedHello).await,
    };
    if !PROTO.is_compatible_with(hello.proto) {
        return reject(&mut writer, RejectReason::ProtoMismatch { server: PROTO }).await;
    }
    if peer == PeerKind::Quic && !token_ok(&ctx, &hello).await {
        return reject(&mut writer, RejectReason::BadToken).await;
    }
    let mut server_session_id = [0u8; 16];
    let _ = getrandom::fill(&mut server_session_id);
    writer
        .send(&ServerMsg::Welcome(Welcome {
            daemon_version: PRODUCT_VERSION.to_string(),
            build: build_id().to_string(),
            proto: PROTO,
            hostname: ctx.info.hostname.clone(),
            server_session_id,
        }))
        .await?;
    loop {
        let msg = tokio::select! {
            _ = ctx.shutdown.cancelled() => return Ok(()),
            m = reader.recv::<ClientMsg>() => m,
        };
        match msg {
            Ok(ClientMsg::Ping { nonce }) => writer.send(&ServerMsg::Pong { nonce }).await?,
            Ok(ClientMsg::Rpc { id, req }) => {
                let result = handle_request(&ctx, req);
                writer.send(&ServerMsg::RpcResult { id, result }).await?;
            }
            Ok(ClientMsg::Hello(_)) => {} // a repeated Hello is harmless; ignore it
            Err(ConnError::Closed) => return Ok(()),
            Err(e) => return Err(e),
        }
    }
}

async fn reject(writer: &mut FrameWriter, reason: RejectReason) -> Result<(), ConnError> {
    writer.send(&ServerMsg::Rejected(reason)).await?;
    let _ = writer.shutdown().await;
    Ok(())
}

async fn token_ok(ctx: &SessionCtx, hello: &Hello) -> bool {
    let (Some(device), Some(token)) = (hello.device_id.clone(), hello.token.clone()) else {
        return false;
    };
    let registry = ctx.registry.clone();
    tokio::task::spawn_blocking(move || registry.verify(&device, &token))
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or(false)
}

fn handle_request(ctx: &SessionCtx, req: Request) -> Result<Response, RpcError> {
    match req {
        Request::DaemonInfo => Ok(Response::DaemonInfo(DaemonInfo {
            version: PRODUCT_VERSION.to_string(),
            build: build_id().to_string(),
            hostname: ctx.info.hostname.clone(),
            pid: std::process::id(),
            uptime_secs: ctx.info.started.elapsed().as_secs(),
            quic_port: ctx.info.quic_port,
            socket_path: ctx.info.socket_path.clone(),
        })),
    }
}

pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}
```

- [ ] **Step 5: Write `listen.rs`**

```rust
//! Accept loops for the Unix socket and QUIC.
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use fl_transport::conn::framed;
use fl_transport::quic::accept_ctl;
use tokio::net::UnixListener;

use crate::ServerError;
use crate::session::{PeerKind, SessionCtx, serve_connection};

/// Bind the control socket, replacing a stale socket file left by a crash.
/// Refuses with `AlreadyRunning` if a live daemon answers on it.
pub fn bind_unix(path: &Path) -> Result<UnixListener, ServerError> {
    if path.exists() {
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            return Err(ServerError::AlreadyRunning(path.to_path_buf()));
        }
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

pub async fn unix_accept_loop(listener: UnixListener, ctx: Arc<SessionCtx>) {
    loop {
        let accepted = tokio::select! {
            _ = ctx.shutdown.cancelled() => break,
            a = listener.accept() => a,
        };
        match accepted {
            Ok((stream, _addr)) => {
                let (r, w) = stream.into_split();
                let (reader, writer) = framed(r, w);
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    if let Err(e) = serve_connection(ctx, reader, writer, PeerKind::Local).await {
                        tracing::debug!("local session ended: {e}");
                    }
                });
            }
            Err(e) => {
                tracing::warn!("unix accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

pub async fn quic_accept_loop(endpoint: quinn::Endpoint, ctx: Arc<SessionCtx>) {
    loop {
        let incoming = tokio::select! {
            _ = ctx.shutdown.cancelled() => break,
            i = endpoint.accept() => match i {
                Some(i) => i,
                None => break,
            },
        };
        let ctx = ctx.clone();
        tokio::spawn(async move {
            let Ok(conn) = incoming.await else { return };
            let Ok((send, recv)) = accept_ctl(&conn).await else { return };
            let (reader, writer) = framed(recv, send);
            if let Err(e) = serve_connection(ctx, reader, writer, PeerKind::Quic).await {
                tracing::debug!("quic session ended: {e}");
            }
            // Let the client read our last frame and close first; then close.
            let _ = tokio::time::timeout(Duration::from_secs(5), conn.closed()).await;
            conn.close(0u32.into(), b"bye");
        });
    }
}
```

- [ ] **Step 6: Write `lib.rs`**

```rust
//! Flotilla daemon core: state home, device registry, control sessions, and
//! listeners (Unix socket for SSH-forwarded peers, QUIC for direct peers).
pub mod devices;
pub mod identity;
pub mod listen;
pub mod paths;
pub mod session;

use std::io::Write;
use std::net::SocketAddr;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub use paths::FlotillaHome;

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("another flotillad is already serving {}", .0.display())]
    AlreadyRunning(PathBuf),
    #[error("corrupt state file {}: {reason}", .path.display())]
    Corrupt { path: PathBuf, reason: String },
    #[error("neither FLOTILLA_HOME nor HOME is set")]
    NoHome,
    #[error(transparent)]
    Quic(#[from] fl_transport::quic::QuicError),
}

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub home: FlotillaHome,
    /// `None` disables QUIC.
    pub quic_bind: Option<SocketAddr>,
    /// How long a new connection may take to send `Hello`.
    pub hello_timeout: Duration,
}

impl DaemonConfig {
    pub fn new(home: FlotillaHome) -> Self {
        Self { home, quic_bind: Some(SocketAddr::from(([0, 0, 0, 0], 7443))), hello_timeout: Duration::from_secs(10) }
    }
}

/// Contents of `run/daemon.json` while the daemon runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeInfo {
    pub pid: u32,
    pub quic_port: Option<u16>,
    pub version: String,
    pub build: String,
    pub socket_path: String,
}

pub struct DaemonHandle {
    pub socket_path: PathBuf,
    pub quic_addr: Option<SocketAddr>,
    pub cert_sha256: [u8; 32],
    home: FlotillaHome,
    cancel: CancellationToken,
    tasks: Vec<JoinHandle<()>>,
    quic: Option<quinn::Endpoint>,
}

/// Start listening. Fails with `AlreadyRunning` if another daemon owns the socket.
/// A QUIC bind failure (e.g. port in use) only disables QUIC, with a warning.
pub async fn start(config: DaemonConfig) -> Result<DaemonHandle, ServerError> {
    let home = config.home.clone();
    home.ensure_dirs()?;
    let identity = identity::load_or_create(&home)?;
    let socket_path = home.socket_path();
    let unix = listen::bind_unix(&socket_path)?;
    let quic = match config.quic_bind {
        Some(bind) => match fl_transport::quic::server_endpoint(&identity, bind) {
            Ok(ep) => Some(ep),
            Err(e) => {
                tracing::warn!("QUIC disabled: cannot listen on {bind}: {e}");
                None
            }
        },
        None => None,
    };
    let quic_addr = quic.as_ref().and_then(|ep| ep.local_addr().ok());
    let cancel = CancellationToken::new();
    let ctx = Arc::new(session::SessionCtx {
        registry: Arc::new(devices::DeviceRegistry::new(home.devices_path())),
        hello_timeout: config.hello_timeout,
        info: session::InfoSource {
            started: Instant::now(),
            hostname: session::hostname(),
            quic_port: quic_addr.map(|a| a.port()),
            socket_path: socket_path.display().to_string(),
        },
        shutdown: cancel.clone(),
    });
    let mut tasks = vec![tokio::spawn(listen::unix_accept_loop(unix, ctx.clone()))];
    if let Some(ep) = &quic {
        tasks.push(tokio::spawn(listen::quic_accept_loop(ep.clone(), ctx.clone())));
    }
    let info = RuntimeInfo {
        pid: std::process::id(),
        quic_port: quic_addr.map(|a| a.port()),
        version: fl_proto::PRODUCT_VERSION.to_string(),
        build: fl_proto::build_id().to_string(),
        socket_path: socket_path.display().to_string(),
    };
    write_json_atomic(&home.runtime_info_path(), &info)?;
    Ok(DaemonHandle { socket_path, quic_addr, cert_sha256: identity.fingerprint(), home, cancel, tasks, quic })
}

impl DaemonHandle {
    pub fn home(&self) -> &FlotillaHome {
        &self.home
    }

    /// Stop accepting, end every session, remove the socket and runtime info.
    pub async fn shutdown(mut self) {
        self.cancel.cancel();
        if let Some(ep) = self.quic.take() {
            ep.close(0u32.into(), b"daemon shutting down");
        }
        for t in std::mem::take(&mut self.tasks) {
            let _ = t.await;
        }
        let _ = std::fs::remove_file(&self.socket_path);
        let _ = std::fs::remove_file(self.home.runtime_info_path());
    }
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

pub fn read_runtime_info(home: &FlotillaHome) -> Option<RuntimeInfo> {
    let bytes = std::fs::read(home.runtime_info_path()).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Write JSON through a unique temp file + rename (mode 0600), so readers
/// never observe a partially written file.
pub(crate) fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp-{}-{n}", std::process::id()));
    {
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}
```

- [ ] **Step 7: Run the unit tests**

Run: `cargo test -p fld-server --lib`
Expected: tests pass in `paths`, `identity` and `devices` (13 tests), including the concurrency test.

- [ ] **Step 8: Write the integration tests** — `crates/daemon/fld-server/tests/server.rs`

```rust
//! Daemon integration tests: real Unix socket and real QUIC, in-process.
use std::path::Path;
use std::time::Duration;

use fl_proto::{ClientMsg, Hello, PROTO, ProtoVersion, RejectReason, Request, Response, ServerMsg};
use fl_transport::conn::{ConnError, FrameReader, FrameWriter, framed};
use fl_transport::quic;
use fld_server::devices::DeviceRegistry;
use fld_server::{DaemonConfig, DaemonHandle, FlotillaHome, ServerError};
use tokio::net::UnixStream;

fn home_in(dir: &Path) -> FlotillaHome {
    FlotillaHome::new(dir.join(".flotilla"))
}

async fn start_daemon(dir: &Path) -> Result<DaemonHandle, ServerError> {
    let mut config = DaemonConfig::new(home_in(dir));
    config.quic_bind = Some("127.0.0.1:0".parse().unwrap());
    config.hello_timeout = Duration::from_millis(300);
    fld_server::start(config).await
}

async fn local(d: &DaemonHandle) -> (FrameReader, FrameWriter) {
    let s = UnixStream::connect(&d.socket_path).await.unwrap();
    let (r, w) = s.into_split();
    framed(r, w)
}

struct QuicLink {
    _endpoint: quinn::Endpoint,
    _conn: quinn::Connection,
    reader: FrameReader,
    writer: FrameWriter,
}

async fn over_quic(d: &DaemonHandle) -> QuicLink {
    let (endpoint, conn) = quic::connect(d.quic_addr.unwrap(), d.cert_sha256, Duration::from_secs(5)).await.unwrap();
    let (send, recv) = quic::open_ctl(&conn).await.unwrap();
    let (reader, writer) = framed(recv, send);
    QuicLink { _endpoint: endpoint, _conn: conn, reader, writer }
}

fn hello_with(proto: ProtoVersion, device_id: Option<&str>, token: Option<&str>) -> ClientMsg {
    ClientMsg::Hello(Hello {
        client_version: "test".into(),
        proto,
        device_id: device_id.map(Into::into),
        token: token.map(Into::into),
        client_session_id: [7; 16],
    })
}

fn hello() -> ClientMsg {
    hello_with(PROTO, None, None)
}

async fn recv(reader: &mut FrameReader) -> ServerMsg {
    tokio::time::timeout(Duration::from_secs(5), reader.recv::<ServerMsg>())
        .await
        .expect("reply within 5 s")
        .expect("valid reply")
}

#[tokio::test]
async fn local_hello_gets_welcome_and_ping_pong() {
    let dir = tempfile::tempdir().unwrap();
    let d = start_daemon(dir.path()).await.unwrap();
    let (mut r, mut w) = local(&d).await;
    w.send(&hello()).await.unwrap();
    match recv(&mut r).await {
        ServerMsg::Welcome(welcome) => {
            assert_eq!(welcome.proto, PROTO);
            assert!(!welcome.hostname.is_empty());
        }
        other => panic!("expected Welcome, got {other:?}"),
    }
    w.send(&ClientMsg::Ping { nonce: 42 }).await.unwrap();
    assert_eq!(recv(&mut r).await, ServerMsg::Pong { nonce: 42 });
    d.shutdown().await;
}

#[tokio::test]
async fn daemon_info_rpc_reports_pid_and_quic_port() {
    let dir = tempfile::tempdir().unwrap();
    let d = start_daemon(dir.path()).await.unwrap();
    let (mut r, mut w) = local(&d).await;
    w.send(&hello()).await.unwrap();
    recv(&mut r).await;
    w.send(&ClientMsg::Rpc { id: 5, req: Request::DaemonInfo }).await.unwrap();
    match recv(&mut r).await {
        ServerMsg::RpcResult { id: 5, result: Ok(Response::DaemonInfo(info)) } => {
            assert_eq!(info.pid, std::process::id());
            assert_eq!(info.quic_port, d.quic_addr.map(|a| a.port()));
            assert_eq!(info.socket_path, d.socket_path.display().to_string());
            assert_eq!(info.version, fl_proto::PRODUCT_VERSION);
        }
        other => panic!("expected DaemonInfo, got {other:?}"),
    }
    d.shutdown().await;
}

#[tokio::test]
async fn quic_with_enrolled_token_is_welcomed() {
    let dir = tempfile::tempdir().unwrap();
    let d = start_daemon(dir.path()).await.unwrap();
    let (id, token) = DeviceRegistry::new(home_in(dir.path()).devices_path()).enroll("pc").unwrap();
    let mut q = over_quic(&d).await;
    q.writer.send(&hello_with(PROTO, Some(&id), Some(&token))).await.unwrap();
    assert!(matches!(recv(&mut q.reader).await, ServerMsg::Welcome(_)));
    q.writer.send(&ClientMsg::Ping { nonce: 1 }).await.unwrap();
    assert_eq!(recv(&mut q.reader).await, ServerMsg::Pong { nonce: 1 });
    d.shutdown().await;
}

#[tokio::test]
async fn quic_with_bad_token_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let d = start_daemon(dir.path()).await.unwrap();
    let (id, _token) = DeviceRegistry::new(home_in(dir.path()).devices_path()).enroll("pc").unwrap();
    let mut q = over_quic(&d).await;
    q.writer.send(&hello_with(PROTO, Some(&id), Some(&"00".repeat(32)))).await.unwrap();
    assert_eq!(recv(&mut q.reader).await, ServerMsg::Rejected(RejectReason::BadToken));
    d.shutdown().await;
}

#[tokio::test]
async fn quic_without_token_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let d = start_daemon(dir.path()).await.unwrap();
    let mut q = over_quic(&d).await;
    q.writer.send(&hello()).await.unwrap();
    assert_eq!(recv(&mut q.reader).await, ServerMsg::Rejected(RejectReason::BadToken));
    d.shutdown().await;
}

#[tokio::test]
async fn silent_client_gets_hello_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let d = start_daemon(dir.path()).await.unwrap();
    let (mut r, _w) = local(&d).await;
    assert_eq!(recv(&mut r).await, ServerMsg::Rejected(RejectReason::HelloTimeout));
    assert!(matches!(r.recv::<ServerMsg>().await, Err(ConnError::Closed)));
    d.shutdown().await;
}

#[tokio::test]
async fn first_message_must_be_hello() {
    let dir = tempfile::tempdir().unwrap();
    let d = start_daemon(dir.path()).await.unwrap();
    let (mut r, mut w) = local(&d).await;
    w.send(&ClientMsg::Ping { nonce: 1 }).await.unwrap();
    assert_eq!(recv(&mut r).await, ServerMsg::Rejected(RejectReason::ExpectedHello));
    d.shutdown().await;
}

#[tokio::test]
async fn incompatible_protocol_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let d = start_daemon(dir.path()).await.unwrap();
    let (mut r, mut w) = local(&d).await;
    w.send(&hello_with(ProtoVersion { major: PROTO.major + 1, minor: 0 }, None, None)).await.unwrap();
    assert_eq!(recv(&mut r).await, ServerMsg::Rejected(RejectReason::ProtoMismatch { server: PROTO }));
    d.shutdown().await;
}

/// Review Focus #3.
#[tokio::test]
async fn stale_socket_is_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let home = home_in(dir.path());
    home.ensure_dirs().unwrap();
    {
        // Bind and drop: the socket file stays behind with nobody listening.
        let _l = std::os::unix::net::UnixListener::bind(home.socket_path()).unwrap();
    }
    assert!(home.socket_path().exists());
    let d = start_daemon(dir.path()).await.expect("a stale socket must not block startup");
    let (mut r, mut w) = local(&d).await;
    w.send(&hello()).await.unwrap();
    assert!(matches!(recv(&mut r).await, ServerMsg::Welcome(_)));
    d.shutdown().await;
}

/// Review Focus #3.
#[tokio::test]
async fn second_daemon_on_same_home_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let d = start_daemon(dir.path()).await.unwrap();
    let err = start_daemon(dir.path()).await.err().expect("second daemon must be refused");
    assert!(matches!(err, ServerError::AlreadyRunning(_)), "got {err}");
    let (mut r, mut w) = local(&d).await;
    w.send(&hello()).await.unwrap();
    assert!(matches!(recv(&mut r).await, ServerMsg::Welcome(_)), "first daemon must keep working");
    d.shutdown().await;
}

#[tokio::test]
async fn shutdown_ends_sessions_and_removes_files() {
    let dir = tempfile::tempdir().unwrap();
    let home = home_in(dir.path());
    let d = start_daemon(dir.path()).await.unwrap();
    assert!(fld_server::read_runtime_info(&home).is_some());
    let (mut r, mut w) = local(&d).await;
    w.send(&hello()).await.unwrap();
    recv(&mut r).await;
    let socket = d.socket_path.clone();
    d.shutdown().await;
    let next = tokio::time::timeout(Duration::from_secs(2), r.recv::<ServerMsg>())
        .await
        .expect("open sessions must end on shutdown");
    assert!(matches!(next, Err(ConnError::Closed)), "got {next:?}");
    assert!(!socket.exists());
    assert!(fld_server::read_runtime_info(&home).is_none());
}

#[tokio::test]
async fn identity_is_stable_across_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let first = start_daemon(dir.path()).await.unwrap();
    let fp = first.cert_sha256;
    first.shutdown().await;
    let second = start_daemon(dir.path()).await.unwrap();
    assert_eq!(second.cert_sha256, fp);
    second.shutdown().await;
}

#[tokio::test]
async fn runtime_info_describes_the_running_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let d = start_daemon(dir.path()).await.unwrap();
    let info = fld_server::read_runtime_info(&home_in(dir.path())).unwrap();
    assert_eq!(info.pid, std::process::id());
    assert_eq!(info.quic_port, d.quic_addr.map(|a| a.port()));
    assert_eq!(info.socket_path, d.socket_path.display().to_string());
    d.shutdown().await;
}

#[tokio::test]
async fn quic_port_in_use_disables_quic_but_daemon_runs() {
    let dir = tempfile::tempdir().unwrap();
    let blocker = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut config = DaemonConfig::new(home_in(dir.path()));
    config.quic_bind = Some(blocker.local_addr().unwrap());
    let d = fld_server::start(config).await.unwrap();
    assert!(d.quic_addr.is_none());
    let (mut r, mut w) = local(&d).await;
    w.send(&hello()).await.unwrap();
    assert!(matches!(recv(&mut r).await, ServerMsg::Welcome(_)));
    d.shutdown().await;
}
```

- [ ] **Step 9: Run all fld-server tests**

Run: `cargo test -p fld-server`
Expected: 13 unit tests and 14 integration tests pass.

- [ ] **Step 10: Commit**

```bash
cargo fmt -p fld-server && cargo clippy -p fld-server --all-targets -- -D warnings
git add crates/daemon/fld-server/src crates/daemon/fld-server/tests
git commit -m "feat(fld-server): state home, device registry, control sessions, unix+QUIC listeners

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 9: flotillad — the daemon CLI

**Files:**
- Create (replace stub): `crates/daemon/flotillad/src/main.rs`
- Create: `crates/daemon/flotillad/tests/cli.rs`

**Interfaces:**
- Consumes: `fld_server::*` (Task 8), `fl_proto::*`, `fl_transport::{conn, quic}`.
- Produces (the CLI contract the client's bootstrap relies on, Task 10):
  - `flotillad serve [--quic ADDR | env FLOTILLA_QUIC_BIND] [--no-quic]`: runs until SIGTERM or Ctrl-C.
  - `flotillad version --json` prints `VersionInfo` JSON.
  - `flotillad status --json`:
    - exit 0: prints `DaemonInfo` JSON;
    - exit 1: prints `{"running":false,"error":"…"}`.
  - `flotillad enroll --device-name NAME` prints `EnrollResponse` JSON.
  - `flotillad attach` bridges stdin/stdout to `run/daemon.sock` and exits when either side closes.
  - `flotillad install-service [--no-start] [--no-systemd]`:
    - writes `$XDG_CONFIG_HOME|~/.config/systemd/user/flotillad.service`;
    - starts the daemon through `systemctl --user`, or as a detached `setsid` process;
    - waits until it answers;
    - prints `{"mode":"systemd"|"detached"|"none","unit_path":…,"linger":bool,"running":bool}`;
    - exits 1 if the daemon is not running.
  - `flotillad stop`: SIGTERM to the pid in `run/daemon.json`, but only if that pid is a live `flotillad serve`. Prints `{"stopped":bool}`.

- [ ] **Step 1: Write the failing CLI tests** — `crates/daemon/flotillad/tests/cli.rs`

```rust
//! End-to-end tests of the flotillad binary.
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use fl_proto::{ClientMsg, DaemonInfo, EnrollResponse, Hello, PROTO, ServerMsg, VersionInfo};
use fl_transport::conn::framed;
use fl_transport::quic;

const BIN: &str = env!("CARGO_BIN_EXE_flotillad");

fn cmd_with(bin: &Path, home: &Path) -> tokio::process::Command {
    let mut c = tokio::process::Command::new(bin);
    c.env("HOME", home)
        .env("FLOTILLA_HOME", home.join(".flotilla"))
        .env_remove("FLOTILLA_QUIC_BIND")
        .env_remove("XDG_CONFIG_HOME")
        .kill_on_drop(true);
    c
}

fn cmd(home: &Path) -> tokio::process::Command {
    cmd_with(Path::new(BIN), home)
}

async fn wait_for(cond: impl Fn() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn hello(device_id: Option<String>, token: Option<String>) -> ClientMsg {
    ClientMsg::Hello(Hello { client_version: "cli-test".into(), proto: PROTO, device_id, token, client_session_id: [3; 16] })
}

/// Runs `flotillad stop` for a home when dropped (cleans up detached daemons).
struct StopOnDrop(PathBuf);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        let _ = std::process::Command::new(BIN)
            .arg("stop")
            .env("HOME", &self.0)
            .env("FLOTILLA_HOME", self.0.join(".flotilla"))
            .output();
    }
}

#[tokio::test]
async fn version_json_matches_versioninfo() {
    let dir = tempfile::tempdir().unwrap();
    let out = cmd(dir.path()).args(["version", "--json"]).output().await.unwrap();
    assert!(out.status.success());
    let v: VersionInfo = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v, VersionInfo::current());
}

#[tokio::test]
async fn status_reports_not_running() {
    let dir = tempfile::tempdir().unwrap();
    let out = cmd(dir.path()).args(["status", "--json"]).output().await.unwrap();
    assert_eq!(out.status.code(), Some(1));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["running"], false);
}

#[tokio::test]
async fn serve_status_enroll_quic_then_stop() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let mut daemon = cmd(home).args(["serve", "--quic", "127.0.0.1:0"]).stderr(Stdio::null()).spawn().unwrap();
    let sock = home.join(".flotilla/run/daemon.sock");
    wait_for(|| sock.exists(), "daemon socket").await;

    let out = cmd(home).args(["status", "--json"]).output().await.unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    let info: DaemonInfo = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(Some(info.pid), daemon.id());
    let port = info.quic_port.expect("QUIC enabled");

    let out = cmd(home).args(["enroll", "--device-name", "test pc"]).output().await.unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let creds: EnrollResponse = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(creds.quic_port, Some(port));
    assert_eq!(creds.token.len(), 64);
    assert_eq!(creds.socket_path, sock.display().to_string());

    let fp = quic::parse_fingerprint(&creds.cert_sha256).unwrap();
    let (_endpoint, conn) = quic::connect(([127, 0, 0, 1], port).into(), fp, Duration::from_secs(5)).await.unwrap();
    let (send, recv) = quic::open_ctl(&conn).await.unwrap();
    let (mut reader, mut writer) = framed(recv, send);
    writer.send(&hello(Some(creds.device_id.clone()), Some(creds.token.clone()))).await.unwrap();
    assert!(matches!(reader.recv::<ServerMsg>().await.unwrap(), ServerMsg::Welcome(_)));

    let out = cmd(home).arg("stop").output().await.unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let status = tokio::time::timeout(Duration::from_secs(10), daemon.wait()).await.expect("daemon exits").unwrap();
    assert!(status.success());
    assert!(!sock.exists());
}

#[tokio::test]
async fn attach_bridges_stdio_to_the_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let mut daemon = cmd(home).args(["serve", "--no-quic"]).stderr(Stdio::null()).spawn().unwrap();
    let sock = home.join(".flotilla/run/daemon.sock");
    wait_for(|| sock.exists(), "daemon socket").await;

    let mut attach = cmd(home).arg("attach").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let (mut reader, mut writer) = framed(attach.stdout.take().unwrap(), attach.stdin.take().unwrap());
    writer.send(&hello(None, None)).await.unwrap();
    assert!(matches!(reader.recv::<ServerMsg>().await.unwrap(), ServerMsg::Welcome(_)));
    writer.send(&ClientMsg::Ping { nonce: 9 }).await.unwrap();
    assert_eq!(reader.recv::<ServerMsg>().await.unwrap(), ServerMsg::Pong { nonce: 9 });
    drop(writer); // closes attach's stdin
    let status = tokio::time::timeout(Duration::from_secs(10), attach.wait()).await.expect("attach exits on EOF").unwrap();
    assert!(status.success());

    daemon.start_kill().unwrap();
    let _ = daemon.wait().await;
}

#[tokio::test]
async fn install_service_detached_starts_daemon_and_stop_stops_it() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let bin_dir = home.join(".flotilla/bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let installed = bin_dir.join("flotillad");
    std::fs::copy(BIN, &installed).unwrap();
    let _guard = StopOnDrop(home.to_path_buf());

    // Exec right after copy can hit ETXTBSY if a concurrently forked test child
    // briefly inherited the write fd; retry a few times.
    let mut attempt = 0;
    let out = loop {
        let mut c = cmd_with(&installed, home);
        c.args(["install-service", "--no-systemd"]).env("FLOTILLA_QUIC_BIND", "127.0.0.1:0");
        match c.output().await {
            Ok(out) => break out,
            Err(e) if e.raw_os_error() == Some(26) && attempt < 20 => {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => panic!("cannot run installed flotillad: {e}"),
        }
    };
    assert!(
        out.status.success(),
        "stdout: {} stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["mode"], "detached");
    assert_eq!(report["running"], true);
    assert!(home.join(".config/systemd/user/flotillad.service").exists());

    let out = cmd(home).args(["status", "--json"]).output().await.unwrap();
    assert!(out.status.success());

    let out = cmd(home).arg("stop").output().await.unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["stopped"], true);
}
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p flotillad --test cli`
Expected: FAIL. The stub binary does nothing (empty output, so JSON parse errors).

- [ ] **Step 3: Write `crates/daemon/flotillad/src/main.rs`**

```rust
//! `flotillad`: the Flotilla workspace daemon and its helper commands.
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use fl_proto::{
    ClientMsg, DaemonInfo, EnrollResponse, Hello, PRODUCT_VERSION, PROTO, Request, Response, ServerMsg, VersionInfo,
    build_id,
};
use fl_transport::conn::framed;
use fld_server::devices::DeviceRegistry;
use fld_server::{DaemonConfig, FlotillaHome};
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

#[derive(Parser)]
#[command(name = "flotillad", version, about = "Flotilla workspace daemon")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon in the foreground (systemd runs this).
    Serve {
        /// UDP address for QUIC.
        #[arg(long, env = "FLOTILLA_QUIC_BIND", default_value = "0.0.0.0:7443")]
        quic: SocketAddr,
        /// Disable QUIC; clients then connect over SSH only.
        #[arg(long)]
        no_quic: bool,
    },
    /// Print this binary's version.
    Version {
        #[arg(long)]
        json: bool,
    },
    /// Query the running daemon (exit status 1 when it is not reachable).
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Register a client device and print its credentials as JSON.
    Enroll {
        #[arg(long)]
        device_name: String,
    },
    /// Bridge stdin/stdout to the daemon socket (SSH fallback transport).
    Attach,
    /// Install and (re)start the per-user service.
    InstallService {
        /// Only write the unit file.
        #[arg(long)]
        no_start: bool,
        /// Never use systemd; start a detached background process instead.
        #[arg(long)]
        no_systemd: bool,
    },
    /// Stop a daemon that was started without systemd.
    Stop,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");
    let result = rt.block_on(run(cli));
    // `attach` reads stdin on a blocking thread; don't wait for it to finish.
    rt.shutdown_timeout(Duration::from_millis(200));
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("flotillad: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<ExitCode> {
    match cli.command {
        Command::Serve { quic, no_quic } => serve(quic, no_quic).await,
        Command::Version { json } => {
            version(json);
            Ok(ExitCode::SUCCESS)
        }
        Command::Status { json } => status(json).await,
        Command::Enroll { device_name } => enroll(&device_name),
        Command::Attach => attach().await,
        Command::InstallService { no_start, no_systemd } => install_service(no_start, no_systemd).await,
        Command::Stop => {
            let home = FlotillaHome::from_env()?;
            let stopped = stop_daemon(&home, Duration::from_secs(10)).await?;
            println!("{}", json!({ "stopped": stopped }));
            Ok(ExitCode::SUCCESS)
        }
    }
}

async fn serve(quic: SocketAddr, no_quic: bool) -> Result<ExitCode> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("FLOTILLA_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    let home = FlotillaHome::from_env()?;
    let mut config = DaemonConfig::new(home);
    config.quic_bind = (!no_quic).then_some(quic);
    let daemon = fld_server::start(config).await?;
    tracing::info!(
        socket = %daemon.socket_path.display(),
        quic = ?daemon.quic_addr,
        "flotillad {} ({}) ready",
        PRODUCT_VERSION,
        build_id()
    );
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    tracing::info!("shutting down");
    daemon.shutdown().await;
    Ok(ExitCode::SUCCESS)
}

fn version(json: bool) {
    if json {
        println!("{}", serde_json::to_string(&VersionInfo::current()).expect("VersionInfo serializes"));
    } else {
        println!("flotillad {} ({}) protocol {}", PRODUCT_VERSION, build_id(), PROTO);
    }
}

async fn status(json: bool) -> Result<ExitCode> {
    let home = FlotillaHome::from_env()?;
    match query_daemon_info(&home.socket_path()).await {
        Ok(info) => {
            if json {
                println!("{}", serde_json::to_string(&info)?);
            } else {
                println!(
                    "flotillad {} ({}) pid {} up {}s, quic {}, socket {}",
                    info.version,
                    info.build,
                    info.pid,
                    info.uptime_secs,
                    info.quic_port.map_or_else(|| "off".to_string(), |p| format!("udp/{p}")),
                    info.socket_path
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => {
            if json {
                println!("{}", json!({ "running": false, "error": format!("{e:#}") }));
            } else {
                eprintln!("flotillad is not running: {e:#}");
            }
            Ok(ExitCode::from(1))
        }
    }
}

async fn query_daemon_info(socket: &Path) -> Result<DaemonInfo> {
    let io_timeout = Duration::from_secs(5);
    let stream = tokio::time::timeout(io_timeout, UnixStream::connect(socket))
        .await
        .context("timed out connecting to the daemon")?
        .with_context(|| format!("cannot connect to {}", socket.display()))?;
    let (r, w) = stream.into_split();
    let (mut reader, mut writer) = framed(r, w);
    writer
        .send(&ClientMsg::Hello(Hello {
            client_version: format!("flotillad-cli/{PRODUCT_VERSION}"),
            proto: PROTO,
            device_id: None,
            token: None,
            client_session_id: [0; 16],
        }))
        .await?;
    match tokio::time::timeout(io_timeout, reader.recv::<ServerMsg>()).await.context("timed out waiting for Welcome")?? {
        ServerMsg::Welcome(_) => {}
        ServerMsg::Rejected(reason) => bail!("daemon rejected the connection: {reason:?}"),
        other => bail!("unexpected reply: {other:?}"),
    }
    writer.send(&ClientMsg::Rpc { id: 1, req: Request::DaemonInfo }).await?;
    loop {
        match tokio::time::timeout(io_timeout, reader.recv::<ServerMsg>()).await.context("timed out waiting for DaemonInfo")?? {
            ServerMsg::RpcResult { id: 1, result: Ok(Response::DaemonInfo(info)) } => return Ok(info),
            ServerMsg::RpcResult { result: Err(e), .. } => bail!("daemon error: {}", e.message),
            ServerMsg::Pong { .. } => continue,
            other => bail!("unexpected reply: {other:?}"),
        }
    }
}

fn enroll(device_name: &str) -> Result<ExitCode> {
    let home = FlotillaHome::from_env()?;
    home.ensure_dirs()?;
    let identity = fld_server::identity::load_or_create(&home)?;
    let (device_id, token) = DeviceRegistry::new(home.devices_path()).enroll(device_name)?;
    let quic_port = fld_server::read_runtime_info(&home).and_then(|i| i.quic_port);
    let response = EnrollResponse {
        device_id,
        token,
        quic_port,
        cert_sha256: fl_transport::quic::fingerprint_hex(&identity.fingerprint()),
        socket_path: home.socket_path().display().to_string(),
    };
    println!("{}", serde_json::to_string(&response)?);
    Ok(ExitCode::SUCCESS)
}

async fn attach() -> Result<ExitCode> {
    let home = FlotillaHome::from_env()?;
    let socket = home.socket_path();
    let stream = UnixStream::connect(&socket).await.with_context(|| format!("cannot connect to {}", socket.display()))?;
    let (mut from_daemon, mut to_daemon) = stream.into_split();
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    tokio::select! {
        r = tokio::io::copy(&mut stdin, &mut to_daemon) => {
            r.context("stdin to daemon")?;
        }
        r = tokio::io::copy(&mut from_daemon, &mut stdout) => {
            r.context("daemon to stdout")?;
            stdout.flush().await?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn install_service(no_start: bool, no_systemd: bool) -> Result<ExitCode> {
    let home = FlotillaHome::from_env()?;
    home.ensure_dirs()?;
    let exe = home.bin_dir().join("flotillad");
    if !exe.exists() {
        bail!("{} does not exist; install-service runs the installed binary", exe.display());
    }
    let unit_path = systemd_unit_path()?;
    std::fs::create_dir_all(unit_path.parent().expect("unit path has a parent"))?;
    std::fs::write(&unit_path, unit_file(&exe, home.root()))?;
    if no_start {
        println!("{}", json!({ "mode": "none", "unit_path": unit_path, "linger": false, "running": false }));
        return Ok(ExitCode::SUCCESS);
    }
    let (mode, linger) = if !no_systemd && systemd_user_available() {
        if !systemd_unit_active() {
            // A daemon started earlier without systemd would hold the socket.
            stop_daemon(&home, Duration::from_secs(10)).await?;
        }
        run_checked("systemctl", &["--user", "daemon-reload"])?;
        run_checked("systemctl", &["--user", "enable", "flotillad.service"])?;
        run_checked("systemctl", &["--user", "restart", "flotillad.service"])?;
        ("systemd", enable_linger())
    } else {
        stop_daemon(&home, Duration::from_secs(10)).await?;
        spawn_detached(&exe, &home)?;
        ("detached", false)
    };
    let running = wait_until_running(&home.socket_path(), Duration::from_secs(10)).await;
    println!("{}", json!({ "mode": mode, "unit_path": unit_path, "linger": linger, "running": running }));
    Ok(if running { ExitCode::SUCCESS } else { ExitCode::from(1) })
}

fn systemd_unit_path() -> Result<PathBuf> {
    let config = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".config"),
    };
    Ok(config.join("systemd").join("user").join("flotillad.service"))
}

fn unit_file(exe: &Path, home: &Path) -> String {
    format!(
        "[Unit]\n\
         Description=Flotilla workspace daemon\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart=\"{exe}\" serve\n\
         Environment=\"FLOTILLA_HOME={home}\"\n\
         Restart=always\n\
         RestartSec=2\n\
         KillMode=process\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exe = exe.display(),
        home = home.display()
    )
}

fn systemd_user_available() -> bool {
    std::process::Command::new("systemctl")
        .args(["--user", "show-environment"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn systemd_unit_active() -> bool {
    std::process::Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", "flotillad.service"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn run_checked(program: &str, args: &[&str]) -> Result<()> {
    let out = std::process::Command::new(program).args(args).output().with_context(|| format!("cannot run {program}"))?;
    if !out.status.success() {
        bail!("{program} {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// Keep user services running after logout. Best effort.
fn enable_linger() -> bool {
    let user = std::env::var("USER").or_else(|_| std::env::var("LOGNAME")).unwrap_or_default();
    if user.is_empty() {
        return false;
    }
    if Path::new("/var/lib/systemd/linger").join(&user).exists() {
        return true;
    }
    std::process::Command::new("loginctl")
        .args(["enable-linger", &user])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// The daemon must outlive this process, so the child is intentionally not waited for.
#[allow(clippy::zombie_processes)]
fn spawn_detached(exe: &Path, home: &FlotillaHome) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let log_path = home.logs_dir().join("daemon.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("cannot open {}", log_path.display()))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("serve").env("FLOTILLA_HOME", home.root()).stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
    // SAFETY: setsid() is async-signal-safe; it detaches the daemon from the SSH session.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().with_context(|| format!("cannot start {}", exe.display()))?;
    Ok(())
}

async fn wait_until_running(socket: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if query_daemon_info(socket).await.is_ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// SIGTERM the daemon recorded in `run/daemon.json` and wait for it to exit.
/// Returns false when no live `flotillad serve` owns that pid (stale file).
async fn stop_daemon(home: &FlotillaHome, wait: Duration) -> Result<bool> {
    let Some(info) = fld_server::read_runtime_info(home) else { return Ok(false) };
    if info.pid == std::process::id() || !is_flotillad_serve(info.pid) {
        let _ = std::fs::remove_file(home.runtime_info_path());
        return Ok(false);
    }
    // SAFETY: plain kill(2) on a pid we just verified is a flotillad serve process.
    unsafe {
        libc::kill(info.pid as libc::pid_t, libc::SIGTERM);
    }
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        if !is_flotillad_serve(info.pid) {
            return Ok(true);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    bail!("flotillad (pid {}) did not exit within {:?}", info.pid, wait)
}

/// True when `pid` is a live `flotillad serve` process (guards against pid reuse).
fn is_flotillad_serve(pid: u32) -> bool {
    match std::fs::read(format!("/proc/{pid}/cmdline")) {
        Ok(raw) => {
            let args: Vec<&[u8]> = raw.split(|b| *b == 0).collect();
            args.first().is_some_and(|a| a.ends_with(b"flotillad")) && args.iter().any(|a| *a == b"serve")
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_file_runs_installed_binary_and_restarts() {
        let u = unit_file(Path::new("/home/u/.flotilla/bin/flotillad"), Path::new("/home/u/.flotilla"));
        assert!(u.contains("ExecStart=\"/home/u/.flotilla/bin/flotillad\" serve\n"), "{u}");
        assert!(u.contains("Environment=\"FLOTILLA_HOME=/home/u/.flotilla\"\n"));
        assert!(u.contains("Restart=always\n"));
        assert!(u.contains("KillMode=process\n"));
        assert!(u.contains("WantedBy=default.target\n"));
    }

    #[test]
    fn cli_accepts_every_command() {
        for args in [
            &["flotillad", "serve", "--quic", "127.0.0.1:0"][..],
            &["flotillad", "serve", "--no-quic"],
            &["flotillad", "version", "--json"],
            &["flotillad", "status", "--json"],
            &["flotillad", "enroll", "--device-name", "my pc"],
            &["flotillad", "attach"],
            &["flotillad", "install-service", "--no-systemd"],
            &["flotillad", "install-service", "--no-start"],
            &["flotillad", "stop"],
        ] {
            Cli::try_parse_from(args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
        }
    }

    #[test]
    fn this_test_process_is_not_a_serve_process() {
        assert!(!is_flotillad_serve(std::process::id()));
        assert!(!is_flotillad_serve(u32::MAX));
    }
}
```

- [ ] **Step 4: Run all flotillad tests**

Run: `cargo test -p flotillad`
Expected: 3 unit tests and 5 CLI tests pass. Then check no daemon was left behind: `pgrep -af 'flotillad serve'` should print nothing from `/tmp`.

- [ ] **Step 5: Commit**

```bash
cargo fmt -p flotillad && cargo clippy -p flotillad --all-targets -- -D warnings
git add crates/daemon/flotillad/src/main.rs crates/daemon/flotillad/tests/cli.rs
git commit -m "feat(flotillad): serve, status, enroll, attach, install-service, stop

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 10: flc-conn — bootstrap (install, start, enroll over SSH)

**Files:**
- Create (replace stub): `crates/client/flc-conn/src/bootstrap.rs`
- Create: `crates/daemon/flotillad/tests/bootstrap_e2e.rs`. It lives in flotillad because only that package's tests can use `CARGO_BIN_EXE_flotillad`; `flc-conn` is already a dev-dependency there.

**Interfaces:**
- Consumes:
  - `fl_transport::ssh::{SshSession, SshError}` (Task 7)
  - `fl_proto::{VersionInfo, DaemonInfo, EnrollResponse}` (Task 3)
  - the `flotillad` CLI contract (Task 9)
- Produces (`flc_conn::bootstrap::…`):
  - `RemoteArch::{X86_64, Aarch64}` and `RemoteArch::parse(&str) -> Option<RemoteArch>`
  - `DaemonArtifacts { x86_64: Cow<'static, [u8]>, aarch64: Cow<'static, [u8]>, version: String, build: String }` with `for_arch(arch) -> &[u8]`. Its `Debug` prints lengths only.
  - `BootstrapOptions { device_name: String, enroll: bool, service_args: Vec<String>, start_timeout: Duration }` with `BootstrapOptions::new(device_name)` (`enroll: true`, no service args, 20 s timeout). It derives `Clone`.
  - `BootstrapStep::{Probing, Uploading { bytes: usize }, Starting, Enrolling, Done}` (`Debug, Clone, PartialEq, Eq`)
  - `BootstrapOutcome { arch, installed: bool, daemon: DaemonInfo, enrollment: Option<EnrollResponse> }`
  - `BootstrapError::{Ssh, UnsupportedOs(String), UnsupportedArch(String), NoEmbeddedDaemon(RemoteArch), MissingTool(&'static str), HashMismatch { expected, actual }, DaemonNotReady { timeout, last_error }, BadOutput { what, detail }}`
  - `Probe { os, arch, has_sha256sum, installed: Option<VersionInfo> }`
  - Free functions:
    - `parse_probe(&str) -> Result<Probe, BootstrapError>`
    - `should_install(Option<&VersionInfo>, &DaemonArtifacts) -> bool`
    - `plan_install(&Probe, &DaemonArtifacts) -> Result<Option<&[u8]>, BootstrapError>`
    - `sh_quote(&str) -> String`
    - `sanitize_device_name(&str) -> String`
  - `async fn bootstrap(ssh: &SshSession, artifacts: &DaemonArtifacts, opts: &BootstrapOptions, progress: &(dyn Fn(BootstrapStep) + Send + Sync)) -> Result<BootstrapOutcome, BootstrapError>`

- [ ] **Step 1: Write `bootstrap.rs`, implementation and unit tests**

```rust
//! Install or upgrade `flotillad` on the remote host over SSH, make sure it is
//! running, and enroll this device.
//!
//! Every remote command runs under `sh -c` so the user's login shell (bash,
//! zsh, fish, ...) does not matter.
use std::borrow::Cow;
use std::time::{Duration, Instant};

use fl_proto::{DaemonInfo, EnrollResponse, VersionInfo};
use fl_transport::ssh::{SshError, SshSession};
use sha2::{Digest, Sha256};

/// Remote path of the installed daemon; `$HOME` is expanded by the remote `sh`.
pub const REMOTE_BIN: &str = "$HOME/.flotilla/bin/flotillad";

/// One line each: OS, arch, sha256sum availability, installed version JSON (or `none`).
pub const PROBE_SCRIPT: &str = r#"uname -s; uname -m; if command -v sha256sum >/dev/null 2>&1; then echo sha256sum=yes; else echo sha256sum=no; fi; "$HOME/.flotilla/bin/flotillad" version --json 2>/dev/null || echo none"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteArch {
    X86_64,
    Aarch64,
}

impl RemoteArch {
    pub fn parse(uname_m: &str) -> Option<Self> {
        match uname_m.trim() {
            "x86_64" | "amd64" => Some(Self::X86_64),
            "aarch64" | "arm64" => Some(Self::Aarch64),
            _ => None,
        }
    }
}

/// Daemon binaries embedded in the client, plus the version/build they report.
#[derive(Clone)]
pub struct DaemonArtifacts {
    pub x86_64: Cow<'static, [u8]>,
    pub aarch64: Cow<'static, [u8]>,
    pub version: String,
    pub build: String,
}

impl DaemonArtifacts {
    pub fn for_arch(&self, arch: RemoteArch) -> &[u8] {
        match arch {
            RemoteArch::X86_64 => &self.x86_64,
            RemoteArch::Aarch64 => &self.aarch64,
        }
    }
}

impl std::fmt::Debug for DaemonArtifacts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonArtifacts")
            .field("x86_64_len", &self.x86_64.len())
            .field("aarch64_len", &self.aarch64.len())
            .field("version", &self.version)
            .field("build", &self.build)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct BootstrapOptions {
    /// Shown in `flotillad` device listings; sanitized before use.
    pub device_name: String,
    /// Run `flotillad enroll` and return credentials.
    pub enroll: bool,
    /// Extra arguments for `flotillad install-service` (tests pass `--no-systemd`).
    pub service_args: Vec<String>,
    /// How long to wait for the daemon to answer after starting it.
    pub start_timeout: Duration,
}

impl BootstrapOptions {
    pub fn new(device_name: impl Into<String>) -> Self {
        Self { device_name: device_name.into(), enroll: true, service_args: Vec::new(), start_timeout: Duration::from_secs(20) }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootstrapStep {
    Probing,
    Uploading { bytes: usize },
    Starting,
    Enrolling,
    Done,
}

#[derive(Debug, Clone)]
pub struct BootstrapOutcome {
    pub arch: RemoteArch,
    /// True when a new binary was uploaded.
    pub installed: bool,
    pub daemon: DaemonInfo,
    pub enrollment: Option<EnrollResponse>,
}

#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    #[error(transparent)]
    Ssh(#[from] SshError),
    #[error("the remote OS is `{0}`; Flotilla's daemon runs on Linux only")]
    UnsupportedOs(String),
    #[error("unsupported remote architecture `{0}` (Flotilla supports x86_64 and aarch64 Linux)")]
    UnsupportedArch(String),
    #[error("this Flotilla build has no embedded daemon for {0:?}; install a release build of Flotilla")]
    NoEmbeddedDaemon(RemoteArch),
    #[error("the remote host is missing `{0}`; install coreutils and try again")]
    MissingTool(&'static str),
    #[error("upload corrupted: expected sha256 {expected}, remote computed {actual}")]
    HashMismatch { expected: String, actual: String },
    #[error("the daemon did not become ready within {timeout:?}: {last_error}")]
    DaemonNotReady { timeout: Duration, last_error: String },
    #[error("unexpected output from remote `{what}`: {detail}")]
    BadOutput { what: &'static str, detail: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub os: String,
    pub arch: RemoteArch,
    pub has_sha256sum: bool,
    /// `None` when absent or when its version output can't be parsed (treated as outdated).
    pub installed: Option<VersionInfo>,
}

pub fn parse_probe(stdout: &str) -> Result<Probe, BootstrapError> {
    let mut lines = stdout.lines().map(str::trim);
    let os = lines.next().unwrap_or("").to_string();
    if os != "Linux" {
        return Err(BootstrapError::UnsupportedOs(os));
    }
    let arch_raw = lines.next().unwrap_or("");
    let arch = RemoteArch::parse(arch_raw).ok_or_else(|| BootstrapError::UnsupportedArch(arch_raw.to_string()))?;
    let has_sha256sum = match lines.next() {
        Some("sha256sum=yes") => true,
        Some("sha256sum=no") => false,
        other => return Err(BootstrapError::BadOutput { what: "probe", detail: format!("expected sha256sum=yes|no, got {other:?}") }),
    };
    let installed = match lines.next() {
        None | Some("") | Some("none") => None,
        Some(json) => serde_json::from_str::<VersionInfo>(json).ok(),
    };
    Ok(Probe { os, arch, has_sha256sum, installed })
}

pub fn should_install(installed: Option<&VersionInfo>, artifacts: &DaemonArtifacts) -> bool {
    match installed {
        None => true,
        Some(v) => v.version != artifacts.version || v.build != artifacts.build,
    }
}

/// What to upload: `Ok(None)` when the installed daemon is current.
pub fn plan_install<'a>(probe: &Probe, artifacts: &'a DaemonArtifacts) -> Result<Option<&'a [u8]>, BootstrapError> {
    if !should_install(probe.installed.as_ref(), artifacts) {
        return Ok(None);
    }
    let bytes = artifacts.for_arch(probe.arch);
    if bytes.is_empty() {
        return Err(BootstrapError::NoEmbeddedDaemon(probe.arch));
    }
    if !probe.has_sha256sum {
        return Err(BootstrapError::MissingTool("sha256sum"));
    }
    Ok(Some(bytes))
}

/// Quote `s` as one POSIX shell word.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Run `script` with `/bin/sh` whatever the user's login shell is.
fn sh(script: &str) -> String {
    format!("sh -c {}", sh_quote(script))
}

/// Keep `[A-Za-z0-9 ._-]`, at most 64 chars; fall back to `windows-pc`.
pub fn sanitize_device_name(name: &str) -> String {
    let kept: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.'))
        .take(64)
        .collect();
    let kept = kept.trim();
    if kept.is_empty() { "windows-pc".to_string() } else { kept.to_string() }
}

pub async fn bootstrap(
    ssh: &SshSession,
    artifacts: &DaemonArtifacts,
    opts: &BootstrapOptions,
    progress: &(dyn Fn(BootstrapStep) + Send + Sync),
) -> Result<BootstrapOutcome, BootstrapError> {
    progress(BootstrapStep::Probing);
    let out = ssh.exec(&sh(PROBE_SCRIPT), None).await?;
    let probe = parse_probe(&out.stdout_str())?;
    let upload_bytes = plan_install(&probe, artifacts)?;
    if let Some(bytes) = upload_bytes {
        progress(BootstrapStep::Uploading { bytes: bytes.len() });
        upload(ssh, bytes).await?;
    }

    progress(BootstrapStep::Starting);
    let current = matches!(
        query_status(ssh).await,
        Ok(info) if info.version == artifacts.version && info.build == artifacts.build
    );
    if upload_bytes.is_some() || !current {
        let mut cmd = format!("\"{REMOTE_BIN}\" install-service");
        for arg in &opts.service_args {
            cmd.push(' ');
            cmd.push_str(&sh_quote(arg));
        }
        let out = ssh.exec(&sh(&cmd), None).await?;
        if out.code != Some(0) {
            return Err(BootstrapError::DaemonNotReady {
                timeout: opts.start_timeout,
                last_error: format!(
                    "install-service exited with {:?}: {} {}",
                    out.code,
                    out.stdout_str().trim(),
                    out.stderr_str().trim()
                ),
            });
        }
    }
    let daemon = wait_ready(ssh, artifacts, opts.start_timeout).await?;

    let enrollment = if opts.enroll {
        progress(BootstrapStep::Enrolling);
        let cmd = format!("\"{REMOTE_BIN}\" enroll --device-name {}", sh_quote(&sanitize_device_name(&opts.device_name)));
        let raw = ssh.exec_checked(&sh(&cmd), None).await?;
        Some(
            serde_json::from_slice::<EnrollResponse>(&raw)
                .map_err(|e| BootstrapError::BadOutput { what: "enroll", detail: e.to_string() })?,
        )
    } else {
        None
    };
    progress(BootstrapStep::Done);
    Ok(BootstrapOutcome { arch: probe.arch, installed: upload_bytes.is_some(), daemon, enrollment })
}

async fn upload(ssh: &SshSession, bytes: &[u8]) -> Result<(), BootstrapError> {
    let expected = hex::encode(Sha256::digest(bytes));
    let script = r#"set -e; d="$HOME/.flotilla/bin"; mkdir -p "$d"; chmod 700 "$HOME/.flotilla"; cat > "$d/flotillad.new"; chmod 755 "$d/flotillad.new"; sha256sum "$d/flotillad.new""#;
    let out = ssh.exec_checked(&sh(script), Some(bytes)).await?;
    let text = String::from_utf8_lossy(&out);
    let actual = text.split_whitespace().next().unwrap_or("").to_string();
    if actual != expected {
        let _ = ssh.exec(&sh(r#"rm -f "$HOME/.flotilla/bin/flotillad.new""#), None).await;
        return Err(BootstrapError::HashMismatch { expected, actual });
    }
    ssh.exec_checked(&sh(r#"mv -f "$HOME/.flotilla/bin/flotillad.new" "$HOME/.flotilla/bin/flotillad""#), None).await?;
    Ok(())
}

async fn query_status(ssh: &SshSession) -> Result<DaemonInfo, BootstrapError> {
    let out = ssh.exec(&sh(&format!("\"{REMOTE_BIN}\" status --json")), None).await?;
    if out.code != Some(0) {
        return Err(BootstrapError::BadOutput {
            what: "status",
            detail: format!("exit {:?}: {} {}", out.code, out.stdout_str().trim(), out.stderr_str().trim()),
        });
    }
    serde_json::from_slice(&out.stdout).map_err(|e| BootstrapError::BadOutput { what: "status", detail: e.to_string() })
}

async fn wait_ready(ssh: &SshSession, artifacts: &DaemonArtifacts, timeout: Duration) -> Result<DaemonInfo, BootstrapError> {
    let deadline = Instant::now() + timeout;
    loop {
        let last_error = match query_status(ssh).await {
            Ok(info) if info.version == artifacts.version && info.build == artifacts.build => return Ok(info),
            Ok(info) => format!("an older daemon {} ({}) is still running", info.version, info.build),
            Err(e) => e.to_string(),
        };
        if Instant::now() >= deadline {
            return Err(BootstrapError::DaemonNotReady { timeout, last_error });
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artifacts(version: &str, build: &str) -> DaemonArtifacts {
        DaemonArtifacts {
            x86_64: Cow::Borrowed(b"x86-bytes"),
            aarch64: Cow::Borrowed(b"arm-bytes"),
            version: version.into(),
            build: build.into(),
        }
    }

    fn version(version: &str, build: &str) -> VersionInfo {
        VersionInfo { version: version.into(), build: build.into(), proto: fl_proto::PROTO }
    }

    fn probe(installed: Option<VersionInfo>, has_sha256sum: bool) -> Probe {
        Probe { os: "Linux".into(), arch: RemoteArch::X86_64, has_sha256sum, installed }
    }

    #[test]
    fn parse_probe_fresh_linux_x86() {
        let p = parse_probe("Linux\nx86_64\nsha256sum=yes\nnone\n").unwrap();
        assert_eq!(p.arch, RemoteArch::X86_64);
        assert!(p.has_sha256sum);
        assert!(p.installed.is_none());
    }

    #[test]
    fn parse_probe_reads_installed_version() {
        let json = serde_json::to_string(&version("0.1.0", "abc")).unwrap();
        let p = parse_probe(&format!("Linux\naarch64\nsha256sum=yes\n{json}\n")).unwrap();
        assert_eq!(p.arch, RemoteArch::Aarch64);
        assert_eq!(p.installed, Some(version("0.1.0", "abc")));
    }

    #[test]
    fn parse_probe_treats_unparseable_version_as_not_installed() {
        let p = parse_probe("Linux\nx86_64\nsha256sum=yes\nflotillad 0.0.1\n").unwrap();
        assert!(p.installed.is_none());
    }

    /// Review Focus #5.
    #[test]
    fn parse_probe_rejects_unsupported_arch() {
        match parse_probe("Linux\nriscv64\nsha256sum=yes\nnone\n") {
            Err(BootstrapError::UnsupportedArch(a)) => assert_eq!(a, "riscv64"),
            other => panic!("expected UnsupportedArch, got {other:?}"),
        }
    }

    /// Review Focus #5.
    #[test]
    fn parse_probe_rejects_non_linux() {
        match parse_probe("Darwin\narm64\nsha256sum=no\nnone\n") {
            Err(e @ BootstrapError::UnsupportedOs(_)) => assert!(e.to_string().contains("Linux")),
            other => panic!("expected UnsupportedOs, got {other:?}"),
        }
    }

    #[test]
    fn parse_probe_rejects_truncated_output() {
        assert!(matches!(parse_probe(""), Err(BootstrapError::UnsupportedOs(_))));
        assert!(matches!(parse_probe("Linux\nx86_64\n"), Err(BootstrapError::BadOutput { .. })));
    }

    #[test]
    fn should_install_compares_version_and_build() {
        let a = artifacts("0.1.0", "abc");
        assert!(should_install(None, &a));
        assert!(!should_install(Some(&version("0.1.0", "abc")), &a));
        assert!(should_install(Some(&version("0.1.0", "old")), &a));
        assert!(should_install(Some(&version("0.0.9", "abc")), &a));
    }

    /// Review Focus #5.
    #[test]
    fn plan_install_requires_sha256sum_only_when_installing() {
        let a = artifacts("0.1.0", "abc");
        assert!(matches!(plan_install(&probe(None, false), &a), Err(BootstrapError::MissingTool("sha256sum"))));
        assert_eq!(plan_install(&probe(Some(version("0.1.0", "abc")), false), &a).unwrap(), None);
        assert_eq!(plan_install(&probe(None, true), &a).unwrap(), Some(&b"x86-bytes"[..]));
    }

    #[test]
    fn plan_install_without_embedded_daemon_is_a_clear_error() {
        let mut a = artifacts("0.1.0", "abc");
        a.x86_64 = Cow::Borrowed(b"");
        assert!(matches!(plan_install(&probe(None, true), &a), Err(BootstrapError::NoEmbeddedDaemon(RemoteArch::X86_64))));
    }

    #[test]
    fn sh_quote_escapes_single_quotes() {
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        assert_eq!(sh("echo hi"), "sh -c 'echo hi'");
    }

    #[cfg(unix)]
    #[test]
    fn sh_quote_roundtrips_through_a_real_shell() {
        for s in ["plain", "it's", "a b", "$HOME", "`id`", "\"q\"", "semi;colon", "new\nline"] {
            let out = std::process::Command::new("sh").arg("-c").arg(format!("printf %s {}", sh_quote(s))).output().unwrap();
            assert_eq!(String::from_utf8(out.stdout).unwrap(), s);
        }
    }

    #[test]
    fn sanitize_device_name_keeps_safe_characters() {
        assert_eq!(sanitize_device_name("DESKTOP-AB12"), "DESKTOP-AB12");
        assert_eq!(sanitize_device_name("my pc; rm -rf /"), "my pc rm -rf");
        assert_eq!(sanitize_device_name("$(boom)"), "boom");
        assert_eq!(sanitize_device_name("   "), "windows-pc");
        assert_eq!(sanitize_device_name(&"x".repeat(100)).len(), 64);
    }

    #[test]
    fn artifacts_debug_hides_the_bytes() {
        let dbg = format!("{:?}", artifacts("0.1.0", "abc"));
        assert!(dbg.contains("x86_64_len: 9"), "{dbg}");
        assert!(!dbg.contains("x86-bytes"));
    }

    #[test]
    fn remote_arch_parsing() {
        assert_eq!(RemoteArch::parse("x86_64\n"), Some(RemoteArch::X86_64));
        assert_eq!(RemoteArch::parse("arm64"), Some(RemoteArch::Aarch64));
        assert_eq!(RemoteArch::parse("i686"), None);
    }
}
```

- [ ] **Step 2: Run the unit tests**

Run: `cargo test -p flc-conn --lib bootstrap`
Expected: 15 tests pass.

- [ ] **Step 3: Write the end-to-end test** — `crates/daemon/flotillad/tests/bootstrap_e2e.rs`

**Prerequisite:** Task 9 is committed (the real `flotillad` CLI). Check with `git log --oneline -- crates/daemon/flotillad/src/main.rs`.

```rust
#![cfg(unix)]
//! Client bootstrap against the real flotillad binary over the in-process SSH server.
use std::borrow::Cow;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use fl_proto::{ClientMsg, Hello, PRODUCT_VERSION, PROTO, ServerMsg, build_id};
use fl_testkit::ssh_server::{TestSshOptions, TestSshServer};
use fl_transport::conn::framed;
use fl_transport::quic;
use fl_transport::ssh::{HostKeyPolicy, SshAuth, SshSession, SshTarget};
use flc_conn::bootstrap::{BootstrapError, BootstrapOptions, BootstrapStep, DaemonArtifacts, bootstrap};
use sha2::{Digest, Sha256};

const BIN: &str = env!("CARGO_BIN_EXE_flotillad");

/// Stops whatever daemon the test started in `home`, even if the test panics.
struct StopOnDrop(PathBuf);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        let _ = std::process::Command::new(BIN)
            .arg("stop")
            .env("HOME", &self.0)
            .env("FLOTILLA_HOME", self.0.join(".flotilla"))
            .output();
    }
}

struct Fx {
    _dir: tempfile::TempDir,
    home: PathBuf,
    _server: TestSshServer,
    ssh: SshSession,
    _stop: StopOnDrop,
}

async fn fixture() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_path_buf();
    let stop = StopOnDrop(home.clone());
    let server = TestSshServer::start_with(
        &home,
        TestSshOptions {
            env: vec![
                ("FLOTILLA_QUIC_BIND".into(), "127.0.0.1:0".into()),
                ("FLOTILLA_HOME".into(), home.join(".flotilla").display().to_string()),
            ],
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let key = home.join("id_ed25519");
    let ok = std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&key)
        .status()
        .unwrap()
        .success();
    assert!(ok);
    let target = SshTarget { host: "127.0.0.1".into(), port: server.addr.port(), user: "tester".into() };
    let ssh = SshSession::connect(&target, &SshAuth::KeyFile { path: key }, &HostKeyPolicy::TrustOnFirstUse, Duration::from_secs(10))
        .await
        .unwrap();
    Fx { _dir: dir, home, _server: server, ssh, _stop: stop }
}

fn real_artifacts() -> DaemonArtifacts {
    let bytes = std::fs::read(BIN).unwrap();
    DaemonArtifacts {
        x86_64: Cow::Owned(bytes.clone()),
        aarch64: Cow::Owned(bytes),
        version: PRODUCT_VERSION.into(),
        build: build_id().into(),
    }
}

fn test_options() -> BootstrapOptions {
    let mut opts = BootstrapOptions::new("e2e pc");
    opts.service_args = vec!["--no-systemd".into()];
    opts
}

fn hello(device_id: Option<String>, token: Option<String>) -> ClientMsg {
    ClientMsg::Hello(Hello { client_version: "e2e".into(), proto: PROTO, device_id, token, client_session_id: [5; 16] })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bootstrap_installs_starts_enrolls_and_both_transports_work() {
    let fx = fixture().await;
    let artifacts = real_artifacts();
    let steps = Mutex::new(Vec::new());
    let outcome = bootstrap(&fx.ssh, &artifacts, &test_options(), &|s| steps.lock().unwrap().push(s)).await.unwrap();

    assert!(outcome.installed);
    let steps = steps.into_inner().unwrap();
    assert_eq!(steps.first(), Some(&BootstrapStep::Probing));
    assert!(steps.contains(&BootstrapStep::Uploading { bytes: artifacts.x86_64.len() }));
    assert!(steps.contains(&BootstrapStep::Starting));
    assert!(steps.contains(&BootstrapStep::Enrolling));
    assert_eq!(steps.last(), Some(&BootstrapStep::Done));
    let installed = std::fs::read(fx.home.join(".flotilla/bin/flotillad")).unwrap();
    assert_eq!(Sha256::digest(&installed), Sha256::digest(&artifacts.x86_64[..]));
    assert_eq!(outcome.daemon.build, build_id());
    let enrollment = outcome.enrollment.clone().expect("enrolled");

    // Already current: no upload, same daemon process, no new enrollment.
    let mut again_opts = test_options();
    again_opts.enroll = false;
    let again = bootstrap(&fx.ssh, &artifacts, &again_opts, &|_| {}).await.unwrap();
    assert!(!again.installed);
    assert_eq!(again.daemon.pid, outcome.daemon.pid);
    assert!(again.enrollment.is_none());

    // SSH transport: direct-streamlocal to the daemon socket.
    let stream = fx.ssh.open_unix_socket(&enrollment.socket_path).await.unwrap();
    let (r, w) = tokio::io::split(stream);
    let (mut reader, mut writer) = framed(r, w);
    writer.send(&hello(None, None)).await.unwrap();
    assert!(matches!(reader.recv::<ServerMsg>().await.unwrap(), ServerMsg::Welcome(_)));

    // QUIC transport with the enrolled credentials.
    let port = enrollment.quic_port.expect("daemon has QUIC");
    let fp = quic::parse_fingerprint(&enrollment.cert_sha256).unwrap();
    let (_endpoint, conn) = quic::connect(([127, 0, 0, 1], port).into(), fp, Duration::from_secs(5)).await.unwrap();
    let (send, recv) = quic::open_ctl(&conn).await.unwrap();
    let (mut qr, mut qw) = framed(recv, send);
    qw.send(&hello(Some(enrollment.device_id.clone()), Some(enrollment.token.clone()))).await.unwrap();
    assert!(matches!(qr.recv::<ServerMsg>().await.unwrap(), ServerMsg::Welcome(_)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn outdated_binary_is_replaced() {
    let fx = fixture().await;
    let bin_dir = fx.home.join(".flotilla/bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let old = bin_dir.join("flotillad");
    std::fs::write(
        &old,
        "#!/bin/sh\ncase \"$1\" in version) echo '{\"version\":\"0.0.1\",\"build\":\"old\",\"proto\":{\"major\":0,\"minor\":1}}';; *) exit 1;; esac\n",
    )
    .unwrap();
    std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o755)).unwrap();

    let outcome = bootstrap(&fx.ssh, &real_artifacts(), &test_options(), &|_| {}).await.unwrap();
    assert!(outcome.installed);
    assert_eq!(outcome.daemon.build, build_id());
    assert_eq!(outcome.daemon.version, PRODUCT_VERSION);
}

#[tokio::test]
async fn missing_embedded_daemon_fails_before_touching_the_host() {
    let fx = fixture().await;
    let empty = DaemonArtifacts {
        x86_64: Cow::Borrowed(b""),
        aarch64: Cow::Borrowed(b""),
        version: PRODUCT_VERSION.into(),
        build: build_id().into(),
    };
    let err = bootstrap(&fx.ssh, &empty, &test_options(), &|_| {}).await.unwrap_err();
    assert!(matches!(err, BootstrapError::NoEmbeddedDaemon(_)), "got {err:?}");
    assert!(!Path::new(&fx.home.join(".flotilla/bin/flotillad")).exists());
}
```

- [ ] **Step 4: Run the e2e tests**

Run: `cargo test -p flotillad --test bootstrap_e2e`
Expected: 3 tests pass. Upload of the debug binary takes a few seconds. Afterwards `pgrep -af 'flotillad serve'` shows nothing under `/tmp`.

- [ ] **Step 5: Commit**

```bash
cargo fmt -p flc-conn && cargo clippy -p flc-conn --all-targets -- -D warnings
rustfmt --edition 2024 crates/daemon/flotillad/tests/bootstrap_e2e.rs
git add crates/client/flc-conn/src/bootstrap.rs crates/daemon/flotillad/tests/bootstrap_e2e.rs
git commit -m "feat(flc-conn): bootstrap daemon over SSH (probe, upload, start, enroll)

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 11: flc-conn — connection supervisor (QUIC/SSH race, heartbeat, failover, upgrade)

**Files:**
- Create (replace stub): `crates/client/flc-conn/src/supervisor.rs`
- Create: `crates/client/flc-conn/tests/supervisor.rs`

**Interfaces:**
- Consumes:
  - `fl_transport::{conn, quic, ssh}` (Tasks 4, 5, 7)
  - `fl_proto` messages (Task 3)
  - `fld_server` and `fl_testkit` (Tasks 6 and 8; test only)
- Produces (`flc_conn::supervisor::…`), used by the client UI (Task 14):
  - `TransportKind::{Quic, Ssh}`; `Display` gives `QUIC` / `SSH`.
  - `ConnPhase::{Idle, Connecting, Connected { kind, rtt_ms: u32, daemon_version: String }, Reconnecting { attempt: u32, error: String }, Failed { error: String }}` (`Clone + PartialEq + Debug`)
  - `Timings { heartbeat, dead_after, quic_reprobe, backoff_min, backoff_max, connect_timeout }`. `Default` gives 2 s / 6 s / 30 s / 250 ms / 5 s / 10 s.
  - `QuicSpec { host: String, port: u16, cert_sha256: [u8; 32] }`
  - `ConnectSpec { ssh: SshTarget, auth: SshAuth, host_key: HostKeyPolicy, socket_path: String, quic: Option<QuicSpec>, device_id: Option<String>, token: Option<String>, client_version: String, timings: Timings }`
  - `ConnectError` with `is_fatal()`
  - `Supervisor`:
    - `spawn(spec)` (inside a Tokio runtime) or `spawn_on(&tokio::runtime::Handle, spec)` (from any thread)
    - `phase() -> watch::Receiver<ConnPhase>`
    - `current() -> ConnPhase`
    - `stop()`
    - `async shutdown(self)`: the phase becomes `Idle`.
  - `ATTACH_COMMAND`

- [ ] **Step 1: Write the failing integration tests** — `crates/client/flc-conn/tests/supervisor.rs`

```rust
#![cfg(unix)]
//! Supervisor tests against an in-process daemon, the test SSH server, and a
//! UDP relay that can black-hole QUIC.
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fl_testkit::ssh_server::TestSshServer;
use fl_testkit::udp_relay::UdpRelay;
use fl_transport::ssh::{HostKeyPolicy, SshAuth, SshTarget};
use flc_conn::supervisor::{ConnPhase, ConnectSpec, QuicSpec, Supervisor, Timings, TransportKind};
use fld_server::devices::DeviceRegistry;
use fld_server::{DaemonConfig, DaemonHandle, FlotillaHome};
use tokio::sync::watch;

fn fast() -> Timings {
    Timings {
        heartbeat: Duration::from_millis(100),
        dead_after: Duration::from_millis(500),
        quic_reprobe: Duration::from_millis(300),
        backoff_min: Duration::from_millis(50),
        backoff_max: Duration::from_millis(300),
        connect_timeout: Duration::from_secs(2),
    }
}

fn home(dir: &Path) -> FlotillaHome {
    FlotillaHome::new(dir.join(".flotilla"))
}

async fn start_daemon(dir: &Path) -> DaemonHandle {
    let mut config = DaemonConfig::new(home(dir));
    config.quic_bind = Some("127.0.0.1:0".parse().unwrap());
    config.hello_timeout = Duration::from_secs(2);
    fld_server::start(config).await.unwrap()
}

struct Harness {
    dir: tempfile::TempDir,
    daemon: Option<DaemonHandle>,
    ssh: TestSshServer,
    key: PathBuf,
    device: (String, String),
    relay: UdpRelay,
}

impl Harness {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let daemon = start_daemon(dir.path()).await;
        let device = DeviceRegistry::new(home(dir.path()).devices_path()).enroll("test").unwrap();
        let ssh = TestSshServer::start(dir.path()).await.unwrap();
        let key = dir.path().join("id_ed25519");
        let ok = std::process::Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let relay = UdpRelay::start(daemon.quic_addr.unwrap()).await.unwrap();
        Self { dir, daemon: Some(daemon), ssh, key, device, relay }
    }

    fn spec(&self, with_quic: bool) -> ConnectSpec {
        let d = self.daemon.as_ref().expect("daemon running");
        ConnectSpec {
            ssh: SshTarget { host: "127.0.0.1".into(), port: self.ssh.addr.port(), user: "tester".into() },
            auth: SshAuth::KeyFile { path: self.key.clone() },
            host_key: HostKeyPolicy::Expect(self.ssh.host_key_fingerprint.clone()),
            socket_path: d.socket_path.display().to_string(),
            quic: with_quic.then(|| QuicSpec {
                host: "127.0.0.1".into(),
                port: self.relay.addr.port(),
                cert_sha256: d.cert_sha256,
            }),
            device_id: Some(self.device.0.clone()),
            token: Some(self.device.1.clone()),
            client_version: "supervisor-test".into(),
            timings: fast(),
        }
    }
}

async fn wait_phase(
    rx: &mut watch::Receiver<ConnPhase>,
    within: Duration,
    what: &str,
    pred: impl Fn(&ConnPhase) -> bool,
) -> ConnPhase {
    let fut = async {
        loop {
            let current = rx.borrow_and_update().clone();
            if pred(&current) {
                return current;
            }
            if rx.changed().await.is_err() {
                panic!("supervisor stopped while waiting for {what}");
            }
        }
    };
    match tokio::time::timeout(within, fut).await {
        Ok(p) => p,
        Err(_) => panic!("timed out after {within:?} waiting for {what}; last phase: {:?}", *rx.borrow()),
    }
}

fn connected(kind: TransportKind) -> impl Fn(&ConnPhase) -> bool {
    move |p| matches!(p, ConnPhase::Connected { kind: k, .. } if *k == kind)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connects_over_quic_when_available() {
    let h = Harness::new().await;
    let sup = Supervisor::spawn(h.spec(true));
    let mut rx = sup.phase();
    match wait_phase(&mut rx, Duration::from_secs(5), "QUIC", connected(TransportKind::Quic)).await {
        ConnPhase::Connected { rtt_ms, daemon_version, .. } => {
            assert!(rtt_ms < 1000);
            assert_eq!(daemon_version, fl_proto::PRODUCT_VERSION);
        }
        other => unreachable!("{other:?}"),
    }
    sup.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uses_ssh_when_quic_is_not_configured() {
    let h = Harness::new().await;
    let sup = Supervisor::spawn(h.spec(false));
    let mut rx = sup.phase();
    wait_phase(&mut rx, Duration::from_secs(5), "SSH", connected(TransportKind::Ssh)).await;
    sup.shutdown().await;
}

/// Review Focus #2: silently dropped UDP must not delay the first connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn falls_back_to_ssh_quickly_when_udp_is_blackholed() {
    let h = Harness::new().await;
    h.relay.set_dropping(true);
    let started = Instant::now();
    let sup = Supervisor::spawn(h.spec(true));
    let mut rx = sup.phase();
    wait_phase(&mut rx, Duration::from_secs(5), "SSH", connected(TransportKind::Ssh)).await;
    assert!(
        started.elapsed() < Duration::from_millis(1800),
        "SSH must win the race instead of waiting for the 2 s QUIC timeout (took {:?})",
        started.elapsed()
    );
    sup.shutdown().await;
}

/// Review Focus #2: a QUIC link that goes silent fails over to SSH, and the
/// supervisor upgrades back to QUIC once UDP flows again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fails_over_to_ssh_when_quic_dies_then_upgrades_back() {
    let h = Harness::new().await;
    let sup = Supervisor::spawn(h.spec(true));
    let mut rx = sup.phase();
    wait_phase(&mut rx, Duration::from_secs(5), "QUIC", connected(TransportKind::Quic)).await;

    h.relay.set_dropping(true);
    let dropped_at = Instant::now();
    wait_phase(&mut rx, Duration::from_secs(5), "failover to SSH", connected(TransportKind::Ssh)).await;
    assert!(dropped_at.elapsed() < Duration::from_secs(3), "failover took {:?}", dropped_at.elapsed());

    h.relay.set_dropping(false);
    wait_phase(&mut rx, Duration::from_secs(10), "upgrade back to QUIC", connected(TransportKind::Quic)).await;
    sup.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconnects_after_the_ssh_session_is_killed() {
    let h = Harness::new().await;
    let sup = Supervisor::spawn(h.spec(false));
    let mut rx = sup.phase();
    wait_phase(&mut rx, Duration::from_secs(5), "SSH", connected(TransportKind::Ssh)).await;
    h.ssh.kill_sessions().await;
    // If the supervisor had not reconnected, it would be Reconnecting/Failed by now
    // (the killed link is noticed immediately, dead_after is 500 ms).
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let now = sup.current();
    assert!(connected(TransportKind::Ssh)(&now), "expected a fresh SSH link, got {now:?}");
    sup.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconnects_after_daemon_restart() {
    let mut h = Harness::new().await;
    let sup = Supervisor::spawn(h.spec(false));
    let mut rx = sup.phase();
    wait_phase(&mut rx, Duration::from_secs(5), "SSH", connected(TransportKind::Ssh)).await;

    h.daemon.take().unwrap().shutdown().await;
    wait_phase(&mut rx, Duration::from_secs(5), "Reconnecting", |p| matches!(p, ConnPhase::Reconnecting { .. })).await;

    h.daemon = Some(start_daemon(h.dir.path()).await);
    wait_phase(&mut rx, Duration::from_secs(10), "SSH after restart", connected(TransportKind::Ssh)).await;
    sup.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_key_mismatch_is_fatal() {
    let h = Harness::new().await;
    let mut spec = h.spec(false);
    spec.host_key = HostKeyPolicy::Expect("SHA256:somebody-else".into());
    let sup = Supervisor::spawn(spec);
    let mut rx = sup.phase();
    match wait_phase(&mut rx, Duration::from_secs(5), "Failed", |p| matches!(p, ConnPhase::Failed { .. })).await {
        ConnPhase::Failed { error } => assert!(error.contains("host key"), "{error}"),
        other => unreachable!("{other:?}"),
    }
    sup.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_returns_to_idle() {
    let h = Harness::new().await;
    let sup = Supervisor::spawn(h.spec(true));
    let mut rx = sup.phase();
    wait_phase(&mut rx, Duration::from_secs(5), "connected", |p| matches!(p, ConnPhase::Connected { .. })).await;
    sup.shutdown().await;
    assert_eq!(*rx.borrow(), ConnPhase::Idle);
}
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p flc-conn --test supervisor`
Expected: compile errors (`ConnPhase`, `Supervisor` … not found).

- [ ] **Step 3: Implement `supervisor.rs`**

```rust
//! Connection supervisor: keeps one live control link to the daemon.
//!
//! * The first connect races QUIC against SSH and takes whichever completes
//!   first, so silently dropped UDP never delays the first link.
//! * While connected it sends `Ping` every `heartbeat` and declares the link
//!   dead after `dead_after` without any frame from the daemon.
//! * The authenticated SSH session is kept warm, so failover only costs a
//!   channel open and a Hello round trip.
//! * While on SSH it re-probes QUIC every `quic_reprobe` and upgrades when it works.
//! * Retries use exponential backoff with ±20 % jitter. Host-key, auth, key and
//!   protocol-version problems are fatal (`ConnPhase::Failed`).
use std::collections::HashMap;
use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use fl_proto::{ClientMsg, Hello, PROTO, RejectReason, ServerMsg};
use fl_transport::conn::{ConnError, FrameReader, FrameWriter, framed};
use fl_transport::quic::{self, QuicError};
use fl_transport::ssh::{HostKeyPolicy, SshAuth, SshError, SshSession, SshTarget};
use tokio::sync::{Mutex, watch};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

/// Fallback when the SSH server refuses `direct-streamlocal` forwarding.
pub const ATTACH_COMMAND: &str = "\"$HOME/.flotilla/bin/flotillad\" attach";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    Quic,
    Ssh,
}

impl fmt::Display for TransportKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            TransportKind::Quic => "QUIC",
            TransportKind::Ssh => "SSH",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnPhase {
    Idle,
    Connecting,
    Connected { kind: TransportKind, rtt_ms: u32, daemon_version: String },
    Reconnecting { attempt: u32, error: String },
    Failed { error: String },
}

#[derive(Debug, Clone)]
pub struct Timings {
    pub heartbeat: Duration,
    pub dead_after: Duration,
    pub quic_reprobe: Duration,
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    pub connect_timeout: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            heartbeat: Duration::from_secs(2),
            dead_after: Duration::from_secs(6),
            quic_reprobe: Duration::from_secs(30),
            backoff_min: Duration::from_millis(250),
            backoff_max: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(10),
        }
    }
}

#[derive(Debug, Clone)]
pub struct QuicSpec {
    pub host: String,
    pub port: u16,
    pub cert_sha256: [u8; 32],
}

#[derive(Debug, Clone)]
pub struct ConnectSpec {
    pub ssh: SshTarget,
    pub auth: SshAuth,
    pub host_key: HostKeyPolicy,
    pub socket_path: String,
    pub quic: Option<QuicSpec>,
    pub device_id: Option<String>,
    pub token: Option<String>,
    pub client_version: String,
    pub timings: Timings,
}

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error("SSH: {0}")]
    Ssh(#[from] SshError),
    #[error("QUIC: {0}")]
    Quic(#[from] QuicError),
    #[error("{0}")]
    Conn(#[from] ConnError),
    #[error("the daemon rejected the connection: {0:?}")]
    Rejected(RejectReason),
    #[error("handshake timed out")]
    HandshakeTimeout,
    #[error("unexpected message during handshake")]
    Unexpected,
    #[error("cannot resolve {0}")]
    Resolve(String),
    #[error("QUIC is not configured for this host")]
    QuicNotConfigured,
    #[error("connection task failed: {0}")]
    Task(String),
}

impl ConnectError {
    /// Errors that retrying cannot fix without user action.
    pub fn is_fatal(&self) -> bool {
        match self {
            ConnectError::Ssh(e) => e.is_fatal(),
            ConnectError::Rejected(RejectReason::ProtoMismatch { .. }) => true,
            _ => false,
        }
    }
}

pub struct Supervisor {
    phase: watch::Receiver<ConnPhase>,
    cancel: CancellationToken,
    task: Option<JoinHandle<()>>,
}

impl Supervisor {
    /// Must be called from inside a Tokio runtime.
    pub fn spawn(spec: ConnectSpec) -> Self {
        Self::spawn_on(&tokio::runtime::Handle::current(), spec)
    }

    /// Spawn onto `runtime` (callable from any thread, e.g. the UI thread).
    pub fn spawn_on(runtime: &tokio::runtime::Handle, spec: ConnectSpec) -> Self {
        let (tx, rx) = watch::channel(ConnPhase::Idle);
        let cancel = CancellationToken::new();
        let task = runtime.spawn(run(spec, tx, cancel.clone()));
        Self { phase: rx, cancel, task: Some(task) }
    }

    pub fn phase(&self) -> watch::Receiver<ConnPhase> {
        self.phase.clone()
    }

    pub fn current(&self) -> ConnPhase {
        self.phase.borrow().clone()
    }

    pub fn stop(&self) {
        self.cancel.cancel();
    }

    /// Stop and wait until the supervisor has closed its links (phase becomes `Idle`).
    pub async fn shutdown(mut self) {
        self.cancel.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

struct Shared {
    spec: ConnectSpec,
    /// Authenticated SSH session kept warm for fast failover.
    ssh: Mutex<Option<Arc<SshSession>>>,
}

struct Link {
    kind: TransportKind,
    reader: FrameReader,
    writer: FrameWriter,
    daemon_version: String,
    handshake_rtt: Duration,
    /// Keeps the QUIC endpoint and connection alive as long as the link.
    _quic: Option<(quinn::Endpoint, quinn::Connection)>,
}

enum LinkEnd {
    Cancelled,
    Dead(String),
    Upgrade(Link),
}

type LinkTask = JoinHandle<Result<Link, ConnectError>>;

fn set(tx: &watch::Sender<ConnPhase>, next: ConnPhase) {
    tx.send_if_modified(|current| {
        if *current == next {
            false
        } else {
            *current = next;
            true
        }
    });
}

fn ms(d: Duration) -> u32 {
    d.as_millis().min(u32::MAX as u128) as u32
}

fn backoff(t: &Timings, attempt: u32) -> Duration {
    let exp = t.backoff_min.saturating_mul(1u32 << attempt.saturating_sub(1).min(16));
    let capped = exp.min(t.backoff_max);
    let mut b = [0u8; 2];
    let _ = getrandom::fill(&mut b);
    let jitter = 0.8 + 0.4 * (u16::from_le_bytes(b) as f64 / u16::MAX as f64);
    capped.mul_f64(jitter)
}

async fn run(spec: ConnectSpec, phase: watch::Sender<ConnPhase>, cancel: CancellationToken) {
    let shared = Arc::new(Shared { spec, ssh: Mutex::new(None) });
    let mut attempt: u32 = 0;
    set(&phase, ConnPhase::Connecting);
    'outer: loop {
        let established = tokio::select! {
            _ = cancel.cancelled() => break 'outer,
            r = establish(&shared) => r,
        };
        match established {
            Ok(mut link) => {
                let connected_at = Instant::now();
                let reason = loop {
                    set(
                        &phase,
                        ConnPhase::Connected {
                            kind: link.kind,
                            rtt_ms: ms(link.handshake_rtt),
                            daemon_version: link.daemon_version.clone(),
                        },
                    );
                    match drive(&shared, &mut link, &phase, &cancel).await {
                        LinkEnd::Cancelled => break 'outer,
                        LinkEnd::Upgrade(better) => link = better,
                        LinkEnd::Dead(reason) => break reason,
                    }
                };
                drop(link);
                // A link that lived a while resets the backoff; a flapping one does not.
                if connected_at.elapsed() >= shared.spec.timings.dead_after {
                    attempt = 0;
                }
                attempt += 1;
                set(&phase, ConnPhase::Reconnecting { attempt, error: reason });
                if attempt > 1 {
                    let delay = backoff(&shared.spec.timings, attempt);
                    tokio::select! {
                        _ = cancel.cancelled() => break 'outer,
                        _ = tokio::time::sleep(delay) => {}
                    }
                }
            }
            Err(e) if e.is_fatal() => {
                set(&phase, ConnPhase::Failed { error: e.to_string() });
                return;
            }
            Err(e) => {
                attempt += 1;
                set(&phase, ConnPhase::Reconnecting { attempt, error: e.to_string() });
                let delay = backoff(&shared.spec.timings, attempt);
                tokio::select! {
                    _ = cancel.cancelled() => break 'outer,
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        }
    }
    set(&phase, ConnPhase::Idle);
}

async fn join_link(task: &mut Option<LinkTask>) -> Result<Link, ConnectError> {
    match task.as_mut() {
        Some(t) => match t.await {
            Ok(result) => result,
            Err(e) => Err(ConnectError::Task(e.to_string())),
        },
        None => std::future::pending().await,
    }
}

/// Race QUIC (if configured) against SSH; first success wins.
async fn establish(shared: &Arc<Shared>) -> Result<Link, ConnectError> {
    let mut quic_task: Option<LinkTask> = shared.spec.quic.clone().map(|q| {
        let s = shared.clone();
        tokio::spawn(async move { quic_link(&s.spec, &q).await })
    });
    let mut ssh_task: Option<LinkTask> = Some({
        let s = shared.clone();
        tokio::spawn(async move { ssh_link(&s).await })
    });
    let mut quic_err = if shared.spec.quic.is_none() { Some(ConnectError::QuicNotConfigured) } else { None };
    let mut ssh_err: Option<ConnectError> = None;
    let result = loop {
        tokio::select! {
            r = join_link(&mut quic_task), if quic_task.is_some() => {
                quic_task = None;
                match r {
                    Ok(link) => break Ok(link),
                    Err(e) => quic_err = Some(e),
                }
            }
            r = join_link(&mut ssh_task), if ssh_task.is_some() => {
                ssh_task = None;
                match r {
                    Ok(link) => break Ok(link),
                    Err(e) => ssh_err = Some(e),
                }
            }
        }
        if quic_task.is_none() && ssh_task.is_none() {
            let ssh_err = ssh_err.take().unwrap_or_else(|| ConnectError::Task("SSH attempt vanished".into()));
            let quic_fatal = quic_err.as_ref().is_some_and(ConnectError::is_fatal);
            break Err(if !ssh_err.is_fatal() && quic_fatal { quic_err.take().expect("checked above") } else { ssh_err });
        }
    };
    // QUIC won: let the SSH attempt finish in the background so its session is
    // warm for failover (dropping the JoinHandle detaches the task).
    drop(ssh_task.take());
    // SSH won: stop the QUIC attempt; drive() re-probes later.
    if let Some(t) = quic_task.take() {
        t.abort();
    }
    result
}

async fn ssh_session(shared: &Shared) -> Result<(Arc<SshSession>, bool), SshError> {
    let mut slot = shared.ssh.lock().await;
    if let Some(s) = slot.as_ref() {
        if !s.is_closed() {
            return Ok((s.clone(), true));
        }
    }
    *slot = None;
    let spec = &shared.spec;
    let session = Arc::new(SshSession::connect(&spec.ssh, &spec.auth, &spec.host_key, spec.timings.connect_timeout).await?);
    *slot = Some(session.clone());
    Ok((session, false))
}

async fn forget_ssh_session(shared: &Shared, session: &Arc<SshSession>) {
    let mut slot = shared.ssh.lock().await;
    if slot.as_ref().is_some_and(|s| Arc::ptr_eq(s, session)) {
        *slot = None;
    }
}

async fn ssh_link(shared: &Shared) -> Result<Link, ConnectError> {
    let t = &shared.spec.timings;
    let (session, reused) = ssh_session(shared).await?;
    if reused {
        // A cached session may be silently dead after a network change:
        // give it a short leash, then retry on a fresh session.
        match tokio::time::timeout(t.heartbeat * 2, open_ssh_link(&shared.spec, &session)).await {
            Ok(Ok(link)) => return Ok(link),
            // The session answered, so the problem is elsewhere (e.g. daemon down).
            Ok(Err(e)) if !session.is_closed() => return Err(e),
            _ => forget_ssh_session(shared, &session).await,
        }
        let (fresh, _) = ssh_session(shared).await?;
        return tokio::time::timeout(t.connect_timeout, open_ssh_link(&shared.spec, &fresh))
            .await
            .map_err(|_| ConnectError::HandshakeTimeout)?;
    }
    tokio::time::timeout(t.connect_timeout, open_ssh_link(&shared.spec, &session))
        .await
        .map_err(|_| ConnectError::HandshakeTimeout)?
}

async fn open_ssh_link(spec: &ConnectSpec, session: &SshSession) -> Result<Link, ConnectError> {
    let stream = match session.open_unix_socket(&spec.socket_path).await {
        Ok(stream) => stream,
        Err(SshError::SocketUnavailable { .. }) if !session.is_closed() => session.open_exec_stream(ATTACH_COMMAND).await?,
        Err(e) => return Err(e.into()),
    };
    let (r, w) = tokio::io::split(stream);
    let (reader, writer) = framed(r, w);
    handshake(spec, TransportKind::Ssh, reader, writer, None).await
}

async fn quic_link(spec: &ConnectSpec, q: &QuicSpec) -> Result<Link, ConnectError> {
    let addr = resolve(&q.host, q.port).await?;
    let (endpoint, conn) = quic::connect(addr, q.cert_sha256, spec.timings.connect_timeout).await?;
    let (send, recv) = quic::open_ctl(&conn).await?;
    let (reader, writer) = framed(recv, send);
    handshake(spec, TransportKind::Quic, reader, writer, Some((endpoint, conn))).await
}

async fn resolve(host: &str, port: u16) -> Result<SocketAddr, ConnectError> {
    tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| ConnectError::Resolve(format!("{host}: {e}")))?
        .next()
        .ok_or_else(|| ConnectError::Resolve(host.to_string()))
}

async fn handshake(
    spec: &ConnectSpec,
    kind: TransportKind,
    mut reader: FrameReader,
    mut writer: FrameWriter,
    quic: Option<(quinn::Endpoint, quinn::Connection)>,
) -> Result<Link, ConnectError> {
    let mut client_session_id = [0u8; 16];
    let _ = getrandom::fill(&mut client_session_id);
    let started = Instant::now();
    writer
        .send(&ClientMsg::Hello(Hello {
            client_version: spec.client_version.clone(),
            proto: PROTO,
            device_id: spec.device_id.clone(),
            token: spec.token.clone(),
            client_session_id,
        }))
        .await?;
    let reply = tokio::time::timeout(spec.timings.connect_timeout, reader.recv::<ServerMsg>())
        .await
        .map_err(|_| ConnectError::HandshakeTimeout)??;
    match reply {
        ServerMsg::Welcome(w) => Ok(Link {
            kind,
            reader,
            writer,
            daemon_version: w.daemon_version,
            handshake_rtt: started.elapsed(),
            _quic: quic,
        }),
        ServerMsg::Rejected(reason) => Err(ConnectError::Rejected(reason)),
        _ => Err(ConnectError::Unexpected),
    }
}

/// Heartbeat the link, report RTT, and (on SSH) re-probe QUIC.
async fn drive(
    shared: &Arc<Shared>,
    link: &mut Link,
    phase: &watch::Sender<ConnPhase>,
    cancel: &CancellationToken,
) -> LinkEnd {
    let t = shared.spec.timings.clone();
    let mut heartbeat = tokio::time::interval(t.heartbeat);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let can_upgrade = link.kind == TransportKind::Ssh && shared.spec.quic.is_some();
    let mut reprobe = tokio::time::interval_at(tokio::time::Instant::now() + t.quic_reprobe, t.quic_reprobe);
    reprobe.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut probe: Option<LinkTask> = None;
    let mut last_rx = Instant::now();
    let mut next_nonce: u64 = 0;
    let mut in_flight: HashMap<u64, Instant> = HashMap::new();
    let mut rtt: Option<Duration> = None;

    let end = loop {
        tokio::select! {
            _ = cancel.cancelled() => break LinkEnd::Cancelled,
            _ = heartbeat.tick() => {
                let silent = last_rx.elapsed();
                if silent >= t.dead_after {
                    break LinkEnd::Dead(format!("no response from the daemon for {} ms", silent.as_millis()));
                }
                next_nonce += 1;
                in_flight.insert(next_nonce, Instant::now());
                in_flight.retain(|_, sent| sent.elapsed() < t.dead_after);
                match tokio::time::timeout(t.heartbeat, link.writer.send(&ClientMsg::Ping { nonce: next_nonce })).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => break LinkEnd::Dead(format!("send failed: {e}")),
                    Err(_) => break LinkEnd::Dead("send timed out".into()),
                }
            }
            msg = link.reader.recv::<ServerMsg>() => match msg {
                Ok(ServerMsg::Pong { nonce: echoed }) => {
                    last_rx = Instant::now();
                    if let Some(sent) = in_flight.remove(&echoed) {
                        let sample = sent.elapsed();
                        let smoothed = rtt.map_or(sample, |prev| (prev * 7 + sample) / 8);
                        rtt = Some(smoothed);
                        set(phase, ConnPhase::Connected {
                            kind: link.kind,
                            rtt_ms: ms(smoothed),
                            daemon_version: link.daemon_version.clone(),
                        });
                    }
                }
                Ok(_) => last_rx = Instant::now(),
                Err(e) => break LinkEnd::Dead(format!("link lost: {e}")),
            },
            _ = reprobe.tick(), if can_upgrade && probe.is_none() => {
                let s = shared.clone();
                let q = shared.spec.quic.clone().expect("can_upgrade implies QUIC is configured");
                probe = Some(tokio::spawn(async move { quic_link(&s.spec, &q).await }));
            }
            r = join_link(&mut probe), if probe.is_some() => {
                probe = None;
                if let Ok(better) = r {
                    break LinkEnd::Upgrade(better);
                }
            }
        }
    };
    if let Some(p) = probe.take() {
        p.abort();
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_timings_match_spec() {
        let t = Timings::default();
        assert_eq!(t.heartbeat, Duration::from_secs(2));
        assert_eq!(t.dead_after, Duration::from_secs(6));
        assert_eq!(t.quic_reprobe, Duration::from_secs(30));
        assert_eq!(t.backoff_min, Duration::from_millis(250));
        assert_eq!(t.backoff_max, Duration::from_secs(5));
        assert_eq!(t.connect_timeout, Duration::from_secs(10));
    }

    #[test]
    fn backoff_grows_and_is_capped_with_jitter() {
        let t = Timings::default();
        for _ in 0..50 {
            let first = backoff(&t, 1);
            assert!(first >= Duration::from_millis(200) && first <= Duration::from_millis(300), "{first:?}");
            let third = backoff(&t, 3);
            assert!(third >= Duration::from_millis(800) && third <= Duration::from_millis(1200), "{third:?}");
            let late = backoff(&t, 40);
            assert!(late >= Duration::from_secs(4) && late <= Duration::from_secs(6), "{late:?}");
        }
    }

    #[test]
    fn transport_kind_display() {
        assert_eq!(TransportKind::Quic.to_string(), "QUIC");
        assert_eq!(TransportKind::Ssh.to_string(), "SSH");
    }

    #[test]
    fn fatality() {
        let mismatch = SshError::HostKeyMismatch { expected: "a".into(), actual: "b".into() };
        assert!(ConnectError::Ssh(mismatch).is_fatal());
        assert!(ConnectError::Rejected(RejectReason::ProtoMismatch { server: PROTO }).is_fatal());
        assert!(!ConnectError::Rejected(RejectReason::BadToken).is_fatal());
        assert!(!ConnectError::HandshakeTimeout.is_fatal());
        assert!(!ConnectError::Ssh(SshError::Timeout).is_fatal());
    }
}
```

- [ ] **Step 4: Run all flc-conn tests**

Run: `cargo test -p flc-conn`
Expected:
- unit tests pass: profile 1, bootstrap 15, supervisor 4;
- the 8 supervisor integration tests pass, all within about 20 s.

Run them three times (`for i in 1 2 3; do cargo test -p flc-conn --test supervisor || break; done`) to catch timing flakes. If one is flaky, find the race in the implementation; don't loosen the test's timing bounds beyond what Review Focus #2 allows.

- [ ] **Step 5: Commit**

```bash
cargo fmt -p flc-conn && cargo clippy -p flc-conn --all-targets -- -D warnings
git add crates/client/flc-conn/src/supervisor.rs crates/client/flc-conn/tests/supervisor.rs
git commit -m "feat(flc-conn): connection supervisor with QUIC/SSH race, heartbeat, failover, upgrade

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```
