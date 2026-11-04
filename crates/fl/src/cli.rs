//! The authoritative `fl` command surface. Clap's generated help is the
//! documentation: every implemented command is listed here exactly once.
use clap::{Parser, Subcommand, ValueEnum};
use fl_model::chat::PermissionMode;
use fl_model::review::{ReviewOrigin, ReviewStatus};
use fl_model::workspace::ScriptKind;
use fl_model::{TabId, ViewId};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
}

#[derive(Debug, Parser)]
#[command(
    name = "fl",
    version,
    about = "Flotilla orchestration CLI",
    long_about = "Talk to the Flotilla daemon over its Unix socket.\n\n\
                  The caller is resolved from FLOTILLA_SESSION_ID and the current \
                  directory. Use --worktree to target another worktree."
)]
pub struct Cli {
    /// Print machine-readable JSON (same as --output json).
    #[arg(long, global = true)]
    pub json: bool,
    /// Output format; orchestration commands accept --output json.
    #[arg(
        long,
        global = true,
        value_enum,
        default_value = "text",
        value_name = "FORMAT"
    )]
    pub output: OutputFormat,
    /// Worktree id, label or path (default: the caller's worktree).
    #[arg(long, global = true, value_name = "TARGET")]
    pub worktree: Option<String>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Show daemon, caller and workspace-selection state.
    Status,
    /// Workspace queries and the durable default selection.
    Workspace {
        #[command(subcommand)]
        command: WorkspaceCommand,
    },
    /// Sidebar sections of the selected workspace.
    Section {
        #[command(subcommand)]
        command: SectionCommand,
    },
    /// Worktree state, git summaries and app-managed reviews.
    Worktree {
        #[command(subcommand)]
        command: WorktreeCommand,
    },
    /// Repository projects and their scripts.
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    /// Layout tabs in the caller's worktree.
    Tab {
        #[command(subcommand)]
        command: TabCommand,
    },
    /// Layout views, split ratios and dry-run op composition.
    Layout {
        #[command(subcommand)]
        command: LayoutCommand,
    },
    /// Chat sessions driven through provider adapters.
    Chat {
        #[command(subcommand)]
        command: ChatCommand,
    },
    /// Cross-worktree session history.
    History {
        #[command(subcommand)]
        command: HistoryCommand,
    },
    /// Orchestration messages to a chat agent.
    ///
    /// Only chat sessions are addressable in this slice; terminal-only agent
    /// delivery is not implemented yet and is refused explicitly.
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    /// Print a Flotilla-authored instruction guide.
    ///
    /// With no topic, lists the topics. Works without a daemon.
    Instructions {
        /// Guide topic.
        #[arg(value_enum)]
        topic: Option<InstructionTopic>,
    },
    /// Durable per-scope coordination state with compare-and-set and locks.
    CoordinationState {
        #[command(subcommand)]
        command: CoordinationCommand,
    },
    /// Team runs: 1-8 role chats, one lead fan-in.
    Team {
        #[command(subcommand)]
        command: TeamCommand,
    },
    /// Record one normalized provider hook event for a chat session.
    Hook {
        /// Hook name, e.g. PreToolUse, PostToolUse, Stop or notify.
        event: String,
        /// Chat session (default: FLOTILLA_SESSION_ID or the caller's session).
        #[arg(long)]
        session: Option<Uuid>,
        /// Hook JSON payload; `-` reads stdin (the default when stdin is piped).
        #[arg(long, value_name = "JSON")]
        payload: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum InstructionTopic {
    Orchestration,
    Layout,
    Worktree,
    Review,
    Browser,
}

impl InstructionTopic {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Orchestration => "orchestration",
            Self::Layout => "layout",
            Self::Worktree => "worktree",
            Self::Review => "review",
            Self::Browser => "browser",
        }
    }
}

/// Namespace a coordination key or lock lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
pub enum CoordinationScopeArg {
    /// The caller's worktree (default).
    #[default]
    Worktree,
    /// The caller's workspace, or the selected workspace.
    Workspace,
    /// The whole Flotilla home.
    Global,
}

