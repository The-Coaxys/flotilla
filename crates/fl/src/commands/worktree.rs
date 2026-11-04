//! `fl worktree …`: status, checks, diff summaries and reviews.
use std::collections::BTreeMap;

use fl_model::WorktreeId;
use fl_model::review::{
    LineRange, ReviewAnchor, ReviewMutation, ReviewOrigin, ReviewState, ReviewStatus, ReviewThread,
};
use fl_model::workspace::{ChangeKind, WorktreeStatus};
use fl_proto::{Request, Response};
use uuid::Uuid;

use super::{resolve_worktree, unexpected};
use crate::cli::{Cli, ReviewOriginArg, ReviewStatusArg, WorktreeCommand};
use crate::client::DaemonClient;
use crate::error::CliError;
use crate::output::Reporter;

pub async fn run(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    command: &WorktreeCommand,
) -> Result<(), CliError> {
    match command {
        WorktreeCommand::Status => status(client, reporter, cli).await,
        WorktreeCommand::Checks => checks(client, reporter, cli).await,
        WorktreeCommand::DiffSummary => diff_summary(client, reporter, cli).await,
        WorktreeCommand::ReviewList => review_list(client, reporter, cli).await,
        WorktreeCommand::ReviewGet { thread } => review_get(client, reporter, cli, *thread).await,
        WorktreeCommand::ReviewAdd {
            path,
            start,
            end,
            body,
            origin,
        } => review_add(client, reporter, cli, path, *start, *end, body, *origin).await,
        WorktreeCommand::ReviewReply {
            thread,
            body,
            origin,
        } => review_reply(client, reporter, cli, *thread, body, *origin).await,
        WorktreeCommand::ReviewSetStatus { thread, status } => {
            review_set_status(client, reporter, cli, *thread, *status).await
        }
        WorktreeCommand::RenameBranch { branch } => {
            rename_branch(client, reporter, cli, branch).await
        }
        WorktreeCommand::SetLabel { label } => set_label(client, reporter, cli, label).await,
        WorktreeCommand::SetTargetBranch { branch, clear } => {
            set_target_branch(client, reporter, cli, branch.as_deref(), *clear).await
        }
    }
}

async fn worktree_status(
    client: &mut DaemonClient,
    worktree: WorktreeId,
) -> Result<WorktreeStatus, CliError> {
    match client.rpc(Request::WorktreeStatus { worktree }).await? {
        Response::WorktreeStatus(status) => Ok(status),
        other => Err(unexpected("worktree status", &other)),
    }
}

async fn status(client: &mut DaemonClient, reporter: &Reporter, cli: &Cli) -> Result<(), CliError> {
    let (worktree, _project) = resolve_worktree(client, cli).await?;
    let status = worktree_status(client, worktree.id).await?;
    if reporter.json {
        return reporter.value(&serde_json::json!({
            "worktree": worktree,
            "status": status,
        }));
    }
    reporter.text(format!(
        "{} [{}] target {}",
        worktree.label,
        status.summary.branch.as_deref().unwrap_or("(detached)"),
        worktree.target.as_deref().unwrap_or("(none)")
    ));
    reporter.text(format!(
        "{}+ {}-  ahead {} behind {}  {}",
        status.summary.added,
        status.summary.removed,
        status.summary.ahead,
        status.summary.behind,
        if status.summary.conflicted {
            "conflicted"
        } else if status.summary.dirty {
            "dirty"
        } else {
            "clean"
        }
    ));
    reporter.text(format!("{} changed file(s)", status.files.len()));
    for file in &status.files {
        reporter.text(format!(
            "  {:?} {}{} +{} -{}",
            file.kind,
            file.path,
            file.old_path
                .as_deref()
                .map(|old| format!(" (from {old})"))
                .unwrap_or_default(),
            file.added.map_or("-".into(), |n| n.to_string()),
            file.removed.map_or("-".into(), |n| n.to_string()),
        ));
    }
    Ok(())
}

