#![cfg(unix)]
//! End-to-end: a real daemon on a temporary FLOTILLA_HOME, representative
//! commands through the built `fl` binary, and durable assertions on state.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::time::Duration;

use fl_model::{ClientSessionId, ProjectId, WorkspaceId};
use fl_proto::{ClientMsg, Hello, PROTO, Request, Response, ServerMsg};
use fl_transport::conn::{FrameReader, FrameWriter, framed};
use fld_server::{DaemonConfig, DaemonHandle, FlotillaHome, LaunchDefaults};
use serde_json::Value;
use tokio::net::UnixStream;

const WAIT: Duration = Duration::from_secs(30);

fn git(dir: &Path, args: &[&str]) {
    let status = StdCommand::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "fl-test")
        .env("GIT_AUTHOR_EMAIL", "fl@example.com")
        .env("GIT_COMMITTER_NAME", "fl-test")
        .env("GIT_COMMITTER_EMAIL", "fl@example.com")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

async fn ensure_prochost(root: &Path) {
    if fld_prochost::Client::connect(root).await.is_ok() {
        return;
    }
    std::fs::create_dir_all(root.join("run")).unwrap();
    tokio::spawn(fld_prochost::serve(root.to_path_buf()));
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match fld_prochost::Client::connect(root).await {
                Ok(_) => break,
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    })
    .await
    .expect("process host starts");
}

struct Control {
    reader: FrameReader,
    writer: FrameWriter,
    next: u64,
}

impl Control {
    async fn connect(socket: &Path) -> Self {
        let stream = UnixStream::connect(socket).await.unwrap();
        let (read, write) = stream.into_split();
        let (mut reader, mut writer) = framed(read, write);
        writer
            .send(&ClientMsg::Hello(Hello {
                client_version: "fl-integration".into(),
                proto: PROTO,
                device_id: None,
                token: None,
                client_session_id: *ClientSessionId::new().as_bytes(),
            }))
            .await
            .unwrap();
        let ServerMsg::Welcome(_) = reader.recv::<ServerMsg>().await.unwrap() else {
            panic!("expected welcome");
        };
        Self {
            reader,
            writer,
            next: 0,
        }
    }

    async fn rpc(&mut self, request: Request) -> Response {
        self.next += 1;
        let id = self.next;
        self.writer
            .send(&ClientMsg::Rpc { id, req: request })
            .await
            .unwrap();
        loop {
            match self.reader.recv::<ServerMsg>().await.unwrap() {
                ServerMsg::RpcResult { id: got, result } if got == id => {
                    return result.unwrap_or_else(|error| panic!("{error:?}"));
                }
                ServerMsg::Event(_) => continue,
                other => panic!("unexpected server message: {other:?}"),
            }
        }
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    daemon: Option<DaemonHandle>,
    config: DaemonConfig,
    repo: PathBuf,
    workspace: WorkspaceId,
    project: ProjectId,
    worktree: fl_model::WorktreeId,
}

impl Fixture {
    async fn new() -> Self {
        Self::new_with(None).await
    }

