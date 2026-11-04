# Flotilla M2 — Workspaces and git

Governing spec: `docs/superpowers/specs/2026-11-02-flotilla-design.md` §4, §5.1, §6 and §8.1.

**Exit criteria (spec §14):** agents are usable in terminal tabs, one worktree per task, with live
+/− counts. The owner adds a project on the VPS from the client, creates a worktree per task,
runs agents in that worktree's terminals, sees its +/− counts update live in the sidebar, reads
and edits files, reviews Changes in a diff view, and runs setup/run scripts.

## Architecture decisions

- **Git runs on the daemon through the `git` CLI** (no libgit2): `status --porcelain=v2 -z
  --branch`, `diff --numstat -z` against `merge-base HEAD <target>`, `worktree add/remove/list
  --porcelain`, `show <rev>:<path>`. The CLI is what the user's repository already works with
  (hooks, config, credentials) and avoids a C dependency in the static daemon.
- **Watcher:** `notify` on the worktree and its git dir, skipping ignored heavy directories
  (`node_modules`, `target`, `.venv`, `dist`, `build`, `.next`), debounced 150 ms, with a 10 s
  poll fallback. Status is recomputed off the async runtime and pushed only when it changed.
- **Push events:** `ServerMsg::Event(DaemonEvent)` goes to every authenticated session. Clients
  refetch on `WorkspaceTreeChanged`/`LayoutChanged` and apply `WorktreeStatus`/`ScriptChanged`
  directly. Events are hints; a reconnect refetches everything.
- **Layouts per worktree:** `LayoutScope::{Home, Worktree(id)}`. `GetLayout`/`ApplyLayout`
  remain the Home scope (the M1 workspace); worktree layouts use the scoped requests and spawn
  their terminals with the worktree as cwd and `FLOTILLA_ROOT_PATH`, `FLOTILLA_WORKTREE_PATH`,
  `FLOTILLA_WORKTREE_NAME` set.
- **Scripts are terminals:** setup/run/teardown commands from `.flotilla/config.json` run as
  daemon terminals (`sh -lc`), so their output is persistent, attachable and survives restarts.
  `ScriptChanged` events report state; the client attaches to `ScriptState.terminal`.
- **Files:** read/write go through the daemon with size bounds (8 MiB), binary detection and
  optimistic concurrency (`expected_sha256`); writes are atomic (temp file + rename, preserving
  mode). Directory listings mark ignored entries instead of hiding them.
- **File and diff tabs are client-side documents in M2:** they open in the worktree's center
  area next to terminal tabs but are not part of the daemon layout (the layout's panes are still
  terminals). Persisting document tabs in the layout is deferred to the layout-surface work in
  M4 (chat surfaces need the same pane-content change).
- **Diffing and highlighting happen on the client** (`imara-diff`, tree-sitter), as the spec says.

## Contract (committed before fan-out)

- `fl-model::workspace` — `WorkspaceInfo`, `Project{Kind}`, `Worktree`, `WorkspaceTree`,
  `FileChange`/`ChangeKind`, `GitSummary`, `WorktreeStatus`, `DirEntry`/`DirListing`,
  `FileContent`, `FileDiff`, `ScriptKind`/`ScriptRunState`/`ScriptState`, `LayoutScope`,
  `DaemonEvent`; `ProjectId`.
- `fl-proto` 0.3 — appended `Request` variants (`GetWorkspaceTree`, `ListDir`, `AddProject`,
  `CloneProject`, `RemoveProject`, `CreateWorktree`, `DeleteWorktree`, `SetWorktreeLabel`,
  `WorktreeStatus`, `ReadFile`, `WriteFile`, `FileDiff`, `GetScopedLayout`, `ApplyScopedLayout`,
  `RunScript`, `StopScript`, `ListScripts`), matching `Response` variants, `ServerMsg::Event`,
  and bounds in `validation` (paths ≤4 KiB and absolute, files ≤8 MiB, ≤10k dir entries, ≤20k
  changed files).

## Slices

