#![cfg(unix)]
//! Deterministic transport tests. These children simulate a protocol peer, not a
//! model; real-provider initialization recordings live in fixtures/.
use fld_agent::*;
use std::time::Duration;

fn config(cwd: &std::path::Path) -> SessionConfig {
    SessionConfig {
        cwd: cwd.into(),
        model: None,
        permission_mode: PermissionMode::Ask,
        instructions: None,
        session_id: None,
        provider_config_json: None,
        output_schema_json: None,
        resume: None,
    }
}
async fn peer(body: &str) -> (tempfile::TempDir, ProviderSpec) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peer.sh");
    let script = format!(
        "IFS= read -r init\nprintf '%s\\n' '{{\"type\":\"control_response\",\"response\":{{\"subtype\":\"success\",\"request_id\":\"1\",\"response\":{{}}}}}}'\n{body}\n"
    );
    tokio::fs::write(&path, script).await.unwrap();
    let mut provider = ProviderRegistry::default().get("claude").unwrap().clone();
    provider.command = "/bin/sh".into();
    provider.args = vec![path.to_str().unwrap().into()];
    (dir, provider)
}
async fn failed(adapter: &Adapter) -> SessionState {
    let mut state = adapter.watch_state();
    tokio::time::timeout(
        Duration::from_secs(5),
        state.wait_for(|state| matches!(state, SessionState::Failed { .. })),
    )
    .await
    .unwrap()
    .unwrap()
    .clone()
}

#[tokio::test]
async fn shutdown_closes_stdin_and_reaps_the_child() {
    let (dir, provider) = peer("while IFS= read -r line; do :; done").await;
    let adapter = Adapter::launch(provider, config(dir.path())).await.unwrap();
    assert_eq!(adapter.state(), SessionState::Idle);
    adapter.shutdown().await.unwrap();
    assert_eq!(adapter.state(), SessionState::Closed);
}

#[tokio::test]
async fn malformed_output_is_fatal_and_observable() {
    let (dir, provider) =
        peer("IFS= read -r prompt\nprintf '%s\\n' '{broken'\nwhile IFS= read -r line; do :; done")
            .await;
    let mut adapter = Adapter::launch(provider, config(dir.path())).await.unwrap();
    adapter.send("hello").await.unwrap();
    assert!(
        matches!(failed(&adapter).await,SessionState::Failed { message } if message.contains("malformed provider JSON"))
    );
    let mut fatal = false;
    while let Some(record) = adapter.events.recv().await {
        fatal |= matches!(record.event, ChatEvent::Error { fatal: true, .. });
    }
    assert!(fatal);
}

#[tokio::test]
async fn oversized_line_is_rejected_without_unbounded_buffering() {
    let (dir, provider) =
        peer("IFS= read -r prompt\nprintf '%01048578d\\n' 0\nwhile IFS= read -r line; do :; done")
            .await;
    let adapter = Adapter::launch(provider, config(dir.path())).await.unwrap();
    adapter.send("hello").await.unwrap();
    assert!(matches!(
        failed(&adapter).await,
        SessionState::Failed { .. }
    ));
}

#[tokio::test]
async fn consumer_overflow_is_explicit_even_when_no_event_slot_remains() {
    let (dir,provider) = peer("IFS= read -r prompt\ni=0\nwhile [ \"$i\" -lt 300 ]; do printf '%s\\n' '{\"type\":\"unknown_for_backpressure_test\"}'; i=$((i+1)); done\nwhile IFS= read -r line; do :; done").await;
    let adapter = Adapter::launch(provider, config(dir.path())).await.unwrap();
    adapter.send("hello").await.unwrap();
    assert!(
        matches!(failed(&adapter).await,SessionState::Failed { message } if message.contains("consumer overflowed"))
    );
}

#[tokio::test]
async fn unexpected_process_exit_does_not_declare_a_turn_complete() {
    let (dir, provider) = peer("IFS= read -r prompt\nexit 17").await;
    let mut adapter = Adapter::launch(provider, config(dir.path())).await.unwrap();
    adapter.send("hello").await.unwrap();
    failed(&adapter).await;
    let mut exited = false;
    while let Some(record) = adapter.events.recv().await {
        assert!(!matches!(
            record.event,
            ChatEvent::TurnEnd {
                outcome: TurnOutcome::Completed,
                ..
            }
        ));
        if let ChatEvent::ProcessExited { code, expected } = record.event {
            assert_eq!(code, Some(17));
            assert!(!expected);
            exited = true;
        }
    }
    assert!(exited);
}

#[tokio::test]
async fn resume_does_not_silently_create_a_fresh_session() {
    let (dir, provider) = peer("exit 99").await;
    let mut config = config(dir.path());
    config.resume = Some(ResumeTarget {
        provider_id: "claude".into(),
        adapter: AdapterKind::ClaudeStream,
        native_id: uuid::Uuid::new_v4().to_string(),
        cwd: dir.path().into(),
    });
    assert!(matches!(
        Adapter::launch(provider, config).await,
        Err(AdapterError::InvalidResume(_))
    ));
}
