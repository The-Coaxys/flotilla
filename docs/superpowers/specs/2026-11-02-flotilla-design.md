# Flotilla: Design Spec

- **Date:** 2026-11-02
- **Status:** Draft, awaiting owner review
- **Repo (to be created after approval):** `github.com/The-Coaxys/flotilla` (private)
- **Artifacts:** `Flotilla.exe` (Windows client), `flotillad` (VPS daemon), `fl` (agent/automation CLI)

---

## 0. Intent

### What the owner asked for
- Rebuild the whole super.engineering application, with every feature (scope option **C**), as a **native Windows app**. No Electron, heavily optimized.
- **The VPS is the main workspace.** The Windows app connects to it, and the connection must feel smooth.
- Copy super.engineering's UI design.
- Support two transports: **SSH and QUIC**, with automatic fallback.
- Keep the code in a private GitHub repo under **The-Coaxys**, built by **GitHub Actions**.
- Name: **Flotilla**.
- Architecture **A**: the daemon on the VPS owns all state and processes, and the Windows app is a native view.

### Assumptions (correct any of these at review)
1. **Single user.** One human with one or more personal devices. No multi-tenant accounts.
2. **Same Unix user.** Agents run as the Unix user that runs `flotillad` (root on this VPS today), using the CLIs already installed and authenticated there: `claude`, `codex`, `cursor-agent`, `opencode`, `grok`, `pi`, `omp`, `hermes`, `aider`.
3. **Out of scope:** the iPhone app, team/enterprise rollout, and telemetry.
4. **GitHub first.** It is the first forge, using `gh`, which is already authenticated on the VPS. GitLab and Bitbucket sit behind the same interface and come later.
5. **Visual design.** Flotilla reproduces super.engineering's layout, density, palette and interaction patterns. The name, logo, icons and all assets are Flotilla's own. No super.engineering code or assets are copied.
6. **No local workspaces.** All repositories, terminals and agents run on the VPS. The Windows app never runs agents locally.

### Success criteria
| Metric | Target |
|---|---|
| Cold start to interactive UI (cached state shown) | < 500 ms |
| Client idle memory | < 200 MB; one process (WebView2 only while a browser tab is open) |
| Daemon idle RSS | < 60 MB, plus about 3 MB per live terminal, plus scrollback |
| Keystroke to echo, focused terminal | RTT + ≤ 5 ms overhead; with predictive echo, perceived ≈ 0 |
| Any network drop | UI never freezes; every visible terminal is current again within 1 RTT + snapshot transfer (< 200 ms for typical screens on a 50 ms link) |
| PC off or asleep | Agents keep running; reopening shows the current state |
| Scale | 20+ concurrent agent sessions with no UI jank (frame time < 8 ms at 120 Hz) |
| Daemon upgrade | Running agents and terminals survive it |

---

## 1. Reverse-engineering findings (reference)

**Sources:**
- 43 docs pages at `super.engineering/docs/*`
- The product recording `app-flex-dark.mp4`, frames extracted
- The macOS nightly `super.engineering-nightly-2fa0a3be-x86_64.dmg` (sha256 `2fe9b8bf…2ebd5`), decompressed with a custom UDIF reader, then crate paths extracted from the binary

### Their stack (observed in the shipped binary)
| Concern | Crate / technology |
|---|---|
| UI framework | **GPUI** (vendored `crates/gpui`), Metal renderer, `taffy` layout, `resvg`/`tiny-skia` for SVG |
| Terminal | `alacritty_terminal` (pinned git rev `936aee8`), `vte 0.15`, `portable-pty 0.9` |
| Syntax / diff | `tree-sitter 0.26` + `tree-sitter-highlight`, `imara-diff 0.2` |
| State | `rusqlite 0.32` (SQLite) |
| Local services | `axum 0.8` (hook / MCP server), `tokio-tungstenite` (`sc_hub`, mobile relay), `reqwest` |
| In-house crates seen | `sc_ui`, `sc_browser`, `sc_hub`, `sc_hub_proto`, `sc_terminal_core`, `sc_terminal_view`, `sc_markdown`, `sc_db`, `sc_chat_transcript(_store)`, `sc_artifact(_types)`, `sc_fuzzy`, `sc_import`, `sc_bezel`, `sc_telemetry`, `sc_browser_automation` |
| Architecture notes (on screen in their video) | 31 crates. The main thread does GPU rendering only, dedicated threads do PTY I/O, a background executor does git and file work, and a tokio runtime does HTTP polling. No LLM proxying: agents are black-box subprocesses. |

### Their remote mode (what we deliberately do differently)
- It shells out to system `ssh` with `ControlMaster`/`ControlPersist`/`BatchMode=yes`, plus `StreamLocalBindUnlink` (a reverse Unix-socket forward for hooks).
- A managed `sc` helper runs on the host.
- UI state and session metadata stay on the Mac; chat runs via `claude -p` over non-interactive SSH.

Why Flotilla differs:
- Windows OpenSSH does not support `ControlMaster`, so that design would be slow on Windows.
- Their design treats the host as a satellite. Flotilla inverts it: the VPS owns everything.

### Feature inventory
The full inventory (workspaces, worktrees, chat, orchestration, review, Git actions, layouts, themes, inbox, palette, PiP, browser, Shared Context, scripts, `sc` CLI) is captured in §4–§9. Each feature maps to a Flotilla component and a milestone (§14).

---

## 2. System architecture

```
 Windows PC                                      VPS (Linux)
┌─────────────────────────┐                ┌──────────────────────────────────────┐
│ Flotilla.exe  (GPUI)    │  SSH (russh)   │ flotillad          (systemd --user)  │
│ · DirectX 11 renderer   │◄══════════════►│ · sessions, subscriptions, RPC       │
│ · state mirror + cache  │  QUIC (quinn)  │ · git / worktrees / forge / scripts  │
│ · terminal emulators    │◄──────────────►│ · chat adapters, orchestration bus   │
│ · WebView2 (lazy)       │  one or both   │ · SQLite state (WAL) + blobs         │
│ · Credential Manager    │                │ · Unix socket API ◄── fl CLI, hooks  │
└─────────────────────────┘                └───────────────┬──────────────────────┘
                                                           │ frozen local protocol
                                           ┌───────────────▼──────────────────────┐
                                           │ fl-prochost (long-lived)             │
                                           │ · owns every PTY / agent child       │
                                           │ · byte rings, survives daemon restart│
                                           └──────────────────────────────────────┘
```