    async fn new_with(providers: Option<fld_agent::ProviderRegistry>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = FlotillaHome::new(dir.path().join(".flotilla"));
        home.ensure_dirs().unwrap();
        ensure_prochost(home.root()).await;
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("README.md"), "one\ntwo\nthree\n").unwrap();
        std::fs::create_dir(repo.join(".flotilla")).unwrap();
        std::fs::write(
            repo.join(".flotilla/config.json"),
            r#"{
                "setup": "true",
                "run": {
                    "test": ["true"],
                    "web": { "command": "sleep 1", "default": false }
                },
                "teardown": "true"
            }"#,
        )
        .unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "initial"]);

        let mut config = DaemonConfig::new(home);
        config.quic_bind = None;
        config.prochost_executable = None;
        config.launch = LaunchDefaults {
            cwd: repo.display().to_string(),
            argv: vec!["/bin/sh".into()],
            env: BTreeMap::from([("TERM".into(), "xterm-256color".into())]),
            quota_bytes: fl_prochost_proto::DEFAULT_QUOTA_BYTES,
        };
        config.providers = providers;
        let start_config = config.clone();
        let daemon = tokio::time::timeout(WAIT, async {
            loop {
                match fld_server::start(config.clone()).await {
                    Ok(daemon) => break daemon,
                    Err(fld_server::ServerError::AlreadyRunning(_)) => {
                        tokio::time::sleep(Duration::from_millis(20)).await
                    }
                    Err(error) => panic!("daemon start: {error}"),
                }
            }
        })
        .await
        .expect("daemon starts");

        let mut control = Control::connect(&daemon.socket_path).await;
        let tree = match control.rpc(Request::GetWorkspaceTree).await {
            Response::WorkspaceTree(tree) => tree,
            other => panic!("unexpected {other:?}"),
        };
        let workspace = tree.workspaces[0].id;
        let project = match control
            .rpc(Request::AddProject {
                workspace,
                path: repo.display().to_string(),
            })
            .await
        {
            Response::Project(project) => project,
            other => panic!("unexpected {other:?}"),
        };
        let tree = match control.rpc(Request::GetWorkspaceTree).await {
            Response::WorkspaceTree(tree) => tree,
            other => panic!("unexpected {other:?}"),
        };
        let worktree = tree
            .worktrees
            .iter()
            .find(|worktree| worktree.project == project.id && worktree.is_primary)
            .expect("primary worktree")
            .id;
        Self {
            dir,
            daemon: Some(daemon),
            config: start_config,
            repo,
            workspace,
            project: project.id,
            worktree,
        }
    }

    fn home(&self) -> &FlotillaHome {
        &self.config.home
    }

    /// Stop and restart the daemon on the same home, as a VPS reboot would.
    async fn restart(&mut self) {
        if let Some(daemon) = self.daemon.take() {
            daemon.shutdown().await;
        }
        let config = self.config.clone();
        let daemon = tokio::time::timeout(WAIT, async {
            loop {
                match fld_server::start(config.clone()).await {
                    Ok(daemon) => break daemon,
                    Err(fld_server::ServerError::AlreadyRunning(_)) => {
                        tokio::time::sleep(Duration::from_millis(20)).await
                    }
                    Err(error) => panic!("daemon restart: {error}"),
                }
            }
        })
        .await
        .expect("daemon restarts");
        self.daemon = Some(daemon);
    }

    async fn fl(&self, args: &[&str]) -> std::process::Output {
        tokio::process::Command::new(env!("CARGO_BIN_EXE_fl"))
            .args(args)
            .env("FLOTILLA_HOME", self.home().root())
            .env_remove("FLOTILLA_SESSION_ID")
            .env_remove("FLOTILLA_SOCKET")
            .current_dir(&self.repo)
            .output()
            .await
            .expect("run fl")
    }

    async fn fl_json(&self, args: &[&str]) -> Value {
        let output = self.fl(args).await;
        assert!(
            output.status.success(),
            "fl {args:?} failed\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "fl {args:?} did not print JSON: {error}\n{}",
                String::from_utf8_lossy(&output.stdout)
            )
        })
    }

    async fn shutdown(mut self) {
        if let Some(daemon) = self.daemon.take() {
            daemon.shutdown().await;
        }
        drop(self.dir);
    }
}

