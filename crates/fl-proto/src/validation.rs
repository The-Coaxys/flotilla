//! Semantic wire bounds, enforced by `WireMessage` at both boundaries.
//! These checks do not authorize sessions, incarnations, leases or transfers.
use fl_model::{ProcessInfo, ProcessState, Size};

pub const MAX_INPUT_BYTES: usize = 64 * 1024;
pub const MAX_OUTPUT_BYTES: usize = 64 * 1024;
pub const MAX_RECORDS_PER_BATCH: usize = 256;
/// Entire postcard-encoded RecordBatch server message, including its envelope.
pub const MAX_RECORD_BATCH_BYTES: usize = 512 * 1024;
pub const MAX_BULK_CHUNK_BYTES: usize = 512 * 1024;
pub const MAX_CHANNEL_LIFETIME_MS: u32 = 60_000;
pub const MAX_ENGINE_ID_BYTES: usize = 128;
/// Matches the terminal checkpoint codec's 512 MiB encoded-byte ceiling,
/// without taking a dependency on the engine.
pub const MAX_CHECKPOINT_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_PROCESS_COUNT: usize = 128;
pub const MAX_ARG_COUNT: usize = 256;
pub const MAX_TEXT_BYTES: usize = 4096;
/// One OSC 52 clipboard store, decoded.
pub const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;
pub const MAX_ARGV_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    #[error("{field} exceeds {max} (got {actual})")]
    Limit {
        field: &'static str,
        max: u64,
        actual: u64,
    },
    #[error("invalid payload: {0}")]
    InvalidPayload(String),
    #[error("invalid {0}")]
    Invalid(&'static str),
    #[error("unsupported {field}: {got} (expected {expected})")]
    Schema {
        field: &'static str,
        expected: u16,
        got: u16,
    },
}

pub(crate) fn bound(field: &'static str, actual: usize, max: usize) -> Result<(), ValidationError> {
    bound_u64(field, actual as u64, max as u64)
}

pub(crate) fn bound_u64(field: &'static str, actual: u64, max: u64) -> Result<(), ValidationError> {
    if actual > max {
        Err(ValidationError::Limit { field, max, actual })
    } else {
        Ok(())
    }
}

pub(crate) fn schema(field: &'static str, got: u16, expected: u16) -> Result<(), ValidationError> {
    if got != expected {
        Err(ValidationError::Schema {
            field,
            got,
            expected,
        })
    } else {
        Ok(())
    }
}

pub(crate) fn text(field: &'static str, value: &str, max: usize) -> Result<(), ValidationError> {
    bound(field, value.len(), max)?;
    if value.contains('\0') {
        return Err(ValidationError::Invalid(field));
    }
    Ok(())
}

pub(crate) fn engine(value: &str) -> Result<(), ValidationError> {
    text("engine_id", value, MAX_ENGINE_ID_BYTES)?;
    if value.trim().is_empty() || value.chars().any(char::is_control) {
        return Err(ValidationError::Invalid("engine_id"));
    }
    Ok(())
}

pub(crate) fn size(value: Size) -> Result<(), ValidationError> {
    value
        .validate()
        .map(|_| ())
        .map_err(|_| ValidationError::Invalid("size"))
}

/// Launch bounds also apply before spawning a child, so its metadata remains
/// representable on every control stream after a successful launch.
pub fn launch(argv: &[String], cwd: &str) -> Result<(), ValidationError> {
    bound("argv count", argv.len(), MAX_ARG_COUNT)?;
    let mut total = 0usize;
    for arg in argv {
        text("argv argument", arg, MAX_TEXT_BYTES)?;
        total += arg.len();
    }
    bound("argv bytes", total, MAX_ARGV_BYTES)?;
    text("cwd", cwd, MAX_TEXT_BYTES)
}

pub(crate) fn process(value: &ProcessInfo) -> Result<(), ValidationError> {
    size(value.size)?;
    launch(&value.argv, &value.cwd)?;
    if let ProcessState::Backpressured {
        reason,
        quota_bytes,
        ..
    } = &value.state
    {
        text("backpressure reason", reason, MAX_TEXT_BYTES)?;
        if *quota_bytes == 0 {
            return Err(ValidationError::Invalid("retention quota"));
        }
    }
    Ok(())
}

// --- M2 bounds -------------------------------------------------------------

/// One absolute path on the daemon host.
pub const MAX_PATH_BYTES: usize = 4096;
/// Largest file read or written in one request (the editor's limit).
pub const MAX_FILE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_DIR_ENTRIES: usize = 10_000;
pub const MAX_CHANGED_FILES: usize = 20_000;
pub const MAX_WORKSPACES: usize = 64;
pub const MAX_PROJECTS: usize = 512;
pub const MAX_WORKTREES: usize = 4096;
pub const MAX_SCRIPTS: usize = 64;