async fn checks(client: &mut DaemonClient, reporter: &Reporter, cli: &Cli) -> Result<(), CliError> {
    let (worktree, _project) = resolve_worktree(client, cli).await?;
    let forge = match client
        .rpc(Request::ForgeStatus {
            worktree: worktree.id,
        })
        .await?
    {
        Response::ForgeState(state) => state,
        other => return Err(unexpected("forge state", &other)),
    };
    let checks_status = forge.pr.as_ref().and_then(|pr| pr.checks_status());
    if reporter.json {
        return reporter.value(&serde_json::json!({
            "worktree": worktree,
            "available": forge.unavailable.is_none(),
            "unavailable": forge.unavailable,
            "pull_request": forge.pr,
            "checks_status": checks_status,
            "checked_ms": forge.fetched_ms,
        }));
    }
    if let Some(reason) = &forge.unavailable {
        reporter.text(format!("forge unavailable: {reason}"));
        return Ok(());
    }
    match &forge.pr {
        None => reporter.text("no pull request for this branch"),
        Some(pr) => {
            reporter.text(format!(
                "#{} {} [{:?}] {:?}{}",
                pr.number,
                pr.title,
                pr.state,
                pr.mergeable,
                if pr.draft { " (draft)" } else { "" }
            ));
            reporter.text(format!(
                "checks: {}",
                checks_status.map_or("none".into(), |status| format!("{status:?}"))
            ));
            for check in &pr.checks {
                reporter.text(format!(
                    "  {:?} {}{}",
                    check.status,
                    check.name,
                    check
                        .url
                        .as_deref()
                        .map(|url| format!(" {url}"))
                        .unwrap_or_default()
                ));
            }
        }
    }
    Ok(())
}

fn change_kind(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Added => "added",
        ChangeKind::Modified => "modified",
        ChangeKind::Deleted => "deleted",
        ChangeKind::Renamed => "renamed",
        ChangeKind::Copied => "copied",
        ChangeKind::TypeChanged => "type_changed",
        ChangeKind::Untracked => "untracked",
        ChangeKind::Conflicted => "conflicted",
    }
}

async fn diff_summary(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
) -> Result<(), CliError> {
    let (worktree, _project) = resolve_worktree(client, cli).await?;
    let status = worktree_status(client, worktree.id).await?;
    let mut by_kind: BTreeMap<&'static str, u32> = BTreeMap::new();
    for file in &status.files {
        *by_kind.entry(change_kind(file.kind)).or_default() += 1;
    }
    if reporter.json {
        return reporter.value(&serde_json::json!({
            "worktree": worktree,
            "branch": status.summary.branch,
            "head": status.summary.head,
            "upstream": status.summary.upstream,
            "ahead": status.summary.ahead,
            "behind": status.summary.behind,
            "added": status.summary.added,
            "removed": status.summary.removed,
            "dirty": status.summary.dirty,
            "conflicted": status.summary.conflicted,
            "files": status.files.len(),
            "by_kind": by_kind,
            "at_ms": status.at_ms,
        }));
    }
    reporter.text(format!(
        "{}: {} file(s), +{} -{}, ahead {}, behind {}",
        worktree.label,
        status.files.len(),
        status.summary.added,
        status.summary.removed,
        status.summary.ahead,
        status.summary.behind
    ));
    for (kind, count) in by_kind {
        reporter.text(format!("  {kind}: {count}"));
    }
    Ok(())
}

async fn list_reviews(
    client: &mut DaemonClient,
    worktree: WorktreeId,
) -> Result<ReviewState, CliError> {
    match client.rpc(Request::ListReviews { worktree }).await? {
        Response::Reviews(state) => Ok(state),
        other => Err(unexpected("review state", &other)),
    }
}

fn find_thread(state: &ReviewState, thread: Uuid) -> Result<ReviewThread, CliError> {
    let needle = thread.to_string();
    state
        .threads
        .iter()
        .find(|candidate| candidate.id.to_string() == needle)
        .cloned()
        .ok_or_else(|| CliError::not_found(format!("review thread {thread} not found")))
}

