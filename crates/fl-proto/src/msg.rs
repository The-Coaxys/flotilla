//! Control-stream messages. Postcard encodes enum variants by index: append
//! variants only. Existing structs (especially Hello/Welcome) are frozen;
//! postcard's positional representation cannot safely add defaulted fields.
use bytes::Bytes;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::channel::{ChannelGrant, ChannelHello, ChannelReady, ChannelRole, TERMINAL_PROTOCOL};
use crate::validation::{self, MAX_PROCESS_COUNT, MAX_TEXT_BYTES, ValidationError};
use crate::{Frame, ProtoVersion};
use fl_model::forge::{ForgeState, MergeMethod};
use fl_model::layout::{Layout, LayoutOp};
use fl_model::sections::{SectionMutation, SectionsState};
use fl_model::workspace::{
    DaemonEvent, DirListing, FileContent, FileDiff, LayoutScope, Project, ScriptKind, ScriptState,
    WorkspaceTree, Worktree, WorktreeStatus,
};
use fl_model::{
    IncarnationId, OperationId, ProcessInfo, ProjectId, TerminalId, WorkspaceId, WorktreeId,
};

pub const KIND_CLIENT: u16 = 1;
pub const KIND_SERVER: u16 = 2;

#[derive(Debug, thiserror::Error)]
pub enum MsgError {
    #[error("unexpected frame kind {got} (expected {expected})")]
    WrongKind { expected: u16, got: u16 },
    #[error("message codec error: {0}")]
    Codec(#[from] postcard::Error),
    #[error(transparent)]
    Validation(#[from] ValidationError),
    #[error("trailing bytes after message")]
    TrailingBytes,
}

/// A message type that travels as one frame of a fixed kind.
pub trait WireMessage: Serialize + DeserializeOwned {
    const KIND: u16;

    /// Structural/semantic payload checks only, never authentication.
    fn validate(&self) -> Result<(), ValidationError> {
        Ok(())
    }

    fn to_frame(&self) -> Result<Frame, MsgError> {
        self.validate()?;
        let len = postcard::experimental::serialized_size(self)?;
        validation::bound("frame payload", len, crate::MAX_FRAME_LEN)?;
        Ok(Frame {
            kind: Self::KIND,
            payload: Bytes::from(postcard::to_stdvec(self)?),
        })
    }

    fn from_frame(frame: &Frame) -> Result<Self, MsgError> {
        if frame.kind != Self::KIND {
            return Err(MsgError::WrongKind {
                expected: Self::KIND,
                got: frame.kind,
            });
        }
        validation::bound("frame payload", frame.payload.len(), crate::MAX_FRAME_LEN)?;
        let (message, rest): (Self, _) = postcard::take_from_bytes(&frame.payload)?;
        if !rest.is_empty() {
            return Err(MsgError::TrailingBytes);
        }
        message.validate()?;
        Ok(message)
    }
}

/// First message from a client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub client_version: String,
    pub proto: ProtoVersion,
    /// Required for QUIC peers; ignored on the local socket (SSH already authenticated).
    pub device_id: Option<String>,
    pub token: Option<String>,
    pub client_session_id: [u8; 16],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    pub daemon_version: String,
    pub build: String,
    pub proto: ProtoVersion,
    pub hostname: String,
    pub server_session_id: [u8; 16],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMsg {
    Hello(Hello),
    Ping { nonce: u64 },
    Rpc { id: u64, req: Request },
    BindChannel(ChannelHello),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMsg {
    Welcome(Welcome),
    Rejected(RejectReason),
    Pong {
        nonce: u64,
    },
    RpcResult {
        id: u64,
        result: Result<Response, RpcError>,
    },
    ChannelReady(ChannelReady),
    /// Unsolicited state change, sent to every authenticated session.
    Event(DaemonEvent),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RejectReason {
    BadToken,
    ProtoMismatch { server: ProtoVersion },
    HelloTimeout,
    ExpectedHello,
    InvalidCapability,
    ExpiredCapability,
    WrongChannelRole,
    UnsupportedChannelSchema,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    DaemonInfo,
    Capabilities,
    GetLayout,
    ApplyLayout {
        operation: OperationId,
        base_rev: u64,
        op: LayoutOp,
    },
    ProcessInfo {
        terminal: TerminalId,
        incarnation: IncarnationId,
    },
    ListProcesses,
    OpenChannel {
        role: ChannelRole,
    },
    // --- M2: workspaces, projects, worktrees, files and scripts ---
    GetWorkspaceTree,
    /// List a directory anywhere the daemon user can read (the remote file
    /// picker and the file tree). `path` must be absolute.
    ListDir {
        path: String,
    },
    AddProject {
        workspace: WorkspaceId,
        path: String,
    },
    CloneProject {
        workspace: WorkspaceId,
        url: String,
        /// Absolute destination directory; must not exist yet.
        dest: String,
    },
    RemoveProject {
        project: ProjectId,
    },
    CreateWorktree {
        project: ProjectId,
        /// Branch name; `None` creates a temporary name.
        branch: Option<String>,
        /// Start point; `None` uses the project's default branch.
        base: Option<String>,
    },
    DeleteWorktree {
        worktree: WorktreeId,
        /// Required when the worktree is dirty or has unpushed commits.
        force: bool,
    },
    SetWorktreeLabel {
        worktree: WorktreeId,
        label: String,
    },
    WorktreeStatus {
        worktree: WorktreeId,
    },
    /// Read a file (absolute path), at most `max_bytes` of it.
    ReadFile {
        path: String,
        max_bytes: u32,
    },
    /// Atomically replace a file. With `expected_sha256`, refuse when the
    /// file changed since it was read.
    WriteFile {
        path: String,
        bytes: Vec<u8>,
        expected_sha256: Option<[u8; 32]>,
    },
    FileDiff {
        worktree: WorktreeId,
        /// Relative to the worktree root.
        path: String,
    },
    GetScopedLayout {
        scope: LayoutScope,
    },
    ApplyScopedLayout {
        scope: LayoutScope,
        operation: OperationId,
        base_rev: u64,
        op: LayoutOp,
    },
    RunScript {
        worktree: WorktreeId,
        kind: ScriptKind,
    },
    StopScript {
        worktree: WorktreeId,
        kind: ScriptKind,
    },
    ListScripts {
        worktree: WorktreeId,
    },
    // --- M3: git actions and the forge ---
    /// Commit staged changes, or everything (including untracked) with `all`.
    GitCommit {
        worktree: WorktreeId,
        message: String,
        all: bool,
    },
    /// Push the branch, setting its upstream on the first push.
    GitPush {
        worktree: WorktreeId,
    },
    /// Merge the latest target branch from the remote into the worktree.
    GitUpdate {
        worktree: WorktreeId,
    },
    /// Refetch and return the worktree's forge state.
    ForgeStatus {
        worktree: WorktreeId,
    },
    CreatePullRequest {
        worktree: WorktreeId,
        title: String,
        body: String,
        draft: bool,
    },
    MarkPullRequestReady {
        worktree: WorktreeId,
    },
    MergePullRequest {
        worktree: WorktreeId,
        /// The pull request displayed in the confirmation dialog.
        number: u64,
        method: MergeMethod,
    },
    ListReviews {
        worktree: WorktreeId,
    },
    MutateReview {
        worktree: WorktreeId,
        base_rev: u64,
        mutation: fl_model::review::ReviewMutation,
    },
    PendingReviewBundle {
        worktree: WorktreeId,
    },
    /// Workspace-local sidebar sections (protocol 0.4).
    ListSections,
    MutateSections {
        expected_rev: u64,
        mutation: SectionMutation,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    DaemonInfo(DaemonInfo),
    Capabilities(Capabilities),
    Layout(Layout),
    ProcessInfo(ProcessInfo),
    Processes(Vec<ProcessInfo>),
    ChannelGrant(ChannelGrant),
    // --- M2 ---
    WorkspaceTree(WorkspaceTree),
    DirListing(DirListing),
    Project(Project),
    Worktree(Worktree),
    WorktreeStatus(WorktreeStatus),
    FileContent(FileContent),
    /// SHA-256 of the written file.
    Written([u8; 32]),
    FileDiff(FileDiff),
    ScriptState(ScriptState),
    Scripts(Vec<ScriptState>),
    /// Success with nothing to return (removals, stops).
    Done,
    // --- M3 ---
    ForgeState(ForgeState),
    /// The new HEAD commit after a commit.
    Committed {
        head: String,
    },
    Reviews(fl_model::review::ReviewState),
    ReviewBundle(fl_model::review::ReviewBundle),
    SectionsState(SectionsState),
}

/// Returned on the authenticated control stream before requesting channels.
/// `engine_id` is supplied by the service from fl-term's ENGINE_ID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub terminal_protocol: u16,
    pub engine_id: String,
    pub channel_schema: u16,
    pub roles: Vec<ChannelRole>,
    /// Linux CLOCK_BOOTTIME sample. Clients use a conservative RTT interval
    /// to issue absolute, non-extendable five-second input deadlines.
    pub clock_ns: u64,
}

impl Capabilities {
    pub fn validate(&self) -> Result<(), ValidationError> {
        validation::schema(
            "terminal protocol",
            self.terminal_protocol,
            TERMINAL_PROTOCOL,
        )?;
        validation::schema(
            "channel schema",
            self.channel_schema,
            crate::channel::CHANNEL_SCHEMA,
        )?;
        validation::engine(&self.engine_id)?;
        validation::bound("channel roles", self.roles.len(), 3)?;
        for (index, role) in self.roles.iter().enumerate() {
            if self.roles[..index].contains(role) {
                return Err(ValidationError::Invalid("duplicate channel role"));
            }
        }
        Ok(())
    }
}
/// Also printed as JSON by `flotillad status --json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub version: String,
    pub build: String,
    pub hostname: String,
    pub pid: u32,
    pub uptime_secs: u64,
    pub quic_port: Option<u16>,
    pub socket_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: RpcErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RpcErrorCode {
    NotFound,
    InvalidArgument,
    Unsupported,
    Internal,
    RevisionConflict,
    WrongIncarnation,
    ExpiredCapability,
    InvalidCapability,
    NotOwner,
}

impl WireMessage for ClientMsg {
    const KIND: u16 = KIND_CLIENT;

    fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::Hello(hello) => {
                validation::text("client version", &hello.client_version, MAX_TEXT_BYTES)?;
                for value in [&hello.device_id, &hello.token].into_iter().flatten() {
                    validation::text("hello credential", value, MAX_TEXT_BYTES)?;
                }
                Ok(())
            }
            Self::Rpc { req, .. } => req.validate(),
            Self::BindChannel(hello) => hello.validate(),
            Self::Ping { .. } => Ok(()),
        }
    }
}

impl WireMessage for ServerMsg {
    const KIND: u16 = KIND_SERVER;

    fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::Welcome(value) => {
                for text in [&value.daemon_version, &value.build, &value.hostname] {
                    validation::text("welcome text", text, MAX_TEXT_BYTES)?;
                }
                Ok(())
            }
            Self::RpcResult {
                result: Ok(response),
                ..
            } => response.validate(),
            Self::RpcResult {
                result: Err(error), ..
            } => validation::text("rpc error", &error.message, MAX_TEXT_BYTES),
            Self::Event(event) => validation::daemon_event(event),
            _ => Ok(()),
        }
    }
}

