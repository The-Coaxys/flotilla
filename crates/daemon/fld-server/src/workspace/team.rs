//! Team runs (M6): launch 1–8 role chats in parallel tabs, derive role state
//! from structured chat events only, and deliver exactly one lead fan-in.
//!
//! Roles are durable rows. A daemon restart keeps the rows and every read
//! command works, but provider processes belong to the chat service and are
//! not reattached: this module never claims a role's live process survived.
use super::*;
use crate::chat::ChatNotice;
use fl_model::chat::{ChatEvent, EventRecord, SessionState, TurnOutcome};
use fl_model::team::{
    MAX_TEAM_SUMMARY_BYTES, TeamRole, TeamRoleSpec, TeamRun, TeamRunSpec, TeamState,
};
use fl_model::{PaneId, Size, TabId, TerminalId};
use std::time::Duration;

/// How long the fan-in waits for a busy lead turn to finish.
const FAN_IN_WAIT: Duration = Duration::from_secs(60);
/// Layout conflicts are retried against a freshly read revision.
const TAB_ATTEMPTS: usize = 3;

impl WorkspaceService {
    /// Create a run and launch every role concurrently.
    pub(crate) async fn team_run(
        self: &Arc<Self>,
        worktree: WorktreeId,
        spec: TeamRunSpec,
    ) -> Result<TeamRun, RpcError> {
        spec.validate().map_err(invalid)?;
        let (tree, _project) = self.worktree(worktree).await?;
        let now = now_ms();
        let lead = spec.lead_label().to_owned();
        let run = TeamRun {
            id: uuid::Uuid::new_v4(),
            worktree,
            lead,
            state: TeamState::Starting,
            roles: spec
                .roles
                .iter()
                .enumerate()
                .map(|(index, role)| TeamRole {
                    index: index as u16,
                    label: role.label.clone(),
                    provider: role.provider.clone(),
                    session: None,
                    tab: None,
                    state: TeamState::Starting,
                    report: None,
                })
                .collect(),
            created_ms: now,
            updated_ms: now,
            fan_in_sent: false,
        };
        {
            let run = run.clone();
            self.store(move |store| store.database.create_team_run(&run))
                .await?;
        }
        let mut launches = tokio::task::JoinSet::new();
        for (index, role) in spec.roles.iter().cloned().enumerate() {
            let service = self.clone();
            let run_id = run.id;
            let worktree_path = tree.path.clone();
            let lead = run.lead.clone();
            launches.spawn(async move {
                service
                    .launch_team_role(worktree, run_id, index as u16, &lead, &worktree_path, role)
                    .await
            });
        }
        while let Some(result) = launches.join_next().await {
            match result {
                Ok(Ok(())) => {}
                Ok(Err((index, label, message))) => {
                    tracing::warn!(role = %label, %message, "team role launch failed");
                    let _team = self.teams.lock().await;
                    let _ = self
                        .store(move |store| {
                            store.database.team_role_state(
                                run.id,
                                index,
                                TeamState::Failed,
                                now_ms(),
                            )
                        })
                        .await;
                }
                Err(error) => {
                    tracing::warn!(%error, "team role task panicked");
                }
            }
        }
        let run = self.settle_team(run.id).await?;
        Ok(run)
    }

    #[allow(clippy::too_many_arguments)]
    async fn launch_team_role(
        self: &Arc<Self>,
        worktree: WorktreeId,
        run: uuid::Uuid,
        index: u16,
        lead: &str,
        worktree_path: &str,
        role: TeamRoleSpec,
    ) -> std::result::Result<(), (u16, String, String)> {
        let fail = |message: String| (index, role.label.clone(), message);
        let tab_name = format!("team:{}", role.label);
        let tab = self
            .open_team_tab(worktree, &tab_name)
            .await
            .map_err(|error| fail(format!("tab: {}", error.message)))?;
        let context = role_context(run, lead, worktree_path, &role);
        let instructions = self.chat.provider(&role.provider).and_then(|spec| {
            fld_agent::supports_instructions(spec.adapter).then(|| context.clone())
        });
        // Adapters without a developer-instruction channel get the same
        // trusted context as the head of the role's first prompt.
        let prompt = if instructions.is_some() {
            role.prompt.clone()
        } else {
            format!("{context}\n\n---\n\n{}", role.prompt)
        };
        let session = self
            .chat
            .start_chat_with(
                worktree,
                role.provider.clone(),
                role.model.clone(),
                None,
                fl_model::chat::PermissionMode::Ask,
                None,
                instructions,
            )
            .await
            .map_err(|error| fail(error.message))?;
        // Record the session before the prompt can complete, so no structured
        // event is lost to a race with the durable role row.
        {
            let _team = self.teams.lock().await;
            self.store(move |store| {
                store
                    .database
                    .team_role_started(run, index, session.id, Some(tab), now_ms())
            })
            .await
            .map_err(|error| fail(error.message))?;
        }
        self.chat
            .send(session.id, prompt)
            .await
            .map_err(|error| fail(format!("prompt: {}", error.message)))?;
        Ok(())
    }

