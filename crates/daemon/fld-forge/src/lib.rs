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
    pullRequests(headRefName: $head, first: 1, states: [OPEN, MERGED, CLOSED],
                 orderBy: {field: CREATED_AT, direction: DESC}) {
      nodes {
        number url title state isDraft baseRefName headRefName mergeable reviewDecision
        reviewThreads(first: 100) { nodes { isResolved } }
        commits(last: 1) { nodes { commit { statusCheckRollup { state contexts(first: 100) {
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
        parse_pull_request(&out)
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
struct Nodes<T> {
    nodes: Vec<T>,
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
    let envelope: Envelope =
        serde_json::from_slice(json).map_err(|e| ForgeError::Parse(e.to_string()))?;
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
    let Some(pr) = repository.pull_requests.nodes.into_iter().next() else {
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
    let checks = pr
        .commits
        .and_then(|commits| commits.nodes.into_iter().next())
        .and_then(|node| node.commit.status_check_rollup)
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
            unresolved_threads,
        }),
        methods,
    ))
}
