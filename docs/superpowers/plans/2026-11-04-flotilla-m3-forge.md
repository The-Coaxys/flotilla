# Flotilla M3 — Forge and Git actions

Governing spec: `docs/superpowers/specs/2026-11-02-flotilla-design.md` §6 (adaptive Git action
button, direct operations, forge) and §8.1 (bottom bar, Checks tab).

**Exit criteria (spec §14):** the full commit → PR → checks → merge → delete loop from the UI.

## Architecture decisions

- **Git actions run on the daemon through the git CLI** (`fld-git::ops`): commit (index or
  everything), push (setting the upstream on the first push), update (fetch + merge the target,
  aborting a conflicting merge so the worktree is untouched), and the GitHub repository read
  from `remote.origin.url` itself (`remote get-url` applies `insteadOf` rewrites).
- **The forge is GitHub through `gh`** (`fld-forge`), reusing the server's sign-in: one GraphQL
  query returns the branch's newest PR, its mergeability, review decision, unresolved threads,
  check runs and status contexts, and the repository's allowed merge methods; mutations are
  `gh pr create` (body on stdin), `gh pr ready` and `gh pr merge`. `FLOTILLA_GH` overrides the
  binary. A missing or signed-out `gh`, a non-GitHub origin or a detached HEAD is reported as
  an `unavailable` reason, not an error.
- **Polling:** the daemon refreshes forge state every 15 s for worktrees a client asked about in
  the last two minutes and every 60 s for the rest, only for branches with an upstream, backing
  off five minutes after a rate limit. Changes are pushed as `DaemonEvent::ForgeChanged`, only
  to protocol ≥ 0.4 peers. Request replies also land in the client mirror (an unchanged state
  is never pushed, so a reconnecting client would otherwise wait for the next change).
- **Adaptive button** (`git_actions.rs`, pure and unit-tested): merged PR → delete worktree;
  dirty → commit; on the base branch → push or up to date; no open PR → create PR; open PR →
  push local commits, update on conflicts, checks when failing or running, mark ready for
  drafts, then awaiting review or merge with the repository's preferred method. The dropdown
  lists every applicable action (only allowed merge methods) plus "Open pull request".
- **M3 remains open.** The direct Git/PR loop and sections implementation are present;
  the required agent-routed operations, Fast/Thorough routes, per-scope prompt templates,
  dispatch modes and app-managed review threads still require their separate acceptance
  evidence. Sections changes below are implementation evidence, not verified acceptance.

## Contract

- `fl-model::forge` — `PrState`, `CheckStatus`, `Check`, `ReviewDecision`, `Mergeable`,
  `MergeMethod`, `MergeMethods`, `PullRequest`, `ForgeState`; `DaemonEvent::ForgeChanged`.
- `fl-proto` 0.4 — requests `GitCommit`, `GitPush`, `GitUpdate`, `ForgeStatus`,
  `CreatePullRequest`, `MarkPullRequestReady`, `MergePullRequest`; responses `ForgeState`,
  `Committed`; bounds for messages (64 KiB) and checks (512).

## Execution evidence

- `fld-git` ops: tests against a real bare origin cover committing everything vs the index,
  refusing an empty commit or message, the first push setting `origin/<branch>`, an update
  merging a moved `main`, refusing a dirty tree and rolling back a conflicting merge, and
  GitHub URL parsing in every common form.
- `fld-forge`: replies recorded from GitHub (an open PR with running checks, a merged PR, no
  PR) and one built from the same schema with failures, a conflict, requested changes and
  unresolved threads parse into the model; GraphQL errors, a missing repository and invalid
  JSON are typed errors.
- Daemon end to end (`git_actions_and_the_pull_request_loop`): a worktree of a clone whose
  GitHub origin git reaches through a URL rewrite to a local bare repository, and a scripted
  `gh`: commit, refused empty commit, no PR, draft PR (branch pushed first, body delivered on
  stdin, base `main`), a pushed `ForgeChanged`, mark ready, a disallowed merge method refused,
  squash merge, update from a moved `main`, and "not installed" when `gh` is missing.
- Adaptive rules: four unit tests cover the whole task-branch loop, blockers on open PRs,
  base-branch and quiet branches, primary checkouts never offering deletion, and the dropdown's
  merge methods.
- Real client run (X11 against a daemon on this host, scripted `gh` for the test account):
  created a worktree, wrote a file in its terminal, **Commit** (dialog) → **Create PR** (title
  from the branch, multi-line body, draft) → Checks tab showing #7 Draft with two green checks
  → **Mark ready** → **Squash and merge** (confirmation) → **Delete worktree**. Bugs found and
  fixed on the way: clicks on a dialog reached the terminal behind it (dialogs and the actions
  menu now occlude), and a reconnecting client never learned an unchanged forge state.

