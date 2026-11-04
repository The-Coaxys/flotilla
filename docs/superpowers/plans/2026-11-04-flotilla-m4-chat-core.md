# Flotilla M4 — Chat UI core

Governing specification: `docs/superpowers/specs/2026-11-02-flotilla-design.md` §§4–5, 8, 13–14.

Exit criterion: Claude and Codex usable in rendered chat with approvals and the inbox.
All specified M4 functionality remains required; this plan does not defer requirements by
calling a partial implementation complete.

## Implementation order and ownership

1. **Structured adapter core (`fld-agent`, `fl-model::chat`).** Discover installed provider
   capabilities; concrete Claude stream-json and Codex app-server transports; normalized
   streaming text/thinking/tool/file-edit/approval/question/subagent/turn/error events.
   Enforce explicit permission decisions, bounded frames/events, real process shutdown,
   supported steering/cancellation, and validated native-session resume targets. One-shot
   structured calls support the M3 Fast/Thorough routes without a second provider protocol.
2. **Authoritative daemon chat service and durable state.** Store sessions, provider config,
   append-only normalized events, attachments and resume targets. Sequence events durably
   before publication. Reattach clients from their last sequence; duplicate requests cannot
   run a turn twice. Persist pending approvals/questions; a reconnect must not accidentally
   approve, submit or replay a decision. Daemon restart must not manufacture successful turns.
3. **Bounded protocol and client mirrors.** Append chat RPCs and events with explicit version
   gating; bound text, attachment metadata, event pages and pending decisions. Follow session
   event sequences off the UI thread; reconnect fetches missing pages and resolves stale
   incarnations. Warm reconnect attaches the same live provider; cold restore validates its
   stored target and otherwise archives the old transcript read-only before creating a new
   session. Provider/profile/model selection must reflect actual discovered capabilities.
4. **Native chat surface and composer.** Render markdown/code/plans/links, collapsible thinking
   and tool activity, inline diffs, grouped subagents, per-message details and per-turn metadata.
   Composer supports file mentions, attachment upload, staged large-paste context, model,
   profile/thinking/permission settings, Enhance prompt, send/steer/stop. Approval and question
   cards send explicit user decisions; multi-select and free text preserve the question schema.
5. **Inbox, notifications, history and search.** Aggregate pending decisions and actionable
   errors without duplicating events on resume. Windows toasts navigate to the originating
   session; inbox remains available without platform notification permission. Per-worktree
   history and global transcript full-text search use durable daemon records, not UI memory.
6. **M3 dependency closure.** Wire routed Git actions and review bundles to real adapter sessions;
   implement Fast/Thorough scope resolution, structured naming and prompt enhancement without
   silently substituting manual entry or fabricated model output. Dispatch modes must expose
   prepare versus submit, reuse versus new tab, background, and separate Ctrl-click defaults.

## Adapter integration contract

`fld_agent::ProviderRegistry` discovers `ProviderSpec` capabilities. `Adapter::launch` accepts
`SessionConfig` (cwd, model, permission mode, instructions, provider configuration, optional
output schema and validated resume target). An adapter exposes an `EventRecord` receiver;
consumers drain it while issuing send/steer/cancel/approve/question/shutdown operations.
`EventRecord` identifies session, incarnation, sequence, timestamp, native session and turn.
Never infer completion from process liveness, a bell, text heuristics or nonempty output.

## Verification gates

- Contract fixtures originate from the installed CLI schemas/protocols; malformed messages,
  lost children, unsupported capability, denied approvals and interrupted turns have explicit
  consumer-visible outcomes. Fixtures do not claim a live provider run succeeded.
- Live Claude and Codex smoke: a prompt produces rendered output; a real guarded tool operation
  pauses for approval; denial prevents it; acceptance is explicit. Exercise cancellation,
  steering when supported, questions, reconnect, cold restore, history and search.
