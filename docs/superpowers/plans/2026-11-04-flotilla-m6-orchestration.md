# Flotilla M6 — Orchestration and the `fl` CLI

Governing specification: `docs/superpowers/specs/2026-11-02-flotilla-design.md` §7, §14.

Exit criterion: a lead agent runs a visible cross-provider team from a prompt.

## Scope

- **`fl` binary** talking to the daemon over the Unix socket. The caller resolves from
  `FLOTILLA_SESSION_ID` and its working directory. `fl help <cmd>` is authoritative; every
  command supports `--json` (orchestration commands `--output json`). Implement the full
  surface in §7:
  - `status`, `instructions [orchestration|layout|worktree|review|browser]`
  - `chat providers|list|new|send|events|cancel|stop|close`, `history list|get`
  - `workspace list|open|select|create|get|update|add|theme|move|delete|watch`,
    `section create|edit|move|delete|assign|unassign|list`
  - `worktree status|checks|diff-summary|review-checklist|select|close|delete|rename-branch|`
    `set-label|set-target-branch|open [--pr N]|create|ensure-child|review-list|review-get|`
    `review-add|review-reply|review-set-status`
  - `project scripts [check|run|status|stop|set|unset]`, `commands list|create|get|update|delete`
  - `tab title|split|split-view`, `layout capabilities|views|set|insert|close|move|save|apply|`
    `list|compose [--dry-run]|run`
  - `agent send [--queue|--interrupt] [--wait-until-idle]|wait|read|interrupt|stop [--kill]|`
    `subscribe|should-stop`, `agents list|label set|group create`
  - `team run (1–8 roles)|report (≤16 KiB summary + result file)|status|list|cancel`
  - `coordination-state get|set|delete|watch [--if-version N]|lock|unlock` (versioned CAS,
    advisory locks with TTL)
  - `browser open|snapshot|click|fill|…` (spec §8.6; automation lives in the client)
  - `hook <event>`
- **Targets** `label:<name>`, `id:<stable-id>`, `group:<name>`; layout indexes are never
  identities.
- **Message delivery:** chat adapters queue for the next turn or steer mid-turn; terminal-only
  agents get bracketed paste plus Enter only when idle (or `--force`) and that delivery stays
  marked unverified in output and status.
- **Team runs:** all roles launch in parallel tabs with role context (label, run id, report
  contract); the lead receives one short fan-in message when the last role finishes. Runs live
  in prochost-owned processes and survive daemon restarts.
- **Instruction guides** authored by us and embedded in the binary, reachable through
  `fl instructions <topic>`; the trusted orchestration contract is injected into sessions per
  adapter (§5.5).
- **Coordination state:** durable per-scope key/value with version compare-and-set, advisory
  locks with expiry and owner identity.

## Execution order

1. Daemon-side command service covering workspace/section/worktree/layout/chat/agent surfaces.
2. `fl` CLI with exhaustive help, JSON output and error codes; integration tests over the
   socket using a real daemon and a scripted provider.
3. Coordination state and locks with durability tests.
4. Team runs end to end (parallel tabs, fan-in, restart survival).
5. Hook ingestion and trusted-contract injection per adapter.
6. Browser automation bridge (client-side WebView2 DevTools endpoints, gated by settings and
   per-workspace grant).

## Verification gates

- `fl help` and `fl <cmd> --json` round-trip against a live daemon; every command has a
  contract test, and mutating commands prove the durable effect, not the printed line.
- A refusal (unknown target, stale `--if-version`, lock held) exits non-zero with the reason.
- A team run of at least three roles across two providers visibly produces parallel tabs and a
  lead fan-in; restarting the daemon mid-run keeps the roles alive and the run resumable.
- Terminal-only delivery is labelled unverified everywhere it appears.
- One batched build/test/clippy pass by the integrator.

## Evidence

Slice 1 delivered (`fl` crate plus append-only protocol 0.4 additions and schema v8
`cli_meta`): `fl status`, `workspace list|get|select`, `section list`, `worktree status|checks|
diff-summary|review-*|rename-branch|set-label|set-target-branch`, `project scripts
check|run|status|stop`, `tab title|split|split-view`, `layout views|set|close|compose`,
`chat providers|list|new|send|events|cancel|close`, `history list|get`, and `agent
send|read|interrupt|stop`. Every command supports `--json`; refusals exit non-zero with the
daemon's reason.