| Slice | Owner | Depends on | Acceptance |
|---|---|---|---|
| A. Daemon workspaces | `fld-git` (new), `fld-store` schema v3, `fld-server` handlers/events/scoped layouts/scripts | contract | Real-git integration tests: add project (git + folder), discover existing worktrees, create/delete worktree with `.worktreeinclude` and setup script, dirty delete refused without force, status/numstat against merge-base, watcher pushes a changed status within 1 s of a write, read/write with conflict detection, diff sides, scoped layout spawns a shell in the worktree cwd with `FLOTILLA_*` env, scripts as terminals |
| B. Editor and diff | `flc-editor` (new GPUI crate) | none | Window-free tests: rope buffer edits/undo/redo/selection/find, tree-sitter highlight spans for each bundled grammar, unified and side-by-side diff rows from imara-diff incl. moved/added/removed blocks and binary/large-file fallbacks |
| C. Client sync | `flc-conn` events, `flc-sync` workspace mirror, scoped layouts, status cache, file/script RPC helpers | A | Integration tests against the real daemon: tree mirror follows add/remove, status events update counts, scoped layout sync, reconnect refetch |
| D. Workspace UI | `flc-shell`/`flotilla`: sidebar, add-project dialog with remote browser, worktree create/delete, right panel Files/Changes/Setup/Run, bottom branch bar, file/diff documents | B, C | Real client run (X11 under Xvfb against a real daemon) with screenshots of each flow; Windows CI UI smoke still green |

## Execution evidence

- Slice A (daemon): `fld-git` wraps the git CLI (porcelain v2 status, numstat
  against the merge-base, worktree add/remove with `.worktreeinclude` copies,
  guarded delete, lenient `.flotilla/config.json`, bounded listings with
  ignore marks, conflict-checked atomic writes, diff sides) and a debounced
  `notify` watcher with a poll fallback; 31 unit and 27 integration tests over
  real repositories. `fld-store` schema v3 adds workspaces, projects,
  worktrees and worktree-scoped layouts with the M1 crash guarantees.
  `fld-server` serves every 0.3 request and pushes events on a bounded
  per-session queue, never to peers older than 0.3. An end-to-end daemon test
  covers projects, a worktree with `.worktreeinclude` and setup, a pushed
  status event with exact +/− counts, conflict-checked writes, diff sides, a
  worktree shell whose cwd and `FLOTILLA_*` environment are read from `/proc`,
  run/stop scripts and a forced delete closing its terminals.
- Slice B (editor): `flc-editor` has a rope buffer (CRLF preserving, grouped
  undo, find), tree-sitter highlighting for 21 grammars (YAML and TOML queries
  rewritten for first-match ordering), a histogram diff with word highlights
  and expandable gaps, and GPUI editor and diff views; 37 window-free tests.
  Checked visually on X11 (lavapipe): highlighting, click-to-caret, drag
  selection, live find, split diffs.
- Slice C (client sync): control links forward daemon events; `Sync` mirrors
  the workspace tree, status and scripts, refetching on each link, and runs one
  layout task per scope with its own cache file. Mirror caches are pruned
  against the daemon's process list. A real-daemon test covers the tree, a
  pushed +/− update, a worktree shell with its environment, a daemon restart
  refetch, and requests awaited from a non-Tokio executor.
- Slice D (UI): the sidebar lists the selected host's projects and worktrees
  with live +/− counts, a dirty dot and ★ for primaries; the center shows the
  selected worktree's terminals or open file/diff documents; the right panel
  has Files, Changes and Setup/Run script tabs; dialogs add a project (remote
  folder picker or clone), create and delete worktrees.
- Real client run (X11 under Xvfb, connecting over SSH/QUIC to a test account
  on this host): the client upgraded the M1 daemon in place without ending its
  shells, added a git project and a folder project, showed +4 −1 and three
  changes live after edits made in the worktree's terminal, opened a
  word-highlighted diff, edited and saved README.md (verified on disk),
  created a worktree whose setup ran and whose shell started in it, ran and
  stopped the run script with live output, and deleted the worktree.
- Bugs found by that run and fixed: requests awaited from the UI executor
  panicked (no Tokio reactor); a daemon with the same version label but an
  older protocol was never replaced (local builds are all `dev`); a new
  project's checkout did not open when the RPC reply overtook the tree event;
  the file tree went stale after changes; a duplicate Run tab; folder
  projects showed git UI.

### Limits

- File and diff tabs are client-side: they are not restored after a client
  restart and are not shared between clients (the daemon layout still holds
  terminals only).
- No divider dragging, no rename/label editing of worktrees from the UI yet
  (the RPC exists), no `.env` symlink option, no named-run-target stop
  commands UI beyond Stop.
