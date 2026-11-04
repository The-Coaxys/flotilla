#![cfg(unix)]
//! M2 workspaces end to end against a real daemon, process host, git and
//! shells: projects, worktrees with `.worktreeinclude` and setup scripts,
//! live status events, files, diffs, worktree layouts and run scripts.
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use fl_model::layout::LayoutOp;
use fl_model::workspace::{
    ChangeKind, DaemonEvent, LayoutScope, ProjectKind, ScriptKind, ScriptRunState, Worktree,
    WorktreeStatus,
};
use fl_model::{ClientSessionId, OperationId, PaneId, ProcessInfo, Size, TabId, TerminalId};
use fl_proto::{ClientMsg, Hello, PROTO, Request, Response, RpcError, RpcErrorCode, ServerMsg};
use fl_transport::conn::{FrameReader, FrameWriter, framed};
use fld_server::{DaemonConfig, DaemonHandle, FlotillaHome, LaunchDefaults, ServerError};
use tokio::net::UnixStream;

mod support;

const WAIT: Duration = Duration::from_secs(20);

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

struct Fixture {
    dir: tempfile::TempDir,
    home: FlotillaHome,
}

impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = FlotillaHome::new(dir.path().join(".flotilla"));
        home.ensure_dirs().unwrap();
        support::ensure_prochost(home.root()).await;
        Self { dir, home }
    }

    async fn start(&self) -> DaemonHandle {
        let mut config = DaemonConfig::new(self.home.clone());
        config.quic_bind = None;
        config.prochost_executable = None;
        config.launch = LaunchDefaults {
            cwd: self.dir.path().display().to_string(),
            argv: vec!["/bin/sh".into()],
            env: BTreeMap::from([
                ("PS1".into(), "$ ".into()),
                ("TERM".into(), "xterm-256color".into()),
                ("ENV".into(), String::new()),
            ]),
            quota_bytes: fl_prochost_proto::DEFAULT_QUOTA_BYTES,
        };
        tokio::time::timeout(WAIT, async {
            loop {
                match fld_server::start(config.clone()).await {
                    Ok(handle) => break handle,
                    Err(ServerError::AlreadyRunning(_)) => {
                        tokio::time::sleep(Duration::from_millis(20)).await
                    }
                    Err(error) => panic!("daemon start failed: {error}"),
                }
            }
        })
        .await
        .expect("daemon starts")
    }

    /// A repository on `main` whose `.env` is ignored, copied into new
    /// worktrees by `.worktreeinclude`, with setup and run scripts.
    fn repo(&self) -> PathBuf {
        let repo = self.dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("README.md"), "one\ntwo\nthree\n").unwrap();
        std::fs::write(repo.join(".gitignore"), ".env\n*.out\n").unwrap();
        std::fs::write(repo.join(".worktreeinclude"), ".env\n").unwrap();
        std::fs::create_dir(repo.join(".flotilla")).unwrap();
        std::fs::write(
            repo.join(".flotilla/config.json"),
            r#"{
                "setup": "echo setup-ran > setup.out",
                "run": "printf '%s|%s|' \"$FLOTILLA_WORKTREE_NAME\" \"$PWD\" > run.out; echo started >> run.out; sleep 60"
            }"#,
        )
        .unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "initial"]);
        std::fs::write(repo.join(".env"), "SECRET=1\n").unwrap();
        repo
    }
}

/// A control session that keeps pushed events instead of failing on them.
struct Control {
    reader: FrameReader,
    writer: FrameWriter,
    next: u64,
    events: VecDeque<DaemonEvent>,
}

impl Control {
    async fn connect(socket: &Path) -> Self {
        Self::connect_as(socket, PROTO).await
    }

    async fn connect_as(socket: &Path, proto: fl_proto::ProtoVersion) -> Self {
        let stream = UnixStream::connect(socket).await.unwrap();
        let (r, w) = stream.into_split();
        let (reader, mut writer) = framed(r, w);
        writer
            .send(&ClientMsg::Hello(Hello {
                client_version: "workspace-test".into(),
                proto,
                device_id: None,
                token: None,
                client_session_id: *ClientSessionId::new().as_bytes(),
            }))
            .await
            .unwrap();
        let mut control = Self {
            reader,
            writer,
            next: 0,
            events: VecDeque::new(),
        };
        let ServerMsg::Welcome(_) = control.recv().await else {
            panic!("expected welcome")
        };
        control
    }