    /// One named tab per role in the target worktree. The tab owns a terminal
    /// surface; the role's chat session is recorded alongside it so a client
    /// can attach the conversation to the same tab.
    async fn open_team_tab(&self, worktree: WorktreeId, name: &str) -> Result<TabId, RpcError> {
        let scope = LayoutScope::Worktree(worktree);
        let mut last = None;
        for _ in 0..TAB_ATTEMPTS {
            let layout = self.scoped_layout(scope).await?;
            let tab = TabId::new();
            let op = LayoutOp::NewTab {
                view: layout.active,
                tab,
                pane: PaneId::new(),
                terminal: TerminalId::new(),
                name: Some(name.into()),
                size: Size::new(120, 32),
            };
            match self
                .apply_scoped_layout(scope, OperationId::new(), layout.rev, op)
                .await
            {
                Ok(_) => return Ok(tab),
                Err(error) if error.code == RpcErrorCode::RevisionConflict => {
                    last = Some(error);
                }
                Err(error) => return Err(error),
            }
        }
        Err(last.unwrap_or_else(|| internal("team tab retry exhausted")))
    }

    pub(crate) async fn team_status(
        &self,
        worktree: WorktreeId,
        run: Option<uuid::Uuid>,
    ) -> Result<TeamRun, RpcError> {
        match run {
            Some(id) => {
                let run = self.store(move |store| store.database.team_run(id)).await?;
                if run.worktree != worktree {
                    return Err(rpc(
                        RpcErrorCode::NotFound,
                        format!("team run {id} does not belong to worktree {worktree}"),
                    ));
                }
                Ok(run)
            }
            None => self
                .store(move |store| store.database.latest_team_run(worktree))
                .await?
                .ok_or_else(|| rpc(RpcErrorCode::NotFound, "this worktree has no team runs")),
        }
    }

    pub(crate) async fn team_list(&self, worktree: WorktreeId) -> Result<Vec<TeamRun>, RpcError> {
        self.store(move |store| store.database.team_runs(worktree))
            .await
    }

    pub(crate) async fn team_report(&self, run: uuid::Uuid) -> Result<TeamRun, RpcError> {
        self.store(move |store| store.database.team_run(run)).await
    }

    pub(crate) async fn team_report_submit(
        &self,
        run: uuid::Uuid,
        role: String,
        summary: String,
        result_file: Option<String>,
    ) -> Result<TeamRun, RpcError> {
        if summary.trim().is_empty() {
            return Err(invalid("team report summary is empty"));
        }
        let now = now_ms();
        self.store(move |store| {
            store
                .database
                .record_team_report(run, &role, &summary, result_file.as_deref(), now)
        })
        .await
    }

    /// Cancel every live role chat, then mark the run cancelled.
    pub(crate) async fn team_cancel(&self, run: uuid::Uuid) -> Result<TeamRun, RpcError> {
        let sessions = self
            .store(move |store| store.database.team_sessions(run))
            .await?;
        for (_index, session) in sessions {
            if let Err(error) = self.chat.cancel(session).await {
                // A role that already finished (or is not live) has nothing to
                // cancel; the durable run still becomes cancelled.
                tracing::debug!(%session, message = %error.message, "team cancel skipped a role");
            }
        }
        let _team = self.teams.lock().await;
        self.store(move |store| store.database.cancel_team_run(run, now_ms()))
            .await
    }

    /// Recompute a run from its role rows and deliver the single fan-in.
    async fn settle_team(&self, run: uuid::Uuid) -> Result<TeamRun, RpcError> {
        let (current, send_fan_in) = {
            let _team = self.teams.lock().await;
            self.store(move |store| store.database.settle_team_run(run, now_ms()))
                .await?
        };
        if send_fan_in {
            self.send_fan_in(&current).await;
        }
        Ok(current)
    }

    async fn send_fan_in(&self, run: &TeamRun) {
        let Some(lead) = run.roles.iter().find(|role| role.label == run.lead) else {
            tracing::warn!(run = %run.id, "team run has no lead role");
            return;
        };
        let Some(session) = lead.session else {
            return;
        };
        let finished = run
            .roles
            .iter()
            .filter(|role| role.state == TeamState::Finished)
            .count();
        let failed: Vec<&str> = run
            .roles
            .iter()
            .filter(|role| role.state == TeamState::Failed)
            .map(|role| role.label.as_str())
            .collect();
        let mut text = format!(
            "Team run {} settled {}: {finished}/{} roles finished.",
            run.id,
            run.state.as_str(),
            run.roles.len()
        );
        if !failed.is_empty() {
            text.push_str(&format!(" Failed roles: {}.", failed.join(", ")));
        }
        text.push_str(&format!(
            " Read every report with `fl team report --run {}`.",
            run.id
        ));
        if let Err(error) = self.chat.send_when_idle(session, text, FAN_IN_WAIT).await {
            tracing::warn!(run = %run.id, message = %error.message, "team fan-in was not delivered");
        }
    }

