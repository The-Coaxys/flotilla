# Flotilla M1 — Persistent remote terminals

**Status:** implementation underway. The shared checkpointable terminal engine is implemented and verified; process persistence, service/client integration and Windows M1 acceptance remain in progress.

**Goal:** a native Windows remote terminal workspace with tabs, panes and view splits. Shells and their output survive client closure, SSH/QUIC migration, sleep/network loss and daemon restart. No Electron or local copy of the workspace.

**Authority:** `../specs/2026-11-02-flotilla-design.md`, especially §§2.1, 3.3–3.7, 4, 5.1, 8.3 and 14. M1 includes predictive echo and the terminal features named in §5.1; they are not deferred to later milestones.

## Architecture decisions

1. The independent prochost owns every PTY master, reader, writer and child. A daemon connection owns subscriptions, never processes. `flotillad prochost` is the same installed static executable; it starts in a checked, detached session with independent stdio. Preserve the daemon service's `KillMode=process`; `setsid` alone does not escape a systemd cgroup.
2. The daemon owns canonical emulator state, layout and resize authority. The native client owns presentation, focus, selection, local composition and speculative overlays. Only the daemon can generate terminal-protocol replies into the PTY.
3. Use the same pinned `fl-term` implementation on Linux and Windows. Pin `alacritty_terminal = 0.26.0` and `vte = 0.15.0`, with narrowly scoped, licensed vendor patches for complete checkpoint import/export and deterministic extension handling. Do not substitute screen-only ANSI replay or an unbounded from-start byte log.
4. Reuse SQLite/WAL for canonical layout, session metadata and durable checkpoints through `fld-store`. The first daemon-owned terminal workspace is the default workspace; terminal records have optional worktree association so M2 can attach them to real worktrees without changing their identities.
5. Keep the 2 MiB hot output ring. Add a segmented prochost disk journal retained until the daemon acknowledges a durable checkpoint covering the corresponding records. A finite in-memory ring cannot preserve arbitrary output through a long daemon outage. Proposed default: 256 MiB unacknowledged journal per terminal, then stop reading that PTY until space is reclaimable. The child remains alive but may block on output; never silently discard required recovery bytes. The quota and disk-full state must be visible and configurable.
6. Transport queues remain bounded and visibility-driven. Use separate authenticated logical streams for control, visible terminal traffic, background resync and bulk snapshots; do not put snapshots ahead of heartbeats or keystrokes on one FIFO.
7. No source of truth on the UI thread. Parse terminal bytes off-thread, publish immutable/generation-tagged render data, and avoid allocation or text shaping for unchanged cells/rows.

## Verified API findings that constrain the implementation