    async fn recv(&mut self) -> ServerMsg {
        tokio::time::timeout(WAIT, self.reader.recv::<ServerMsg>())
            .await
            .expect("server message in time")
            .expect("valid server message")
    }

    async fn rpc(&mut self, req: Request) -> Result<Response, RpcError> {
        self.next += 1;
        let id = self.next;
        self.writer.send(&ClientMsg::Rpc { id, req }).await.unwrap();
        loop {
            match self.recv().await {
                ServerMsg::RpcResult { id: got, result } if got == id => return result,
                ServerMsg::Event(event) => self.events.push_back(event),
                other => panic!("unexpected control message {other:?}"),
            }
        }
    }

    async fn ok(&mut self, req: Request) -> Response {
        let what = format!("{req:?}");
        self.rpc(req)
            .await
            .unwrap_or_else(|error| panic!("{what}: {error:?}"))
    }

    /// The first buffered or newly pushed event `pick` accepts.
    async fn event<T>(&mut self, what: &str, mut pick: impl FnMut(&DaemonEvent) -> Option<T>) -> T {
        if let Some(index) = self.events.iter().position(|e| pick(e).is_some()) {
            let event = self.events.remove(index).unwrap();
            return pick(&event).unwrap();
        }
        tokio::time::timeout(WAIT, async {
            loop {
                if let ServerMsg::Event(event) = self.recv().await
                    && let Some(value) = pick(&event)
                {
                    return value;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no {what} event"))
    }

    async fn processes(&mut self) -> Vec<ProcessInfo> {
        match self.ok(Request::ListProcesses).await {
            Response::Processes(list) => list,
            other => panic!("unexpected {other:?}"),
        }
    }
}

async fn wait_file(path: &Path, needle: &str) -> String {
    tokio::time::timeout(WAIT, async {
        loop {
            if let Ok(text) = std::fs::read_to_string(path)
                && text.contains(needle)
            {
                return text;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{} never contained {needle:?}", path.display()))
}

fn environ(pid: u32) -> Vec<String> {
    std::fs::read(format!("/proc/{pid}/environ"))
        .unwrap()
        .split(|byte| *byte == 0)
        .map(|entry| String::from_utf8_lossy(entry).into_owned())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn projects_worktrees_status_files_layouts_and_scripts() {
    let fixture = Fixture::new().await;
    let repo = fixture.repo();
    let folder = fixture.dir.path().join("notes");
    std::fs::create_dir(&folder).unwrap();
    let daemon = fixture.start().await;
    let mut control = Control::connect(&daemon.socket_path).await;

    // A default workspace exists; projects are added to it.
    let Response::WorkspaceTree(tree) = control.ok(Request::GetWorkspaceTree).await else {
        panic!("expected tree")
    };
    let workspace = tree.workspaces[0].id;
    let Response::Project(project) = control
        .ok(Request::AddProject {
            workspace,
            path: repo.display().to_string(),
        })
        .await
    else {
        panic!("expected project")
    };
    assert_eq!(project.kind, ProjectKind::Git);
    assert_eq!(project.default_branch.as_deref(), Some("main"));
    control
        .event("tree change", |e| {
            matches!(e, DaemonEvent::WorkspaceTreeChanged { .. }).then_some(())
        })
        .await;
    let Response::Project(notes) = control
        .ok(Request::AddProject {
            workspace,
            path: folder.display().to_string(),
        })
        .await
    else {
        panic!("expected project")
    };
    assert_eq!(notes.kind, ProjectKind::Folder);
    let Response::WorkspaceTree(tree) = control.ok(Request::GetWorkspaceTree).await else {
        panic!("expected tree")
    };
    let primary: Vec<&Worktree> = tree
        .worktrees
        .iter()
        .filter(|w| w.project == project.id)
        .collect();
    assert_eq!(primary.len(), 1);
    assert!(primary[0].is_primary);
    assert!(
        tree.worktrees
            .iter()
            .any(|w| w.project == notes.id && w.is_primary),
        "a folder project has its own pseudo-worktree"
    );

    // A worktree per task: .worktreeinclude copies the ignored .env and the
    // setup script runs in it.
    let Response::Worktree(task) = control
        .ok(Request::CreateWorktree {
            project: project.id,
            branch: Some("feature-x".into()),
            base: None,
        })
        .await
    else {
        panic!("expected worktree")
    };
    let path = PathBuf::from(&task.path);
    assert_eq!(task.branch.as_deref(), Some("feature-x"));
    assert!(!task.is_primary);
    assert_eq!(
        std::fs::read_to_string(path.join(".env")).unwrap(),
        "SECRET=1\n"
    );
    wait_file(&path.join("setup.out"), "setup-ran").await;

    // Live status: edits are pushed with +/- counts against the merge-base.
    std::fs::write(path.join("README.md"), "one\nTWO\nthree\nfour\n").unwrap();
    std::fs::write(path.join("new.txt"), "a\nb\nc\n").unwrap();
    let status: WorktreeStatus = control
        .event("worktree status", |e| match e {
            DaemonEvent::WorktreeStatus(s)
                if s.worktree == task.id && s.files.iter().any(|f| f.path == "new.txt") =>
            {
                Some(s.clone())
            }
            _ => None,
        })
        .await;
    let readme = status.files.iter().find(|f| f.path == "README.md").unwrap();
    assert_eq!(readme.kind, ChangeKind::Modified);
    assert_eq!((readme.added, readme.removed), (Some(2), Some(1)));
    let new = status.files.iter().find(|f| f.path == "new.txt").unwrap();
    assert_eq!(new.kind, ChangeKind::Untracked);
    assert_eq!(new.added, Some(3));
    assert!(status.summary.dirty);
    assert_eq!(status.summary.added, 5);
    assert_eq!(status.summary.removed, 1);

    // Files: conflict-checked writes.
    let file = path.join("README.md").display().to_string();
    let Response::FileContent(content) = control
        .ok(Request::ReadFile {
            path: file.clone(),
            max_bytes: 1 << 20,
        })
        .await
    else {
        panic!("expected content")
    };
    assert_eq!(content.bytes, b"one\nTWO\nthree\nfour\n");
    let stale = control
        .rpc(Request::WriteFile {
            path: file.clone(),
            bytes: b"lost update\n".to_vec(),
            expected_sha256: Some([0; 32]),
        })
        .await
        .unwrap_err();
    assert_eq!(stale.code, RpcErrorCode::RevisionConflict);
    control
        .ok(Request::WriteFile {
            path: file.clone(),
            bytes: b"one\nTWO\nthree\nfour\nfive\n".to_vec(),
            expected_sha256: Some(content.sha256),
        })
        .await;
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "one\nTWO\nthree\nfour\nfive\n"
    );

    // Diff sides: base content against the working file; added files have
    // no old side.
    let Response::FileDiff(diff) = control
        .ok(Request::FileDiff {
            worktree: task.id,
            path: "README.md".into(),
        })
        .await
    else {
        panic!("expected diff")
    };
    assert_eq!(diff.old.unwrap().bytes, b"one\ntwo\nthree\n");
    assert_eq!(diff.new.unwrap().bytes, b"one\nTWO\nthree\nfour\nfive\n");
    let Response::FileDiff(added) = control
        .ok(Request::FileDiff {
            worktree: task.id,
            path: "new.txt".into(),
        })
        .await
    else {
        panic!("expected diff")
    };
    assert!(added.old.is_none());

    // The worktree's own layout: its terminals start in the worktree with
    // the FLOTILLA_* environment.
    let scope = LayoutScope::Worktree(task.id);
    let Response::Layout(layout) = control.ok(Request::GetScopedLayout { scope }).await else {
        panic!("expected layout")
    };
    assert!(layout.views.iter().all(|view| view.tabs.is_empty()));
    let terminal = TerminalId::new();
    control
        .ok(Request::ApplyScopedLayout {
            scope,
            operation: OperationId::new(),
            base_rev: layout.rev,
            op: LayoutOp::NewTab {
                view: layout.active,
                tab: TabId::new(),
                pane: PaneId::new(),
                terminal,
                name: None,
                size: Size::new(80, 24),
            },
        })
        .await;
    control
        .event("layout change", |e| match e {
            DaemonEvent::LayoutChanged { scope: s, .. } if *s == scope => Some(()),
            _ => None,
        })
        .await;
    let shell = control
        .processes()
        .await
        .into_iter()
        .find(|p| p.terminal == terminal)
        .expect("worktree terminal spawned");
    let canonical = std::fs::canonicalize(&path).unwrap();
    assert_eq!(
        std::fs::read_link(format!("/proc/{}/cwd", shell.pid)).unwrap(),
        canonical
    );
    let env = environ(shell.pid);
    assert!(
        env.contains(&format!("FLOTILLA_WORKTREE_PATH={}", task.path)),
        "{env:?}"
    );
    assert!(env.contains(&format!("FLOTILLA_WORKTREE_NAME={}", task.label)));
    assert!(env.contains(&format!("FLOTILLA_ROOT_PATH={}", project.path)));

    // Run script: a terminal in the worktree, observable and stoppable.
    let Response::ScriptState(running) = control
        .ok(Request::RunScript {
            worktree: task.id,
            kind: ScriptKind::Run(None),
        })
        .await
    else {
        panic!("expected script state")
    };
    assert_eq!(running.state, ScriptRunState::Running);
    assert!(running.terminal.is_some());
    let out = wait_file(&path.join("run.out"), "started").await;
    assert!(
        out.starts_with(&format!("{}|{}|", task.label, canonical.display()))
            || out.starts_with(&format!("{}|{}|", task.label, task.path)),
        "{out}"
    );
    let Response::Scripts(scripts) = control.ok(Request::ListScripts { worktree: task.id }).await
    else {
        panic!("expected scripts")
    };
    assert!(
        scripts
            .iter()
            .any(|s| s.kind == ScriptKind::Run(None) && s.state == ScriptRunState::Running)
    );
    control
        .ok(Request::StopScript {
            worktree: task.id,
            kind: ScriptKind::Run(None),
        })
        .await;
    control
        .event("script stopped", |e| match e {
            DaemonEvent::ScriptChanged(s)
                if s.worktree == task.id
                    && s.kind == ScriptKind::Run(None)
                    && matches!(s.state, ScriptRunState::Exited { .. }) =>
            {
                Some(())
            }
            _ => None,
        })
        .await;

    // A dirty worktree is only deleted on request; its terminals go with it.
    let refused = control
        .rpc(Request::DeleteWorktree {
            worktree: task.id,
            force: false,
        })
        .await
        .unwrap_err();
    assert_eq!(refused.code, RpcErrorCode::InvalidArgument, "{refused:?}");
    assert!(path.exists());
    control
        .ok(Request::DeleteWorktree {
            worktree: task.id,
            force: true,
        })
        .await;
    assert!(!path.exists());
    let Response::WorkspaceTree(tree) = control.ok(Request::GetWorkspaceTree).await else {
        panic!("expected tree")
    };
    assert!(tree.worktrees.iter().all(|w| w.id != task.id));
    tokio::time::timeout(WAIT, async {
        while control
            .processes()
            .await
            .iter()
            .any(|p| p.terminal == terminal)
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the worktree's terminals are closed with it");

    daemon.shutdown().await;
}

/// Peers outside the supported minor-version window are rejected before
/// a control session can receive messages it cannot decode.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn incompatible_peer_is_rejected_before_workspace_events() {
    let fixture = Fixture::new().await;
    let daemon = fixture.start().await;
    let stream = UnixStream::connect(&daemon.socket_path).await.unwrap();
    let (r, w) = stream.into_split();
    let (mut reader, mut writer) = framed(r, w);
    writer
        .send(&ClientMsg::Hello(Hello {
            client_version: "workspace-test".into(),
            proto: fl_proto::ProtoVersion { major: 0, minor: 2 },
            device_id: None,
            token: None,
            client_session_id: *ClientSessionId::new().as_bytes(),
        }))
        .await
        .unwrap();
    let reply = tokio::time::timeout(WAIT, reader.recv::<ServerMsg>())
        .await
        .expect("handshake reply in time")
        .unwrap();
    assert_eq!(
        reply,
        ServerMsg::Rejected(fl_proto::RejectReason::ProtoMismatch { server: PROTO })
    );
    daemon.shutdown().await;
}

/// A stand-in for `gh` that keeps one pull request's state in files: it
/// answers the PR query, and records `pr create` (title, body from stdin,
/// draft), `pr ready` and `pr merge`.
fn fake_gh(dir: &Path) -> PathBuf {
    let state = dir.join("gh-state");
    std::fs::create_dir_all(&state).unwrap();
    let script = dir.join("gh");
    std::fs::write(
        &script,
        format!(
            r#"#!/bin/sh
S={state}
case "$1 $2" in
  "api graphql")
    if [ -f $S/number ]; then
      printf '{{"data":{{"repository":{{"mergeCommitAllowed":false,"squashMergeAllowed":true,"rebaseMergeAllowed":false,"pullRequests":{{"nodes":[{{"number":7,"url":"https://github.com/acme/widgets/pull/7","title":"%s","state":"%s","isDraft":%s,"baseRefName":"main","headRefName":"feature","mergeable":"MERGEABLE","reviewDecision":null,"reviewThreads":{{"nodes":[]}},"commits":{{"nodes":[{{"commit":{{"statusCheckRollup":{{"state":"SUCCESS","contexts":{{"nodes":[{{"__typename":"CheckRun","name":"test","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"https://ci/1"}}]}}}}}}}}]}}}}]}}}}}}}}' "$(cat $S/title)" "$(cat $S/state)" "$(cat $S/draft)"
    else
      printf '{{"data":{{"repository":{{"mergeCommitAllowed":false,"squashMergeAllowed":true,"rebaseMergeAllowed":false,"pullRequests":{{"nodes":[]}}}}}}}}'
    fi ;;
  "pr create")
    shift 2
    draft=false
    while [ $# -gt 0 ]; do
      case "$1" in
        --title) echo "$2" > $S/title; shift ;;
        --base) echo "$2" > $S/base; shift ;;
        --head) echo "$2" > $S/head; shift ;;
        --draft) draft=true ;;
      esac
      shift
    done
    cat > $S/body
    echo $draft > $S/draft; echo OPEN > $S/state; echo 7 > $S/number
    echo https://github.com/acme/widgets/pull/7 ;;
  "pr ready") echo false > $S/draft ;;
  "pr merge") echo "$4 $5 $6" > $S/merge; echo MERGED > $S/state ;;
  *) echo "unexpected gh $*" >&2; exit 1 ;;
esac
"#,
            state = state.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    script
}

/// Commit, push, pull request, ready, merge and update from the UI's
/// requests, against a real bare origin that git reaches through a GitHub
/// URL rewrite, and a scripted `gh`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn git_actions_and_the_pull_request_loop() {
    let fixture = Fixture::new().await;
    let root = fixture.dir.path();
    let seed = fixture.repo();
    let origin = root.join("origin.git");
    git(
        root,
        &[
            "clone",
            "-q",
            "--bare",
            seed.to_str().unwrap(),
            origin.to_str().unwrap(),
        ],
    );
    let work = root.join("widgets");
    git(
        root,
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            work.to_str().unwrap(),
        ],
    );
    let github = "https://github.com/acme/widgets.git";
    git(&work, &["remote", "set-url", "origin", github]);
    git(
        &work,
        &[
            "config",
            &format!("url.{}.insteadOf", origin.display()),
            github,
        ],
    );
    let gh = fake_gh(root);
    // SAFETY: set before the daemon starts; no other test in this binary
    // reads FLOTILLA_GH.
    unsafe { std::env::set_var("FLOTILLA_GH", &gh) };

    let daemon = fixture.start().await;
    let mut control = Control::connect(&daemon.socket_path).await;
    let mut previous = Control::connect_as(
        &daemon.socket_path,
        fl_proto::ProtoVersion { major: 0, minor: 3 },
    )
    .await;
    let Response::WorkspaceTree(tree) = control.ok(Request::GetWorkspaceTree).await else {
        panic!("expected tree")
    };
    let Response::Project(project) = control
        .ok(Request::AddProject {
            workspace: tree.workspaces[0].id,
            path: work.display().to_string(),
        })
        .await
    else {
        panic!("expected project")
    };
    let Response::Worktree(task) = control
        .ok(Request::CreateWorktree {
            project: project.id,
            branch: Some("feature".into()),
            base: None,
        })
        .await
    else {
        panic!("expected worktree")
    };
    let path = PathBuf::from(&task.path);

    // Commit everything.
    std::fs::write(path.join("feature.txt"), "feature\n").unwrap();
    let Response::Committed { head } = control
        .ok(Request::GitCommit {
            worktree: task.id,
            message: "Add the feature".into(),
            all: true,
        })
        .await
    else {
        panic!("expected commit")
    };
    assert_eq!(head.len(), 40);
    let refused = control
        .rpc(Request::GitCommit {
            worktree: task.id,
            message: "nothing".into(),
            all: true,
        })
        .await
        .unwrap_err();
    assert_eq!(refused.code, RpcErrorCode::InvalidArgument);

    // No pull request yet.
    let Response::ForgeState(state) = control.ok(Request::ForgeStatus { worktree: task.id }).await
    else {
        panic!("expected forge state")
    };
    assert_eq!(state.unavailable, None);
    assert!(state.pr.is_none());

    // Open a draft: the branch is pushed first and the body reaches gh.
    let Response::ForgeState(state) = control
        .ok(Request::CreatePullRequest {
            worktree: task.id,
            title: "Add the feature".into(),
            body: "Line one\nLine two".into(),
            draft: true,
        })
        .await
    else {
        panic!("expected forge state")
    };
    let pr = state.pr.expect("a pull request");
    assert!(pr.draft);
    assert_eq!(pr.title, "Add the feature");
    assert_eq!(
        std::fs::read_to_string(root.join("gh-state/body")).unwrap(),
        "Line one\nLine two"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("gh-state/base"))
            .unwrap()
            .trim(),
        "main"
    );
    let pushed = Command::new("git")
        .arg("-C")
        .arg(&origin)
        .args(["rev-parse", "feature"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&pushed.stdout).trim(), head);
    control
        .event("forge change", |e| match e {
            DaemonEvent::ForgeChanged(s) if s.worktree == task.id && s.pr.is_some() => Some(()),
            _ => None,
        })
        .await;
    // The adjacent minor supports workspace events, but cannot decode the
    // newly appended forge event. It must stay connected without receiving it.
    previous.ok(Request::DaemonInfo).await;
    assert!(
        previous
            .events
            .iter()
            .all(|event| !matches!(event, DaemonEvent::ForgeChanged(_))),
        "{:?}",
        previous.events
    );