#[derive(Debug, Subcommand)]
pub enum CoordinationCommand {
    /// Show a value, its version and any unexpired lock.
    Get {
        key: String,
        #[arg(long, value_enum, default_value = "worktree")]
        scope: CoordinationScopeArg,
    },
    /// Write a value; --if-version is a compare-and-set (0 creates only).
    Set {
        key: String,
        /// Value text; pass --value-file for longer content.
        #[arg(long, conflicts_with = "value_file")]
        value: Option<String>,
        /// Read the value from a file.
        #[arg(long, value_name = "PATH", conflicts_with = "value")]
        value_file: Option<String>,
        /// Expected current version (0 means create only).
        #[arg(long)]
        if_version: Option<u64>,
        #[arg(long, value_enum, default_value = "worktree")]
        scope: CoordinationScopeArg,
    },
    /// Delete a value, optionally only at an expected version.
    Delete {
        key: String,
        #[arg(long)]
        if_version: Option<u64>,
        #[arg(long, value_enum, default_value = "worktree")]
        scope: CoordinationScopeArg,
    },
    /// Print the current value and every new version until interrupted.
    Watch {
        key: String,
        #[arg(long, value_enum, default_value = "worktree")]
        scope: CoordinationScopeArg,
        /// Exit after this many seconds (0 waits until interrupted).
        #[arg(long, default_value_t = 0, value_name = "SECONDS")]
        timeout: u64,
    },
    /// Acquire or extend an advisory lock.
    Lock {
        key: String,
        /// Identity that owns the lock.
        #[arg(long)]
        owner: String,
        /// Lock lifetime; minimum 1 second, maximum 24 hours.
        #[arg(long, default_value_t = 300, value_name = "SECONDS")]
        ttl_seconds: u64,
        #[arg(long, value_enum, default_value = "worktree")]
        scope: CoordinationScopeArg,
    },
    /// Release a lock held by --owner.
    Unlock {
        key: String,
        #[arg(long)]
        owner: String,
        #[arg(long, value_enum, default_value = "worktree")]
        scope: CoordinationScopeArg,
    },
}

