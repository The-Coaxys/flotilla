# Flotilla M1 — Persistent remote terminals

**Status:** implementation plan; no M1 code has landed. M0 delivery and its Windows acceptance remain separate gates.

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

## Completion gate

M1 is complete only with a released, installed Windows build used as a remote terminal against the VPS, the resilience/interaction matrix above, updated licenses/notices and docs, and recorded performance evidence. A compiling renderer, an interactive shell without durable recovery, or tests of disconnected pieces do not satisfy the milestone.

Resource limits, lossless-journal backpressure and the checkpoint/history distinction are explicit design decisions. Confirm them during plan review before implementation; do not silently weaken exact resume or allow unbounded memory to claim compatibility.