### Limits

- GitHub only; GitLab/Bitbucket later behind the same calls.
- Remaining M3 acceptance work: app-managed anchored review threads and triage;
  Fast/Thorough routes, prompt templates and dispatch modes; temporary branch naming and
  agent-driven actions. Sections require the checks and native smoke described below.
- The direct-loop UI smoke uses a scripted forge. It does not prove GitHub mutations against
  a real remote repository or installed Windows behavior.
- Protocol boundary verification now checks rejection of 0.2 peers and suppression of 0.4
  forge events for supported 0.3 peers. All three daemon workspace tests pass; native
  shell/editor/app tests pass (77 total). Workspace and UI clippy pass with warnings denied.

### Safety review follow-through

- Reproduced two data-loss paths with real Git repositories before fixing them:
  Commit staged unmerged index entries, and Update aborted an already pending merge whose
  worktree was clean. Both regressions now preserve the user's index and merge state.
- Merge confirmation carries the displayed PR number through the wire; a different current
  PR is rejected with `RevisionConflict` before calling the forge.
- Forge lookup verifies the head repository and branch, and prefers an open PR over newer
  closed history. Ambiguous incomplete PR history is reported unavailable, never guessed.
- Check and review-thread connections are paginated; check state retains GitHub's
  authoritative rollup rather than inferring a pass from the first context page.
- Rate-limit backoff is enforced before explicit refreshes and each poll candidate.
- Exercised the production `GitHub::fetch` path against the real repository: PR #3, branch
  `m2-workspaces`, six checks, aggregate Pending, zero unresolved threads, squash preferred.
  The temporary driver was removed. The multi-page fixture also proves a late failing check
  and unresolved thread are included before publication.
## App-managed review implementation notes (not verification evidence)

- `fl-model::review` holds typed thread/comment IDs, repository-relative file anchors and
  optional 1-based inclusive line ranges, User/Agent origin, triage and lifecycle status.
  Human comments start Pending; agent findings start Open and enter the pending bundle
  only after Should fix. Dismiss resolves and excludes a finding; Reopen restores triage.
- `fld-store::database::Database::{list_reviews,mutate_review}` owns bounded, atomic
  worktree snapshots and checks `base_rev` before every mutation. `review::SCHEMA_V5`
  creates the separate review table with cascading worktree deletion. Integration must
  apply the sections V4 migration before V5 and raise `SCHEMA_VERSION` to 5; this slice
  deliberately does not invent or skip V4. Migration-fixture downgrades must drop reviews.
- Append-only protocol 0.4 requests are ListReviews, MutateReview and PendingReviewBundle;
  responses are Reviews and ReviewBundle. ReviewChanged carries worktree/revision and is
  gated to 0.4 peers. `Sync::{list_reviews,mutate_review,pending_review_bundle}` exposes
  these calls; `WorkspacesState::reviews` follows events and reconnects with stale-response
  protection. M2 workspace/worktree positional structs remain unchanged.
- Native `host_review.rs` adds a Review panel, file comments from the document strip,
  comments from the existing inclusive DiffView selection event, anchored file navigation,
  replies and Should fix/Dismiss/Reopen/Fixing/Resolve. Drafts remain on errors/conflicts.
  Copy pending reads the authoritative bundle and copies it without marking anything sent.
  The public sync bundle API is the parent action router's dispatch integration point;
  this slice does not claim a current/new/background chat delivery.
- Added domain tests for anchor validation, transitions/replies and dismissed-bundle
  exclusion, plus a store test for durable restart, revision conflicts and cascade cleanup.
  No builds, tests, lint, formatters or native smoke were run for this slice.
- Integration checks to run: `cargo test -p fl-model review`, `cargo test -p fld-store
  reviews_survive_restart_and_conflicts_are_atomic`, `cargo test -p fl-proto`, and a native
  client build. Manual smoke: open a worktree file, Comment on file, submit, reply; select
  one line then Shift-select a range in Changes, submit; navigate each anchor; triage,
  resolve/reopen and Copy pending; restart daemon and reconnect; edit the same snapshot
  from two clients to exercise revision rejection while preserving the losing draft.
## Sections (§6) — implementation evidence, not yet verified

