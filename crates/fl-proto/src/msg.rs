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
    /// Fast/Thorough routes and prompt templates (protocol 0.4).
    GetRouting,
    MutateRouting {
        expected_rev: u64,
        mutation: fl_model::routing::RoutingMutation,
    },
    /// Providers the daemon can actually drive through a chat adapter.
    ListChatProviders,
    /// Render and (for submit modes) run a routed Git action.
    DispatchGitTask {
        worktree: WorktreeId,
        task: fl_model::routing::GitTask,
        /// Overrides the configured click / alternate-click mode.
        mode: Option<fl_model::routing::DispatchMode>,
        alternate: bool,
        /// Extra user context appended through `{{task}}`.
        context: String,
    },
    // --- M4: chat sessions, transcripts, approvals and the inbox (0.4) ---
    /// Sessions of one worktree, ordered by creation time.
    ListChatSessions {
        worktree: WorktreeId,
    },
    /// Durable events from `from` (inclusive).
    GetChatEvents {
        session: uuid::Uuid,
        from: u64,
        limit: u16,
    },
    /// Launch a provider adapter for a new chat in `worktree`.
    StartChat {
        worktree: WorktreeId,
        provider: String,
        model: Option<String>,
        thinking: Option<String>,
        permission_mode: fl_model::chat::PermissionMode,
        /// Continue a validated native session instead of starting fresh.
        resume: Option<fl_model::chat::ResumeTarget>,
    },
    SendChat {
        session: uuid::Uuid,
        text: String,
    },
    SteerChat {
        session: uuid::Uuid,
        text: String,
    },
    CancelChat {
        session: uuid::Uuid,
    },
    ApproveChat {
        session: uuid::Uuid,
        /// The approval request ID, valid only while it is pending.
        request: String,
        decision: fl_model::chat::ApprovalDecision,
    },
    AnswerChat {
        session: uuid::Uuid,
        request: String,
        response: fl_model::chat::QuestionResponse,
    },
    CloseChat {
        session: uuid::Uuid,
    },
    /// Full-text search over durable transcripts.
    SearchChats {
        query: String,
        limit: u16,
    },
    ListInbox,
    DismissInbox {
        item: uuid::Uuid,
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
    RoutingState(fl_model::routing::RoutingState),
    ChatProviders(Vec<fl_model::chat::ProviderDiscovery>),
    TaskDispatch(fl_model::routing::TaskDispatch),
    // --- M4 ---
    ChatSessions(Vec<fl_model::chat::ChatSession>),
    ChatEvents(fl_model::chat::ChatEventPage),
    ChatHits(Vec<fl_model::chat::ChatHit>),
    Inbox(Vec<fl_model::chat::InboxItem>),
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
            Self::MutateRouting { mutation, .. } => {
                mutation.validate().map_err(ValidationError::Invalid)
            }
            Self::ListChatSessions { worktree } => {
                validation::non_nil_id("chat worktree", worktree.as_bytes())
            }
            Self::GetChatEvents { session, limit, .. } => {
                validation::non_nil("chat session", *session)?;
                if *limit == 0 {
                    return Err(ValidationError::Invalid("chat page limit"));
                }
                bound(
                    "chat page limit",
                    *limit as usize,
                    fl_model::chat::MAX_CHAT_MESSAGES,
                )
            }
            Self::StartChat {
                worktree,
                provider,
                model,
                thinking,
                resume,
                ..
            } => {
                validation::non_nil_id("chat worktree", worktree.as_bytes())?;
                text(
                    "chat provider",
                    provider,
                    validation::MAX_CHAT_PROVIDER_BYTES,
                )?;
                if provider.trim().is_empty() {
                    return Err(ValidationError::Invalid("chat provider"));
                }
                for value in [model, thinking].into_iter().flatten() {
                    text("chat setting", value, validation::MAX_CHAT_MODEL_BYTES)?;
                }
                if let Some(resume) = resume {
                    validation::chat_resume(resume)?;
                }
                Ok(())
            }
            Self::SendChat {
                session,
                text: value,
            }
            | Self::SteerChat {
                session,
                text: value,
            } => {
                validation::non_nil("chat session", *session)?;
                if value.trim().is_empty() {
                    return Err(ValidationError::Invalid("empty chat message"));
                }
                text("chat message", value, fl_model::chat::MAX_CHAT_TEXT)
            }
            Self::CancelChat { session } | Self::CloseChat { session } => {
                validation::non_nil("chat session", *session)
            }
            Self::ApproveChat {
                session, request, ..
            } => {
                validation::non_nil("chat session", *session)?;
                text(
                    "approval request",
                    request,
                    validation::MAX_CHAT_REQUEST_BYTES,
                )?;
                if request.trim().is_empty() {
                    return Err(ValidationError::Invalid("approval request"));
                }
                Ok(())
            }
            Self::AnswerChat {
                session,
                request,
                response,
            } => {
                validation::non_nil("chat session", *session)?;
                text(
                    "question request",
                    request,
                    validation::MAX_CHAT_REQUEST_BYTES,
                )?;
                if request.trim().is_empty() {
                    return Err(ValidationError::Invalid("question request"));
                }
                validation::question_response(response)
            }
            Self::SearchChats { query, limit } => {
                if query.trim().is_empty() {
                    return Err(ValidationError::Invalid("empty chat query"));
                }
                text("chat query", query, validation::MAX_CHAT_QUERY_BYTES)?;
                if *limit == 0 {
                    return Err(ValidationError::Invalid("chat search limit"));
                }
                bound(
                    "chat search limit",
                    *limit as usize,
                    fl_model::chat::MAX_CHAT_HISTORY,
                )
            }
            Self::DismissInbox { item } => validation::non_nil("inbox item", *item),
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
            Self::RoutingState(value) => value.validate().map_err(ValidationError::Invalid),
            Self::ChatProviders(values) => {
                validation::bound("chat providers", values.len(), 64)?;
                for value in values {
                    validation::text("provider id", &value.provider.id, MAX_TEXT_BYTES)?;
                    validation::text("provider name", &value.provider.name, MAX_TEXT_BYTES)?;
                    if let Some(reason) = &value.unavailable_reason {
                        validation::text("provider reason", reason, MAX_TEXT_BYTES)?;
                    }
                }
                Ok(())
            }
            Self::TaskDispatch(value) => validation::task_dispatch(value),
            Self::ChatSessions(values) => {
                validation::bound(
                    "chat sessions",
                    values.len(),
                    fl_model::chat::MAX_CHAT_HISTORY,
                )?;
                for (index, value) in values.iter().enumerate() {
                    validation::chat_session(value)?;
                    if values[..index].iter().any(|other| other.id == value.id) {
                        return Err(ValidationError::Invalid("duplicate chat session"));
                    }
                }
                Ok(())
            }
            Self::ChatEvents(value) => validation::chat_events(value),
            Self::ChatHits(values) => validation::chat_hits(values),
            Self::Inbox(values) => validation::inbox_items(values),
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

    fn chat_fixture() -> fl_model::chat::ChatSession {
        use fl_model::chat::{PermissionMode, StatusSource};
        fl_model::chat::ChatSession {
            id: uuid::Uuid::from_bytes([7; 16]),
            worktree: WorktreeId::from_bytes([8; 16]),
            provider: "claude".into(),
            label: "Claude Code 1234abcd".into(),
            status: fl_model::chat::SessionState::Idle,
            status_source: StatusSource::Structured,
            model: Some("sonnet".into()),
            thinking: Some("default".into()),
            permission_mode: PermissionMode::Ask,
            resume: Some(fl_model::chat::ResumeTarget {
                provider_id: "claude".into(),
                adapter: fl_model::chat::AdapterKind::ClaudeStream,
                native_id: "native-session".into(),
                cwd: std::path::PathBuf::from("/srv/wt"),
            }),
            seq: 9,
            created_ms: 10,
            updated_ms: 20,
        }
    }

    fn chat_event(session: uuid::Uuid, sequence: u64) -> fl_model::chat::EventRecord {
        use fl_model::chat::{ChatEvent, TextUpdate};
        fl_model::chat::EventRecord {
            schema_version: fl_model::chat::CHAT_SCHEMA_VERSION,
            session_id: session,
            incarnation: uuid::Uuid::from_bytes([9; 16]),
            sequence,
            timestamp_ms: sequence,
            native_session_id: Some("native-session".into()),
            turn_id: None,
            event: ChatEvent::AssistantText {
                id: format!("m{sequence}"),
                update: TextUpdate::Final,
                text: "hello".into(),
            },
        }
    }

    #[test]
    fn chat_requests_responses_and_events_roundtrip() {
        use fl_model::chat::{ApprovalDecision, QuestionResponse};
        let session = chat_fixture();
        let id = session.id;
        let worktree = session.worktree;
        let requests = vec![
            Request::ListChatSessions { worktree },
            Request::GetChatEvents {
                session: id,
                from: 1,
                limit: 64,
            },
            Request::StartChat {
                worktree,
                provider: "claude".into(),
                model: Some("sonnet".into()),
                thinking: None,
                permission_mode: fl_model::chat::PermissionMode::Ask,
                resume: session.resume.clone(),
            },
            Request::SendChat {
                session: id,
                text: "hi".into(),
            },
            Request::SteerChat {
                session: id,
                text: "more".into(),
            },
            Request::CancelChat { session: id },
            Request::ApproveChat {
                session: id,
                request: "req".into(),
                decision: ApprovalDecision::AllowOnce,
            },
            Request::AnswerChat {
                session: id,
                request: "q".into(),
                response: QuestionResponse::default(),
            },
            Request::CloseChat { session: id },
            Request::SearchChats {
                query: "hello".into(),
                limit: 10,
            },
            Request::ListInbox,
            Request::DismissInbox {
                item: uuid::Uuid::from_bytes([3; 16]),
            },
        ];
        for (index, request) in requests.into_iter().enumerate() {
            let message = ClientMsg::Rpc {
                id: index as u64,
                req: request,
            };
            assert_eq!(
                ClientMsg::from_frame(&message.to_frame().unwrap()).unwrap(),
                message
            );
        }
        let page = fl_model::chat::ChatEventPage {
            session: id,
            incarnation: uuid::Uuid::from_bytes([9; 16]),
            events: vec![chat_event(id, 1), chat_event(id, 2)],
            more: true,
        };
        let hit = fl_model::chat::ChatHit {
            session: id,
            worktree,
            sequence: 1,
            snippet: "hello".into(),
        };
        let inbox = fl_model::chat::InboxItem {
            id: uuid::Uuid::from_bytes([4; 16]),
            session: id,
            worktree,
            kind: fl_model::chat::InboxKind::Approval,
            state: fl_model::chat::InboxState::Unread,
            title: "Approval needed".into(),
            created_ms: 3,
        };
        for response in [
            Response::ChatSessions(vec![session]),
            Response::ChatEvents(page),
            Response::ChatHits(vec![hit]),
            Response::Inbox(vec![inbox]),
        ] {
            let message = ServerMsg::RpcResult {
                id: 1,
                result: Ok(response),
            };
            assert_eq!(
                ServerMsg::from_frame(&message.to_frame().unwrap()).unwrap(),
                message
            );
        }
        let event = ServerMsg::Event(fl_model::workspace::DaemonEvent::ChatEventAppended {
            session: id,
            worktree,
            incarnation: uuid::Uuid::from_bytes([9; 16]),
            sequence: 2,
        });
        assert_eq!(
            ServerMsg::from_frame(&event.to_frame().unwrap()).unwrap(),
            event
        );
    }

    #[test]
    fn chat_payloads_are_bounded_at_both_wire_boundaries() {
        use fl_model::chat::MAX_CHAT_TEXT;
        let session = chat_fixture();
        let id = session.id;
        for request in [
            Request::SendChat {
                session: id,
                text: String::new(),
            },
            Request::SendChat {
                session: id,
                text: "   ".into(),
            },
            Request::SteerChat {
                session: id,
                text: "x".repeat(MAX_CHAT_TEXT + 1),
            },
            Request::GetChatEvents {
                session: id,
                from: 1,
                limit: 0,
            },
            Request::SearchChats {
                query: "  ".into(),
                limit: 1,
            },
            Request::ListChatSessions {
                worktree: WorktreeId::from_bytes([0; 16]),
            },
            Request::CloseChat {
                session: uuid::Uuid::nil(),
            },
        ] {
            assert!(request.validate().is_err(), "{request:?} was accepted");
        }
        let mut page = fl_model::chat::ChatEventPage {
            session: id,
            incarnation: uuid::Uuid::from_bytes([9; 16]),
            events: vec![chat_event(id, 2), chat_event(id, 1)],
            more: false,
        };
        assert!(Response::ChatEvents(page.clone()).validate().is_err());
        page.events = (1..=(fl_model::chat::MAX_CHAT_MESSAGES as u64 + 1))
            .map(|sequence| chat_event(id, sequence))
            .collect();
        assert!(Response::ChatEvents(page).validate().is_err());
    }
}
