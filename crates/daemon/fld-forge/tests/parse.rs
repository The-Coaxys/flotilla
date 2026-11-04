//! Replies recorded from GitHub (and one hand-built from the same schema
//! for failure states) parse into the forge model.
use fl_model::forge::{CheckStatus, MergeMethod, Mergeable, PrState, ReviewDecision};
use fld_forge::{ForgeError, parse_pull_request};

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

#[test]
fn an_open_pull_request_with_running_checks() {
    let (pr, methods) = parse_pull_request(&fixture("pr_open_pending.json")).unwrap();
    let pr = pr.expect("a pull request");
    assert_eq!(pr.number, 3);
    assert_eq!(pr.state, PrState::Open);
    assert!(!pr.draft);
    assert_eq!(
        (pr.base.as_str(), pr.head.as_str()),
        ("main", "m2-workspaces")
    );
    assert_eq!(pr.mergeable, Mergeable::Mergeable);
    assert!(!pr.checks.is_empty());
    assert_eq!(pr.checks_status(), Some(CheckStatus::Pending));
    assert!(pr.checks.iter().all(|c| c.url.is_some()));
    assert_eq!(methods.preferred(), Some(MergeMethod::Squash));
}

#[test]
fn a_merged_pull_request() {
    let (pr, _) = parse_pull_request(&fixture("pr_merged.json")).unwrap();
    let pr = pr.unwrap();
    assert_eq!(pr.number, 1);
    assert_eq!(pr.state, PrState::Merged);
    assert_eq!(pr.checks_status(), Some(CheckStatus::Success));
}

#[test]
fn no_pull_request_for_the_branch() {
    let (pr, methods) = parse_pull_request(&fixture("pr_none.json")).unwrap();
    assert!(pr.is_none());
    assert!(methods.squash && methods.merge && methods.rebase);
}

#[test]
fn failures_conflicts_reviews_and_threads() {
    let (pr, methods) = parse_pull_request(&fixture("pr_failing.json")).unwrap();
    let pr = pr.unwrap();
    assert!(pr.draft);
    assert_eq!(pr.mergeable, Mergeable::Conflicting);
    assert_eq!(pr.review, Some(ReviewDecision::ChangesRequested));
    assert_eq!(pr.unresolved_threads, 2);
    let status = |name: &str| pr.checks.iter().find(|c| c.name == name).unwrap().status;
    assert_eq!(status("test"), CheckStatus::Failure);
    assert_eq!(status("lint"), CheckStatus::Success);
    assert_eq!(status("docs"), CheckStatus::Neutral);
    assert_eq!(status("build"), CheckStatus::Pending);
    assert_eq!(status("ci/legacy"), CheckStatus::Failure);
    assert_eq!(status("deploy/preview"), CheckStatus::Pending);
    assert_eq!(pr.checks_status(), Some(CheckStatus::Failure));
    // Only squash is allowed here.
    assert_eq!(methods.preferred(), Some(MergeMethod::Squash));
    assert!(!methods.allows(MergeMethod::Merge));
}

#[test]
fn graphql_errors_and_missing_repositories_are_reported() {
    let errors = br#"{"data":null,"errors":[{"message":"Could not resolve to a Repository"}]}"#;
    assert!(matches!(
        parse_pull_request(errors),
        Err(ForgeError::Command { stderr, .. }) if stderr.contains("Could not resolve")
    ));
    let missing = br#"{"data":{"repository":null}}"#;
    assert!(matches!(
        parse_pull_request(missing),
        Err(ForgeError::Unavailable(_))
    ));
    assert!(matches!(
        parse_pull_request(b"not json"),
        Err(ForgeError::Parse(_))
    ));
}