### 2.1 Processes
- **`flotillad`** is one static (musl) multi-call Linux binary for x86_64 and aarch64. Its modes are selected by `argv[0]` or a subcommand:
  - `flotillad serve` (the daemon)
  - `flotillad attach` (stdio bridge to the daemon socket, the SSH fallback)
  - `fl …` (the CLI, via an `fl` symlink)
  - `flotillad prochost`
- **`fl-prochost`** is a deliberately small process holding every child process (PTY-backed terminals and pipe-backed chat adapters) and a ring buffer of each child's output.
  - Its local protocol to the daemon is frozen and versioned separately.
  - Daemon upgrades and crashes never kill agents: the new daemon reconnects, replays unacknowledged output, and rebuilds emulator state.
  - The prochost itself is only upgraded when no children are alive, or when the user confirms.
- **`Flotilla.exe`** is the Windows x64 GPUI app. It holds no source of truth: it mirrors daemon state, caches it for instant start, and renders.

### 2.2 Cargo workspace layout
```
crates/
  fl-proto/        wire types, versioning, framing codec (postcard + zstd)
  fl-model/        domain types and IDs shared by daemon and client
  fl-transport/    multiplexed logical channels over SSH (russh) or QUIC (quinn); resume
  fl-term/         alacritty_terminal wrapper, snapshot encode/decode, byte-offset log
  fl-prochost-proto/  frozen daemon↔prochost protocol
  daemon/
    fld-store/     SQLite schema + migrations (rusqlite, WAL, FTS5), blob store
    fld-prochost/  prochost binary logic (PTY via portable-pty, pipes, rings)
    fld-git/       git CLI wrapper, status/diff, worktrees, inotify watcher, imara-diff
    fld-forge/     Forge trait; GitHub via `gh api graphql`; polling scheduler
    fld-agents/    provider registry, launch specs, hooks, chat adapters
    fld-chat/      normalized chat event model, transcript store, search
    fld-orch/      labels/groups, messaging, team runs, coordination state, layout control
    fld-scripts/   .flotilla/config.json, setup/run/teardown, .worktreeinclude
    fld-server/    client sessions, auth, subscriptions, RPC routing, QUIC listener, Unix socket
    flotillad/     multi-call binary entry (serve | attach | prochost | fl)
  client/
    flc-sync/      state mirror, optimistic ops, subscription manager, disk cache
    flc-ui/        design system: tokens, theme engine, primitives, icons
    flc-shell/     window, chrome, sidebar, right panel, bottom bar, layout tree (views/tabs/panes)
    flc-terminal/  terminal view (GPUI renderer), predictive echo, selection, links
    flc-chat/      chat view + composer
    flc-code/      file viewer/editor + diff view (tree-sitter highlighting)
    flc-overlays/  command palette, inbox, settings, theme editor, toasts, banners, PiP
    flc-browser/   WebView2 host (Windows only), automation bridge
    flotilla/      Windows binary (Win32 integration: notifications, power events, jump list)
```
GPUI is pinned to an exact version (crates.io `gpui 0.2.x`, or a Zed git revision if Windows fixes require it).

### 2.3 Threading
- **Client:**
  - The GPUI main thread does UI only.
  - The transport runs on a dedicated tokio runtime (2 worker threads).
  - Terminal byte parsing runs on the GPUI background executor. Each terminal has a `Term` behind a mutex, and the main thread takes render snapshots.
  - Syntax highlighting and diffing run on the background executor.
- **Daemon:**
  - tokio multi-thread runtime.
  - Git work runs in `spawn_blocking`, capped at 2 concurrent operations per repo.
  - SQLite has one writer task with batched transactions and a pool of readers.
  - The prochost link is a single Unix socket with multiplexed child streams.

---

## 3. Connection layer (approved in conversation)

### 3.1 First connect and daemon install
1. **Host setup.** The user adds a host: alias, `user@host[:port]`, and a key source.
2. **Key source.** Tried in this order:
   - the Windows OpenSSH agent (`\\.\pipe\openssh-ssh-agent`)
   - Pageant
   - a key file in `%USERPROFILE%\.ssh`, with the passphrase asked once and kept in memory
   - a Flotilla-generated ed25519 key, protected with DPAPI. "Install key" needs one password login, so it can't be used on hosts with password auth disabled, such as this VPS. There, the user selects an existing key.
3. **Host key.** Trust on first use (TOFU), stored per host. A mismatch is a hard stop with a clear explanation and no override.
4. **Version probe.** Exec `~/.flotilla/bin/flotillad version --json`, which returns `{version, proto: {min, max}}`.
5. **Install or upgrade when needed.** The Windows installer **bundles** the matching `flotillad` builds for x86_64 and aarch64, about 8 MB each compressed.
   - If the host is missing the daemon, or has an older version, the client first verifies the bundled binary's minisign signature locally. It then uploads the binary over an SSH exec channel to `~/.flotilla/bin/flotillad.new`.
   - It checks the upload's SHA-256 on the host, swaps the binary atomically, and installs or refreshes the `systemd --user` unit, with `loginctl enable-linger` when needed.
   - Without systemd, it falls back to `setsid` plus a pidfile.
6. **Device enrollment** (over SSH exec). The daemon returns:
   - the QUIC endpoint (UDP port, default **7443**)
   - the SHA-256 fingerprint of its self-signed certificate
   - a fresh 256-bit **device token**

   The token is stored in Windows Credential Manager. The daemon stores `sha256(token)` and device metadata, and devices can be revoked in Settings → Devices.

### 3.2 Every connect after that
- The client races **QUIC** (ALPN `flotilla/1`, certificate pinned) against **SSH**:
  - SSH opens a `direct-streamlocal@openssh.com` channel to `~/.flotilla/run/daemon.sock`.
  - If stream-local forwarding is disabled, it falls back to exec `flotillad attach`.