impl Request {
    pub fn validate(&self) -> Result<(), ValidationError> {
        use validation::{bound, path, text};
        match self {
            Self::MutateReview {
                worktree, mutation, ..
            } => mutation
                .validate(*worktree)
                .map_err(ValidationError::Invalid),
            Self::ApplyLayout { op, .. } | Self::ApplyScopedLayout { op, .. } => op
                .validate()
                .map_err(|_| ValidationError::Invalid("layout operation")),
            Self::ListDir { path: p } | Self::AddProject { path: p, .. } => path(p),
            Self::CloneProject { url, dest, .. } => {
                text("clone url", url, MAX_TEXT_BYTES)?;
                path(dest)
            }
            Self::CreateWorktree { branch, base, .. } => {
                for value in [branch, base].into_iter().flatten() {
                    text("branch", value, MAX_TEXT_BYTES)?;
                }
                Ok(())
            }
            Self::SetWorktreeLabel { label, .. } => text("label", label, MAX_TEXT_BYTES),
            Self::ReadFile { path: p, max_bytes } => {
                bound("read size", *max_bytes as usize, validation::MAX_FILE_BYTES)?;
                path(p)
            }
            Self::WriteFile { path: p, bytes, .. } => {
                bound("file bytes", bytes.len(), validation::MAX_FILE_BYTES)?;
                path(p)
            }
            Self::FileDiff { path: p, .. } => text("diff path", p, validation::MAX_PATH_BYTES),
            Self::RunScript { kind, .. } | Self::StopScript { kind, .. } => {
                validation::script_kind(kind)
            }
            Self::GitCommit { message, .. } => {
                if message.trim().is_empty() {
                    return Err(ValidationError::Invalid("empty commit message"));
                }
                text("commit message", message, validation::MAX_MESSAGE_BYTES)
            }
            Self::CreatePullRequest { title, body, .. } => {
                if title.trim().is_empty() {
                    return Err(ValidationError::Invalid("empty pull request title"));
                }
                text("pull request title", title, MAX_TEXT_BYTES)?;
                text("pull request body", body, validation::MAX_MESSAGE_BYTES)
            }
            Self::MergePullRequest { number, .. } if *number == 0 => {
                Err(ValidationError::Invalid("zero pull request number"))
            }
            Self::MutateSections { mutation, .. } => validation::section_mutation(mutation),
            _ => Ok(()),
        }
    }
}