- The published Alacritty 0.26.0 release identifies commit [`94e7c8874e526b1e67b349d9ba30ddf81669119e`](https://docs.rs/crate/alacritty_terminal/0.26.0/source/.cargo_vcs_info.json). The `v0.16.0` application tag is **not** this crate version; do not use it as the pin.
- [`Term`](https://docs.rs/alacritty_terminal/0.26.0/alacritty_terminal/term/struct.Term.html) exposes render/grid/mode accessors, not a complete resumable-state API. Existing grid serde skips current and saved cursors; grid equality also omits state needed by the next byte.
- [`vte::ansi::Processor`](https://github.com/alacritty/vte/blob/v0.15.0/src/ansi.rs) owns previous-character and synchronized-update state in addition to the incremental parser. Partial UTF-8, CSI/OSC/DCS and buffered synchronized updates must survive arbitrary byte-boundary checkpoints.
- Alacritty's hyperlink IDs use process-global state, and its keyboard-stack overflow code mutates the title stack. Patch these narrowly: deterministic per-terminal identities and the correct bounded stack. Tests must demonstrate the consumer-visible defects, not match source text.
- Upstream OSC buffers and combining-character sequences are not strictly bounded. A shared, explicit overflow policy is required; truncating only the serialized snapshot is invalid. Task 1 fixes and documents limits consistently in live parsing and restore.
- Existing GPUI 0.2.2 `flc-ui/src/input.rs` demonstrates `Element` layout/prepaint/paint and `EntityInputHandler`. It is not a terminal widget: do not copy its single-line paste handling or Ctrl-C/V actions.
- GPUI keybindings run before ordinary key listeners. No application-level plain Ctrl-letter binding may consume focused terminal input. Windows `WM_CHAR` does not deliver terminal control keys; encode them from key events and avoid duplicate printable text/IME commits.
- GPUI `shape_line` run lengths are UTF-8 bytes; its forced width advances per glyph, not terminal column. Wide and combining cells need explicit cell positioning. Shape visible runs and clip them to the terminal content rectangle.
- [`portable-pty 0.9.0`](https://docs.rs/portable-pty/0.9.0/portable_pty/) supplies `openpty`, `try_clone_reader`, one `take_writer`, `resize`, child wait and kill handles. Dropping its Unix writer injects newline plus VEOF. Its Unix `kill` sends SIGHUP to the direct child, not a complete process-tree kill. `process_group_leader` observes the current foreground group. Preserve the writer across every daemon reconnect and implement explicit terminal termination semantics.

## Shared contracts

### Identities and ordered terminal state

Introduce typed UUID IDs in `fl-model`: workspace, terminal, surface, tab, view and operation IDs. Distinguish terminal identity from **child incarnation**; a cold relaunch must not accept stale offsets or input from an earlier process.

Every terminal event has a monotonically increasing record sequence. Output records additionally cover absolute byte ranges `[start, end)`. Resize, configuration, synchronized-update flush, exit and metadata records participate in the same ordering even when no output byte advances. A checkpoint identifies both the consumed byte offset and record position. Do not order a resize or parser timeout using only byte offsets or independent client clocks.

Input carries `(client_session_id, input_sequence, bytes)` and an explicit acknowledgment. The prochost retains per-client write progress/deduplication across daemon restarts; partial PTY writes advance only the acknowledged byte prefix. A replay cannot write a committed prefix twice. Generated terminal replies use stable effect IDs and the same deduplicated prochost input boundary.

A resize is accepted only from the most recent input owner. Approved dimensions are broadcast as ordered state; a different client's local pane size does not independently resize its emulator. Render the canonical dimensions clipped/letterboxed until ownership changes.

Predictive echo additionally requires daemon-reported TTY echo enabled, a confirmed OSC133 shell-prompt state and no alternate screen. Refresh the echo observation at the input boundary; a stale prompt flag must not speculate password-entry text. Predictions are client-only overlays and never mutate the canonical parser.

### Checkpoints, history and effects

A versioned checkpoint contains engine/fork/configuration identity, terminal/incarnation IDs, record/byte positions, both grids and their current/saved cursors, margins, charsets, tabs, modes, palette, title and keyboard stacks, parser/processor state, OSC7/133 extension state, and effect delivery watermarks. Reconstruct listeners/locks/damage state; never serialize callback closures or OS `Instant` values.

- A durable daemon recovery checkpoint contains all retained history (10k lines by default). Reclaim prochost journal segments only after checkpoint file durability and the database reference commit; a crash must leave a recoverable checkpoint plus contiguous suffix.
- An initial client history view carries the last 1000 lines and a history generation. Older pages are fetched on demand. This is a deliberately partial history view, not proof of full-history checkpoint equivalence. Task 1 must prove visible-state/resize convergence with omitted older rows before adopting the optimized tail form; otherwise transmit the complete engine checkpoint separately from the paged UI history. Do not ship a knowingly divergent restore.
- Snapshot installation is atomic. Buffer bounded live records after the advertised checkpoint position, validate the complete snapshot/engine identity, swap state, then apply the suffix. Late chunks/events from another incarnation or attach generation cannot overwrite current state.
- Snapshots are chunked on the bulk stream; never exceed the existing frame bound. When a client lags more than 512 KiB, replace that client's pending output with a newer checkpoint. This does not discard recovery data from the prochost.
- Live daemon parsing alone may originate device queries/replies, bells, clipboard requests and other external effects. Mirror parsing has no external effects. Recovery replays persisted typed effects/delivery state, neither duplicating committed effects nor losing a reply parsed just before a crash.
- OSC52 requires explicit per-host consent for reads and writes. Permission prompts and clipboard contents are not replayed merely because the client reattaches.

### Protocol and connection cutover

Keep the existing Hello/Welcome wire layouts stable. Postcard is positional: a serde default does **not** justify appending arbitrary struct fields. Append enum variants without reordering discriminants, negotiate terminal capabilities after Welcome, and add a compatibility fixture for the previous minor protocol.

M1 increments the application/protocol minor version. A new client observing a daemon that cannot serve M1 must bootstrap the bundled daemon over an authenticated, host-key-pinned SSH session before subscribing to terminal traffic. Previously remembered hosts need this upgrade path too; do not require deleting/re-adding the host. The unchanged prochost protocol lets existing children survive that upgrade.

Logical streams bind to an authenticated control session using a short-lived daemon-issued capability and channel role; a tokenless new QUIC stream is not implicitly authorized by a guessed terminal ID. Close all stream tasks on generation change or client shutdown. Reuse M0's reconnect, failover and deadline contracts.

### Offline input and cache

The client persists the last acknowledged layout/mirror with a debounced atomic write. Render it immediately as stale. Keep server output checkpoints and local predictive overlays distinct.

Terminal input waiting through an outage is visibly pending, bounded, and sent only if the outage is shorter than five seconds and the same child incarnation resumes. Otherwise discard it with an explicit notice. IME preedit never reaches the server until commit. Persist layout operations with stable IDs and reconcile against the authoritative layout revision; do not replay stale terminal keystrokes from disk.

## Implementation order and ownership

Each task ends with its narrow behavior proof. No task may replace a missing real path with a mock/no-op; test fixtures only exercise production code. Parallel implementation begins only after the listed contract prerequisites exist.

| Task | Owned files/modules | Depends on | Observable acceptance |
|---|---|---|---|
| 1. Exact terminal engine | New `fl-term`; pinned vendor Alacritty/VTE patches, notices; workspace manifests | M0 protocol baseline | Linux-to-Windows checkpoints at every byte cut preserve future visible state, modes and effect transcript; hidden cursor/charset/stack/parser state and resource boundaries covered |
| 2. Typed model and wire contracts | New `fl-model`, `fl-prochost-proto`; append terminal/control message types in `fl-proto` | Task 1 checkpoint schema | Frame round trips at real bounds; malformed/unknown schema and stale incarnation rejected; previous-minor Hello/Welcome still decode; frozen prochost handshake explicit |
| 3. Persistent prochost | New `daemon/fld-prochost`; `flotillad prochost` entry; process launch paths | Task 2 | Real login PTY, write/resize, final output drain and exit; writer/child identities survive daemon SIGKILL/restart; journal overflow backpressures without losing bytes; partial-input replay does not duplicate bytes |
| 4. Durable daemon state | New `daemon/fld-store`; schema/migrations/checkpoint journal references; daemon startup/restore | Tasks 1–3 | Crash windows leave a usable checkpoint plus contiguous journal; restored terminal accepts continued output; layout and terminal identities survive daemon/client restart |
| 5. Terminal service and channels | `fld-server` terminal registry/RPC/subscriptions; session-bound stream auth; emulator/effect workers | Tasks 2–4 | Create/list/attach/input/resize/close against actual prochost; same-offset control events ordered; parked terminals stop streaming; slow client receives a replacement snapshot; unauthorized IDs/streams cannot attach |
| 6. Client session and sync | `flc-conn` connection broker/owned channels; new `client/flc-sync`; remembered-host upgrade | Tasks 2,5 | RPC and subscriptions survive QUIC↔SSH migration; no duplicate output/input; pending operations reconcile; snapshot generations and child incarnations reject stale traffic; daemon upgrade preserves children |
| 7. Native terminal surface | New `client/flc-terminal` custom Element, input/IME, metrics, selection/search/links; shared key/mouse encoders in `fl-term` | Tasks 1,6 | Real shell, colors, full-screen app, wide/combining glyphs, IME, control keys, mouse reporting, clipboard consent, bracketed paste and resize at real Windows DPI |
| 8. Workspace shell/layout | New `client/flc-shell` split/tab/view model; move corresponding app shell ownership from binary; default terminal workspace | Tasks 2,6,7 | New terminal, close, rename, reorder, tab switching, pane/view splits and ratio adjustment; focus and server-approved size correct; restart restores layout and reattaches existing shells |
| 9. Predictive echo and resilience | `flc-terminal` prediction overlay and `flc-sync` offline state; Windows power/network notifications | Tasks 6–8 | Off/Auto/On; Auto only above 150 ms RTT at a confirmed shell prompt, never alt-screen; matching echo commits, mismatch rolls back; >5 s offline input discarded; resume reconnects immediately |
| 10. Delivery and performance | Existing CI/Windows installer/smoke, package notices, milestone docs | Tasks 1–9 | Installed Windows terminal app demonstrated against real VPS; daemon crash and network/firewall/sleep scenarios preserve child/state; throughput, frame time, startup and idle memory recorded |

Task 1's checkpoint schema is the first shared prerequisite; Task 2 freezes the model and wire contracts before process/service integration. Later, the renderer/input and shell/layout implementations can proceed independently against the stabilized Task 6 session API, then integrate for Task 8's real terminal workspace proof. Do not pretend an integration dependency is independent merely to add another lane. One owner integrates manifests, protocol discriminants and binary startup.

## Acceptance matrix

### Engine and protocol

- Every-cut continuation for split UTF-8 (valid/invalid), CSI subparameters, OSC BEL/ST boundaries, ignored/DCS states and cancellation.
- Saved cursor SGR/charset/hyperlink, primary/alternate-screen return, delayed wrap, custom tabs/margins, title push/pop, palette query/reset, kitty mode stacks, CSI REP, combining/wide cells and history eviction/reflow.
- Synchronized-update buffering and daemon-authored flush records, not independent wall-clock guesses.
- Deterministic hyperlink IDs; mirrors do not emit PTY replies/clipboard effects. A crash between query parsing and input acknowledgment neither drops nor duplicates the reply.
- Rejected malformed checkpoints include bounds, sizes, enum bits, ranges and mismatched engine identity. Limits apply during live parsing as well as decode; unknown/oversized data never becomes an allocation bomb.

### Real processes, durability and transport

- Run real `$SHELL -l`, inspect `stty size`, use `vim`/`less` and a continuously printing child. Verify PID and a monotonic output sequence before/after daemon SIGTERM, SIGKILL and service restart.
- Keep producing output while the daemon is absent beyond the hot-ring size. Reconnect, recover contiguous output/state, then demonstrate the documented quota/backpressure behavior. Test disk-full/checkpoint interruption and final output arriving after child exit.
- Use real OpenSSH with stream-local forwarding both enabled and disabled. Blackhole UDP rather than merely closing the endpoint; restore QUIC and verify offsets/input deduplication across both directions of migration.
- Two clients with different pane sizes obey last-input resize ownership. An old snapshot, old child incarnation or delayed former connection cannot corrupt the active view.
- A remembered M0 host upgrades in place without re-enrollment or terminating the prochost's children. Changing authentication/host keys still fails closed.

### Native interaction

- Plain Ctrl-A/C/D/L/V/Z reach the terminal. Copy/paste use Ctrl-Shift-C/V; app actions do not steal Ctrl-letter. Bracketed paste retains multiline Unicode and wraps exactly once when enabled.
- App shortcuts match the spec: Ctrl-Shift-T, pane splits Alt-Shift-= / Alt-Shift--, view splits Ctrl-Alt-Shift-= / Ctrl-Alt-Shift--, Ctrl-Tab / Ctrl-Shift-Tab. M0's Add-host shortcut must not become the future New-worktree action accidentally.
- Verify US/AltGr layouts, dead keys, CJK preedit/commit/cancel, emoji surrogate pairs, repeat keys and IME candidate bounds at multiple DPI settings. Record GPUI upstream limitations rather than substituting incorrect key encodings.
- Selection across soft wraps, wide-cell boundaries and scrollback, search, OSC8/URL/path actions, output while scrolled back, fractional wheel movement, drag release outside the pane and application mouse-reporting mode.
- OSC7 cwd, OSC133 prompt marks, title and bell observable; OSC52 asks once per host and remains denied without consent. All §5.1 features need an exercised path.
- Predictive echo is an overlay only: high-latency shell typing looks immediate, wrong predictions disappear, and full-screen/password input is never guessed.

## Performance evidence (M1, Linux x86_64, release build)

Measured with a temporary driver over the production `fl-term` engine, then removed:

| Measurement | Result |
|---|---|
| Parse throughput, 120×40, SGR + wide glyphs sent as 20 KiB chunks | 40 MiB in 535 ms → **74.8 MiB/s** |
| Frame build for a full 120×40 screen | 2000 frames in 14.3 ms → **7.1 µs/frame** (0.04 % of a 60 Hz budget) |
| Checkpoint save / restore of a full scrollback (10.9 MB) | 141 ms / 147 ms, byte-identical cells on both sides |

Caveats: these are engine-side numbers on a Linux workstation, not the Windows
client's measured paint time and not end-to-end daemon→client throughput over
the VPS. A 10.9 MB checkpoint reflects a completely full scrollback; the daemon
writes it off the UI path. Client window frame timing, DPI/IME behavior and RSS
must be recorded from the installed Windows build before the milestone closes.

## Completion gate

M1 is complete only with a released, installed Windows build used as a remote terminal against the VPS, the resilience/interaction matrix above, updated licenses/notices and docs, and recorded performance evidence. A compiling renderer, an interactive shell without durable recovery, or tests of disconnected pieces do not satisfy the milestone.

Resource limits, lossless-journal backpressure and the checkpoint/history distinction are explicit design decisions. Confirm them during plan review before implementation; do not silently weaken exact resume or allow unbounded memory to claim compatibility.

## Execution evidence

- Task 1: `fl-term` and the pinned Alacritty/VTE forks preserve incremental parser, synchronized-update, grid and terminal metadata state. Canonical snapshots exclude local focus/selection/viewport state. Mirrors suppress externally observable effects.
- Allocation-bounded checkpoint decoding, live resource limits, keyboard/paste/mouse encoding and selection/history extraction are covered by the shared-engine suite: 36 tests passed. Alacritty's library suite passed 139 tests; its no-default-features build passed. Both VTE ANSI/serde feature variants and the no-default-features parser build passed. `cargo clippy --locked -p fl-term --all-targets -- -D warnings` passed.
- A temporary runtime driver captured real shell output, saved a checkpoint midway through the UTF-8 encoding of `界`, restored it from disk, and proved identical continued state and query replies. Observed cwd: `file://localhost/srv/work`; resumed byte offset: 67. The driver and its disk checkpoint were removed after proof.
- Removed upstream fixtures tied to a 9,999-column regex-cache implementation and a fixed 4,096-entry title stack; these conflict with the bounded engine contract. Title restoration is exercised through actual ANSI input and checkpoint continuation instead.

- Task 2: `fl-model` implements the approved view-split → ordered-tabs → pane-split hierarchy, bounded tree decoding, atomic layout operations and stable typed identities. `fl-proto` preserves M0 Hello/Welcome layouts at protocol 0.2, adds validated terminal/bulk channels and chunked checkpoint offers; `fl-prochost-proto` freezes the independent local protocol at version 1. Output records use shared byte storage rather than copying retained payloads.
- Contract verification passed 92 tests across model, engine and both protocol crates, plus warnings-denied Clippy. A temporary runtime driver sent fragmented/compressed frames over a real Unix socket, created a tab and split its pane through the production layout API, transferred 65,536 bytes captured from a real shell process and verified exact record/byte cursor continuity. The driver was removed after proof. This is contract proof, not yet a working terminal service or Windows M1 acceptance.

- Task 3: `fld-prochost` owns native PTYs in a dedicated OS-I/O actor, with bounded multiplexed Unix IPC, same-UID peer checks, detached startup and independent stdio. `flotillad prochost` is wired as the installed executable's helper entry. Journal records and ack/tail watermarks are checksummed and durable; quota or I/O pressure retains pending data and pauses reads rather than evicting recovery bytes.
- Native/journal/IPC regressions passed, including same-offset controls, restart/corruption/ack crash windows, input-prefix replay, real kernel-backpressured input expiry, final drain, background session termination and delivery of idle-shutdown acknowledgment. The reaped-leader `pidfd_open(EINVAL)` case failed before correction; an independent kernel probe confirmed that a retained session ID need not still identify a signalable task.
- A temporary standalone executable used production detached-startup, server and client code with `/bin/bash -l`. PID 1144157 survived an attached frontend's SIGKILL. After all consumers disconnected, a file barrier released exactly 3 MiB of output; every byte was recovered beyond the hot ring. A 128 KiB quota then stopped and resumed a further 512 KiB without loss after a durable consumer capture/ack. Kernel size 33×100, ECHO disabled, final drain and acknowledged idle shutdown were observed. This proves the prochost path; daemon canonical-checkpoint recovery and installed Windows acceptance remain later integration gates.
- Prochost requires Linux ≥5.3, mounted `/proc`, usable PTYs and permitted pidfd syscalls. Limits: 128 retained terminals, 64 live/draining PTYs, 128 actor commands, 256 non-evicted client writer identities per incarnation; 64 KiB output/input records, 512 KiB/256-record reads, a 2 MiB/4,096-record hot ring, and default 256 MiB journal quota (minimum 128 KiB). Atomic segment compaction may temporarily need filesystem workspace. Environment overrides are bounded to 128 entries/128 KiB; metadata to 512 KiB.
- The wire carries absolute Linux boot-clock input deadlines, enforced at the prochost's actual write boundary. Client retries cannot extend the original deadline; expired suffixes are reported with the actual committed prefix. Deterministic engine replies do not expire. The daemon admits input only with a deadline at most five seconds ahead of its boot clock; client-side clock conversion remains pending.
- Task 4: `fld-store` keeps layouts and terminal intents in SQLite (WAL, synchronous FULL), with operation-ID dedup, revision checks and monotonic checkpoint references. Checkpoints are content-addressed files published durably before the database commit that references them. A crash-window test proves that a committed checkpoint without a journal acknowledgment replays the exact suffix, yielding identical state and replies. The pre-upgrade backup was found to depend on WAL sidecar files and is now converted to a self-contained file.
- Task 5: `fld-server` runs one authoritative engine per terminal, restores from checkpoint plus journal suffix at startup, and reconciles desired, lost and closed terminals against the process host. Channels bind with one-use, session-scoped 30-second tickets over Unix or QUIC; the owner lease guards resize; input deadlines are checked against the daemon's boot clock. Slow subscribers receive a snapshot instead of an unbounded backlog. Real-shell tests cover input echo, `stty size` after resize, a daemon restart with the same PID and byte-identical mirror state, ticket abuse, parking, a 9 MB flood, QUIC streams, startup reconciliation and close cleanup.
- Task 9 (prediction): `flc_terminal::predict` models predictive echo as a presentation overlay. It refuses full-screen programs, a stale mirror, an exited child and a terminal whose canonical echo is off (password prompts); `Auto` additionally requires a link of at least 150 ms and a prompt confirmed by OSC 133. Matched cells commit and advance the anchor, a contradicting cell rolls the whole overlay back, and any control byte clears it. The view paints the pending text dimmed at the cursor.
- Task 6: `flc-conn` multiplexes RPCs and opens one-use ticket-bound channels per control-link generation; `flc-sync` mirrors the record stream into a local engine off the UI thread, retries input from the daemon's committed prefix, discards input after a five-second outage, installs hash-verified snapshots, reconciles layout revision conflicts and renders a stale cached layout with no server. Integration tests use a real daemon, process host and shells: QUIC↔SSH failover with no duplicate or lost marker output, snapshot-versus-streamed checkpoint equality, daemon restart on the same PID, outage discard, concurrent layout reconciliation and offline cache rendering.
- Offline input during a restart under five seconds is written exactly once; over five seconds it is discarded with an `Outage` reason and never reaches the PTY.
- Task 7: `flc-terminal` renders a `TerminalSource` (frame generation + one-shot wakers, input, resize, local viewport scroll, text/history extraction, stale/exited status) with a custom GPUI element: batched same-style runs, explicitly positioned wide/combining clusters, merged background quads, visible rows only, and per-row reuse of shaped runs. Plain Ctrl+letter is taken by a keystroke interceptor ahead of app bindings; spec app chords pass through; Ctrl+Shift+C/V copy/paste; IME preedit is local until commit. Selection, scrollback search, OSC 8/URL/path links, mouse reporting, wheel accumulation and resize debounce are window-free modules with 49 tests over real `fl-term` frames. Linux is a headless type-check; the Windows window run (DPI, IME, AltGr/dead keys) remains the acceptance gate. GPUI limits found: interceptors lack key-repeat state; Windows keystrokes drop Shift on shifted punctuation and carry no keypad identity; only straight/wavy underlines exist.
- Task 7 verification: 55 window-free tests pass plus warnings-denied Clippy. The rendered surface itself must be reviewed from Windows CI output, not here.
- Task 8: `flc-shell` renders the daemon's projected layout as a recursive view split tree with per-view tab strips and terminal panes backed by `flc-terminal` views over `flc-sync` mirrors. Intents preview locally and submit `LayoutOp`s; unacknowledged operations, the offline copy and rejected operations are visible. New terminal, close tab/pane/view, rename (inline editor), pin, reorder, tab/view/pane selection, pane and view splits, and ratio changes are reachable from the strip and keyboard. Eight window-free model tests cover projection, focus after close, split geometry, ratio clamping, pinned ordering and attachable incarnations. The app builds one session and shell per connected host and keeps the host card while connecting.
- Task 8 shortcuts are now a pure table (`shortcut_for`) with tests: the spec chords map to their actions, plain Ctrl+letter and shifted text stay with the terminal, and Ctrl-Tab / Ctrl-Shift-Tab cycle forward/back. The extracted table exposed that Ctrl-Shift-Tab previously matched no arm at all: the single `ctrl + tab` arm required shift to be released, so backwards cycling was unreachable. The old arm also passed a constant `!shift` (always true).
- Task 8 limits: split ratios are adjusted by keyboard and a divider click resets to even; pointer-dragging a divider, tab drag-and-drop with a drop-ring preview and the saved-layout launcher are not implemented yet. Panes report their own grid size, so the daemon's approved size stays authoritative.
- A rejected layout intent is never silently dropped: the shell shows a banner and the next published layout replaces the local preview.
- Task 9 (offline and resume): the supervisor exposes `retry_now()`, which skips an accumulated backoff with a fresh attempt budget; the app calls it for every host when the window is activated, so returning to a laptop reconnects at once instead of after the full backoff. A supervisor test with a real daemon, killed transports and three failed reconnects proves a new attempt starts within 500 ms of the call.
- Task 9 limits: OS power-resume notifications are not wired (no `WM_POWERBROADCAST` handler), so a resume detected only while the app is backgrounded waits for the next activation; input accepted before a transport drop but acknowledged after the five-second TTL is reported expired and never reaches the PTY.
- Framing regression found while running the full workspace suite: the large-message test sent a client version that protocol 0.2 validation rejects, so the writer failed before writing a byte while the reader waited forever — the workspace suite and both platform jobs deadlocked for the full job timeout. The compression test now frames 200 KiB of incompressible data through a free-standing wire type, and a new test pins that payload validation precedes writing: a rejected message surfaces as a clean close, never as a peer blocked on a frame.
- Not yet carried on the terminal protocol: bell, OSC52 clipboard and title/cwd notifications. OSC52 reads receive no reply until the per-host consent flow lands. Owner leases reset on daemon restart.
