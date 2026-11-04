# Flotilla M5 — More adapters

Governing specification: `docs/superpowers/specs/2026-11-02-flotilla-design.md` §5.2, §5.6, §14.

Exit criterion: every installed provider works in its best surface — chat where an adapter
exists, terminal otherwise — and a chat can switch to a terminal and back.

## Scope

- **ACP generic adapter.** One adapter for Agent Client Protocol CLIs (`opencode acp`,
  `hermes acp`, and any configured provider declaring ACP): discovery of the handshake,
  streaming text/thinking/tool events, permission requests mapped to `ApprovalRequest`,
  session load/resume where the CLI supports it, and explicit terminal-only degradation with
  the concrete protocol failure. Bounded reads, child ownership and backpressure reuse
  `fld-agent`'s existing process layer.
- **Cursor (`stream-json`)** and **Pi/OMP (`--mode rpc`)** adapters with their real flag sets,
  partial output, resume identifiers and permission surfaces.
- **Grok** through ACP, or its own protocol when it does not expose ACP; the same tests apply.
- **Provider registry extensions:** user overrides in settings (command, args, env, adapter
  kind, resume strategy, hook support), persisted and validated, exposed to the routes UI so a
  custom provider can be selected without code changes. A provider whose override fails
  validation must degrade with an explanation, never silently.
- **Chat ↔ terminal switching.** A chat can move to a terminal surface and back while idle,
  only when the adapter exposes a validated resume target; the switch preserves the transcript,
  the session identity and the layout tab. Switching while a turn runs is refused.
- **Profiles.** Per provider: config dir (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, equivalents),
  executable, flags. Switching a profile stages a new continuation and leaves the original
  chat on its old account; both transcripts stay available.
- **Hooks and status.** Provider hook ingestion through `fl hook <event>` and the status-source
  labeling already modeled (structured, hook, heuristic). Heuristic status is never used to
  approve anything or to declare a task complete.

## Execution order

1. Extract the shared protocol plumbing (`fld-agent`) so each adapter only maps messages.
2. ACP adapter + tests; then Cursor; then Pi; then Grok. Each adapter's fixtures come from its
   installed CLI's own protocol output.
3. Registry overrides + persisted settings; then chat ↔ terminal switching; then profiles.
4. Wire routes and dispatch to arbitrary discovered providers (the route UI already lists
   discovery results).

## Verification gates

- Per adapter: recorded protocol fixtures, malformed message handling, child death, unsupported
  capability, permission denial, resume validation and bounded output; a live smoke with the
  actually installed CLI for each provider, reporting the version used.
- Degradation: an adapter that fails discovery leaves a terminal-only provider with the reason
  shown in the UI and routes.
- Switch surfaces: a chat moves to a terminal and back with the same session and transcript;
  a running turn refuses the switch with a clear message.
- Profiles: switching profiles starts a continuation without mutating the original chat's
  account state; both transcripts remain readable and searchable.
- One batched build/test/clippy pass at the end by the integrator; no per-slice builds.

## Evidence

Recorded 2026-11-04 on the integration host. Fixtures and their provenance live in
`crates/daemon/fld-agent/tests/fixtures/`; every recorded handshake below was performed by
the adapter/discovery code with bound timeouts and no prompt or model request.

| Surface | Installed version | Verified |
|---|---|---|
| ACP initialize (`opencode acp`) | opencode 1.18.30 | `initialize` returned protocolVersion 1 with `loadSession`; `session/new` returned a session ID; `session/set_model` returned an empty result |
| ACP initialize (`hermes acp`) | hermes-agent 0.22.5 | `initialize` returned protocolVersion 1 with `loadSession` and `sessionCapabilities.resume` |
| Cursor `create-chat` + flag set | cursor-agent 2026.07.01-41b2de7 | `--help` exposes `-p`, `--output-format stream-json`, `--stream-partial-output`, `--resume`, `--mode`, `create-chat`; `create-chat` returned a UUID |
| Pi `--mode rpc` `get_state` | pi 1.4.0 | Response returned a UUID session ID and model; JSONL framing matched `docs/rpc.md` |
| OMP `--mode rpc` `get_state` | omp 18.16.0 | `ready` announced protocol 1 (supports 1/2); `get_state` returned a UUID session ID |

Adapter IDs and capabilities (`ProviderRegistry::discover`; ACP rows require a configured
override such as command `opencode`, args `["acp"]`):

| id | adapter | chat | resume | steer | approvals | questions | partial |
|---|---|---|---|---|---|---|---|
| `cursor` | CursorStreamJson | yes | yes | no | no | no | yes |
| `pi` | PiRpc | yes | yes | yes | no | yes | yes |
| `omp` | PiRpc | yes | yes | yes | no | yes | yes |
| configured ACP | Acp | yes | if advertised | no | yes | no | yes |

Unverified without credentials/model use: live streaming turns for any provider, real ACP
`session/request_permission` traffic, Cursor `interaction_query` decisions, Pi extension UI
dialogs, and ACP `session/load`/`session/resume` against a stored session. These paths are
covered by synthetic protocol fixtures and unit tests; they are not claimed as live-verified.
Cursor headless has no client response channel for permission queries, so its approvals and
questions capabilities are false and queries are surfaced as diagnostics instead of being
auto-approved. Pi/OMP read-only mode is rejected explicitly because their built-in tools
execute without a client permission prompt.