- Whichever completes `Hello` first carries traffic. QUIC is preferred whenever it is healthy.
- The SSH connection stays up as an idle standby, so failing over costs about one heartbeat.
- QUIC is re-probed every 30 s while on SSH, and traffic migrates back automatically.
- `Hello{device_id, token, client_version, proto, resume: {client_session_id, collections: {id → rev}, terminals: {id → byte_offset}}}`.
  - Over SSH, the SSH authentication is the credential, and the token is still sent for device identity.
  - No 0-RTT.
  - Unauthenticated QUIC connections are closed after 3 s.

### 3.3 Logical channels
| Channel | Carries | Priority |
|---|---|---|
| `ctl` | RPC, state deltas, events, heartbeats | highest |
| `term.hot` | input and output for terminals visible on this client | high |
| `term.bg` | resyncs for terminals becoming visible | normal |
| `bulk` | file contents, diffs, snapshots, attachments, updates | low |

- On QUIC, each channel is its own stream, and big transfers use their own unidirectional streams, so nothing waits behind anything else.
- On SSH, each channel is its own SSH channel. TCP head-of-line blocking still applies; this is the accepted fallback.
- Frame format: `u32 len | u8 flags | u16 kind | postcard payload`. Payloads over 1 KiB are zstd-compressed (level 3), signalled by a flag bit.
- The protocol version is negotiated in `Hello`. The daemon supports the current minor version and the previous one, and the client auto-upgrades the daemon on a major mismatch.

### 3.4 State synchronization
- **Collections.** Daemon state is partitioned into collections, each with a monotonically increasing `rev`:
  - workspace tree
  - per-worktree git status
  - per-worktree layout
  - inbox
  - agents
  - settings
  - per-session chat transcript
- **Subscriptions.** The client subscribes with `since_rev`. The daemon replies with `Delta{rev, ops[]}`, or with `Snapshot{rev, data}` when `since_rev` has fallen outside the bounded op log (10k ops or 10 min).
- **Mutations.** Each mutation is `Op{client_op_id, base_rev, op}`.
  - The client applies it **optimistically**.
  - The daemon acknowledges it with a rev, or rejects it with a reason. On rejection the client rolls back and shows a toast.
  - Layout and settings fields are last-writer-wins per field. Tree operations (split, move, close) are validated against the current tree.
- **Disk cache.** The client keeps the last mirror on disk at `%LOCALAPPDATA%\Flotilla\cache\<host-id>.bin` (postcard + zstd, written with a debounce of 1 s). On launch the UI renders from the cache immediately, marked stale with dimmed live indicators until resync completes.
- **Visibility-driven streaming.** The daemon streams terminal output and transcript deltas only for surfaces visible on that client (a visible tab, a PiP window, or an inbox peek). Hidden terminals are "parked": the daemon keeps emulating them but stops sending. Showing one sends a byte delta or a snapshot.

### 3.5 Terminal synchronization (mosh-style hybrid)
- **Per terminal, the daemon keeps:**
  - a PTY, held by the prochost
  - an `alacritty_terminal::Term`, with 10k lines of scrollback by default
  - a byte log addressed by absolute offset, holding 2 MiB
- **Attach.** The client sends `TermAttach{id, have_offset?}`.
  - If the offset is still in the log, the daemon sends the bytes from there.
  - Otherwise it sends `TermSnapshot{offset, screen, cursor, modes, title, cwd, scrollback_tail(1000 lines)}` followed by live bytes from `offset`.
  - Older scrollback is fetched in pages on demand.
- **Same emulator on both sides.** The client feeds the bytes into its own `Term`, built from the same `fl-term` code and the same alacritty revision, so both sides converge on the same state.
- **Collapse.** If a client falls more than 512 KiB behind (for example, someone ran `cat huge.log`), the daemon drops the backlog for that client and sends a fresh snapshot. A slow link therefore can't build an unbounded queue.
- **Input.** `TermInput{id, bytes}` goes on `term.hot`. `TermResize{id, cols, rows}` is applied only from the client that most recently typed into that terminal, the way tmux does.
- **Predictive echo** (setting: Off / **Auto** / On). Auto enables it when the smoothed RTT exceeds 150 ms.
  - Like mosh: printable keystrokes at a detected shell prompt (not alt-screen, echo confirmed) are drawn immediately, underlined, and reconciled when output arrives.
  - A wrong prediction rolls back silently.

### 3.6 Liveness, failover, disconnects
- A heartbeat runs every 2 s and measures RTT, which shows in the status-bar tooltip and the performance HUD.
- After 6 s without a reply, the state becomes **Reconnecting**:
  - a small yellow dot appears in the chrome
  - each terminal pane shows a subtle "offline" scrim
  - everything else stays interactive from the mirror
- Retries back off with jitter from 250 ms up to 5 s.
- On resume from Windows sleep (`WM_POWERBROADCAST`) or a network change, the client reconnects immediately.
- **Input while offline:**
  - Composer text, layout changes and other UI operations are kept locally and sent on reconnect.
  - Terminal keystrokes are held as greyed "pending" input. They are sent only if the outage lasted less than 5 s; otherwise they are discarded with a notice, so a stale command never runs.
- **Resume is exact:** collections resume from their revs, and terminals resume from byte offsets. Data is never duplicated or lost; this is covered by the network chaos tests in §13.

### 3.7 Security
- **SSH:** standard host-key verification and key authentication.
- **QUIC:** TLS 1.3 with the certificate pinned by fingerprint delivered over SSH, plus a revocable device token.
- **Daemon socket:** `~/.flotilla/run/daemon.sock` has mode `0600`.
- **Managed child environment:** every managed child gets `FLOTILLA_SOCKET`, `FLOTILLA_SESSION_ID` and `FLOTILLA_WORKTREE` so that `fl` can identify the calling agent.
- **Credentials:** Flotilla never stores provider credentials. Agents use their own CLI authentication on the VPS. Client secrets live in Windows Credential Manager.
- **Updates:** all release artifacts are minisign-signed in CI. The public key is compiled into both binaries.

