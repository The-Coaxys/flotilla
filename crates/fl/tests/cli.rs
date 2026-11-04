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
    daemon: DaemonHandle,
    repo: PathBuf,
    workspace: WorkspaceId,
    project: ProjectId,
    worktree: fl_model::WorktreeId,
}

impl Fixture {
    async fn new() -> Self {
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
            daemon,
            repo,
            workspace,
            project: project.id,
            worktree,
        }
    }

    fn home(&self) -> &FlotillaHome {
        self.daemon.home()
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

    async fn shutdown(self) {
        self.daemon.shutdown().await;
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
    assert_eq!(string(&tree["worktree"], "id"), fixture.worktree.to_string());
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

    let listed = fixture.fl_json(&["worktree", "review-list", "--json"]).await;
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
        "status", "workspace", "section", "worktree", "project", "tab", "layout", "chat",
        "history", "agent",
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