async fn review_list(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
) -> Result<(), CliError> {
    let (worktree, _project) = resolve_worktree(client, cli).await?;
    let state = list_reviews(client, worktree.id).await?;
    if reporter.json {
        return reporter.value(&serde_json::json!({
            "worktree": worktree,
            "review": state,
        }));
    }
    reporter.text(format!(
        "{} review rev {}: {} thread(s)",
        worktree.label,
        state.rev,
        state.threads.len()
    ));
    for thread in &state.threads {
        reporter.text(format!(
            "  {} {:?}/{:?} {:?} {}:{}{} ({} comment(s))",
            thread.id,
            thread.status,
            thread.triage,
            thread.origin,
            thread.anchor.path,
            thread
                .anchor
                .lines
                .map_or("-".into(), |lines| lines.start.to_string()),
            thread
                .anchor
                .lines
                .map_or(String::new(), |lines| format!("-{}", lines.end)),
            thread.comments.len()
        ));
    }
    Ok(())
}

async fn review_get(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    thread: Uuid,
) -> Result<(), CliError> {
    let (worktree, _project) = resolve_worktree(client, cli).await?;
    let state = list_reviews(client, worktree.id).await?;
    let thread = find_thread(&state, thread)?;
    if reporter.json {
        return reporter.value(&serde_json::json!({
            "worktree": worktree,
            "thread": thread,
        }));
    }
    reporter.text(format!(
        "{} {:?}/{:?} {:?} {} comments",
        thread.id,
        thread.status,
        thread.triage,
        thread.anchor.path,
        thread.comments.len()
    ));
    for comment in &thread.comments {
        reporter.text(format!("  {:?}: {}", comment.origin, comment.body));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn review_add(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    path: &str,
    start: Option<u32>,
    end: Option<u32>,
    body: &str,
    origin: ReviewOriginArg,
) -> Result<(), CliError> {
    let (worktree, _project) = resolve_worktree(client, cli).await?;
    let lines = match (start, end) {
        (Some(start), Some(end)) => {
            if start == 0 || end < start {
                return Err(CliError::invalid(
                    "review lines must be an ordered 1-based inclusive range",
                ));
            }
            Some(LineRange { start, end })
        }
        (None, None) => None,
        _ => {
            return Err(CliError::invalid(
                "--start and --end must be passed together",
            ));
        }
    };
    let mutation = ReviewMutation::Add {
        anchor: ReviewAnchor {
            worktree: worktree.id,
            path: path.to_owned(),
            lines,
        },
        origin: ReviewOrigin::from(origin),
        body: body.to_owned(),
    };
    let state = mutate_review(client, worktree.id, mutation).await?;
    let created = state.threads.last().cloned();
    if reporter.json {
        return reporter.value(&serde_json::json!({
            "worktree": worktree,
            "review": state,
            "thread": created,
        }));
    }
    reporter.text(format!(
        "added review thread {} (rev {})",
        created
            .as_ref()
            .map(|thread| thread.id.to_string())
            .unwrap_or_else(|| "(unknown)".into()),
        state.rev
    ));
    Ok(())
}

async fn review_reply(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    thread: Uuid,
    body: &str,
    origin: ReviewOriginArg,
) -> Result<(), CliError> {
    let (worktree, _project) = resolve_worktree(client, cli).await?;
    let state = list_reviews(client, worktree.id).await?;
    let found = find_thread(&state, thread)?;
    let mutation = ReviewMutation::Reply {
        thread: found.id,
        origin: ReviewOrigin::from(origin),
        body: body.to_owned(),
    };
    let state = mutate_review(client, worktree.id, mutation).await?;
    let updated = find_thread(&state, thread)?;
    if reporter.json {
        return reporter.value(&serde_json::json!({
            "worktree": worktree,
            "review": state,
            "thread": updated,
        }));
    }
    reporter.text(format!(
        "replied to review thread {} ({} comments, rev {})",
        updated.id,
        updated.comments.len(),
        state.rev
    ));
    Ok(())
}

async fn review_set_status(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    thread: Uuid,
    status: ReviewStatusArg,
) -> Result<(), CliError> {
    let (worktree, _project) = resolve_worktree(client, cli).await?;
    let state = list_reviews(client, worktree.id).await?;
    let found = find_thread(&state, thread)?;
    let mutation = ReviewMutation::SetStatus {
        thread: found.id,
        status: ReviewStatus::from(status),
    };
    let state = mutate_review(client, worktree.id, mutation).await?;
    let updated = find_thread(&state, thread)?;
    if reporter.json {
        return reporter.value(&serde_json::json!({
            "worktree": worktree,
            "review": state,
            "thread": updated,
        }));
    }
    reporter.text(format!(
        "review thread {} is {:?} (rev {})",
        updated.id, updated.status, state.rev
    ));
    Ok(())
}

async fn mutate_review(
    client: &mut DaemonClient,
    worktree: WorktreeId,
    mutation: ReviewMutation,
) -> Result<ReviewState, CliError> {
    let mut last = None;
    for _ in 0..2 {
        let state = list_reviews(client, worktree).await?;
        match client
            .rpc(Request::MutateReview {
                worktree,
                base_rev: state.rev,
                mutation: mutation.clone(),
            })
            .await
        {
            Ok(Response::Reviews(state)) => return Ok(state),
            Ok(other) => return Err(unexpected("review state", &other)),
            Err(error) if error.code == "revision_conflict" => last = Some(error),
            Err(error) => return Err(error),
        }
    }
    Err(last.unwrap_or_else(|| CliError::internal("review retry exhausted")))
}

async fn rename_branch(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    branch: &str,
) -> Result<(), CliError> {
    let (worktree, _project) = resolve_worktree(client, cli).await?;
    let updated = match client
        .rpc(Request::RenameWorktreeBranch {
            worktree: worktree.id,
            branch: branch.to_owned(),
        })
        .await?
    {
        Response::Worktree(worktree) => worktree,
        other => return Err(unexpected("worktree", &other)),
    };
    if reporter.json {
        return reporter.value(&serde_json::json!({ "worktree": updated }));
    }
    reporter.text(format!("renamed {} to {}", updated.label, branch));
    Ok(())
}

async fn set_label(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    label: &str,
) -> Result<(), CliError> {
    let (worktree, _project) = resolve_worktree(client, cli).await?;
    let updated = match client
        .rpc(Request::SetWorktreeLabel {
            worktree: worktree.id,
            label: label.to_owned(),
        })
        .await?
    {
        Response::Worktree(worktree) => worktree,
        other => return Err(unexpected("worktree", &other)),
    };
    if reporter.json {
        return reporter.value(&serde_json::json!({ "worktree": updated }));
    }
    reporter.text(format!("label is now {}", updated.label));
    Ok(())
}

async fn set_target_branch(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    branch: Option<&str>,
    clear: bool,
) -> Result<(), CliError> {
    let target = match (branch, clear) {
        (Some(branch), false) => Some(branch.to_owned()),
        (None, true) => None,
        _ => {
            return Err(CliError::invalid(
                "pass a branch name or --clear (exactly one)",
            ));
        }
    };
    let (worktree, _project) = resolve_worktree(client, cli).await?;
    let updated = match client
        .rpc(Request::SetWorktreeTarget {
            worktree: worktree.id,
            target,
        })
        .await?
    {
        Response::Worktree(worktree) => worktree,
        other => return Err(unexpected("worktree", &other)),
    };
    if reporter.json {
        return reporter.value(&serde_json::json!({ "worktree": updated }));
    }
    reporter.text(format!(
        "target branch is {}",
        updated.target.as_deref().unwrap_or("(project default)")
    ));
    Ok(())
}