---

## 4. Domain model and persistence

| Entity | Key fields |
|---|---|
| Workspace | id, name, icon, kind (`individual` \| `shared_context`), order, theme (per mode), defaults, routing overrides |
| Project | id, workspace_id, path, kind (`git` \| `folder`), default branch, default target, worktree root, scripts, provider routing |
| Worktree | id, project_id, path, branch, base, target, upstream, label (manual/auto), is_primary, section_id, temp_name flag, created/last-active |
| Section | id, workspace_id, kind (`manual` \| `filter`), rules (PR state, review decision, checks, agent state, run state, branch glob), priority; built-in Pinned |
| Layout | per worktree: view split tree → views (ordered tabs, active) → each tab a pane split tree → surfaces; ratios |
| Surface / Tab | id, kind (`terminal` \| `chat` \| `file` \| `diff` \| `browser` \| `review` \| `launcher`), title (auto/manual), status, pinned |
| Session | id, worktree_id, provider, surface, resume_target, profile, model, thinking, label, groups, status (`idle` \| `working` \| `waiting_input` \| `waiting_approval` \| `done` \| `error`), status_source (`structured` \| `hook` \| `heuristic`) |
| Terminal | id, session_id?, child_id (prochost), argv, cwd, env, size, title, cwd (OSC 7) |
| ChatEvent | session_id, seq, normalized event (§5.3) |
| InboxItem | id, kind (approval, question, completed, error), source (workspace/worktree/tab), state (unread, read, dismissed) |
| ReviewThread | id, worktree_id, anchor (file + line range, or file), status (`open` \| `pending` \| `fix` \| `resolved`), comments, origin (human, AI finding) |
| Forge cache | per worktree: PR number/url/state/draft, checks, reviews, unresolved threads, mergeability, allowed strategies, fetched_at |
| Profile | id, provider (`claude` \| `codex`), config dir (`CLAUDE_CONFIG_DIR` / `CODEX_HOME`), executable, flags |
| Route | scope (global/workspace/project), group (`fast` \| `thorough`) or action, provider/profile/model/thinking, click/⌘-click dispatch |
| CustomCommand / PromptTemplate | scoped (global/workspace/project/shared-in-repo), fields as their docs |
| TeamRun | id, lead session, roles[], status, reports |
| CoordEntry | scope, key, JSON value, version, updated_by, updated_at; advisory locks with a TTL |
| SavedLayout | name, scope (user/worktree), arrangement |
| Device | id, name, token_hash, last_seen, revoked |
| Settings | cascading global → workspace → project |

- **Storage:**
  - `~/.flotilla/state.db` (SQLite, WAL, FTS5 over chat transcripts)
  - `~/.flotilla/blobs/<sha256>` for attachments and large artifacts
  - Migrations are transactional, with an automatic `state.db.bak-<ver>` copy before every schema upgrade.
- **Repository config:** `.flotilla/config.json`, the same schema as theirs (`setup`, `run` as an array or named targets, `teardown`, `commands`), plus `.worktreeinclude`.
- **Script environment variables:** `FLOTILLA_ROOT_PATH`, `FLOTILLA_WORKTREE_PATH`, `FLOTILLA_WORKTREE_NAME`.

---

## 5. Terminals and agents

### 5.1 Terminals
- **Process:** `$SHELL -l` in the worktree directory, with `TERM=xterm-256color`, `COLORTERM=truecolor`, the `FLOTILLA_*` variables, and the login-shell `PATH` (resolved once per daemon start).
- **Supported features:**
  - true color and mouse reporting
  - bracketed paste
  - OSC 8 hyperlinks, OSC 7 cwd, OSC 133 prompt marks (for command boundaries and predictive echo)
  - OSC 52 clipboard, after asking once per host
  - the kitty keyboard protocol
  - window title
  - bell, which feeds attention status
