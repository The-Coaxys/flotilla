# Worktrees

A worktree is a git checkout the daemon owns under the project's worktree
root. Terminals, layouts, scripts, chats, reviews, team runs and coordination
state are all scoped to one worktree. `fl` resolves the caller's worktree from
its working directory; `--worktree <id|label|path>` targets another one.

## Inspecting

```sh
fl worktree status --json          # branch, target, changed files, counts
fl worktree checks --json          # forge checks for the branch
fl worktree diff-summary --json    # totals and per-kind counts
fl status --json                   # caller, worktree, selected workspace
```

## Naming and targets

```sh
fl worktree set-label "Payments spike"
fl worktree rename-branch feature/payments
fl worktree set-target-branch main
fl worktree set-target-branch --clear
```

`rename-branch` renames the checked-out branch in the repository, so it is a
real git operation, not a display setting. The target branch is the merge base
for diffs and status; clearing it falls back to the project default.

## Reviews

Reviews are app-managed threads anchored to a file or an inclusive 1-based
line range:

```sh
fl worktree review-add --path src/main.rs --start 10 --end 12 --body "race here"
fl worktree review-list --json
fl worktree review-get <thread-id> --json
fl worktree review-reply <thread-id> --body "fixed in 4a1c"
fl worktree review-set-status <thread-id> resolved
```

Origins are `user` (default) or `agent`; statuses are `open`, `pending`, `fix`
and `resolved`. A review mutation carries the revision it read; a stale
revision is a conflict with the daemon's current revision.

## Scripts

```sh
fl project scripts check --json
fl project scripts run            # the default run target
fl project scripts run test
fl project scripts run --setup
fl project scripts stop --teardown
```

Scripts come from `.flotilla/config.json` in the repository. Running one
starts a daemon terminal whose output survives reconnects; `check` reports
parse errors and the configured targets without running anything.

## Not implemented in this build

`worktree select|close|delete|open|create|ensure-child|review-checklist` are
not implemented. Deleting or creating worktrees is refused with a clear
message instead of a partial action.