#[derive(Debug, Subcommand)]
pub enum TeamCommand {
    /// Launch a run from a JSON file.
    Run {
        /// JSON run file: {"lead": "...", "roles": [{"label","provider","prompt"}]}.
        #[arg(long, value_name = "PATH")]
        file: String,
    },
    /// Show one run, or the newest run of the caller's worktree.
    Status {
        #[arg(long)]
        run: Option<Uuid>,
    },
    /// List the worktree's runs, newest first.
    List,
    /// Cancel a run and its live role chats.
    Cancel {
        /// Run id (default: the newest run of the caller's worktree).
        #[arg(long)]
        run: Option<Uuid>,
    },
    /// Read every role report, or submit the caller role's report.
    Report {
        /// Run id (default: the newest run of the caller's worktree).
        #[arg(long)]
        run: Option<Uuid>,
        /// Role label (default: the role whose session is FLOTILLA_SESSION_ID).
        #[arg(long)]
        role: Option<String>,
        /// Report summary (at most 16 KiB); omit to read reports instead.
        #[arg(long, conflicts_with = "summary_file")]
        summary: Option<String>,
        /// Read the summary from a file.
        #[arg(long, value_name = "PATH", conflicts_with = "summary")]
        summary_file: Option<String>,
        /// Path of a result file produced by the role.
        #[arg(long)]
        result_file: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum WorkspaceCommand {
    /// List workspaces, their project counts and the selection.
    List,
    /// Show one workspace with its projects and worktrees.
    Get {
        /// Workspace id or name (default: the selected workspace).
        workspace: Option<String>,
    },
    /// Persist the default workspace for this Flotilla home.
    Select {
        /// Workspace id or name.
        workspace: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum SectionCommand {
    /// Show section definitions, filter rules, pins and assignments.
    List,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SplitDirection {
    Right,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ReviewStatusArg {
    Open,
    Pending,
    Fix,
    Resolved,
}

impl From<ReviewStatusArg> for ReviewStatus {
    fn from(value: ReviewStatusArg) -> Self {
        match value {
            ReviewStatusArg::Open => Self::Open,
            ReviewStatusArg::Pending => Self::Pending,
            ReviewStatusArg::Fix => Self::Fix,
            ReviewStatusArg::Resolved => Self::Resolved,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ReviewOriginArg {
    User,
    Agent,
}

impl From<ReviewOriginArg> for ReviewOrigin {
    fn from(value: ReviewOriginArg) -> Self {
        match value {
            ReviewOriginArg::User => Self::User,
            ReviewOriginArg::Agent => Self::Agent,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum WorktreeCommand {
    /// Git status and changed files against the target branch.
    Status,
    /// Pull-request checks for the worktree's branch.
    Checks,
    /// Totals and per-kind counts for the worktree diff.
    DiffSummary,
    /// App-managed review threads.
    ReviewList,
    /// One review thread by id.
    ReviewGet { thread: Uuid },
    /// Add a review thread anchored to a repository-relative path.
    ReviewAdd {
        /// Repository-relative file path.
        #[arg(long)]
        path: String,
        /// First line of an inclusive 1-based range; pass with --end.
        #[arg(long)]
        start: Option<u32>,
        /// Last line of an inclusive 1-based range; pass with --start.
        #[arg(long)]
        end: Option<u32>,
        /// Comment body (1-4096 bytes).
        #[arg(long)]
        body: String,
        /// Who authored the finding.
        #[arg(long, value_enum, default_value = "user")]
        origin: ReviewOriginArg,
    },
    /// Reply to a review thread.
    ReviewReply {
        thread: Uuid,
        #[arg(long)]
        body: String,
        #[arg(long, value_enum, default_value = "user")]
        origin: ReviewOriginArg,
    },
    /// Set a review thread's status.
    ReviewSetStatus {
        thread: Uuid,
        #[arg(value_enum)]
        status: ReviewStatusArg,
    },
    /// Rename the checked-out branch.
    RenameBranch { branch: String },
    /// Set the worktree display label.
    SetLabel { label: String },
    /// Set the branch this worktree diffs against.
    SetTargetBranch {
        /// Branch name; omit with --clear.
        branch: Option<String>,
        /// Clear the target (diff against the project default).
        #[arg(long, conflicts_with = "branch")]
        clear: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum ProjectCommand {
    /// Repository setup, run and teardown scripts.
    Scripts {
        #[command(subcommand)]
        command: ScriptCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum ScriptCommand {
    /// Parse .flotilla/config.json and report its configured scripts.
    Check,
    /// Run the default run target or a named one.
    Run {
        /// Named run target.
        #[arg(value_name = "TARGET", conflicts_with_all = ["setup", "teardown"])]
        target: Option<String>,
        /// Run the setup script.
        #[arg(long, conflicts_with = "teardown")]
        setup: bool,
        /// Run the teardown script.
        #[arg(long, conflicts_with = "setup")]
        teardown: bool,
    },
    /// Show configured scripts and their run state.
    Status {
        /// Only this named run target.
        #[arg(value_name = "TARGET", conflicts_with_all = ["setup", "teardown"])]
        target: Option<String>,
        #[arg(long, conflicts_with = "teardown")]
        setup: bool,
        #[arg(long, conflicts_with = "setup")]
        teardown: bool,
    },
    /// Stop a running script.
    Stop {
        #[arg(value_name = "TARGET", conflicts_with_all = ["setup", "teardown"])]
        target: Option<String>,
        #[arg(long, conflicts_with = "teardown")]
        setup: bool,
        #[arg(long, conflicts_with = "setup")]
        teardown: bool,
    },
}

impl ScriptCommand {
    /// The concrete script kind a run/stop command names. For status, `None`
    /// means every script (no target and no setup/teardown flag).
    pub fn kind(&self) -> Option<ScriptKind> {
        fn kind_of(target: &Option<String>, setup: bool, teardown: bool) -> ScriptKind {
            match (setup, teardown, target.as_deref()) {
                (true, _, _) => ScriptKind::Setup,
                (_, true, _) => ScriptKind::Teardown,
                (_, _, Some(name)) => ScriptKind::Run(Some(name.to_owned())),
                (_, _, None) => ScriptKind::Run(None),
            }
        }
        match self {
            Self::Check => None,
            Self::Run {
                target,
                setup,
                teardown,
            } => Some(kind_of(target, *setup, *teardown)),
            Self::Status {
                target,
                setup,
                teardown,
            } if target.is_none() && !*setup && !*teardown => None,
            Self::Status {
                target,
                setup,
                teardown,
            } => Some(kind_of(target, *setup, *teardown)),
            Self::Stop {
                target,
                setup,
                teardown,
            } => Some(kind_of(target, *setup, *teardown)),
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum TabCommand {
    /// Rename the active tab, or an explicit --view/--tab.
    Title {
        #[arg(long)]
        view: Option<ViewId>,
        #[arg(long)]
        tab: Option<TabId>,
        title: String,
    },
    /// Split the active pane with a new terminal.
    Split {
        #[arg(long)]
        view: Option<ViewId>,
        #[arg(long)]
        tab: Option<TabId>,
        #[arg(long, value_enum, default_value = "right")]
        direction: SplitDirection,
        #[arg(long, default_value_t = 120)]
        columns: u16,
        #[arg(long, default_value_t = 32)]
        rows: u16,
    },
    /// Split the layout into a new view with its own tab and terminal.
    SplitView {
        #[arg(long)]
        view: Option<ViewId>,
        #[arg(long, value_enum, default_value = "right")]
        direction: SplitDirection,
        #[arg(long, default_value_t = 120)]
        columns: u16,
        #[arg(long, default_value_t = 32)]
        rows: u16,
    },
}

#[derive(Debug, Subcommand)]
pub enum LayoutCommand {
    /// List views, tabs, panes and split ratios.
    Views,
    /// Select a view; with --ratio, set the split that contains it.
    Set {
        view: ViewId,
        /// Basis points (1000-9000) for the split leading to this view.
        #[arg(long)]
        ratio: Option<u16>,
    },
    /// Close a view. The final view cannot be closed.
    Close { view: ViewId },
    /// Validate a JSON op list against the current layout. This slice never
    /// applies composed ops; set/close are the mutation commands.
    Compose {
        /// Accepted for interface compatibility; composition is always a dry run.
        #[arg(long)]
        dry_run: bool,
        /// JSON array of LayoutOp values, or `-` to read stdin.
        #[arg(long, value_name = "PATH", default_value = "-")]
        file: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum PermissionModeArg {
    Ask,
    ReadOnly,
}

impl From<PermissionModeArg> for PermissionMode {
    fn from(value: PermissionModeArg) -> Self {
        match value {
            PermissionModeArg::Ask => Self::Ask,
            PermissionModeArg::ReadOnly => Self::ReadOnly,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum ChatCommand {
    /// Providers the daemon can drive, with reasons when unavailable.
    Providers,
    /// Chat sessions in the caller's worktree.
    List,
    /// Launch a provider adapter for a new chat.
    New {
        provider: String,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        thinking: Option<String>,
        #[arg(long, value_enum, default_value = "ask")]
        permission_mode: PermissionModeArg,
    },
    /// Send a message to an idle session.
    Send {
        /// Session id (default: FLOTILLA_SESSION_ID).
        #[arg(long)]
        session: Option<Uuid>,
        /// Message text; `-` reads stdin.
        message: String,
    },
    /// Durable events from a sequence (default 0).
    Events {
        /// Session id (default: FLOTILLA_SESSION_ID).
        session: Option<Uuid>,
        #[arg(long, default_value_t = 0)]
        from: u64,
        #[arg(long, default_value_t = 64)]
        limit: u16,
    },
    /// Interrupt the current turn.
    Cancel {
        /// Session id (default: FLOTILLA_SESSION_ID).
        session: Option<Uuid>,
    },
    /// Close the session and stop its adapter.
    Close {
        /// Session id (default: FLOTILLA_SESSION_ID).
        session: Option<Uuid>,
    },
}

#[derive(Debug, Subcommand)]
pub enum HistoryCommand {
    /// All sessions newest first, or full-text search hits with --query.
    List {
        #[arg(long)]
        query: Option<String>,
        #[arg(long, default_value_t = 32)]
        limit: u16,
    },
    /// One session's durable event page.
    Get {
        session: Uuid,
        #[arg(long, default_value_t = 0)]
        from: u64,
        #[arg(long, default_value_t = 64)]
        limit: u16,
    },
}

#[derive(Debug, Subcommand)]
pub enum AgentCommand {
    /// Send a message, optionally waiting for an idle session first.
    ///
    /// Chat sessions only: terminal-only delivery is not implemented yet, and
    /// an active turn without --wait-until-idle is refused rather than queued
    /// in memory.
    Send {
        /// Message text; `-` reads stdin.
        message: String,
        /// Session id (default: FLOTILLA_SESSION_ID).
        #[arg(long)]
        session: Option<Uuid>,
        /// `id:<uuid>` or `label:<label>` in the caller's worktree.
        #[arg(long, value_name = "TARGET")]
        target: Option<String>,
        /// Deliver for the next turn; with an active turn this requires --wait-until-idle.
        #[arg(long, conflicts_with = "interrupt")]
        queue: bool,
        /// Steer the active turn. Fails when no turn is running.
        #[arg(long, conflicts_with = "queue")]
        interrupt: bool,
        /// Wait for the session to become idle before and after sending.
        #[arg(long)]
        wait_until_idle: bool,
        /// Bound for --wait-until-idle, in seconds.
        #[arg(long, default_value_t = 600, value_name = "SECONDS")]
        timeout: u64,
    },
    /// Read durable events.
    Read {
        #[arg(long)]
        session: Option<Uuid>,
        /// `id:<uuid>` or `label:<label>` in the caller's worktree.
        #[arg(long, value_name = "TARGET")]
        target: Option<String>,
        #[arg(long, default_value_t = 0)]
        from: u64,
        #[arg(long, default_value_t = 64)]
        limit: u16,
    },
    /// Interrupt the current turn.
    Interrupt {
        #[arg(long)]
        session: Option<Uuid>,
        /// `id:<uuid>` or `label:<label>` in the caller's worktree.
        #[arg(long, value_name = "TARGET")]
        target: Option<String>,
    },
    /// Close the session; --kill also interrupts the current turn first.
    Stop {
        #[arg(long)]
        session: Option<Uuid>,
        /// `id:<uuid>` or `label:<label>` in the caller's worktree.
        #[arg(long, value_name = "TARGET")]
        target: Option<String>,
        #[arg(long)]
        kill: bool,
    },
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args.iter().copied())
            .unwrap_or_else(|error| panic!("{args:?}: {error}"))
    }

    #[test]
    fn clap_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn global_json_and_output_work_after_subcommands() {
        let cli = parse(&["fl", "status", "--json"]);
        assert!(cli.json);
        let cli = parse(&["fl", "status", "--output", "json"]);
        assert_eq!(cli.output, OutputFormat::Json);
        let cli = parse(&["fl", "worktree", "status", "--worktree", "feature"]);
        assert_eq!(cli.worktree.as_deref(), Some("feature"));
    }

    #[test]
    fn every_implemented_command_parses() {
        for args in [
            vec!["fl", "status"],
            vec!["fl", "workspace", "list"],
            vec!["fl", "workspace", "get"],
            vec!["fl", "workspace", "get", "Default"],
            vec!["fl", "workspace", "select", "Default"],
            vec!["fl", "section", "list"],
            vec!["fl", "worktree", "status"],
            vec!["fl", "worktree", "checks"],
            vec!["fl", "worktree", "diff-summary"],
            vec!["fl", "worktree", "review-list"],
            vec![
                "fl",
                "worktree",
                "review-get",
                "00000000-0000-0000-0000-000000000001",
            ],
            vec![
                "fl",
                "worktree",
                "review-add",
                "--path",
                "src/lib.rs",
                "--body",
                "fix this",
            ],
            vec![
                "fl",
                "worktree",
                "review-add",
                "--path",
                "src/lib.rs",
                "--start",
                "2",
                "--end",
                "4",
                "--body",
                "fix this",
                "--origin",
                "agent",
            ],
            vec![
                "fl",
                "worktree",
                "review-reply",
                "00000000-0000-0000-0000-000000000001",
                "--body",
                "done",
            ],
            vec![
                "fl",
                "worktree",
                "review-set-status",
                "00000000-0000-0000-0000-000000000001",
                "resolved",
            ],
            vec!["fl", "worktree", "rename-branch", "feature/x"],
            vec!["fl", "worktree", "set-label", "Renamed"],
            vec!["fl", "worktree", "set-target-branch", "main"],
            vec!["fl", "worktree", "set-target-branch", "--clear"],
            vec!["fl", "project", "scripts", "check"],
            vec!["fl", "project", "scripts", "run"],
            vec!["fl", "project", "scripts", "run", "web"],
            vec!["fl", "project", "scripts", "run", "--setup"],
            vec!["fl", "project", "scripts", "status"],
            vec!["fl", "project", "scripts", "stop", "web"],
            vec!["fl", "tab", "title", "Build"],
            vec![
                "fl",
                "tab",
                "title",
                "--tab",
                "00000000-0000-0000-0000-000000000001",
                "Build",
            ],
            vec!["fl", "tab", "split"],
            vec!["fl", "tab", "split", "--direction", "down"],
            vec!["fl", "tab", "split-view"],
            vec!["fl", "layout", "views"],
            vec![
                "fl",
                "layout",
                "set",
                "00000000-0000-0000-0000-000000000001",
                "--ratio",
                "4000",
            ],
            vec![
                "fl",
                "layout",
                "close",
                "00000000-0000-0000-0000-000000000001",
            ],
            vec!["fl", "layout", "compose", "--dry-run", "--file", "ops.json"],
            vec!["fl", "chat", "providers"],
            vec!["fl", "chat", "list"],
            vec!["fl", "chat", "new", "claude"],
            vec!["fl", "chat", "send", "hello"],
            vec![
                "fl",
                "chat",
                "send",
                "--session",
                "00000000-0000-0000-0000-000000000001",
                "hello",
            ],
            vec!["fl", "chat", "events"],
            vec!["fl", "chat", "cancel"],
            vec!["fl", "chat", "close"],
            vec!["fl", "history", "list"],
            vec!["fl", "history", "list", "--query", "fix"],
            vec![
                "fl",
                "history",
                "get",
                "00000000-0000-0000-0000-000000000001",
            ],
            vec!["fl", "agent", "send", "hello"],
            vec!["fl", "agent", "send", "--interrupt", "stop"],
            vec!["fl", "agent", "send", "--queue", "--wait-until-idle", "hi"],
            vec!["fl", "agent", "read"],
            vec!["fl", "agent", "interrupt"],
            vec!["fl", "agent", "stop"],
            vec!["fl", "agent", "stop", "--kill"],
        ] {
            parse(&args);
        }
    }

    #[test]
    fn help_lists_every_implemented_command() {
        let mut root = Cli::command();
        let help = root.render_long_help().to_string();
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
            assert!(help.contains(command), "root help omits {command}:\n{help}");
        }
        let mut worktree = Cli::command();
        let worktree = worktree
            .find_subcommand_mut("worktree")
            .expect("worktree subcommand");
        let help = worktree.render_long_help().to_string();
        for command in [
            "status",
            "checks",
            "diff-summary",
            "review-list",
            "review-get",
            "review-add",
            "review-reply",
            "review-set-status",
            "rename-branch",
            "set-label",
            "set-target-branch",
        ] {
            assert!(
                help.contains(command),
                "worktree help omits {command}:\n{help}"
            );
        }
    }

    #[test]
    fn conflicting_flags_are_rejected() {
        assert!(
            Cli::try_parse_from(["fl", "project", "scripts", "run", "--setup", "--teardown"])
                .is_err()
        );
        assert!(
            Cli::try_parse_from(["fl", "agent", "send", "--queue", "--interrupt", "hi"]).is_err()
        );
        assert!(
            Cli::try_parse_from(["fl", "worktree", "set-target-branch", "main", "--clear"])
                .is_err()
        );
    }
}