- `fl-model::sections` owns workspace-local manual/filter definitions, explicit display
  order and filter priority, manual worktree assignments and independent pin membership.
  The deterministic classifier chooses Pinned, explicit manual assignment, matching filter
  by descending priority (display order and stable identity break ties), then Unassigned.
  Predicates combine with AND: PR state, review decision, rolled-up checks, structured agent
  state, run state and whole-branch glob. `*` and `?` are supported; other characters are
  literal, including slash and brackets. No PR, review, checks, run or agent observation
  satisfies a predicate asking for a known value. A fresh detached Git summary does not
  fall back to a stale stored branch.
- `fld-store::database::sections` introduces schema V4 through the existing transactional,
  backed-up migration convention. Definitions, memberships and their own unsigned revision
  commit as one bounded snapshot. Mutations use revision compare-and-set. Assignment
  requires a manual section in the worktree's own workspace. Deletion or conversion to a
  filter clears assignments but preserves pins; worktree deletion prunes membership in the
  same transaction. No fields were added to the M2 `WorkspaceTree` or `Worktree` payloads.
- Protocol 0.4 adds append-only section list/mutation requests, snapshot responses and a
  section-change event. New events are suppressed for 0.3 peers. Client sync follows
  snapshots monotonically, reloads after reconnect, and immediately remembers mutation
  replies. Native sidebar/editor integration uses this real daemon state rather than
  local-only section definitions.
- Added behavior tests cover precedence, missing observations, conjunctive forge filters,
  neutral checks, detached branches, wildcard boundaries, run/setup distinction and unknown
  exits, workspace isolation, durable reopen, stale revisions, invalid edits, pin-preserving
  section deletion and V3 migration backup. A real-daemon socket test covers list/create/
  assign/pin, pushed state, revision conflict, unchanged M2 tree, 0.3 event suppression,
  daemon restart and membership pruning. Protocol/sync tests accompany their wire paths.

### Sections limits and pending verification

- Structured agent observations are currently unsupported by this slice: classification
  receives `None`, never inferred terminal text or invented Idle/Running state. The model
  and filter editor retain the optional future structured-agent predicate contract.
- Run filters aggregate **run targets only**, never setup/teardown. Deterministic precedence
  across multiple targets is Running, Failed, ExitedUnknown, Idle, Succeeded; an absent
  target is unknown, not Idle. Check predicates distinguish neutral from success and no
  checks from either; integration must use the authoritative forge aggregate when supplied.
- No builds, tests, formatters, linters or UI smoke were run for this section slice. Parent
  integration must run `cargo test -p fl-model -p fl-proto -p fld-store -p flc-sync`,
  `cargo test -p fld-server --test workspaces sections_mutations_events_restart_and_legacy_peer`,
  and the native client build/check on its supported target, after shared M3 merges.
- Native smoke: connect two clients; create a manual section and an ordered branch-glob
  filter, assign a matching worktree manually, pin it, unpin it, unassign it and observe the
  filter reclaim it. Edit filter priority/order and each predicate, verify unknown agent
  state does not match, delete the manual section, reconnect/restart the daemon and confirm
  persistence and cross-client updates. Disconnected/unsupported controls and stale-edit
  errors must remain visible and safe. This interaction is prescribed, not exercised.


## Sections, app-managed review threads and routing preparation

Implemented and exercised after the direct loop:

- `fl-model::sections` + store schema V4 + `ListSections`/`MutateSections` + `SectionsChanged`:
  manual and filter sections with display order, filter priority, pins and manual assignments;
  classification is pinned → manual assignment → filter (descending priority) → unassigned.
- `fl-model::review` + store schema V5 + `ListReviews`/`MutateReview`/`PendingReviewBundle`:
  file and line-range anchors, replies, triage (Should fix / Dismiss), lifecycle transitions,
  revision compare-and-set, and `ReviewChanged`. Copying the pending bundle never claims a send.
- `fl-model::routing` + store schema V6 + `GetRouting`/`MutateRouting`/`ListChatProviders`/
  `DispatchGitTask`: scopes global/workspace/project, group and action overrides, dispatch modes
  including alternate click, per-scope prompt templates with bounded `{{name}}` interpolation,
  and real provider discovery through the adapter core. Prepare modes render the real prompt;
  submit modes report explicitly that the chat runtime is required and never pretend to run.

Native X11 evidence (real daemon, scripted forge): created a section, assigned a worktree
(pin precedence then manual precedence both visible in the sidebar), opened the Review panel,
added a thread on `z.txt`, restarted the daemon and confirmed the section, assignment and
thread survived with `Copy pending` producing the thread's prompt on the clipboard.

Known gaps at this point: routed submit execution (needs the chat runtime), routes/templates UI
and the agent-routed Git actions UI (in progress), and a Windows client run of these surfaces.