fn string(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("missing string `{key}` in {value}"))
        .to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commands_round_trip_with_durable_effects() {
    let fixture = Fixture::new().await;

    let status = fixture.fl_json(&["status", "--json"]).await;
    assert!(status["daemon"]["socket_path"].is_string());
    assert_eq!(
        string(&status["caller"]["worktree"], "id"),
        fixture.worktree.to_string()
    );
    assert!(status["selected_workspace"].is_null());

    let workspaces = fixture.fl_json(&["workspace", "list", "--json"]).await;
    assert_eq!(workspaces["workspaces"].as_array().unwrap().len(), 1);
    assert_eq!(
        string(&workspaces["workspaces"][0], "id"),
        fixture.workspace.to_string()
    );

    let workspace_id = fixture.workspace.to_string();
    let selected = fixture
        .fl_json(&["workspace", "select", workspace_id.as_str(), "--json"])
        .await;
    assert_eq!(
        string(&selected["selected"], "id"),
        fixture.workspace.to_string()
    );
    let got = fixture.fl_json(&["workspace", "get", "--json"]).await;
    assert_eq!(
        string(&got["workspace"], "id"),
        fixture.workspace.to_string()
    );
    assert_eq!(
        got["projects"][0]["id"],
        serde_json::json!(fixture.project.to_string())
    );

    let sections = fixture.fl_json(&["section", "list", "--json"]).await;
    assert!(sections["rev"].as_u64().unwrap() >= 1);

    let tree = fixture.fl_json(&["worktree", "status", "--json"]).await;
    assert_eq!(
        string(&tree["worktree"], "id"),
        fixture.worktree.to_string()
    );
    assert_eq!(tree["status"]["summary"]["branch"], "main");

    let labelled = fixture
        .fl_json(&["worktree", "set-label", "Renamed", "--json"])
        .await;
    assert_eq!(labelled["worktree"]["label"], "Renamed");
    let tree = fixture.fl_json(&["worktree", "status", "--json"]).await;
    assert_eq!(tree["worktree"]["label"], "Renamed");

    let targeted = fixture
        .fl_json(&["worktree", "set-target-branch", "main", "--json"])
        .await;
    assert_eq!(targeted["worktree"]["target"], "main");

    let renamed = fixture
        .fl_json(&["worktree", "rename-branch", "feature/cli-test", "--json"])
        .await;
    assert_eq!(renamed["worktree"]["branch"], "feature/cli-test");
    let branch = StdCommand::new("git")
        .arg("-C")
        .arg(&fixture.repo)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&branch.stdout).trim(),
        "feature/cli-test"
    );

    let added = fixture
        .fl_json(&[
            "worktree",
            "review-add",
            "--path",
            "README.md",
            "--start",
            "1",
            "--end",
            "1",
            "--body",
            "needs work",
            "--json",
        ])
        .await;
    let thread = string(&added["thread"], "id");
    assert_eq!(added["review"]["threads"].as_array().unwrap().len(), 1);
    assert_eq!(added["thread"]["status"], "Pending");

    let listed = fixture
        .fl_json(&["worktree", "review-list", "--json"])
        .await;
    assert_eq!(listed["review"]["threads"].as_array().unwrap().len(), 1);

    let fetched = fixture
        .fl_json(&["worktree", "review-get", &thread, "--json"])
        .await;
    assert_eq!(string(&fetched["thread"], "id"), thread);

    let replied = fixture
        .fl_json(&[
            "worktree",
            "review-reply",
            &thread,
            "--body",
            "acknowledged",
            "--json",
        ])
        .await;
    assert_eq!(replied["thread"]["comments"].as_array().unwrap().len(), 2);

    let resolved = fixture
        .fl_json(&[
            "worktree",
            "review-set-status",
            &thread,
            "resolved",
            "--json",
        ])
        .await;
    assert_eq!(resolved["thread"]["status"], "Resolved");

    let check = fixture
        .fl_json(&["project", "scripts", "check", "--json"])
        .await;
    assert!(check["check"]["error"].is_null());
    assert_eq!(check["check"]["setup"], true);
    assert_eq!(check["check"]["teardown"], true);
    let runs: Vec<&str> = check["check"]["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();
    assert_eq!(runs, vec!["test", "web"]);

    let views = fixture.fl_json(&["layout", "views", "--json"]).await;
    let base_rev = views["layout"]["rev"].as_u64().unwrap();
    let view = string(&views["layout"]["views"][0], "id");
    let ops = serde_json::json!([{
        "NewTab": {
            "view": view,
            "tab": uuid::Uuid::new_v4(),
            "pane": uuid::Uuid::new_v4(),
            "terminal": uuid::Uuid::new_v4(),
            "name": "composed",
            "size": { "columns": 120, "rows": 32, "cell_width": 0, "cell_height": 0 }
        }
    }]);
    let ops_path = fixture.dir.path().join("ops.json");
    std::fs::write(&ops_path, serde_json::to_vec(&ops).unwrap()).unwrap();
    let composed = fixture
        .fl_json(&[
            "layout",
            "compose",
            "--dry-run",
            "--file",
            ops_path.to_str().unwrap(),
            "--json",
        ])
        .await;
    assert_eq!(composed["dry_run"], true);
    assert_eq!(composed["base_rev"].as_u64().unwrap(), base_rev);
    assert_eq!(composed["final_rev"].as_u64().unwrap(), base_rev + 1);
    assert_eq!(composed["spawn"].as_array().unwrap().len(), 1);
    assert_eq!(composed["ops"].as_array().unwrap().len(), 1);
    let after = fixture.fl_json(&["layout", "views", "--json"]).await;
    assert_eq!(after["layout"]["rev"].as_u64().unwrap(), base_rev);

    let history = fixture.fl_json(&["history", "list", "--json"]).await;
    assert!(history["sessions"].as_array().unwrap().is_empty());

    let refusal = fixture
        .fl(&[
            "worktree",
            "review-get",
            "00000000-0000-0000-0000-000000000001",
            "--json",
        ])
        .await;
    assert!(!refusal.status.success());
    let error: Value = serde_json::from_slice(&refusal.stdout).unwrap();
    assert_eq!(error["error"]["code"], "not_found");

    let missing = fixture
        .fl(&["workspace", "get", "no-such-workspace", "--json"])
        .await;
    assert!(!missing.status.success());
    let error: Value = serde_json::from_slice(&missing.stdout).unwrap();
    assert_eq!(error["error"]["code"], "not_found");

    let help = fixture.fl(&["help"]).await;
    assert!(help.status.success());
    let help_text = String::from_utf8_lossy(&help.stdout);
    for command in [
        "status",
        "workspace",
        "section",
        "worktree",
        "project",
        "tab",
        "layout",
        "chat",
        "history",
        "agent",
    ] {
        assert!(help_text.contains(command), "root help omits {command}");
    }
    let help = fixture.fl(&["help", "worktree"]).await;
    assert!(help.status.success());
    let help_text = String::from_utf8_lossy(&help.stdout);
    for command in [
        "review-list",
        "review-get",
        "review-add",
        "review-reply",
        "review-set-status",
        "rename-branch",
        "set-label",
        "set-target-branch",
    ] {
        assert!(help_text.contains(command), "worktree help omits {command}");
    }

    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreachable_daemon_is_a_nonzero_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let home = FlotillaHome::new(dir.path().join(".flotilla"));
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_fl"))
        .args(["status", "--json"])
        .env("FLOTILLA_HOME", home.root())
        .env_remove("FLOTILLA_SESSION_ID")
        .env_remove("FLOTILLA_SOCKET")
        .current_dir(dir.path())
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    let error: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(error["error"]["code"], "unavailable");
}

