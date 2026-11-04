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

Bug found by that run and fixed: `fl chat new` expected `Response::ChatSession` while the
daemon replies `ChatSessions([session])`; the CLI now accepts the list reply (the same shape
`flc-sync` reads).

Deferred to the second M6 slice: `team run/report/status/list/cancel`, `instructions`,
coordination state/locks, browser automation, `hook`, workspace/section/worktree CRUD beyond
the listed commands, commands CRUD, script set/unset, the remaining `layout` operations
(`capabilities/insert/move/save/apply/list/run`) and `agent wait/subscribe/should-stop` /
`agents list|label|group`. Terminal-only agent delivery remains explicitly refused.

