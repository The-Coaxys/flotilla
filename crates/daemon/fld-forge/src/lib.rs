//! The repository's forge, through its official CLI so the user's existing
//! sign-in is reused: GitHub via `gh` (GitLab and Bitbucket come later
//! behind the same calls).
//!
//! Everything here is blocking; the daemon calls it from `spawn_blocking`.
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use fl_model::forge::{
    Check, CheckStatus, MergeMethod, MergeMethods, Mergeable, PrState, PullRequest, ReviewDecision,
};
use serde::Deserialize;

/// Stderr carried in errors.
const MAX_STDERR: usize = 2000;

#[derive(Debug, thiserror::Error)]
pub enum ForgeError {
    /// The forge cannot be used here; the message says why.
    #[error("{0}")]
    Unavailable(String),
    #[error("gh {args} failed: {stderr}")]
    Command { args: String, stderr: String },
    #[error("unexpected reply from GitHub: {0}")]
    Parse(String),
}

pub type Result<T, E = ForgeError> = std::result::Result<T, E>;

/// A GitHub repository reached through `gh`.
#[derive(Debug, Clone)]
pub struct GitHub {
    gh: PathBuf,
    owner: String,
    repo: String,
}

const PR_QUERY: &str = r#"query($owner: String!, $repo: String!, $head: String!) {
  repository(owner: $owner, name: $repo) {
    mergeCommitAllowed
    squashMergeAllowed
    rebaseMergeAllowed
    pullRequests(headRefName: $head, first: 100, states: [OPEN, MERGED, CLOSED],
                 orderBy: {field: CREATED_AT, direction: DESC}) {
      pageInfo { hasNextPage }
      nodes {
        number url title state isDraft baseRefName headRefName mergeable reviewDecision
        headRepository { nameWithOwner }
        reviewThreads(first: 100) {
          pageInfo { hasNextPage endCursor } nodes { isResolved }
        }
        commits(last: 1) { nodes { commit { statusCheckRollup { state contexts(first: 100) {
          pageInfo { hasNextPage endCursor }
          nodes {
            __typename
            ... on CheckRun { name status conclusion detailsUrl }
            ... on StatusContext { context state targetUrl }
          }
        } } } } }
      }
    }
  }
}"#;

const THREADS_QUERY: &str = r#"query($owner: String!, $repo: String!, $number: Int!, $cursor: String!) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      reviewThreads(first: 100, after: $cursor) {
        pageInfo { hasNextPage endCursor } nodes { isResolved }
      }
    }
  }
}"#;

const CHECKS_QUERY: &str = r#"query($owner: String!, $repo: String!, $number: Int!, $cursor: String!) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      commits(last: 1) { nodes { commit { statusCheckRollup {
        contexts(first: 100, after: $cursor) {
          pageInfo { hasNextPage endCursor }
          nodes {
            __typename
            ... on CheckRun { name status conclusion detailsUrl }
            ... on StatusContext { context state targetUrl }
          }
        }
      } } } }
    }
  }
}"#;

