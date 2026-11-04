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
- **M3 remains open.** The direct Git/PR loop is implemented; the required agent-routed
  operations, Fast/Thorough routes, per-scope prompt templates, dispatch modes, app-managed
  review threads and sections are not yet implemented. Adapter work is a prerequisite for
  routed actions, not permission to omit them from M3 acceptance.

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
- Remaining M3 acceptance work: app-managed anchored review threads and triage; manual/filter
  sections with manual-assignment precedence and pinned ordering; Fast/Thorough routes,
  prompt templates and dispatch modes; temporary branch naming and agent-driven actions.
- The direct-loop UI smoke uses a scripted forge. It does not prove GitHub mutations against
  a real remote repository or installed Windows behavior.
- Protocol boundary verification now checks rejection of 0.2 peers and suppression of 0.4
  forge events for supported 0.3 peers. All three daemon workspace tests pass; native
  shell/editor/app tests pass (77 total). Workspace and UI clippy pass with warnings denied.