    // Ready, then merge with the only allowed method.
    let Response::ForgeState(state) = control
        .ok(Request::MarkPullRequestReady { worktree: task.id })
        .await
    else {
        panic!("expected forge state")
    };
    assert!(!state.pr.unwrap().draft);
    let disallowed = control
        .rpc(Request::MergePullRequest {
            worktree: task.id,
            method: fl_model::forge::MergeMethod::Rebase,
        })
        .await
        .unwrap_err();
    assert_eq!(disallowed.code, RpcErrorCode::InvalidArgument);
    let Response::ForgeState(state) = control
        .ok(Request::MergePullRequest {
            worktree: task.id,
            method: fl_model::forge::MergeMethod::Squash,
        })
        .await
    else {
        panic!("expected forge state")
    };
    assert_eq!(state.pr.unwrap().state, fl_model::forge::PrState::Merged);
    assert!(
        std::fs::read_to_string(root.join("gh-state/merge"))
            .unwrap()
            .contains("--squash")
    );

    // Update: main moved on the origin; the worktree merges it in.
    let other = root.join("other");
    git(
        root,
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            other.to_str().unwrap(),
        ],
    );
    std::fs::write(other.join("upstream.txt"), "u\n").unwrap();
    git(&other, &["add", "."]);
    git(&other, &["commit", "-q", "-m", "upstream"]);
    git(&other, &["push", "-q", "origin", "main"]);
    control.ok(Request::GitUpdate { worktree: task.id }).await;
    assert!(path.join("upstream.txt").exists());

    // Without gh the forge is unavailable, with the reason.
    // SAFETY: as above.
    unsafe { std::env::set_var("FLOTILLA_GH", root.join("no-such-gh")) };
    let Response::ForgeState(state) = control.ok(Request::ForgeStatus { worktree: task.id }).await
    else {
        panic!("expected forge state")
    };
    assert!(
        state
            .unavailable
            .as_deref()
            .is_some_and(|r| r.contains("not installed")),
        "{state:?}"
    );
    daemon.shutdown().await;
}