/// A scripted `claude` CLI: its help and initialize handshake match the real
/// adapter probe, and each user message gets one completed turn.
const STUB_PROVIDER: &str = r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  echo "stub-claude 1.0.0"
  exit 0
fi
if [ "$1" = "--help" ]; then
  echo "--input-format --output-format --include-partial-messages --resume --session-id manual"
  exit 0
fi
sid=""
previous=""
for argument in "$@"; do
  if [ "$previous" = "--session-id" ] || [ "$previous" = "--resume" ]; then
    sid="$argument"
  fi
  previous="$argument"
done
if [ -z "$sid" ]; then
  sid="00000000-0000-0000-0000-000000000000"
fi
while IFS= read -r line; do
  case "$line" in
    *'"subtype":"initialize"'*)
      printf '{"type":"control_response","response":{"subtype":"success","request_id":"1","response":{}}}\n'
      ;;
    *'"type":"user"'*)
      printf '{"type":"result","subtype":"success","is_error":false,"result":"stub answer","session_id":"%s","duration_ms":1,"num_turns":1,"total_cost_usd":0,"usage":{}}\n' "$sid"
      ;;
  esac
done
"#;

fn stub_provider(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("claude-stub");
    std::fs::write(&path, STUB_PROVIDER).unwrap();
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions).unwrap();
    path
}

/// Two distinct provider entries that both speak the Claude stream protocol.
fn stub_registry(stub: &Path) -> fld_agent::ProviderRegistry {
    let mut registry = fld_agent::ProviderRegistry::default();
    for id in ["claude", "codex"] {
        let mut spec = registry.get("claude").unwrap().clone();
        spec.id = id.into();
        spec.name = if id == "claude" {
            "Stub Claude"
        } else {
            "Stub Codex"
        }
        .into();
        spec.command = stub.to_path_buf();
        spec.args.clear();
        registry.override_provider(spec).unwrap();
    }
    registry
}