    /// Subscribe to structured chat events and settle roles from them.
    pub(crate) fn watch_team_notices(self: &Arc<Self>) {
        let service = self.clone();
        let mut notices = self.chat.subscribe();
        let cancel = self.cancel.clone();
        tokio::spawn(async move {
            loop {
                let notice = tokio::select! {
                    _ = cancel.cancelled() => return,
                    notice = notices.recv() => match notice {
                        Ok(notice) => notice,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                            tracing::warn!(skipped, "team watcher lagged; durable run state still settles on the next event");
                            continue;
                        }
                        Err(_) => return,
                    },
                };
                let ChatNotice::Event { session, record } = notice else {
                    continue;
                };
                service.on_team_chat_event(session, &record).await;
            }
        });
    }

    async fn on_team_chat_event(&self, session: uuid::Uuid, record: &EventRecord) {
        let Some(state) = role_state_from_event(&record.event) else {
            return;
        };
        let found = self
            .store(move |store| store.database.team_role_by_session(session))
            .await;
        let Ok(Some((run, index))) = found else {
            return;
        };
        let (_, send_fan_in) = {
            let _team = self.teams.lock().await;
            match self
                .store(move |store| store.database.team_role_state(run, index, state, now_ms()))
                .await
            {
                Ok(result) => result,
                Err(error) => {
                    tracing::warn!(message = %error.message, "team role state was not recorded");
                    return;
                }
            }
        };
        if send_fan_in {
            match self.team_report(run).await {
                Ok(current) => self.send_fan_in(&current).await,
                Err(error) => {
                    tracing::warn!(message = %error.message, "team run could not be read")
                }
            }
        }
    }
}

/// Only conclusive structured facts move a role; hooks and heuristics cannot.
fn role_state_from_event(event: &ChatEvent) -> Option<TeamState> {
    match event {
        ChatEvent::TurnEnd { outcome, .. } => match outcome {
            TurnOutcome::Completed => Some(TeamState::Finished),
            TurnOutcome::Failed => Some(TeamState::Failed),
            // A cancelled turn is not a finished role; the role keeps working.
            TurnOutcome::Cancelled => None,
        },
        ChatEvent::Error { fatal: true, .. } => Some(TeamState::Failed),
        ChatEvent::StateChanged(SessionState::Failed { .. }) => Some(TeamState::Failed),
        ChatEvent::StateChanged(SessionState::Closed) => Some(TeamState::Failed),
        ChatEvent::ProcessExited {
            expected: false, ..
        } => Some(TeamState::Failed),
        _ => None,
    }
}

fn role_context(run: uuid::Uuid, lead: &str, worktree_path: &str, role: &TeamRoleSpec) -> String {
    format!(
        "Flotilla team role context:\n\
         - Run: {run}\n\
         - Role: {} ({})\n\
         - Lead: {lead}\n\
         - Worktree: {worktree_path}\n\
         - Report contract: when your work is done, run\n  \
           `fl team report --run {run} --summary \"<what you did and found>\"`\n  \
           and optionally `--result-file <path>`. The summary is at most {MAX_TEAM_SUMMARY_BYTES} bytes.\n\
         - Only the lead receives the fan-in message; every role's report stays readable with `fl team report --run {run}`.\n\
         - Run `fl instructions orchestration` for the trusted contract. Coordinate through `fl coordination-state` (compare-and-set and advisory locks), not by assuming.",
        role.label, role.provider
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_model::chat::ChatEvent;
    use fl_model::coordination::HookFact;
    use fl_model::coordination::HookFactKind as Kind;

    #[test]
    fn only_conclusive_structured_events_settle_a_role() {
        assert_eq!(
            role_state_from_event(&ChatEvent::TurnEnd {
                duration_ms: None,
                turns: 1,
                usage: Default::default(),
                model: None,
                outcome: TurnOutcome::Completed,
            }),
            Some(TeamState::Finished)
        );
        assert_eq!(
            role_state_from_event(&ChatEvent::TurnEnd {
                duration_ms: None,
                turns: 1,
                usage: Default::default(),
                model: None,
                outcome: TurnOutcome::Failed,
            }),
            Some(TeamState::Failed)
        );
        // A cancelled turn leaves the role running; only the run can be cancelled.
        assert_eq!(
            role_state_from_event(&ChatEvent::TurnEnd {
                duration_ms: None,
                turns: 1,
                usage: Default::default(),
                model: None,
                outcome: TurnOutcome::Cancelled,
            }),
            None
        );
        for kind in [Kind::TurnStarted, Kind::TurnEnded, Kind::Notification] {
            assert_eq!(
                role_state_from_event(&ChatEvent::HookFact(HookFact {
                    kind,
                    detail: "hook".into(),
                })),
                None,
                "a hook must never settle a role"
            );
        }
    }
}