pub fn path(value: &str) -> Result<(), ValidationError> {
    text("path", value, MAX_PATH_BYTES)?;
    if !value.starts_with('/') {
        return Err(ValidationError::Invalid("path is not absolute"));
    }
    if value.bytes().any(|byte| byte == 0) {
        return Err(ValidationError::Invalid("path contains NUL"));
    }
    Ok(())
}

pub fn script_kind(kind: &fl_model::workspace::ScriptKind) -> Result<(), ValidationError> {
    match kind {
        fl_model::workspace::ScriptKind::Run(Some(name)) => {
            text("run target", name, MAX_TEXT_BYTES)
        }
        _ => Ok(()),
    }
}

pub fn project(value: &fl_model::workspace::Project) -> Result<(), ValidationError> {
    text("project name", &value.name, MAX_TEXT_BYTES)?;
    path(&value.path)?;
    path(&value.worktree_root)?;
    if let Some(branch) = &value.default_branch {
        text("branch", branch, MAX_TEXT_BYTES)?;
    }
    Ok(())
}

pub fn worktree(value: &fl_model::workspace::Worktree) -> Result<(), ValidationError> {
    path(&value.path)?;
    text("label", &value.label, MAX_TEXT_BYTES)?;
    for branch in [&value.branch, &value.target].into_iter().flatten() {
        text("branch", branch, MAX_TEXT_BYTES)?;
    }
    Ok(())
}

pub fn workspace_tree(value: &fl_model::workspace::WorkspaceTree) -> Result<(), ValidationError> {
    bound("workspaces", value.workspaces.len(), MAX_WORKSPACES)?;
    bound("projects", value.projects.len(), MAX_PROJECTS)?;
    bound("worktrees", value.worktrees.len(), MAX_WORKTREES)?;
    for workspace in &value.workspaces {
        text("workspace name", &workspace.name, MAX_TEXT_BYTES)?;
    }
    value.projects.iter().try_for_each(project)?;
    value.worktrees.iter().try_for_each(worktree)
}

pub fn dir_listing(value: &fl_model::workspace::DirListing) -> Result<(), ValidationError> {
    path(&value.path)?;
    bound("directory entries", value.entries.len(), MAX_DIR_ENTRIES)?;
    for entry in &value.entries {
        text("entry name", &entry.name, MAX_PATH_BYTES)?;
    }
    Ok(())
}

pub fn file_content(value: &fl_model::workspace::FileContent) -> Result<(), ValidationError> {
    text("file path", &value.path, MAX_PATH_BYTES)?;
    bound("file bytes", value.bytes.len(), MAX_FILE_BYTES)
}

pub fn worktree_status(value: &fl_model::workspace::WorktreeStatus) -> Result<(), ValidationError> {
    bound("changed files", value.files.len(), MAX_CHANGED_FILES)?;
    for file in &value.files {
        text("changed path", &file.path, MAX_PATH_BYTES)?;
        if let Some(old) = &file.old_path {
            text("changed path", old, MAX_PATH_BYTES)?;
        }
    }
    for value in [
        &value.summary.branch,
        &value.summary.head,
        &value.summary.upstream,
    ]
    .into_iter()
    .flatten()
    {
        text("git ref", value, MAX_TEXT_BYTES)?;
    }
    Ok(())
}

pub fn daemon_event(value: &fl_model::workspace::DaemonEvent) -> Result<(), ValidationError> {
    use fl_model::workspace::DaemonEvent;
    match value {
        DaemonEvent::WorktreeStatus(status) => worktree_status(status),
        DaemonEvent::ScriptChanged(state) => script_kind(&state.kind),
        DaemonEvent::ForgeChanged(state) => forge_state(state),
        DaemonEvent::SectionsChanged(state) => sections_state(state),
        DaemonEvent::RoutingChanged(state) => {
            state.validate().map_err(ValidationError::Invalid)?;
            Ok(())
        }
        DaemonEvent::TaskDispatchChanged(dispatch) => task_dispatch(dispatch),
        DaemonEvent::WorkspaceTreeChanged { .. }
        | DaemonEvent::LayoutChanged { .. }
        | DaemonEvent::ReviewChanged { .. } => Ok(()),
    }
}

/// A routed dispatch carries its rendered prompt; bound it like a message.
pub fn task_dispatch(value: &fl_model::routing::TaskDispatch) -> Result<(), ValidationError> {
    text("dispatch prompt", &value.prompt, MAX_MESSAGE_BYTES)?;
    if let fl_model::routing::TaskDispatchState::Failed { message } = &value.state {
        text("dispatch failure", message, MAX_TEXT_BYTES)?;
    }
    Ok(())
}