async fn wait_for_team(fixture: &Fixture) -> Value {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let value = fixture.fl_json(&["team", "status", "--json"]).await;
        let state = value["run"]["state"].as_str().unwrap_or_default();
        if state == "Finished" || state == "Failed" {
            return value;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "team run never settled: {value}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn team_runs_coordination_locks_hooks_and_instructions() {
    let stub_dir = tempfile::tempdir().unwrap();
    let stub = stub_provider(stub_dir.path());
    let mut fixture = Fixture::new_with(Some(stub_registry(&stub))).await;

    // --- instructions: embedded, no daemon needed ------------------------
    let guide = fixture.fl(&["instructions", "orchestration"]).await;
    assert!(guide.status.success());
    let text = String::from_utf8_lossy(&guide.stdout);
    assert!(
        text.contains("# Orchestration"),
        "guide missing title:\n{text}"
    );
    assert!(text.contains("fl coordination-state"));
    assert!(text.contains("fl team run"));
    let guide_json = fixture
        .fl_json(&["instructions", "orchestration", "--json"])
        .await;
    assert_eq!(guide_json["topic"], "orchestration");
    assert!(string(&guide_json, "markdown").contains("fl team"));
    let topics = fixture.fl_json(&["instructions", "--json"]).await;
    assert_eq!(topics["topics"].as_array().unwrap().len(), 5);

    // --- coordination: CAS, lock ownership and watch ---------------------
    let set = fixture
        .fl_json(&[
            "coordination-state",
            "set",
            "plan/owner",
            "--value",
            "alice",
            "--if-version",
            "0",
            "--json",
        ])
        .await;
    assert_eq!(set["entry"]["version"], 1);
    let stale = fixture
        .fl(&[
            "coordination-state",
            "set",
            "plan/owner",
            "--value",
            "bob",
            "--if-version",
            "2",
            "--json",
        ])
        .await;
    assert!(!stale.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&stale.stdout).unwrap()["error"]["code"],
        "revision_conflict",
        "stale CAS must carry the daemon's conflict"
    );

    let locked = fixture
        .fl_json(&[
            "coordination-state",
            "lock",
            "plan/owner",
            "--owner",
            "agent-a",
            "--ttl-seconds",
            "60",
            "--json",
        ])
        .await;
    assert_eq!(locked["lock"]["owner"], "agent-a");
    let held = fixture
        .fl(&[
            "coordination-state",
            "lock",
            "plan/owner",
            "--owner",
            "agent-b",
            "--json",
        ])
        .await;
    assert!(!held.status.success());
    let held_json: Value = serde_json::from_slice(&held.stdout).unwrap();
    assert_eq!(held_json["error"]["code"], "revision_conflict");
    assert!(string(&held_json["error"], "message").contains("agent-a"));
    let unlocked = fixture.fl(&[
        "coordination-state",
        "unlock",
        "plan/owner",
        "--owner",
        "agent-b",
        "--json",
    ]);
    let unlocked = unlocked.await;
    assert!(!unlocked.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&unlocked.stdout).unwrap()["error"]["code"],
        "not_owner"
    );
    fixture
        .fl_json(&["coordination-state", "get", "plan/owner", "--json"])
        .await;

    // A watcher sees the initial version and the next write, then exits.
    let binary = env!("CARGO_BIN_EXE_fl").to_owned();
    let home = fixture.home().root().to_path_buf();
    let repo = fixture.repo.clone();
    let watcher = tokio::spawn(async move {
        tokio::process::Command::new(binary)
            .args([
                "coordination-state",
                "watch",
                "plan/owner",
                "--json",
                "--timeout",
                "5",
            ])
            .env("FLOTILLA_HOME", home)
            .env_remove("FLOTILLA_SESSION_ID")
            .env_remove("FLOTILLA_SOCKET")
            .current_dir(repo)
            .output()
            .await
            .unwrap()
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    fixture
        .fl_json(&[
            "coordination-state",
            "set",
            "plan/owner",
            "--value",
            "bob",
            "--if-version",
            "1",
            "--json",
        ])
        .await;
    let watched = tokio::time::timeout(WAIT, watcher).await.unwrap().unwrap();
    assert!(watched.status.success());
    let lines: Vec<Value> = String::from_utf8_lossy(&watched.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        lines.len() >= 2,
        "watcher missed the new version: {lines:?}"
    );
    assert_eq!(lines.last().unwrap()["value"]["version"], 2);
    assert_eq!(lines.last().unwrap()["value"]["value"], "bob");

    // Refusals: wrong version delete and an unknown worktree target.
    let wrong_delete = fixture
        .fl(&[
            "coordination-state",
            "delete",
            "plan/owner",
            "--if-version",
            "1",
            "--json",
        ])
        .await;
    assert!(!wrong_delete.status.success());
    let unknown = fixture
        .fl(&[
            "coordination-state",
            "set",
            "plan/owner",
            "--value",
            "x",
            "--worktree",
            "no-such-worktree",
            "--json",
        ])
        .await;
    assert!(!unknown.status.success());

    // --- team run across two provider entries ----------------------------
    let team_file = fixture.dir.path().join("team.json");
    std::fs::write(
        &team_file,
        serde_json::to_vec(&serde_json::json!({
            "lead": "architect",
            "roles": [
                {"label": "architect", "provider": "claude", "prompt": "Design the change."},
                {"label": "builder", "provider": "codex", "prompt": "Build the change."},
                {"label": "reviewer", "provider": "claude", "prompt": "Review the change."}
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    let launched = fixture
        .fl_json(&[
            "team",
            "run",
            "--file",
            team_file.to_str().unwrap(),
            "--json",
        ])
        .await;
    let run_id = string(&launched["run"], "id");
    let status = wait_for_team(&fixture).await;
    assert_eq!(status["run"]["state"], "Finished");
    let roles = status["run"]["roles"].as_array().unwrap();
    assert_eq!(roles.len(), 3);
    let mut sessions = Vec::new();
    let mut tabs = Vec::new();
    for role in roles {
        assert_eq!(role["state"], "Finished");
        sessions.push(string(role, "session"));
        tabs.push(string(role, "tab"));
    }
    sessions.sort();
    sessions.dedup();
    tabs.sort();
    tabs.dedup();
    assert_eq!(sessions.len(), 3, "each role has its own session");
    assert_eq!(tabs.len(), 3, "each role has its own tab");
    assert_eq!(status["run"]["lead"], "architect");
    assert_eq!(status["run"]["fan_in_sent"], true);

    let chats = fixture.fl_json(&["chat", "list", "--json"]).await;
    assert_eq!(chats["sessions"].as_array().unwrap().len(), 3);
    let layout = fixture.fl_json(&["layout", "views", "--json"]).await;
    let names: Vec<String> = layout["layout"]["views"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|view| view["tabs"].as_array().unwrap())
        .filter_map(|tab| tab["name"].as_str().map(str::to_owned))
        .collect();
    for label in ["architect", "builder", "reviewer"] {
        assert!(
            names.iter().any(|name| name == &format!("team:{label}")),
            "layout has no tab for {label}: {names:?}"
        );
    }

    // Exactly one fan-in message, and it is short.
    let lead = roles
        .iter()
        .find(|role| role["label"] == "architect")
        .unwrap();
    let lead_session = string(lead, "session");
    let page = fixture
        .fl_json(&["chat", "events", &lead_session, "--json"])
        .await;
    let fan_ins: Vec<&Value> = page["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|record| {
            record["event"]["UserMessage"]["text"]
                .as_str()
                .is_some_and(|text| text.starts_with("Team run "))
        })
        .collect();
    assert_eq!(fan_ins.len(), 1, "exactly one lead fan-in");
    assert!(
        fan_ins[0]["event"]["UserMessage"]["text"]
            .as_str()
            .unwrap()
            .len()
            < 2048
    );

    // Reports are durable and readable, and a role can submit one.
    let submitted = fixture
        .fl_json(&[
            "team",
            "report",
            "--run",
            &run_id,
            "--role",
            "builder",
            "--summary",
            "Built the change.",
            "--result-file",
            "out/build.md",
            "--json",
        ])
        .await;
    let builder = submitted["run"]["roles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|role| role["label"] == "builder")
        .unwrap();
    assert_eq!(builder["report"]["summary"], "Built the change.");
    assert_eq!(builder["report"]["result_file"], "out/build.md");
    let report = fixture
        .fl_json(&["team", "report", "--run", &run_id, "--json"])
        .await;
    assert_eq!(report["run"]["roles"].as_array().unwrap().len(), 3);
    let listed = fixture.fl_json(&["team", "list", "--json"]).await;
    assert_eq!(listed["runs"].as_array().unwrap().len(), 1);

    // A second run can be cancelled; the first survives untouched.
    let cancel_file = fixture.dir.path().join("team-cancel.json");
    std::fs::write(
        &cancel_file,
        serde_json::to_vec(&serde_json::json!({
            "roles": [
                {"label": "one", "provider": "claude", "prompt": "First."},
                {"label": "two", "provider": "codex", "prompt": "Second."}
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    let second = fixture
        .fl_json(&[
            "team",
            "run",
            "--file",
            cancel_file.to_str().unwrap(),
            "--json",
        ])
        .await;
    let second_id = string(&second["run"], "id");
    let cancelled = fixture
        .fl_json(&["team", "cancel", "--run", &second_id, "--json"])
        .await;
    assert_eq!(cancelled["run"]["state"], "Cancelled");

    // Malformed team files are refused locally with a reason.
    let malformed = fixture.dir.path().join("bad-team.json");
    std::fs::write(&malformed, "{\"roles\": []}").unwrap();
    let refused = fixture
        .fl(&[
            "team",
            "run",
            "--file",
            malformed.to_str().unwrap(),
            "--json",
        ])
        .await;
    assert!(!refused.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&refused.stdout).unwrap()["error"]["code"],
        "invalid_argument"
    );

    // --- hooks become chat facts; never a turn outcome -------------------
    let session = sessions[0].clone();
    let before = fixture
        .fl_json(&["chat", "events", &session, "--json"])
        .await["events"]
        .as_array()
        .unwrap()
        .len();
    fixture
        .fl_json(&[
            "hook",
            "PreToolUse",
            "--session",
            &session,
            "--payload",
            "{\"tool_name\":\"Bash\",\"tool_use_id\":\"t1\",\"tool_input\":{\"command\":\"ls\"}}",
            "--json",
        ])
        .await;
    fixture
        .fl_json(&[
            "hook",
            "PostToolUse",
            "--session",
            &session,
            "--payload",
            "{\"tool_name\":\"Bash\",\"tool_use_id\":\"t1\",\"tool_response\":{\"is_error\":false}}",
            "--json",
        ])
        .await;
    fixture
        .fl_json(&[
            "hook",
            "Stop",
            "--session",
            &session,
            "--payload",
            "{\"hook_event_name\":\"Stop\"}",
            "--json",
        ])
        .await;
    let page = fixture
        .fl_json(&["chat", "events", &session, "--json"])
        .await;
    let mut statuses: Vec<String> = Vec::new();
    let mut saw_hook_status = false;
    for record in page["events"].as_array().unwrap().iter().skip(before) {
        if let Some(call) = record["event"]["ToolCall"].as_object() {
            assert_eq!(call["title"], "Bash");
            statuses.push(call["status"].as_str().unwrap().to_owned());
        }
        if let Some(fact) = record["event"]["HookFact"].as_object()
            && fact["kind"] == "TurnEnded"
        {
            saw_hook_status = true;
        }
        // A hook must not fabricate a turn end.
        assert!(
            record["event"].get("TurnEnd").is_none(),
            "hook invented a turn outcome: {record}"
        );
    }
    assert!(statuses.contains(&"Running".to_owned()));
    assert!(statuses.contains(&"Completed".to_owned()));
    assert!(saw_hook_status);
    let session_value = fixture.fl_json(&["chat", "list", "--json"]).await;
    let listed_session = session_value["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|candidate| candidate["id"] == serde_json::json!(session))
        .unwrap();
    assert_eq!(listed_session["status_source"], "Hook");

    // A hook for an unknown session is a refusal with the daemon's reason.
    let unknown_hook = fixture
        .fl(&[
            "hook",
            "Stop",
            "--session",
            "00000000-0000-0000-0000-000000000001",
            "--payload",
            "{}",
            "--json",
        ])
        .await;
    assert!(!unknown_hook.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&unknown_hook.stdout).unwrap()["error"]["code"],
        "not_found"
    );

    // --- restart: durable runs, roles and reports ------------------------
    fixture.restart().await;
    let after = fixture
        .fl_json(&["team", "status", "--run", &run_id, "--json"])
        .await;
    assert_eq!(after["run"]["state"], "Finished");
    let builder = after["run"]["roles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|role| role["label"] == "builder")
        .unwrap();
    assert_eq!(builder["report"]["summary"], "Built the change.");
    let listed = fixture.fl_json(&["team", "list", "--json"]).await;
    assert_eq!(listed["runs"].as_array().unwrap().len(), 2);

    fixture.shutdown().await;
}
/// Custom commands, prompt templates and branch groups through the built
/// `fl` binary: create/get/update/delete/list, duplicate and invalid
/// refusals that exit non-zero, durability across a daemon restart, and the
/// shared context plus grouped diff of a lazily created child.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commands_templates_and_branch_groups_are_durable() {
    let mut fixture = Fixture::new().await;

    // Create at the caller's project scope (the default) and globally.
    let created = fixture
        .fl_json(&[
            "commands",
            "create",
            "deploy",
            "--body",
            "Ship {{branch}} to {{base}}",
            "--description",
            "Ship the branch",
            "--json",
        ])
        .await;
    assert_eq!(created["action"], "created");
    assert_eq!(created["rev"], 1);
    let global = fixture
        .fl_json(&[
            "commands",
            "create",
            "audit",
            "--body",
            "Audit {{branch}}",
            "--scope",
            "global",
            "--json",
        ])
        .await;
    assert_eq!(global["rev"], 2);

    let listed = fixture.fl_json(&["commands", "list", "--json"]).await;
    let names: Vec<&str> = listed["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|command| command["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["audit", "deploy"], "{listed}");
    assert_eq!(listed["scope"], "the caller");

    let got = fixture
        .fl_json(&["commands", "get", "deploy", "--json"])
        .await;
    assert_eq!(got["command"]["body"], "Ship {{branch}} to {{base}}");
    assert_eq!(
        got["command"]["scope"]["Project"].as_str().unwrap(),
        fixture.project.to_string()
    );

    // Duplicate and invalid definitions are refusals, not silent upserts.
    let duplicate = fixture
        .fl(&["commands", "create", "deploy", "--body", "again", "--json"])
        .await;
    assert!(!duplicate.status.success());
    let duplicate_json: Value = serde_json::from_slice(&duplicate.stdout).unwrap();
    assert_eq!(duplicate_json["error"]["code"], "invalid_argument");
    assert!(
        duplicate_json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("already exists"),
        "{duplicate_json}"
    );
    let invalid = fixture
        .fl(&["commands", "create", "bad name", "--body", "x", "--json"])
        .await;
    assert!(!invalid.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&invalid.stdout).unwrap()["error"]["code"],
        "invalid_argument"
    );
    let unknown = fixture.fl(&["commands", "get", "absent", "--json"]).await;
    assert!(!unknown.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&unknown.stdout).unwrap()["error"]["code"],
        "not_found"
    );

    // Update and delete prove their durable effect through a restart.
    let updated = fixture
        .fl_json(&[
            "commands",
            "update",
            "deploy",
            "--body",
            "Ship {{branch}} now",
            "--json",
        ])
        .await;
    assert_eq!(updated["rev"], 3);
    let after_update = fixture
        .fl_json(&["commands", "get", "deploy", "--json"])
        .await;
    assert_eq!(after_update["command"]["body"], "Ship {{branch}} now");

    // --- prompt templates at every scope --------------------------------
    let template = fixture
        .fl_json(&[
            "prompt-template",
            "set",
            "fix_ci",
            "--body",
            "Fix {{branch}} checks",
            "--scope",
            "global",
            "--json",
        ])
        .await;
    assert_eq!(template["action"], "fix_ci");
    let templates = fixture
        .fl_json(&["prompt-template", "list", "--json"])
        .await;
    assert_eq!(templates["templates"].as_array().unwrap().len(), 1);
    let effective = fixture
        .fl_json(&["prompt-template", "get", "fix_ci", "--json"])
        .await;
    assert_eq!(effective["template"], "Fix {{branch}} checks");
    assert_eq!(effective["stored"], true);
    let bad_action = fixture
        .fl(&["prompt-template", "set", "nope", "--body", "x", "--json"])
        .await;
    assert!(!bad_action.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&bad_action.stdout).unwrap()["error"]["code"],
        "invalid_argument"
    );

    // --- branch group: shared context and grouped diff -------------------
    let child = fixture
        .fl_json(&["worktree", "ensure-child", "--json"])
        .await;
    assert_eq!(child["created"], true);
    assert_eq!(child["root"]["is_primary"], true);
    let child_path = PathBuf::from(string(&child["worktree"], "path"));
    for name in ["FEATURE.md", "AGENTS.md"] {
        assert!(
            child_path.join(name).is_file(),
            "{name} missing from {}",
            child_path.display()
        );
    }
    assert!(child_path.join("context").is_dir());
    assert!(child["shared"]["missing"].as_array().unwrap().is_empty());

    let again = fixture
        .fl_json(&["worktree", "ensure-child", "--json"])
        .await;
    assert_eq!(again["created"], false);
    assert_eq!(again["worktree"]["id"], child["worktree"]["id"]);

    // A child edit lands in the grouped diff.
    std::fs::write(child_path.join("FEATURE.md"), "# Feature\nlocal\n").unwrap();
    std::fs::write(child_path.join("extra.txt"), "a\nb\n").unwrap();
    let diff = fixture.fl_json(&["worktree", "group-diff", "--json"]).await;
    let child_id = string(&child["worktree"], "id");
    let entry = diff["diff"]["children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["worktree"] == serde_json::json!(child_id))
        .expect("child in group diff");
    let paths: Vec<&str> = entry["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| file["path"].as_str().unwrap())
        .collect();
    assert!(paths.contains(&"extra.txt"), "{paths:?}");
    assert!(paths.contains(&"FEATURE.md"), "{paths:?}");
    assert!(diff["diff"]["changed_files"].as_u64().unwrap() >= 2);
    assert_eq!(diff["diff"]["group"]["base"], "main");

    // --- restart: commands and templates survive ------------------------
    fixture.restart().await;
    let listed = fixture.fl_json(&["commands", "list", "--json"]).await;
    assert!(
        listed["commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command["name"] == "deploy")
    );
    let templates = fixture
        .fl_json(&["prompt-template", "list", "--json"])
        .await;
    assert_eq!(templates["templates"].as_array().unwrap().len(), 1);

    let deleted = fixture
        .fl_json(&["commands", "delete", "deploy", "--json"])
        .await;
    assert_eq!(deleted["action"], "deleted");
    let gone = fixture.fl(&["commands", "get", "deploy", "--json"]).await;
    assert!(!gone.status.success());
    let removed_template = fixture
        .fl_json(&[
            "prompt-template",
            "remove",
            "fix_ci",
            "--scope",
            "global",
            "--json",
        ])
        .await;
    assert!(removed_template["templates"].as_array().unwrap().is_empty());
    let default = fixture
        .fl_json(&["prompt-template", "get", "fix_ci", "--json"])
        .await;
    assert_eq!(default["stored"], false);

    fixture.shutdown().await;
}