/// `gh`, or the path in `FLOTILLA_GH` (tests and unusual installs).
pub fn gh_path() -> PathBuf {
    std::env::var_os("FLOTILLA_GH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("gh"))
}

fn bounded(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr).trim().to_string();
    if text.len() <= MAX_STDERR {
        return text;
    }
    let mut end = MAX_STDERR;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

impl GitHub {
    pub fn new(gh: PathBuf, owner: impl Into<String>, repo: impl Into<String>) -> Self {
        Self {
            gh,
            owner: owner.into(),
            repo: repo.into(),
        }
    }

    pub fn repository(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    fn run(&self, cwd: Option<&Path>, args: &[&str], stdin: Option<&[u8]>) -> Result<Vec<u8>> {
        let mut command = Command::new(&self.gh);
        command
            .args(args)
            .env("GH_PROMPT_DISABLED", "1")
            .env("NO_COLOR", "1")
            .env("GH_NO_UPDATE_NOTIFIER", "1")
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let mut child = command.spawn().map_err(|error| {
            ForgeError::Unavailable(if error.kind() == std::io::ErrorKind::NotFound {
                "the GitHub CLI (gh) is not installed on the server".into()
            } else {
                format!("cannot run gh: {error}")
            })
        })?;
        if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
            // Small payloads (a PR body): written before reading output.
            pipe.write_all(input)
                .map_err(|e| ForgeError::Unavailable(format!("cannot write to gh: {e}")))?;
        }
        let out = child
            .wait_with_output()
            .map_err(|e| ForgeError::Unavailable(format!("gh did not finish: {e}")))?;
        if out.status.success() {
            return Ok(out.stdout);
        }
        let stderr = bounded(&out.stderr);
        if stderr.contains("gh auth login") || stderr.contains("not logged into") {
            return Err(ForgeError::Unavailable(
                "the GitHub CLI on the server is not signed in (run `gh auth login`)".into(),
            ));
        }
        Err(ForgeError::Command {
            args: args.first().copied().unwrap_or_default().to_string(),
            stderr,
        })
    }

    /// The newest pull request for `branch` and the allowed merge methods.
    pub fn fetch(&self, branch: &str) -> Result<(Option<PullRequest>, MergeMethods)> {
        let out = self.run(
            None,
            &[
                "api",
                "graphql",
                "-f",
                &format!("query={PR_QUERY}"),
                "-f",
                &format!("owner={}", self.owner),
                "-f",
                &format!("repo={}", self.repo),
                "-f",
                &format!("head={branch}"),
            ],
            None,
        )?;
        let mut envelope: Envelope =
            serde_json::from_slice(&out).map_err(|e| ForgeError::Parse(e.to_string()))?;
        let repository = self.repository();
        if let Some(repo) = envelope
            .data
            .as_mut()
            .and_then(|data| data.repository.as_mut())
        {
            for pr in &mut repo.pull_requests.nodes {
                if pr.head_ref_name != branch
                    || !pr
                        .head_repository
                        .as_ref()
                        .is_some_and(|head| head.name_with_owner.eq_ignore_ascii_case(&repository))
                {
                    continue;
                }
                if let Some(threads) = pr.review_threads.as_mut() {
                    self.complete_connection(
                        pr.number,
                        THREADS_QUERY,
                        "/data/repository/pullRequest/reviewThreads",
                        threads,
                    )?;
                }
                if let Some(rollup) = pr
                    .commits
                    .as_mut()
                    .and_then(|commits| commits.nodes.first_mut())
                    .and_then(|node| node.commit.status_check_rollup.as_mut())
                {
                    self.complete_connection(pr.number, CHECKS_QUERY,
                        "/data/repository/pullRequest/commits/nodes/0/commit/statusCheckRollup/contexts",
                        &mut rollup.contexts)?;
                }
            }
        }
        project_reply(envelope, Some((&repository, branch)))
    }

    fn complete_connection<T: serde::de::DeserializeOwned>(
        &self,
        number: u64,
        query: &str,
        pointer: &str,
        connection: &mut Nodes<T>,
    ) -> Result<()> {
        while connection
            .page_info
            .as_ref()
            .is_some_and(|page| page.has_next_page)
        {
            let cursor = connection
                .page_info
                .as_ref()
                .and_then(|page| page.end_cursor.as_deref())
                .ok_or_else(|| ForgeError::Parse("missing pagination cursor".into()))?;
            let out = self.run(
                None,
                &[
                    "api",
                    "graphql",
                    "-f",
                    &format!("query={query}"),
                    "-f",
                    &format!("owner={}", self.owner),
                    "-f",
                    &format!("repo={}", self.repo),
                    "-F",
                    &format!("number={number}"),
                    "-f",
                    &format!("cursor={cursor}"),
                ],
                None,
            )?;
            let mut reply: serde_json::Value =
                serde_json::from_slice(&out).map_err(|e| ForgeError::Parse(e.to_string()))?;
            if let Some(message) = reply.pointer("/errors/0/message").and_then(|v| v.as_str()) {
                return Err(ForgeError::Command {
                    args: "api".into(),
                    stderr: message.to_owned(),
                });
            }
            let mut page: Nodes<T> = serde_json::from_value(
                reply
                    .pointer_mut(pointer)
                    .ok_or_else(|| ForgeError::Parse("missing paginated forge connection".into()))?
                    .take(),
            )
            .map_err(|e| ForgeError::Parse(e.to_string()))?;
            if page.page_info.as_ref().is_some_and(|next| {
                next.has_next_page && next.end_cursor.as_deref() == Some(cursor)
            }) {
                return Err(ForgeError::Parse(
                    "forge pagination cursor did not advance".into(),
                ));
            }
            connection.nodes.append(&mut page.nodes);
            connection.page_info = page.page_info;
        }
        Ok(())
    }

    /// Open a pull request from `head` into `base`; returns its URL.
    pub fn create(
        &self,
        cwd: &Path,
        base: &str,
        head: &str,
        title: &str,
        body: &str,
        draft: bool,
    ) -> Result<String> {
        let repo = self.repository();
        let mut args = vec![
            "pr",
            "create",
            "--repo",
            &repo,
            "--base",
            base,
            "--head",
            head,
            "--title",
            title,
            "--body-file",
            "-",
        ];
        if draft {
            args.push("--draft");
        }
        let out = self.run(Some(cwd), &args, Some(body.as_bytes()))?;
        Ok(String::from_utf8_lossy(&out).trim().to_string())
    }

    pub fn mark_ready(&self, number: u64) -> Result<()> {
        let repo = self.repository();
        self.run(
            None,
            &["pr", "ready", &number.to_string(), "--repo", &repo],
            None,
        )?;
        Ok(())
    }

    pub fn merge(&self, number: u64, method: MergeMethod) -> Result<()> {
        let repo = self.repository();
        let flag = match method {
            MergeMethod::Squash => "--squash",
            MergeMethod::Merge => "--merge",
            MergeMethod::Rebase => "--rebase",
        };
        self.run(
            None,
            &["pr", "merge", &number.to_string(), "--repo", &repo, flag],
            None,
        )?;
        Ok(())
    }
}

#[derive(Deserialize)]
struct Envelope {
    data: Option<Data>,
    #[serde(default)]
    errors: Vec<GraphQlError>,
}

#[derive(Deserialize)]
struct GraphQlError {
    message: String,
}

#[derive(Deserialize)]
struct Data {
    repository: Option<Repository>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Repository {
    merge_commit_allowed: bool,
    squash_merge_allowed: bool,
    rebase_merge_allowed: bool,
    pull_requests: Nodes<Pr>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Nodes<T> {
    nodes: Vec<T>,
    page_info: Option<PageInfo>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HeadRepository {
    name_with_owner: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Pr {
    number: u64,
    url: String,
    title: String,
    state: String,
    is_draft: bool,
    base_ref_name: String,
    head_ref_name: String,
    head_repository: Option<HeadRepository>,
    mergeable: Option<String>,
    review_decision: Option<String>,
    review_threads: Option<Nodes<Thread>>,
    commits: Option<Nodes<CommitNode>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Thread {
    is_resolved: bool,
}

#[derive(Deserialize)]
struct CommitNode {
    commit: CommitRollup,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommitRollup {
    status_check_rollup: Option<Rollup>,
}

#[derive(Deserialize)]
struct Rollup {
    contexts: Nodes<RollupItem>,
    state: String,
}

#[derive(Deserialize)]
#[serde(tag = "__typename")]
/// One entry of a status-check rollup; variants are GraphQL type names.
enum RollupItem {
    CheckRun {
        name: String,
        status: String,
        conclusion: Option<String>,
        #[serde(rename = "detailsUrl")]
        details_url: Option<String>,
    },
    StatusContext {
        context: String,
        state: String,
        #[serde(rename = "targetUrl")]
        target_url: Option<String>,
    },
    #[serde(other)]
    Other,
}

fn check_run_status(status: &str, conclusion: Option<&str>) -> CheckStatus {
    if status != "COMPLETED" {
        return CheckStatus::Pending;
    }
    match conclusion {
        Some("SUCCESS") => CheckStatus::Success,
        Some("FAILURE" | "TIMED_OUT" | "STARTUP_FAILURE" | "ACTION_REQUIRED") => {
            CheckStatus::Failure
        }
        _ => CheckStatus::Neutral,
    }
}

fn status_context_status(state: &str) -> CheckStatus {
    match state {
        "SUCCESS" => CheckStatus::Success,
        "FAILURE" | "ERROR" => CheckStatus::Failure,
        "PENDING" | "EXPECTED" => CheckStatus::Pending,
        _ => CheckStatus::Neutral,
    }
}

/// Parse the reply of [`PR_QUERY`].
pub fn parse_pull_request(json: &[u8]) -> Result<(Option<PullRequest>, MergeMethods)> {
    parse_reply(json, None)
}

fn parse_reply(
    json: &[u8],
    scope: Option<(&str, &str)>,
) -> Result<(Option<PullRequest>, MergeMethods)> {
    let envelope: Envelope =
        serde_json::from_slice(json).map_err(|e| ForgeError::Parse(e.to_string()))?;
    project_reply(envelope, scope)
}

fn project_reply(
    envelope: Envelope,
    scope: Option<(&str, &str)>,
) -> Result<(Option<PullRequest>, MergeMethods)> {
    if let Some(error) = envelope.errors.first() {
        return Err(ForgeError::Command {
            args: "api".into(),
            stderr: error.message.clone(),
        });
    }
    let repository = envelope
        .data
        .and_then(|data| data.repository)
        .ok_or_else(|| ForgeError::Unavailable("the repository is not visible on GitHub".into()))?;
    let methods = MergeMethods {
        squash: repository.squash_merge_allowed,
        merge: repository.merge_commit_allowed,
        rebase: repository.rebase_merge_allowed,
    };
    let mut candidates = repository.pull_requests.nodes.into_iter().filter(|pr| {
        scope.is_none_or(|(repo, branch)| {
            pr.head_ref_name == branch
                && pr
                    .head_repository
                    .as_ref()
                    .is_some_and(|head| head.name_with_owner.eq_ignore_ascii_case(repo))
        })
    });
    let first = candidates.next();
    let selected = match first {
        Some(pr) if pr.state == "OPEN" => Some(pr),
        fallback => candidates.find(|pr| pr.state == "OPEN").or(fallback),
    };
    if scope.is_some()
        && selected.as_ref().is_none_or(|pr| pr.state != "OPEN")
        && repository
            .pull_requests
            .page_info
            .is_some_and(|page| page.has_next_page)
    {
        return Err(ForgeError::Unavailable(
            "too many pull requests share this branch name to resolve it safely".into(),
        ));
    }
    let Some(pr) = selected else {
        return Ok((None, methods));
    };
    let state = match pr.state.as_str() {
        "OPEN" => PrState::Open,
        "MERGED" => PrState::Merged,
        "CLOSED" => PrState::Closed,
        other => return Err(ForgeError::Parse(format!("pull request state {other}"))),
    };
    let mergeable = match pr.mergeable.as_deref() {
        Some("MERGEABLE") => Mergeable::Mergeable,
        Some("CONFLICTING") => Mergeable::Conflicting,
        _ => Mergeable::Unknown,
    };
    let review = match pr.review_decision.as_deref() {
        Some("APPROVED") => Some(ReviewDecision::Approved),
        Some("CHANGES_REQUESTED") => Some(ReviewDecision::ChangesRequested),
        Some("REVIEW_REQUIRED") => Some(ReviewDecision::ReviewRequired),
        _ => None,
    };
    let rollup = pr
        .commits
        .and_then(|commits| commits.nodes.into_iter().next())
        .and_then(|node| node.commit.status_check_rollup);
    let check_rollup = rollup
        .as_ref()
        .map(|rollup| status_context_status(&rollup.state));
    let checks = rollup
        .map(|rollup| rollup.contexts.nodes)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|context| match context {
            RollupItem::CheckRun {
                name,
                status,
                conclusion,
                details_url,
            } => Some(Check {
                status: check_run_status(&status, conclusion.as_deref()),
                name,
                url: details_url,
            }),
            RollupItem::StatusContext {
                context,
                state,
                target_url,
            } => Some(Check {
                name: context,
                status: status_context_status(&state),
                url: target_url,
            }),
            RollupItem::Other => None,
        })
        .collect();
    let unresolved_threads = pr
        .review_threads
        .map(|threads| threads.nodes.iter().filter(|t| !t.is_resolved).count() as u32)
        .unwrap_or(0);
    Ok((
        Some(PullRequest {
            number: pr.number,
            url: pr.url,
            title: pr.title,
            state,
            draft: pr.is_draft,
            base: pr.base_ref_name,
            head: pr.head_ref_name,
            mergeable,
            review,
            checks,
            check_rollup,
            unresolved_threads,
        }),
        methods,
    ))
}

#[cfg(test)]
mod selection_tests {
    use super::*;
    use serde_json::{Value, json};

    fn candidate(number: u64, repository: &str, state: &str) -> Value {
        json!({
            "number": number,
            "url": format!("https://github.com/acme/widgets/pull/{number}"),
            "title": "Example", "state": state, "isDraft": false,
            "baseRefName": "main", "headRefName": "feature",
            "headRepository": {"nameWithOwner": repository},
            "mergeable": "MERGEABLE"
        })
    }

    fn reply(nodes: Vec<Value>, more: bool) -> Vec<u8> {
        serde_json::to_vec(&json!({"data":{"repository":{
            "mergeCommitAllowed": true, "squashMergeAllowed": true,
            "rebaseMergeAllowed": false, "pullRequests":{
                "nodes":nodes, "pageInfo":{"hasNextPage":more}
            }
        }}}))
        .unwrap()
    }

    #[test]
    fn selects_the_origins_open_pr_not_a_fork_or_newer_closed_pr() {
        let bytes = reply(
            vec![
                candidate(9, "contributor/widgets", "OPEN"),
                candidate(8, "acme/widgets", "MERGED"),
                candidate(7, "Acme/Widgets", "OPEN"),
            ],
            false,
        );
        let (pr, _) = parse_reply(&bytes, Some(("acme/widgets", "feature"))).unwrap();
        assert_eq!(pr.unwrap().number, 7);
        let (pr, _) = parse_reply(&bytes, Some(("acme/widgets", "unrelated"))).unwrap();
        assert!(pr.is_none());
    }

    #[test]
    fn missing_ownership_and_incomplete_history_never_authorize_a_mutation() {
        let mut unknown = candidate(9, "acme/widgets", "OPEN");
        unknown.as_object_mut().unwrap().remove("headRepository");
        let (pr, _) = parse_reply(
            &reply(vec![unknown], false),
            Some(("acme/widgets", "feature")),
        )
        .unwrap();
        assert!(pr.is_none());
        assert!(matches!(
            parse_reply(
                &reply(vec![candidate(8, "acme/widgets", "MERGED")], true),
                Some(("acme/widgets", "feature"))
            ),
            Err(ForgeError::Unavailable(_))
        ));
    }

    #[test]
    fn later_check_and_thread_pages_are_included_before_publishing() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let mut pr = candidate(7, "acme/widgets", "OPEN");
        pr["reviewThreads"] = json!({
            "nodes":[{"isResolved":true}],
            "pageInfo":{"hasNextPage":true,"endCursor":"threads-1"}
        });
        pr["commits"] = json!({"nodes":[{"commit":{"statusCheckRollup":{
            "state":"FAILURE", "contexts":{
                "nodes":[{"__typename":"CheckRun","name":"first","status":"COMPLETED","conclusion":"SUCCESS"}],
                "pageInfo":{"hasNextPage":true,"endCursor":"checks-1"}
            }
        }}}]});
        std::fs::write(dir.path().join("initial.json"), reply(vec![pr], false)).unwrap();
        std::fs::write(
            dir.path().join("threads.json"),
            json!({"data":{"repository":{"pullRequest":{
                "reviewThreads":{"nodes":[{"isResolved":false}],
                    "pageInfo":{"hasNextPage":false,"endCursor":"threads-2"}}
            }}}})
            .to_string(),
        )
        .unwrap();
        std::fs::write(dir.path().join("checks.json"), json!({"data":{"repository":{"pullRequest":{
            "commits":{"nodes":[{"commit":{"statusCheckRollup":{"contexts":{
                "nodes":[{"__typename":"CheckRun","name":"last","status":"COMPLETED","conclusion":"FAILURE"}],
                "pageInfo":{"hasNextPage":false,"endCursor":"checks-2"}
            }}}}]}
        }}}}).to_string()).unwrap();
        let script = dir.path().join("gh");
        std::fs::write(&script, format!(
            "#!/bin/sh\ncase \"$*\" in\n*cursor=threads-1*) cat '{0}/threads.json';;\n*cursor=checks-1*) cat '{0}/checks.json';;\n*) cat '{0}/initial.json';;\nesac\n",
            dir.path().display()
        )).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let (pr, _) = GitHub::new(script, "acme", "widgets")
            .fetch("feature")
            .unwrap();
        let pr = pr.unwrap();
        assert_eq!(pr.unresolved_threads, 1);
        assert_eq!(pr.checks_status(), Some(CheckStatus::Failure));
        assert_eq!(
            pr.checks
                .iter()
                .map(|check| (check.name.as_str(), check.status))
                .collect::<Vec<_>>(),
            vec![
                ("first", CheckStatus::Success),
                ("last", CheckStatus::Failure)
            ]
        );
    }
}