Verified against a live daemon on this host (as the daemon's user):

- `fl status --json` resolves the caller, daemon identity and selected workspace.
- `fl workspace list --json`, `chat list --worktree <id> --json`, `history list --json` and
  `chat events <session> --json` read durable state.
- `fl chat new claude`, `fl chat send --session <id>` and `fl chat events` completed a real
  turn through the scripted provider (13 events, final `StateChanged(Idle)`).
- `fl tab title`, `fl tab split`, `fl tab split-view`, `fl layout set <view> --ratio 3500`
  changed the durable layout (rev 1 -> 6, ratio 3500 in the tree) and refused `--ratio 100`.
- `fl worktree review-add --path src/main.rs --body ...` appended a review thread (rev 0 -> 1).

Integrated verification (parent, same live daemon, scripted `claude`):

- Schema v9 migrated the existing state database with a
  `state.db.before-v8-to-v9-*.db` backup.
- `fl coordination-state` create/CAS/stale-conflict/lock/unlock/refusal all behaved as
  specified (`revision_conflict`, `not_found` with non-zero exits).
- A 2-role `fl team run` (`architect` + `builder`, one provider) started two sessions and two
  named tabs (`team:architect`, `team:builder`), settled both roles `Finished`,
  `fan_in_sent: true`, and the lead transcript contains exactly one fan-in `UserMessage`
  naming the run and the settled count. `fl team list` and `fl team report` are durable.
- `fl hook PreToolUse` appended a `ToolCall` row; `fl hook Stop` appended a `HookFact` and the
  session's `status_source` became `Hook`; `fl instructions orchestration` prints the
  embedded guide.
- The native chat transcript renders `ChatEvent::HookFact` as a muted `hook` status row
  (unit test `hook_facts_render_as_status_rows_not_turns`), so hook facts never look like a
  turn outcome.

Bug found by that run and fixed: `fl chat new` expected `Response::ChatSession` while the
daemon replies `ChatSessions([session])`; the CLI now accepts the list reply (the same shape
`flc-sync` reads).

Deferred to the second M6 slice: `team run/report/status/list/cancel`, `instructions`,
coordination state/locks, browser automation, `hook`, workspace/section/worktree CRUD beyond
the listed commands, commands CRUD, script set/unset, the remaining `layout` operations
(`capabilities/insert/move/save/apply/list/run`) and `agent wait/subscribe/should-stop` /
`agents list|label|group`. Terminal-only agent delivery remains explicitly refused.


Slice 2 delivered (schema v9; protocol 0.4 appended): coordination state and
advisory locks, team runs with durable roles/reports and one lead fan-in,
embedded instruction guides, and hook ingestion with trusted contract
injection.

What landed:

- **Schema v9** (`fld-store`): `coordination_state`, `coordination_locks`,
  `team_runs`, `team_roles` (all STRICT) in one versioned migration with the
  existing self-contained `state.db.before-v8-to-v9-*.db` backup. Reopening at
  v9 neither migrates nor backs up again; every stored row is revalidated on
  open.
- **Protocol 0.4 (append-only)**: `Request::{CoordinationGet, CoordinationSet,
  CoordinationDelete, LockCoordination, UnlockCoordination, TeamRun, TeamStatus,
  TeamList, TeamCancel, TeamReportSubmit, TeamReport, HookEvent}`;
  `Response::{CoordinationSnapshot, CoordinationValue, CoordinationLock,
  TeamRun, TeamRuns, HookRecorded}`; `DaemonEvent::CoordinationChanged`;
  `ChatEvent::HookFact`. All requests/responses have `validate()` bounds and
  round-trip tests, and `session.rs` gates the new event at protocol 0.4.
- **Coordination**: per-worktree (default), per-workspace or global values with
  a monotonic version, `--if-version` compare-and-set (0 creates only),
  version-checked delete, an NDJSON `watch`, and owner+TTL advisory locks
  (1 s–24 h, expired locks are free). Stale versions and held locks are
  refusals with the daemon's reason.
- **Team runs**: `fl team run --file <json>` (JSON only, documented in the
  orchestration guide), `status`, `report` (submit or read), `list`, `cancel`.
  Each role gets its own chat session and a named `team:<label>` tab in the
  target worktree; its role context (label, run id, report contract) is
  injected as trusted instructions where the adapter supports them, else as the
  head of the first prompt. Roles settle only from structured adapter events
  (`starting`/`running`/`finished`/`failed`/`cancelled`); the run settles from
  its roles, and the single lead fan-in is claimed transactionally with
  `fan_in_sent`.
- **Instructions**: `crates/fl/instructions/{orchestration,layout,worktree,
  review,browser}.md`, authored for Flotilla, embedded with `include_str!`,
  printed as text and JSON, and usable without a daemon (`fl instructions`
  runs before any socket connect).
- **Hooks**: `fl hook <event>` reads the provider's JSON (stdin or `--payload`)
  over the same user-only Unix socket that every `fl` command uses — the
  0600 socket is the authentication, so no new network surface is opened. Tool
  start/end become `ToolCall` rows; turn start/end become `HookFact` status
  facts with `StatusSource::Hook`; everything else is a notification. Hooks
  cannot approve, answer, resolve, or produce `TurnEnd`/`Error`.
  Provider launches now carry `FLOTILLA_SESSION_ID` and
  `FLOTILLA_WORKTREE_PATH` so hooks can find their session.
- **Trusted contract injection**: adapters that advertise hook support and
  have a real developer-instruction channel (Claude, Codex, Pi/OMP) receive a
  short Flotilla contract pointing at `fl instructions orchestration`; ACP and
  Cursor are not injected. `DaemonConfig.providers` exists only so tests can
  install scripted provider entries; production uses the built-in registry.

Live proof: a real daemon and process host in a temporary `FLOTILLA_HOME`, with
a scripted `claude` stub (its `--version`, `--help` and initialize handshake
match the adapter probe) installed as two provider entries (`claude`, `codex`),
driven through the built `fl` binary. Observed:

- `fl instructions orchestration` printed the embedded guide; `fl instructions
  --json` listed the five topics; `fl instructions orchestration --json`
  returned `{"topic":"orchestration","title":...,"markdown":...}`.
- `fl coordination-state set plan/owner --value alice --if-version 0 --json`
  returned version 1; `--if-version 2` exited 1 with
  `revision_conflict: coordination key `plan/owner` is at version 1, not 2`.
- `lock --owner agent-a --ttl-seconds 60` succeeded; `lock --owner agent-b`
  exited 1 with `revision_conflict: ... held by `agent-a` for another 59s`;
  `unlock --owner agent-b` exited 1 with `not_owner`. `get --json` showed the
  value plus the live lock. A concurrent `watch --timeout 5` printed the
  initial version and the next write.
- `fl team run --file team.json --json` (architect/claude,
  builder/codex, reviewer/claude) returned a run whose three roles each had a
  distinct session id and tab id; `fl team status --json` settled to
  `Finished` with `fan_in_sent: true`; `fl chat list --json` showed the three
  sessions (two providers); `fl layout views --json` showed the three tabs
  `team:architect`, `team:builder`, `team:reviewer`; the lead's transcript
  contained exactly one `UserMessage` beginning `Team run ... settled finished:
  3/3 roles finished.`
- `fl team report --role builder --summary "Built the change." --result-file
  out/build.md` recorded the report; `team report --run <id>` read all roles;
  `team list --json` listed the run; a second two-role run cancelled through
  `fl team cancel` settled `Cancelled`; a `{"roles": []}` file exited 1 with
  `invalid_argument: a team run needs 1-8 roles, got 0`.
- Hooks for the lead session: `PreToolUse`/`PostToolUse` produced `ToolCall`
  rows `Running` then `Completed` for tool `t1`, and `Stop` produced a
  `HookFact{TurnEnded}`; `fl chat list --json` then reported
  `status_source: Hook`. An unknown session exited 1 with `not_found`.
- Daemon restart on the same home: `fl team status --run <id> --json` still
  returned `Finished` with the builder report, and `team list` still returned
  both runs. Provider processes were **not** reattached (the chat service
  resets sessions after restart); no restart survival of live providers is
  claimed.

Deferred/limits recorded: browser automation (`fl browser …`) remains
unimplemented and refused; role tabs are named terminal tabs with the role's
chat session recorded (there is no chat-surface tab in the layout model yet);
roles must submit `team report` themselves (the contract asks them to, and the
fan-in is delivered when every role settles, not when every report arrives);
`workspace`/`section`/`worktree` CRUD beyond slice 1, commands CRUD, script
set/unset, `layout capabilities/insert/move/save/apply/list/run`,
`agent wait/subscribe/should-stop` and `agents list|label|group` remain
deferred; terminal-only agent delivery is still refused.