- Run the native client against the actual daemon and observe chat, expanded tools, inline
  edits, inbox navigation and decision state after reconnect. Verify Windows toast activation
  in the Windows job or report the platform prerequisite rather than claiming an X11 proof.
- Run the affected existing tests and warnings-denied clippy once after integration. Permanent
  regression tests cover durable transitions, sequencing, request deduplication and permission
  boundaries, not component wiring or incidental copy.
- Publish only after the full milestone acceptance flow and CI succeed. Record exact exercised
  scenarios and missing prerequisites here; do not label unexercised behavior verified.

## Execution evidence

### Slice `m4-chat` (daemon chat service, durable state and wire contract)

Implemented on branch `m4-chat` on top of `cf7d5d8`:

- `fl-model::chat` owns the wire records (`ChatSession`, `ChatEventPage`, `InboxItem`,
  `ChatHit`, constants) plus `search_text`, the bounded plain-text projection indexed for
  transcript search. `DaemonEvent` gained `ChatSessionChanged`, `ChatEventAppended` and
  `InboxChanged` (appended variants).
- `fl-proto` 0.4 appends every chat request/response. Chat requests, events, pages, hits and
  inbox items are bounded at both boundaries; empty messages/queries and nil identities are
  rejected. Chat events are only pushed to peers speaking protocol 0.4.
- Store V7 (`crates/daemon/fld-store/src/chat.rs`): `chat_sessions`, append-only
  `chat_events` (PK `(session, sequence)`, worktree FK cascade, per-session durable cursor),
  `chat_pending` (approvals/questions), `chat_inbox` (unique dedupe key) and an FTS5 index
  over `chat_events.search_text` with a bounded `LIKE` fallback recorded in `chat_meta`.
  Events commit with their status/pending/inbox effects before publication. Migration keeps
  the existing `state.db.before-v<old>-to-v<new>-<id>.db` backup convention; the v2/v3
  downgrade fixtures drop the new tables.
- `fld-server::chat` supervises one `fld_agent::Adapter` per live session (bounded to eight),
  assigns durable per-session sequences, drains events into the store, applies status only
  from structured adapter events, persists/clears pending decisions, records inbox cards,
  and publishes `ChatEventAppended`/`ChatSessionChanged`/`InboxChanged` after the commit.
  Daemon restart clears unanswerable pending decisions and normalizes working sessions to
  idle; a later send reattaches with the validated stored resume target, or archives the
  transcript read-only when the target is missing/rejected. Send/steer/start/cancel and
  one-shot calls have timeouts; shutdown closes every adapter.
- `workspace::routing::dispatch_git_task` now executes submit modes: structured one-shot
  calls for NameWorktree/Commit/CommitPush/CreatePr/EnhancePrompt (validated JSON before any
  Git/forge mutation), real sessions for Resolve/FixCi/FixChanges/FixComments/Investigate,
  FixComments uses the pending review bundle, and dispatch state is published truthfully.
  Routed one-shot calls never answer an approval/question on the user's behalf.
- `flc-sync::chat` mirrors sessions, ordered transcript pages and the inbox on watch
  channels, refetches from the last durable sequence on reconnect, and exposes a request
  helper for every chat RPC.

Commands exercised (all passing):

```
cargo check --workspace --exclude flc-ui --exclude flc-terminal --exclude flc-editor --exclude flotilla
cargo test -p fld-store --lib
cargo test -p fl-model -p fl-proto -p flc-sync --lib
cargo test -p fld-server
cargo test -p flc-sync
cargo clippy -p fl-model -p fl-proto -p fld-store -p fld-agent -p fld-server -p fld-git -p flc-sync --all-targets -- -D warnings
```

Not exercised, and therefore not claimed: a live Claude/Codex turn, a live approval or
question round-trip, native chat rendering, Windows toast activation, and a real routed
dispatch against an installed provider. The daemon chat service and routed dispatch have no
fake-CLI integration fixture yet; their provider interaction is compiled and unit-tested at
the store/protocol/mirror layer but not proven against a running model.
