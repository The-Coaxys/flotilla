//! Team runs: 1–8 roles, each with its own chat session and layout tab, and
//! one short fan-in message to the lead when the last role reaches a terminal
//! state (M6).
//!
//! A run is durable. Roles are launched with a trusted role context; their
//! state is derived only from structured chat events, never guessed from a
//! hook or a process check. A report is a bounded summary plus an optional
//! result file path.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{TabId, WorktreeId};

pub const MAX_TEAM_ROLES: usize = 8;
pub const MIN_TEAM_ROLES: usize = 1;
pub const MAX_TEAM_LABEL_BYTES: usize = 64;
pub const MAX_TEAM_PROVIDER_BYTES: usize = 256;
pub const MAX_TEAM_MODEL_BYTES: usize = 1024;
/// The report contract bounds the summary at 16 KiB.
pub const MAX_TEAM_SUMMARY_BYTES: usize = 16 * 1024;
pub const MAX_TEAM_RESULT_FILE_BYTES: usize = 4096;
/// Runs listed for one worktree, newest first.
pub const MAX_TEAM_RUNS: usize = 32;

/// The constrained lifecycle shared by a run and each of its roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TeamState {
    Starting,
    Running,
    Finished,
    Failed,
    Cancelled,
}

impl TeamState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Finished | Self::Failed | Self::Cancelled)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Finished => "finished",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// One role in a `team run` file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamRoleSpec {
    pub label: String,
    pub provider: String,
    /// Optional model override; absent JSON becomes `None`.
    #[serde(default)]
    pub model: Option<String>,
    pub prompt: String,
}

/// The JSON accepted by `fl team run --file`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamRunSpec {
    /// Role label that receives the fan-in. Defaults to the first role.
    /// Always on the wire: postcard is sequential and cannot skip an Option.
    #[serde(default)]
    pub lead: Option<String>,
    pub roles: Vec<TeamRoleSpec>,
}

/// A role's completion report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamReport {
    pub summary: String,
    /// Absolute or worktree-relative path supplied by the role.
    pub result_file: Option<String>,
    pub submitted_ms: u64,
}

/// One durable role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamRole {
    pub index: u16,
    pub label: String,
    pub provider: String,
    pub session: Option<Uuid>,
    /// The layout tab created for this role in the target worktree.
    pub tab: Option<TabId>,
    pub state: TeamState,
    pub report: Option<TeamReport>,
}

/// One durable run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamRun {
    pub id: Uuid,
    pub worktree: WorktreeId,
    pub lead: String,
    pub state: TeamState,
    pub roles: Vec<TeamRole>,
    pub created_ms: u64,
    pub updated_ms: u64,
    /// True once the single fan-in message was handed to the lead's session.
    pub fan_in_sent: bool,
}

/// Human-readable problem with a run file. The CLI prints it as-is.
pub type SpecError = String;

impl TeamRunSpec {
    pub fn validate(&self) -> Result<(), SpecError> {
        if !(MIN_TEAM_ROLES..=MAX_TEAM_ROLES).contains(&self.roles.len()) {
            return Err(format!(
                "a team run needs {MIN_TEAM_ROLES}-{MAX_TEAM_ROLES} roles, got {}",
                self.roles.len()
            ));
        }
        let mut labels: Vec<&str> = Vec::with_capacity(self.roles.len());
        for role in &self.roles {
            if role.label.trim().is_empty() {
                return Err("every role needs a non-empty label".into());
            }
            if role.label.len() > MAX_TEAM_LABEL_BYTES {
                return Err(format!(
                    "role label `{}` exceeds {MAX_TEAM_LABEL_BYTES} bytes",
                    role.label
                ));
            }
            if role.label.contains('\0') {
                return Err("role labels cannot contain NUL".into());
            }
            if labels.contains(&role.label.as_str()) {
                return Err(format!("duplicate role label `{}`", role.label));
            }
            labels.push(&role.label);
            if role.provider.trim().is_empty() || role.provider.len() > MAX_TEAM_PROVIDER_BYTES {
                return Err(format!(
                    "role `{}` needs a provider of 1-{MAX_TEAM_PROVIDER_BYTES} bytes",
                    role.label
                ));
            }
            if let Some(model) = &role.model
                && (model.trim().is_empty() || model.len() > MAX_TEAM_MODEL_BYTES)
            {
                return Err(format!(
                    "role `{}` model must be 1-{MAX_TEAM_MODEL_BYTES} bytes when set",
                    role.label
                ));
            }
            if role.prompt.trim().is_empty() {
                return Err(format!("role `{}` needs a non-empty prompt", role.label));
            }
            if role.prompt.len() > crate::chat::MAX_CHAT_TEXT {
                return Err(format!(
                    "role `{}` prompt exceeds {} bytes",
                    role.label,
                    crate::chat::MAX_CHAT_TEXT
                ));
            }
        }
        if let Some(lead) = &self.lead
            && !self.roles.iter().any(|role| &role.label == lead)
        {
            return Err(format!("lead `{lead}` is not one of the roles"));
        }
        Ok(())
    }

    /// The label that receives the fan-in.
    pub fn lead_label(&self) -> &str {
        self.lead.as_deref().unwrap_or_else(|| {
            self.roles
                .first()
                .map(|role| role.label.as_str())
                .unwrap_or("")
        })
    }
}