impl Response {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::Reviews(state) => state.validate().map_err(ValidationError::Invalid),
            Self::ReviewBundle(bundle) => {
                validation::bound(
                    "review bundle threads",
                    bundle.threads.len(),
                    fl_model::review::MAX_THREADS,
                )?;
                validation::text(
                    "review bundle",
                    &bundle.prompt,
                    fl_model::review::MAX_REVIEW_BYTES + 32768,
                )
            }
            Self::Capabilities(value) => value.validate(),
            Self::Layout(value) => value
                .validate()
                .map_err(|_| ValidationError::Invalid("layout")),
            Self::ProcessInfo(value) => validation::process(value),
            Self::Processes(values) => {
                validation::bound("process count", values.len(), MAX_PROCESS_COUNT)?;
                for (index, value) in values.iter().enumerate() {
                    validation::process(value)?;
                    if values[..index]
                        .iter()
                        .any(|other| other.terminal == value.terminal)
                    {
                        return Err(ValidationError::Invalid("duplicate process terminal"));
                    }
                }
                Ok(())
            }
            Self::ChannelGrant(value) => value.validate(),
            Self::WorkspaceTree(value) => validation::workspace_tree(value),
            Self::DirListing(value) => validation::dir_listing(value),
            Self::Project(value) => validation::project(value),
            Self::Worktree(value) => validation::worktree(value),
            Self::WorktreeStatus(value) => validation::worktree_status(value),
            Self::FileContent(value) => validation::file_content(value),
            Self::FileDiff(value) => {
                validation::text("diff path", &value.path, validation::MAX_PATH_BYTES)?;
                for side in [&value.old, &value.new].into_iter().flatten() {
                    validation::file_content(side)?;
                }
                Ok(())
            }
            Self::ScriptState(value) => validation::script_kind(&value.kind),
            Self::Scripts(values) => {
                validation::bound("scripts", values.len(), validation::MAX_SCRIPTS)?;
                values
                    .iter()
                    .try_for_each(|value| validation::script_kind(&value.kind))
            }
            Self::ForgeState(value) => validation::forge_state(value),
            Self::Committed { head } => validation::text("commit", head, MAX_TEXT_BYTES),
            Self::SectionsState(value) => validation::sections_state(value),
            Self::Written(_) | Self::Done => Ok(()),
            Self::DaemonInfo(value) => {
                for text in [
                    &value.version,
                    &value.build,
                    &value.hostname,
                    &value.socket_path,
                ] {
                    validation::text("daemon info text", text, MAX_TEXT_BYTES)?;
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Static M0/0.1 postcard payloads, not generated by today's serializer.
    const M0_HELLO: &[u8] = b"\x00\x0eflotilla/0.1.0\x00\x01\x01\x03dev\x01\x03tok\x09\x09\x09\x09\x09\x09\x09\x09\x09\x09\x09\x09\x09\x09\x09\x09";
    const M0_WELCOME: &[u8] = b"\x00\x050.1.0\x03dev\x00\x01\x03vps\x01\x01\x01\x01\x01\x01\x01\x01\x01\x01\x01\x01\x01\x01\x01\x01";

    #[test]
    fn previous_minor_hello_abi_is_unchanged() {
        let frame = Frame {
            kind: KIND_CLIENT,
            payload: Bytes::from_static(M0_HELLO),
        };
        let decoded = ClientMsg::from_frame(&frame).unwrap();
        let ClientMsg::Hello(value) = &decoded else {
            panic!("expected hello")
        };
        assert_eq!(value.client_version, "flotilla/0.1.0");
        assert_eq!(value.proto, ProtoVersion { major: 0, minor: 1 });
        assert_eq!(value.device_id.as_deref(), Some("dev"));
        assert_eq!(value.token.as_deref(), Some("tok"));
        assert_eq!(value.client_session_id, [9; 16]);
        assert_eq!(decoded.to_frame().unwrap().payload.as_ref(), M0_HELLO);
    }

    #[test]
    fn previous_minor_welcome_abi_is_unchanged() {
        let frame = Frame {
            kind: KIND_SERVER,
            payload: Bytes::from_static(M0_WELCOME),
        };
        let decoded = ServerMsg::from_frame(&frame).unwrap();
        let ServerMsg::Welcome(value) = &decoded else {
            panic!("expected welcome")
        };
        assert_eq!(value.daemon_version, "0.1.0");
        assert_eq!(value.build, "dev");
        assert_eq!(value.hostname, "vps");
        assert_eq!(value.proto, ProtoVersion { major: 0, minor: 1 });
        assert_eq!(value.server_session_id, [1; 16]);
        assert_eq!(decoded.to_frame().unwrap().payload.as_ref(), M0_WELCOME);
    }

    #[test]
    fn previous_minor_control_discriminants_are_unchanged() {
        for (bytes, expected) in [
            (&b"\x01\x07"[..], ClientMsg::Ping { nonce: 7 }),
            (
                &b"\x02\x09\x00"[..],
                ClientMsg::Rpc {
                    id: 9,
                    req: Request::DaemonInfo,
                },
            ),
        ] {
            assert_eq!(
                ClientMsg::from_frame(&Frame {
                    kind: KIND_CLIENT,
                    payload: Bytes::copy_from_slice(bytes),
                })
                .unwrap(),
                expected
            );
            assert_eq!(expected.to_frame().unwrap().payload.as_ref(), bytes);
        }
        for (bytes, expected) in [
            (
                &b"\x01\x00"[..],
                ServerMsg::Rejected(RejectReason::BadToken),
            ),
            (
                &b"\x01\x01\x00\x01"[..],
                ServerMsg::Rejected(RejectReason::ProtoMismatch {
                    server: ProtoVersion { major: 0, minor: 1 },
                }),
            ),
            (
                &b"\x01\x02"[..],
                ServerMsg::Rejected(RejectReason::HelloTimeout),
            ),
            (
                &b"\x01\x03"[..],
                ServerMsg::Rejected(RejectReason::ExpectedHello),
            ),
            (&b"\x02\x07"[..], ServerMsg::Pong { nonce: 7 }),
            (
                &b"\x03\x09\x01\x02\x02no"[..],
                ServerMsg::RpcResult {
                    id: 9,
                    result: Err(RpcError {
                        code: RpcErrorCode::Unsupported,
                        message: "no".into(),
                    }),
                },
            ),
        ] {
            assert_eq!(
                ServerMsg::from_frame(&Frame {
                    kind: KIND_SERVER,
                    payload: Bytes::copy_from_slice(bytes),
                })
                .unwrap(),
                expected
            );
            assert_eq!(expected.to_frame().unwrap().payload.as_ref(), bytes);
        }
    }

    #[test]
    fn extra_payload_is_not_silently_ignored() {
        let frame = Frame {
            kind: KIND_CLIENT,
            payload: Bytes::from_static(b"\x01\x07\x00"),
        };
        assert!(matches!(
            ClientMsg::from_frame(&frame),
            Err(MsgError::TrailingBytes)
        ));
    }

    fn info() -> DaemonInfo {
        DaemonInfo {
            version: "0.1.0".into(),
            build: "dev".into(),
            hostname: "vps".into(),
            pid: 42,
            uptime_secs: 7,
            quic_port: Some(7443),
            socket_path: "/root/.flotilla/run/daemon.sock".into(),
        }
    }

    #[test]
    fn wrong_kind_is_reported() {
        let frame = ServerMsg::Pong { nonce: 1 }.to_frame().unwrap();
        assert!(matches!(
            ClientMsg::from_frame(&frame),
            Err(MsgError::WrongKind {
                expected: KIND_CLIENT,
                got: KIND_SERVER
            })
        ));
    }

    #[test]
    fn garbage_payload_is_a_codec_error() {
        let frame = Frame {
            kind: KIND_CLIENT,
            payload: Bytes::from_static(&[0xFF, 0xFF, 0xFF]),
        };
        assert!(matches!(
            ClientMsg::from_frame(&frame),
            Err(MsgError::Codec(_))
        ));
    }

    /// Pins the wire encoding: variant index 1 (`Ping`), then varint 7.
    #[test]
    fn ping_wire_bytes_are_stable() {
        assert_eq!(
            postcard::to_stdvec(&ClientMsg::Ping { nonce: 7 }).unwrap(),
            vec![1, 7]
        );
    }

    #[test]
    fn daemon_info_json_field_names_are_stable() {
        let json = serde_json::to_value(info()).unwrap();
        for key in [
            "version",
            "build",
            "hostname",
            "pid",
            "uptime_secs",
            "quic_port",
            "socket_path",
        ] {
            assert!(json.get(key).is_some(), "missing {key}");
        }
    }

    fn sections_fixture() -> SectionsState {
        use fl_model::SectionId;
        use fl_model::sections::{Section, SectionKind, SectionMembership};
        let section = Section {
            id: SectionId::from_bytes([1; 16]),
            workspace: WorkspaceId::from_bytes([2; 16]),
            name: "In progress".into(),
            order: 3,
            priority: 10,
            kind: SectionKind::Manual,
        };
        SectionsState {
            rev: 17,
            memberships: vec![SectionMembership {
                worktree: WorktreeId::from_bytes([3; 16]),
                manual: Some(section.id),
                pinned: true,
            }],
            sections: vec![section],
        }
    }

    #[test]
    fn sections_requests_snapshots_and_events_roundtrip() {
        use fl_model::sections::{SectionFilter, SectionKind};
        let state = sections_fixture();
        let section = state.sections[0].clone();
        let worktree = state.memberships[0].worktree;
        let mut filter = section.clone();
        filter.kind = SectionKind::Filter(SectionFilter {
            branch_glob: Some("feature/?*".into()),
            ..Default::default()
        });
        let mutations = [
            SectionMutation::Upsert(section.clone()),
            SectionMutation::Upsert(filter),
            SectionMutation::Delete(section.id),
            SectionMutation::Assign {
                worktree,
                section: Some(section.id),
            },
            SectionMutation::Assign {
                worktree,
                section: None,
            },
            SectionMutation::Pin {
                worktree,
                pinned: true,
            },
            SectionMutation::Pin {
                worktree,
                pinned: false,
            },
        ];
        let list = ClientMsg::Rpc {
            id: 1,
            req: Request::ListSections,
        };
        assert_eq!(
            ClientMsg::from_frame(&list.to_frame().unwrap()).unwrap(),
            list
        );
        for mutation in mutations {
            let request = ClientMsg::Rpc {
                id: 2,
                req: Request::MutateSections {
                    expected_rev: state.rev,
                    mutation,
                },
            };
            assert_eq!(
                ClientMsg::from_frame(&request.to_frame().unwrap()).unwrap(),
                request,
            );
        }
        for message in [
            ServerMsg::RpcResult {
                id: 2,
                result: Ok(Response::SectionsState(state.clone())),
            },
            ServerMsg::Event(DaemonEvent::SectionsChanged(state)),
        ] {
            assert_eq!(
                ServerMsg::from_frame(&message.to_frame().unwrap()).unwrap(),
                message,
            );
        }
        // New variants must not disturb the pre-sections protocol indices.
        assert_eq!(
            postcard::to_stdvec(&Request::GetWorkspaceTree).unwrap(),
            vec![7]
        );
        assert_eq!(postcard::to_stdvec(&Response::Done).unwrap(), vec![16]);
    }

    fn reject_section_request(section: fl_model::sections::Section) {
        let message = ClientMsg::Rpc {
            id: 1,
            req: Request::MutateSections {
                expected_rev: 0,
                mutation: SectionMutation::Upsert(section),
            },
        };
        assert!(matches!(message.to_frame(), Err(MsgError::Validation(_))));
        let frame = Frame {
            kind: KIND_CLIENT,
            payload: Bytes::from(postcard::to_stdvec(&message).unwrap()),
        };
        assert!(matches!(
            ClientMsg::from_frame(&frame),
            Err(MsgError::Validation(_))
        ));
    }

    fn reject_sections_snapshot(state: SectionsState) {
        for message in [
            ServerMsg::RpcResult {
                id: 1,
                result: Ok(Response::SectionsState(state.clone())),
            },
            ServerMsg::Event(DaemonEvent::SectionsChanged(state)),
        ] {
            assert!(matches!(message.to_frame(), Err(MsgError::Validation(_))));
            let frame = Frame {
                kind: KIND_SERVER,
                payload: Bytes::from(postcard::to_stdvec(&message).unwrap()),
            };
            assert!(matches!(
                ServerMsg::from_frame(&frame),
                Err(MsgError::Validation(_))
            ));
        }
    }

    #[test]
    fn section_names_and_filters_are_bounded_at_both_wire_boundaries() {
        use fl_model::sections::{MAX_BRANCH_GLOB, MAX_SECTION_NAME, SectionFilter, SectionKind};
        for name in [
            "".into(),
            "   ".into(),
            "bad\nname".into(),
            "x".repeat(MAX_SECTION_NAME + 1),
        ] {
            let mut section = sections_fixture().sections.remove(0);
            section.name = name;
            reject_section_request(section.clone());
            reject_sections_snapshot(SectionsState {
                sections: vec![section],
                ..Default::default()
            });
        }
        for glob in [
            "".into(),
            "bad\0glob".into(),
            "*".repeat(MAX_BRANCH_GLOB + 1),
        ] {
            let mut section = sections_fixture().sections.remove(0);
            section.kind = SectionKind::Filter(SectionFilter {
                branch_glob: Some(glob),
                ..Default::default()
            });
            reject_section_request(section.clone());
            reject_sections_snapshot(SectionsState {
                sections: vec![section],
                ..Default::default()
            });
        }
        let mut section = sections_fixture().sections.remove(0);
        section.name = "x".repeat(MAX_SECTION_NAME);
        section.kind = SectionKind::Filter(SectionFilter {
            branch_glob: Some("*".repeat(MAX_BRANCH_GLOB)),
            ..Default::default()
        });
        assert!(
            Request::MutateSections {
                expected_rev: u64::MAX,
                mutation: SectionMutation::Upsert(section),
            }
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn sections_snapshots_bound_identity_counts_and_enforce_model_integrity() {
        use fl_model::sections::{MAX_MEMBERSHIPS, MAX_SECTIONS};
        let fixture = sections_fixture();
        let mut state = fixture.clone();
        state.sections = vec![state.sections[0].clone(); MAX_SECTIONS + 1];
        assert!(matches!(
            validation::sections_state(&state),
            Err(ValidationError::Limit {
                field: "sections",
                ..
            }),
        ));
        reject_sections_snapshot(state);
        let mut state = fixture.clone();
        state.memberships = vec![state.memberships[0].clone(); MAX_MEMBERSHIPS + 1];
        assert!(matches!(
            validation::sections_state(&state),
            Err(ValidationError::Limit {
                field: "section memberships",
                ..
            }),
        ));
        reject_sections_snapshot(state);
        let mut state = fixture.clone();
        state.sections.push(state.sections[0].clone());
        reject_sections_snapshot(state);
        let mut state = fixture.clone();
        state.memberships.push(state.memberships[0].clone());
        reject_sections_snapshot(state);
        let mut state = fixture;
        state.sections.clear();
        reject_sections_snapshot(state);
    }
}