- **Interaction:** selection, copy, scrollback search, and URL/path detection with actions: open the file in Flotilla, copy the path, open in the browser tab, or open in the OS browser.
- **Rendering:** GPUI with batched cell runs and background quads, and only visible rows are laid out (Zed's `terminal_view` is the reference). The default font is Cascadia Mono, which ships with Windows 11, and it is configurable.

### 5.2 Providers
The provider registry is data-driven. It has built-in entries plus user overrides for command, flags, environment, chat adapter kind, resume strategy and hook support.

| Provider | Command | Terminal | Chat adapter (transport) |
|---|---|---|---|
| Claude Code | `claude` | ✓ | **claude-stream**: `claude -p --input-format stream-json --output-format stream-json --include-partial-messages --verbose --session-id/--resume --permission-prompt-tool mcp__flotilla__approve --mcp-config <flotilla>`; mid-turn steering via stdin |
| Codex | `codex` | ✓ | **codex-app-server**: `codex app-server` JSON-RPC (threads, turns, approvals, diffs) |
| Cursor | `cursor-agent` | ✓ | **stream-json**: `cursor-agent -p --output-format stream-json --stream-partial-output --resume` |
| Pi / Oh My Pi | `pi` / `omp` | ✓ | **pi-rpc**: `--mode rpc` |
| OpenCode, Hermes, Gemini, Grok, Kimi, Qwen and others | various | ✓ | **acp**: the generic Agent Client Protocol adapter, used when the CLI exposes ACP (`opencode acp`, `hermes acp`, …) |
| Copilot, Droid, Kiro, Aider, Antigravity, custom | various | ✓ | terminal only |

- Adapter support is detected and contract-tested against the CLI versions actually installed (§13).
- An adapter that fails detection degrades to terminal-only with an explanation; it is never broken silently.

### 5.3 Normalized chat events
All adapters normalize to these events:
- `UserMessage`
- `AssistantText{delta|final}` (markdown)
- `Thinking{delta}` (collapsed by default)
- `ToolCall{id, kind: read|edit|exec|search|web|mcp|other, title, input_summary, status, output(truncated), diff?}`
- `FileEdit{path, unified_diff}`
- `ApprovalRequest{id, tool, details, options}`
- `QuestionRequest{id, questions[{text, options[{label, description}], multi, free_text}]}`
- `Subagent{parent_tool_id, events}`
- `TurnEnd{duration, turns, usage{tokens, cache, cost}, model}`
- `Error`

Events are stored append-only and are full-text searchable.

### 5.4 Chat view and composer (matching their Chat UI)
- **Message rendering:**
  - Markdown with code blocks, plans and links.
  - Tool activity rows that can be expanded.
  - Inline diffs.
  - "Input needed" question cards with option rows and free text.
  - Approval cards.
  - Subagent activity grouped under its parent.
  - A message-details popover showing model, thinking, tokens and cost.
  - Per-turn metadata such as "1:04 PM · 14s · 3 turns".
- **Composer:**
  - `@` file mentions (fuzzy index served by the daemon)
  - attachments (uploaded to blobs; the provider receives paths)
  - large pastes staged as context
  - model, profile and thinking pickers
  - permission mode
  - **Enhance prompt** (a one-shot call on the Fast route)
  - send, **steer** (mid-turn, where the provider supports it), stop
- **Switching surfaces:** a chat can switch to a terminal, or back, in place when the session is idle and a validated resume target exists (Claude, Codex, Cursor, OpenCode, Pi, Grok).
- **Profiles:** switching profiles stages a new continuation and leaves the original chat on its account.

### 5.5 Status, hooks and instruction injection
- **Status sources, in order of trust:**
  1. structured adapter events
  2. provider hooks (Claude Code hooks, Codex notify, …), which call `fl hook <event>`
  3. process state plus OSC 133 plus bell

  Heuristic status is labelled as such and is **never** used to approve anything or to declare a task complete.
- **Orchestration contract:** when orchestration is enabled, a short trusted contract is injected into each session (Claude `--append-system-prompt`, Codex developer instructions, or an equivalent per adapter). It points the agent at `fl instructions <topic>`, which Flotilla authors itself.

### 5.6 Restore
- **Warm restore** is the normal case. Agents live in the prochost, so reopening the client simply reattaches.
- **Cold restore** applies after a VPS reboot. Each provider is relaunched with its stored resume target if that target validates; otherwise a fresh session starts and the old transcript is kept as an archived, read-only, searchable record.
- **History:** a per-worktree history picker, plus global conversation search in the command palette.

---

## 6. Git, worktrees, forge and review

- **Projects:**
  - Added by browsing the VPS filesystem (a remote file picker) or by cloning a URL on the VPS.
  - Non-git folders are supported, without branch workflows.
  - Existing `git worktree` checkouts are discovered automatically.
- **Worktrees:**
  - Created at `~/.flotilla/worktrees/<project>/<slug>`; the location is configurable per project.
  - A worktree starts with a temporary name. The first meaningful prompt gives it a durable branch and label through the Fast route, using a one-shot structured call.
  - Creation steps, in order: copy files matched by `.worktreeinclude`, optionally symlink `.env`, run setup scripts (foreground or background, automatic or manual), and initialize submodules.
  - Cleanup steps, in order: stop the worktree's run processes, run `pre_cleanup` (user-owned; a nonzero exit aborts), run `teardown`, run `git worktree remove`, clean up the branch per settings, then run `post_cleanup`. Dirty worktrees and unpushed commits require `--force`.
- **Git status:**
  - An inotify watcher (the `notify` crate, skipping ignored heavy directories) covers the worktree and its `.git` directory.
  - Changes are debounced by 150 ms, then the daemon runs `git status --porcelain=v2 -z --branch` and `git diff --numstat` against the merge-base with the target. That produces the sidebar's +/− counts and the right panel's file list.
  - A 10 s poll is the fallback.
- **Diff view:**
  - The daemon sends the file list and, per opened file, the old and new contents (hunks only for very large files).
  - The client diffs them (`imara-diff`), highlights them (tree-sitter, 20+ grammars), and renders unified or side-by-side.
  - Line-range selection creates a review comment.
- **Files:**
  - A lazy file tree with icons.
  - A file viewer/editor with highlighting, find, and edit-and-save. It is not a full IDE.
  - "Open in…" deep links to VS Code Remote-SSH and Zed remote for the same path.
- **Adaptive Git action button**, following their documented state priority:
  1. temporary name
  2. dirty base branch → Commit & push / Commit / Push
  3. task branch with no review → Create PR (or draft, or manual)
  4. open review with local work → Commit & push / Push
  5. Resolve (conflicts), Fix CI, Fix changes, Fix comments, Checks, Update, Investigate
  6. Ready, Awaiting review, Merge (squash / merge commit / rebase, as the repo allows)
  7. Delete worktree

  It shows transient states such as Checking… and Creating… while it refreshes stale forge context before a mutation.
  - **Direct operations** (no agent): push, update branch, mark ready, merge, delete worktree, refresh checks.
  - **Agent operations:** commit, commit & push, create PR, resolve, fix CI, investigate, fix comments, fix changes. These are routed through Fast or Thorough routes using per-scope prompt templates.
  - **Dispatch modes:** reuse (prepare or submit), new tab (prepare or submit), background, with separate defaults for click and Ctrl-click.
- **Forge (GitHub):**
  - Uses `gh api graphql` on the VPS, reusing its authentication. It reads the branch's PR, `statusCheckRollup`, reviews, unresolved threads, mergeability and allowed merge methods.
  - Polls every 15 s for the active worktree and every 60 s for the others, with backoff on rate limits.
  - GitLab (`glab`) and Bitbucket (`bkt`) come later, behind the same `Forge` trait.
- **Review:**
  - App-managed review threads with line or file anchors.
  - AI findings come from a review agent through `fl worktree review-add`, and are triaged as Should fix or Dismiss.
  - Pending threads are bundled and sent to the current chat, a new chat, or the background.
  - The Checks tab lists forge checks with links.
- **Sections:** manual sections, plus filter sections (PR state, review decision, checks, agent state, run state, branch glob) with priority. Manual assignment beats filters. Pinned is always first.
- **Shared Context:**
  - A branch group root holds `FEATURE.md`, `AGENTS.md`/`CLAUDE.md` and `context/`.
  - Child worktrees are created lazily (`fl worktree ensure-child <repo>`).
  - Grouped diffs show a repo summary that expands per child.
  - Repository-role guidance is generated by an analysis agent (optional) or from a template.
- **Scripts:**
  - Named run targets with `default` and `stop_commands`.
  - The right panel has Setup / Run / Terminal tabs with output and state.
  - Ctrl+Shift+R runs the active target.

---

## 7. Orchestration and the `fl` CLI

- `fl` talks to the daemon over the Unix socket. The caller is resolved from `FLOTILLA_SESSION_ID` and its working directory.
- `fl help <cmd>` is authoritative, and every command supports `--json` (orchestration commands use `--output json`).
- **Command surface.** Flotilla's own implementation, mirroring their documented capabilities:
  - `status`, `instructions [orchestration|layout|worktree|review|browser]`
  - `chat providers|list|new|send|events|cancel|stop|close`, `history list|get`
  - `workspace list|open|select|create|get|update|add|theme|move|delete|watch`, `section create|edit|move|delete|assign|unassign|list`
  - `worktree status|checks|diff-summary|review-checklist|select|close|delete|rename-branch|set-label|set-target-branch|open [--pr N]|create|ensure-child|review-list|review-get|review-add|review-reply|review-set-status`
  - `project scripts [check|run|status|stop|set|unset]`, `commands list|create|get|update|delete`
  - `tab title|split|split-view`, `layout capabilities|views|set|insert|close|move|save|apply|list|compose [--dry-run]|run`
  - `agent send [--queue|--interrupt] [--wait-until-idle]|wait|read|interrupt|stop [--kill]|subscribe|should-stop`, `agents list|label set|group create`
  - `team run (1–8 roles)|report (≤16 KiB summary + result file)|status|list|cancel`
  - `coordination-state get|set|delete|watch [--if-version N]|lock|unlock` (versioned compare-and-set, advisory locks with a TTL)
  - `browser open|snapshot|click|fill|…` (§8.6)
  - `hook <event>` (provider hook ingestion)
- **Targets:** `label:<name>`, `id:<stable-id>` and `group:<name>`. Layout indexes are never used as identities.
- **Message delivery:**
  - Chat adapters queue the message for the next turn, or steer mid-turn where supported.
  - Terminal-only agents receive the message via bracketed paste plus Enter, only when their status is idle (or with `--force`). This delivery is marked **unverified**.
- **Team runs:**
  - All roles launch in parallel tabs, each receiving role context: label, run id, report contract.
  - When the last role finishes, the lead gets one short fan-in message.
  - Because processes live in the prochost, team runs **survive daemon restarts**. This is an improvement over theirs, which marks runs Interrupted.
- **Instruction guides** are written by us and embedded in the binary. Their text is Flotilla-specific and not copied.

---

## 8. User interface

### 8.1 Window and chrome
- **Window:** a GPUI client-decorated window with Windows caption controls (minimize / maximize / close) on the right. Snap Layouts work through correct `WM_NCHITTEST` handling of the maximize button. An optional Mica/Acrylic backdrop is tied to the theme's Alpha setting (Windows 11).
- **Three regions, matching their app:**
  - **Left sidebar:**
    - Workspace header (label in small caps).
    - Project rows with an avatar letter, a menu chevron and **+**.
    - Worktree rows: branch glyph, name (accent-colored when active elsewhere), +adds / −dels in green and red, relative time, status dot, ★ for the primary worktree.
    - A selected-row card with a hairline border.
    - Footer: settings, a folder icon, and workspace pager dots with +.
    - Compact and Detailed styles.
  - **Center:**
    - A tab bar with provider glyphs and an accent underline on the active tab, plus a history clock.
    - Content: terminal, chat, file, diff, browser or review.
    - A bottom bar with the branch name and the **adaptive Git action** button (pill with a chevron).
  - **Right panel:**
    - A segmented control: **Files / Changes N / Checks** (and **Review**).
    - Below it, the Setup / Run / Terminal tabs with **+**, and empty states ("No setup script output", plus a Run setup button).
  - **Title row:** sidebar toggle, back/forward, open-in, run, and the right-panel toggle.
- **Overlays:**
  - Command palette (modes: Global, Commands, Files, Worktrees, Tabs, Conversations)
  - Inbox (Running / Ready / Open, with peek)
  - Settings
  - Theme editor
  - Hover cards for worktrees
  - Bottom-right toasts and a top banner
  - Context menus
  - Drag-and-drop of tabs and panes with a drop-ring preview
  - An empty-view launcher with tool cards and Recent
- **PiP:** a separate always-on-top GPUI window showing one terminal, chat, run or review surface (Ctrl+Shift+P), which can be pinned. There is also a separate "pin app window" (always on top).

### 8.2 Design tokens (dark; sampled from their recording, then tuned against frames during implementation)
| Token | Value |
|---|---|
| `chrome.bg` (sidebar, titlebar, right panel) | `#27262B` |
| `surface.bg` (center content) | `#1D1E21` |
| `surface.raised` (cards, composer) | `#25262B` |
| `surface.raised.hover` / option rows | `#2A2B30` |
| `row.selected` | `#2D2E33` + 1 px `rgba(255,255,255,0.08)` border |
| `control.bg` (pills, e.g. Create PR) | `#35363B` |
| `text.primary` | `#DFE0E5` |
| `text.secondary` | `#838285` |
| `text.tertiary` (timestamps) | `#6C6D72` |
| `accent` (links, active tab underline, active branch) | ≈ `#3D8BFD` (to be refined) |
| `diff.add` / `diff.del` | muted green ≈ `#5DBB63` / muted red ≈ `#E5534B` (to be refined) |
| Radii | 10 px panels, 8 px cards, 6 px controls, pill buttons fully rounded |
| Type | UI: Inter (bundled, OFL) 13 px / 11 px secondary; Mono: JetBrains Mono (bundled, OFL) 12.5 px; terminal default Cascadia Mono |

- A light theme is derived the same way from `app-flex-light.mp4`.
- **Theme engine:**
  - Modes: System, Light, Dark.
  - Per-workspace Background, Accent and Highlight colors per mode.
  - Sliders: Intensity and Alpha for Background, Prominence for Accent and Highlight.
  - A color canvas, swatches (including the other workspaces' colors), "Copy from Light/Dark/Background/Accent", Save, Reset, and Set as default.
  - Terminal color palettes are independent of the theme.
  - Font and size are set per surface: chat scale, editor/diff, terminal tab, right-panel terminal.
  - Every change applies live.

### 8.3 Keyboard
Two rules decide the defaults:
- **The terminal comes first.** While a terminal has focus, plain `Ctrl+<letter>` always goes to the terminal.
- **App actions** use `Ctrl+Shift+<key>`, `Alt+<key>`, or `Ctrl+<key>` only where it doesn't collide with a terminal key.

| Action | Their macOS | Flotilla default |
|---|---|---|
| New worktree | ⌘N | Ctrl+Shift+N |
| New terminal / tool picker | ⌘T / ⌘⇧T | Ctrl+Shift+T / Ctrl+Shift+Alt+T |
| Split right / down | ⌘D / ⌘⇧D | Alt+Shift+= / Alt+Shift+- |
| View split right / down | ⌘⌥D / ⌘⌥⇧D | Ctrl+Alt+Shift+= / Ctrl+Alt+Shift+- |
| Toggle right panel / left / both | ⌘E / ⌘⇧E / ⌘⌃E | Ctrl+Shift+E / Ctrl+Shift+B / Ctrl+Alt+B |
| Run the Git action / alternate | ⌘G / ⌘⇧G | Ctrl+Shift+G / Ctrl+Alt+G |
| Inbox | ⌘I | Ctrl+Shift+I |
| Palette (Global, Commands, Files, Worktrees, Tabs, Conversations) | ⌃G ⌃C ⌃F ⌃W ⌃T ⌃S | Ctrl+Shift+K opens Global; inside the palette, Ctrl+1…6 switch modes; Ctrl+Shift+O = Files directly |
| Find in terminal scrollback / file | — | Ctrl+Shift+F |
| PiP | ⌘⇧P | Ctrl+Shift+P |
| Run target | ⌘R | Ctrl+Shift+R |
| Next / previous tab | ⌃⇥ | Ctrl+Tab / Ctrl+Shift+Tab |

All bindings can be changed in `keymap.json` and in the Settings → Keyboard editor.

### 8.4 Notifications
- In-app inbox, toasts and banner.
- **Windows toast notifications** through WinRT `ToastNotificationManager`. The installer registers the AppUserModelID and the Start-menu shortcut.
- Delivery options: Always, Only when not focused, Never, configured separately for completion events and for approval/input events, with sounds.
- Clicking a toast opens the source tab.

### 8.5 Performance techniques
- Retained GPUI entities, and virtualized lists (`uniform_list` / `list`) for the sidebar, file tree, transcript and inbox.
- Terminal renders visible rows only, through a glyph atlas.
- No per-frame allocation in hot paths.
- `mimalloc`.
- Release profile: LTO thin, `codegen-units = 1`, `opt-level = 3`.
- The UI renders from the disk cache before the network connects.
- A built-in **performance HUD** (Ctrl+Alt+Shift+P) shows FPS, frame time, RTT, transport (QUIC/SSH), throughput and memory.

### 8.6 Browser tabs (optional, lazy)
- **WebView2** (the system Edge runtime, preinstalled on Windows 10/11). Not Electron, and not bundled. It is hosted as a child HWND sized to its pane and created only when a browser tab opens. This is the counterpart of their macOS WebKit tabs.
- **Previews:** the daemon detects ports that processes in the worktree are listening on (`/proc/net/tcp` plus the process tree) and offers **Open preview**. The client forwards `127.0.0.1:<local>` to `VPS 127.0.0.1:<port>` over the active transport (a QUIC stream or SSH `direct-tcpip`).
- **HTML previews** of worktree files are served through a daemon static handler over the same forward.
- **Annotate an element** sends a batch of selected DOM elements with comments to a chat.
- **`fl browser …` automation** runs in the client through WebView2's DevTools Protocol bridge. It requires a connected client that has the tab open, and is gated by Settings → Experimental plus a one-time grant per workspace. Page content is treated as untrusted.

---

## 9. Build, release and update

- **Toolchain:** pinned in `rust-toolchain.toml`. Workspace-wide clippy and rustfmt.
- **GitHub Actions:**
  - `daemon` (ubuntu-latest): builds `flotillad` for `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` (via `cargo-zigbuild`) and runs the daemon tests and the network chaos tests.
  - `client` (windows-latest): builds `Flotilla.exe` (`x86_64-pc-windows-msvc`), embeds the signed daemon artifacts, and packages a **per-user Inno Setup installer** (no admin; installs to `%LOCALAPPDATA%\Programs\Flotilla`) plus a portable zip. Runs the client smoke test (§13).
  - `visual` (ubuntu-latest, Xvfb + Mesa lavapipe): builds the GPUI client for Linux purely for automated screenshot tests against reference frames.
  - `release`: on a `v*` tag, a release. On `main`, a `nightly` prerelease. All artifacts are minisign-signed with a key held in repo secrets.
- **Updates (no GitHub credentials needed on Windows):**
  1. The daemon checks GitHub Releases through `gh` on the VPS (every 6 h, or on demand), verifies signatures, stages the new daemon, and caches the new client installer.
  2. The daemon restarts itself via systemd. Agents survive because they live in the prochost.
  3. Connected clients receive "Update available". The client pulls the installer over `bulk`, verifies it, and applies it on the next restart.
- **No Authenticode certificate:** Windows SmartScreen will warn on first run. This is an accepted limitation until a code-signing certificate is bought.
- **Development loop:**
  - Code is written on this VPS.
  - Daemon crates build and test here; the release build is limited by `-j2` for memory.
  - Windows builds come from CI on every push.
  - The owner installs a nightly at each milestone and reports back.

---

## 10. Error handling

- **The UI never blocks on the network.** Every remote operation has a timeout and can be cancelled, and the UI degrades to cached, dimmed state.
- **Failures are visible and actionable:**
  - Toasts carry an action, such as Retry or Open log.
  - Agent errors become inbox items.
  - Daemon subsystem failures appear in a Diagnostics panel with logs (`~/.flotilla/logs`, rotated).
- **Daemon resilience:**
  - Tasks are supervised: a panic is caught, logged, and the subsystem restarts.
  - The prochost isolates children from daemon crashes.
  - A protocol decode error drops the connection and triggers a resync; it never crashes the process.
- **SQLite:** migrations are backed up first and rolled back on failure.
- **Adapters:** if a chat adapter fails to start, the user is offered to open the same provider in a terminal surface.
- **Version skew:** the client upgrades the daemon automatically. If the daemon is newer, the client update flow is offered.
- **Destructive actions** (delete worktree, force-stop, `--force`) always confirm, and state what will be lost.

---

## 11. Non-goals (this spec)

- iPhone/mobile app
- Team or enterprise features
- Telemetry
- Local (Windows-side) workspaces
- macOS or Linux client builds for end users (the Linux build exists only for CI visual tests)
- Multi-user daemons
- GitLab and Bitbucket (designed for, implemented after M7 if wanted)
- Code signing

---

## 12. Risks

| Risk | Mitigation |
|---|---|
| GPUI API churn or Windows bugs | Pin the version; be ready to vendor and patch; isolate GPUI usage behind `flc-ui` primitives |
| No Windows machine on the dev side | CI builds and smoke tests, Linux visual tests, and owner testing of a nightly at each milestone |
| Agent CLI protocols change often | Versioned adapters, contract tests against the installed CLIs, terminal fallback |
| VPS memory pressure (about 4 GB free, swap in use) | Lean daemon budgets; parked terminals; limit build parallelism on the VPS |
| UDP blocked by a network or provider firewall | SSH fallback is automatic and continuous |
| Very large scope (their product is 30+ crates) | Ordered milestones, each runnable (§14); one plan per milestone |
| No code signing | SmartScreen warning documented; minisign protects update integrity |

---

## 13. Testing strategy

- **Unit:**
  - Protocol round-trips (proptest).
  - Terminal snapshot equivalence: random and recorded byte streams fed to two emulators, snapshot and restore, then compare grids.
  - Git action state-machine table tests.
  - Layout tree operations.
  - Settings cascade.
  - Coordination compare-and-set and locks.
  - Section filter rules.
- **Daemon integration:**
  - The daemon runs in a temporary `HOME` with git fixture repos.
  - Test clients connect over in-memory, QUIC and SSH (`russh` test server) transports.
  - Fake agent CLIs replay recorded `stream-json`, app-server and ACP sessions to exercise the adapters.
- **Network chaos:**
  - Transports run through an in-process impairment layer (latency up to 400 ms, jitter, 0–10 % loss, blackholes of 1–60 s, QUIC↔SSH failover).
  - Asserts: terminal byte streams arrive exactly once and in order, collections converge, and the UI never waits on the network.
- **Prochost:** kill and upgrade the daemon mid-stream, then verify every child and every byte survives.
- **Adapter contract tests:** opt-in and run on the VPS against the real installed CLIs with minimal prompts. They record fixtures for the fake CLIs.
- **UI:**
  - GPUI `TestAppContext` tests for view logic.
  - Visual snapshot tests on Linux CI compared with reference renders.
  - Windows release checks are separate: `--self-test=<report.json>` verifies bundled daemons, Credential Manager and runtime startup; `scripts/smoke_windows_ui.ps1` launches the actual GPUI window, captures workspace/onboarding screenshots and verifies that reopening onboarding preserves the draft. Transport loopback/failover coverage runs in the daemon/client integration suites.
  - A manual checklist per milestone.
- **Performance (tracked in CI):** terminal throughput (MB/s parse + sync), snapshot sizes, cold start, idle memory, frame time with 20 active terminals.

---

## 14. Milestones (build order; every milestone ends with a runnable build)

| # | Milestone | Exit criteria |
|---|---|---|
| M0 | **Foundations:** repo, CI (daemon, client, release), `fl-proto`, `fl-transport` (SSH + QUIC + failover), daemon skeleton, client window shell with design tokens, connect/install/enroll flow; prochost moves to M1 per the M0 plan | Owner installs Flotilla.exe, adds the VPS, and sees "Connected · QUIC · 42 ms" in Flotilla chrome; daemon installed by the client |
| M1 | **Terminals:** prochost PTYs, hybrid terminal sync, terminal view, tabs, panes, views, layout persistence, predictive echo | A full remote terminal app with splits that survives sleep, Wi-Fi changes, and daemon restarts |
| M2 | **Workspaces and git:** workspace/project/worktree sidebar, worktree create/delete, `.worktreeinclude`, scripts (setup/run/teardown), file tree, file viewer/editor, Changes + diff view, git watcher | Agents usable in terminal tabs, one worktree per task, with live +/− counts |
| M3 | **Forge and Git actions:** GitHub integration, adaptive Git button + dropdown, routes, prompt templates, dispatch modes, Checks tab, review threads, sections | Full commit → PR → checks → merge → delete loop from the UI |
| M4 | **Chat UI core:** adapter framework, Claude + Codex adapters, chat view, composer, approvals and questions, inbox, Windows toasts, history, search, restore | Claude and Codex usable in rendered chat with approvals and the inbox |
| M5 | **More adapters:** ACP generic (OpenCode, Hermes, …), Cursor, Pi/OMP, Grok; chat↔terminal switch; profiles | Every installed provider works in its best surface |
| M6 | **Orchestration and `fl`:** the full CLI surface, team runs, messaging, coordination state, layout control, instruction guides, injection | A lead agent runs a visible cross-provider team from a prompt |
| M7 | **Polish and extras:** command palette (all modes), theme editor, PiP, browser tabs + previews + automation, Shared Context, custom commands, auto-update, performance HUD, keymap editor | Feature parity with the documented super.engineering feature set (minus non-goals) |

Each milestone gets its own implementation plan (`docs/superpowers/plans/…`), written when the milestone starts. This spec governs all of them.