/// Commit messages and pull request bodies.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;
pub const MAX_CHECKS: usize = 512;

pub fn forge_state(value: &fl_model::forge::ForgeState) -> Result<(), ValidationError> {
    if let Some(reason) = &value.unavailable {
        text("forge reason", reason, MAX_TEXT_BYTES)?;
    }
    if let Some(pr) = &value.pr {
        for field in [&pr.url, &pr.title, &pr.base, &pr.head] {
            text("pull request", field, MAX_TEXT_BYTES)?;
        }
        bound("checks", pr.checks.len(), MAX_CHECKS)?;
        for check in &pr.checks {
            text("check name", &check.name, MAX_TEXT_BYTES)?;
            if let Some(url) = &check.url {
                text("check url", url, MAX_TEXT_BYTES)?;
            }
        }
    }
    Ok(())
}

pub fn section_mutation(
    value: &fl_model::sections::SectionMutation,
) -> Result<(), ValidationError> {
    match value {
        fl_model::sections::SectionMutation::Upsert(section) => {
            section.validate().map_err(ValidationError::Invalid)
        }
        // Typed identities have a fixed 16-byte representation.
        fl_model::sections::SectionMutation::Delete(_)
        | fl_model::sections::SectionMutation::Assign { .. }
        | fl_model::sections::SectionMutation::Pin { .. } => Ok(()),
    }
}

pub fn sections_state(value: &fl_model::sections::SectionsState) -> Result<(), ValidationError> {
    use fl_model::sections::{MAX_MEMBERSHIPS, MAX_SECTIONS};
    bound("sections", value.sections.len(), MAX_SECTIONS)?;
    bound(
        "section memberships",
        value.memberships.len(),
        MAX_MEMBERSHIPS,
    )?;
    value.validate().map_err(ValidationError::Invalid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_model::{IncarnationId, LogCursor, TerminalId};

    fn info() -> ProcessInfo {
        ProcessInfo {
            terminal: TerminalId::from_bytes([1; 16]),
            incarnation: IncarnationId::from_bytes([2; 16]),
            pid: 1,
            size: Size::new(80, 24),
            argv: vec!["sh".into()],
            cwd: "/".into(),
            state: ProcessState::Running,
            tail: LogCursor::default(),
            echo: true,
        }
    }

    #[test]
    fn process_metadata_cannot_bypass_wire_geometry_and_string_bounds() {
        let mut value = info();
        assert!(process(&value).is_ok());
        value.size.cell_width = u16::MAX;
        assert!(process(&value).is_err());
        value.size = Size::new(80, 24);
        value.argv = vec!["x".repeat(MAX_TEXT_BYTES); MAX_ARGV_BYTES / MAX_TEXT_BYTES];
        assert!(process(&value).is_ok());
        value.argv.push("x".into());
        assert!(matches!(
            process(&value),
            Err(ValidationError::Limit {
                field: "argv bytes",
                ..
            })
        ));
        value.argv = vec!["x".into(); MAX_ARG_COUNT + 1];
        assert!(matches!(
            process(&value),
            Err(ValidationError::Limit {
                field: "argv count",
                ..
            })
        ));
        value.argv.clear();
        value.cwd = "/bad\0path".into();
        assert!(process(&value).is_err());
    }

    #[test]
    fn process_list_rejects_duplicate_terminal_identity() {
        let value = info();
        assert!(
            crate::Response::Processes(vec![value.clone(), value])
                .validate()
                .is_err()
        );
    }

    #[test]
    fn capabilities_reject_unknown_schema_duplicate_roles_and_invalid_engine() {
        let mut value = crate::Capabilities {
            terminal_protocol: crate::TERMINAL_PROTOCOL,
            channel_schema: crate::CHANNEL_SCHEMA,
            engine_id: "engine".into(),
            roles: vec![crate::ChannelRole::TerminalHot],
            clock_ns: 1,
        };
        assert!(value.validate().is_ok());
        value.terminal_protocol += 1;
        assert!(matches!(
            value.validate(),
            Err(ValidationError::Schema { .. })
        ));
        value.terminal_protocol = crate::TERMINAL_PROTOCOL;
        value.roles.push(crate::ChannelRole::TerminalHot);
        assert!(value.validate().is_err());
        value.roles.pop();
        value.engine_id = " \n".into();
        assert!(value.validate().is_err());
        value.engine_id = "x".repeat(MAX_ENGINE_ID_BYTES + 1);
        assert!(value.validate().is_err());
    }
}
