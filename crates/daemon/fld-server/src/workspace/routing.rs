//! Fast/Thorough routes, provider discovery and routed action execution.
//!
//! Prepare modes render the prompt without starting anything. Submit modes run
//! the real adapter: structured Git tasks use one bounded JSON-schema call and
//! apply only validated output; interactive tasks run a chat session and stream
//! its durable events. A routed action never grants a permission on the user's
//! behalf, and a dispatch reports Prepared/Running/Completed/Failed truthfully.
use super::{WorkspaceService, blocking, invalid, now_ms, rpc};
use fl_model::WorktreeId;
use fl_model::chat::{ChatEvent, ChatSession, PermissionMode, SessionState, TurnOutcome};
use fl_model::routing::{
    DispatchMode, GitTask, RouteSpec, RoutingMutation, RoutingState, TaskDispatch,
    TaskDispatchState, TemplateContext, render_template,
};
use fl_model::workspace::DaemonEvent;
use fl_proto::{RpcError, RpcErrorCode};
use fld_agent::{ProviderDiscovery, ProviderRegistry};
use tokio::sync::broadcast;
use tokio::time::{Duration, timeout};
use tokio_util::sync::CancellationToken;

use crate::chat::{ChatNotice, ChatService, bounded_with_marker};
use crate::terminal::bounded;

/// Provider discovery runs real CLI handshakes; keep it bounded.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(20);
/// Cached discovery avoids one handshake per `ListChatProviders` call.
const DISCOVERY_TTL: Duration = Duration::from_secs(30);
const MAX_STRUCTURED_TITLE: usize = 200;
const MAX_STRUCTURED_BODY: usize = 60_000;
/// In-memory dispatch history; older entries fall off (prompts included).
const MAX_DISPATCH_HISTORY: usize = 128;
const MAX_PROMPT_BYTES: usize = fl_proto::validation::MAX_MESSAGE_BYTES;

impl WorkspaceService {
    pub(crate) async fn routing_state(&self) -> Result<RoutingState, RpcError> {
        self.store(|store| store.database.routing_state()).await
    }

    pub(crate) async fn mutate_routing(
        &self,
        expected_rev: u64,
        mutation: RoutingMutation,
    ) -> Result<RoutingState, RpcError> {
        let _structure = self.structure.lock().await;
        let state = self
            .store(move |store| store.database.mutate_routing(expected_rev, mutation))
            .await?;
        self.publish(DaemonEvent::RoutingChanged(state.clone()));
        Ok(state)
    }

    /// Providers the daemon can actually drive, with the reason when it cannot.
    /// Discovery performs real CLI handshakes, so a short cache prevents one
    /// handshake per `ListChatProviders` call.
    pub(crate) async fn chat_providers(&self) -> Result<Vec<ProviderDiscovery>, RpcError> {
        let mut cache = self.providers.lock().await;
        if let Some((at, providers)) = cache.as_ref()
            && at.elapsed() < DISCOVERY_TTL
        {
            return Ok(providers.clone());
        }
        let discovered = timeout(DISCOVERY_TIMEOUT, ProviderRegistry::default().discover())
            .await
            .map_err(|_| {
                rpc(
                    RpcErrorCode::Internal,
                    "provider discovery timed out; a provider CLI did not answer its handshake",
                )
            })?;
        *cache = Some((std::time::Instant::now(), discovered.clone()));
        Ok(discovered)
    }

    /// Render a routed action and, for submit modes, execute it.
    pub(crate) async fn dispatch_git_task(
        self: &std::sync::Arc<Self>,
        id: WorktreeId,
        task: GitTask,
        mode: Option<DispatchMode>,
        alternate: bool,
        context: String,
    ) -> Result<TaskDispatch, RpcError> {
        let (worktree, project) = self.worktree(id).await?;
        if project.kind != fl_model::workspace::ProjectKind::Git {
            return Err(invalid("a plain folder has no routed Git actions"));
        }
        let routing = self.routing_state().await?;
        let Some(route) = routing.route(task, project.id, project.workspace) else {
            return Err(rpc(
                RpcErrorCode::Unsupported,
                format!(
                    "no route for {}; configure one for the {:?} group",
                    task.label(),
                    task.group()
                ),
            ));
        };
        let mode = mode.unwrap_or_else(|| route.mode(alternate));
        let base = worktree.target.clone().or(project.default_branch.clone());
        // Fill the placeholders from real state: failing checks, pending
        // reviews and the worktree's diff summary. Anything unknown stays a
        // literal placeholder rather than invented text.
        let checks = if matches!(task, GitTask::FixCi | GitTask::Investigate) {
            match self.forge_status(id).await {
                Ok(state) => state.pr.and_then(|pr| {
                    (!pr.checks.is_empty()).then(|| {
                        pr.checks
                            .iter()
                            .map(|check| format!("{}: {:?}", check.name, check.status))
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                }),
                Err(_) => None,
            }
        } else {
            None
        };
        let reviews = if task == GitTask::FixComments {
            self.pending_review_bundle(id).await.ok().map(|bundle| {
                if bundle.threads.is_empty() || bundle.prompt.trim().is_empty() {
                    "No pending review threads are recorded for this worktree.".to_owned()
                } else {
                    bundle.prompt
                }
            })
        } else {
            None
        };
        let diff_root = std::path::PathBuf::from(&worktree.path);
        let diff = tokio::task::spawn_blocking(move || {
            fld_git::cmd::run(&diff_root, &["diff", "--stat", "HEAD"])
                .ok()
                .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_owned())
                .filter(|text| !text.is_empty())
        })
        .await
        .ok()
        .flatten();
        let template_context = TemplateContext {
            branch: worktree.branch.clone(),
            base,
            diff,
            checks,
            reviews,
            task: Some(context).filter(|text| !text.trim().is_empty()),
        };
        // A repository's own templates apply below the stored scopes.
        let root = std::path::PathBuf::from(&worktree.path);
        let repo_prompt = tokio::task::spawn_blocking(move || {
            fld_git::config::load_config(&root)
                .map(|config| config.and_then(|config| config.prompts.get(task.id()).cloned()))
        })
        .await
        .map_err(|error| {
            rpc(
                RpcErrorCode::Internal,
                format!("background task failed: {error}"),
            )
        })?
        .map_err(|error| invalid(format!("invalid .flotilla/config.json: {error}")))?;
        let template =
            routing.template_with_repo(task, project.id, project.workspace, repo_prompt.as_deref());
        // Wire-bound the rendered prompt at construction; the adapter call and
        // the displayed dispatch use exactly this bounded prompt.
        let prompt = bounded_with_marker(
            &render_template(&template, &template_context),
            MAX_PROMPT_BYTES,
        );
        if prompt.trim().is_empty() {
            return Err(invalid("the routed prompt is empty"));
        }
        let mut dispatch = TaskDispatch {
            id: fl_model::TaskRunId::new(),
            worktree: id,
            task,
            mode,
            session: None,
            prompt: prompt.clone(),
            state: if mode.prepares_only() {
                TaskDispatchState::Prepared
            } else {
                TaskDispatchState::Running
            },
            created_ms: now_ms(),
        };
        if mode.prepares_only() {
            self.remember_dispatch(dispatch.clone());
            return Ok(dispatch);
        }
        if ChatService::is_structured(task) {
            self.remember_dispatch(dispatch.clone());
            self.spawn_structured_dispatch(dispatch.clone(), route, worktree.path.clone(), prompt);
            return Ok(dispatch);
        }
        // Interactive task: a real provider session, reused only when it is
        // idle and speaks the routed provider.
        let session = match mode {
            DispatchMode::ReuseSubmit | DispatchMode::ReusePrepare => {
                self.idle_chat(id, &route).await
            }
            _ => None,
        };
        let session = match session {
            Some(session) => session,
            None => {
                self.chat
                    .start_chat(
                        id,
                        route.provider.clone(),
                        route.model.clone(),
                        route.thinking.clone(),
                        PermissionMode::Ask,
                        None,
                    )
                    .await?
                    .id
            }
        };
        dispatch.session = Some(session);
        self.remember_dispatch(dispatch.clone());
        let notices = self.chat.subscribe();
        match self.chat.send(session, prompt).await {
            Ok(()) => {
                self.spawn_interactive_monitor(dispatch.id, session, notices);
                Ok(dispatch)
            }
            Err(error) => {
                let failed = TaskDispatchState::Failed {
                    message: bounded(error.message.clone()),
                };
                self.update_dispatch(dispatch.id, |dispatch| {
                    dispatch.state = failed.clone();
                    true
                });
                dispatch.state = failed;
                Ok(dispatch)
            }
        }
    }

    fn remember_dispatch(&self, dispatch: TaskDispatch) {
        {
            let mut dispatches = self.dispatches.lock().expect("dispatches poisoned");
            dispatches.insert(dispatch.id, dispatch.clone());
            prune_dispatch_history(&mut dispatches);
        }
        self.publish(DaemonEvent::TaskDispatchChanged(dispatch));
    }

    fn update_dispatch(
        &self,
        id: fl_model::TaskRunId,
        update: impl FnOnce(&mut TaskDispatch) -> bool,
    ) {
        let mut dispatches = self.dispatches.lock().expect("dispatches poisoned");
        let Some(dispatch) = dispatches.get_mut(&id) else {
            return;
        };
        if !update(dispatch) {
            return;
        }
        let changed = dispatch.clone();
        drop(dispatches);
        self.publish(DaemonEvent::TaskDispatchChanged(changed));
    }

    fn finish_dispatch(
        &self,
        id: fl_model::TaskRunId,
        state: TaskDispatchState,
        prompt: Option<String>,
    ) {
        let state = match state {
            TaskDispatchState::Failed { message } => TaskDispatchState::Failed {
                message: bounded(message),
            },
            other => other,
        };
        self.dispatch_cancels
            .lock()
            .expect("dispatch cancels poisoned")
            .remove(&id);
        self.update_dispatch(id, |dispatch| {
            let mut changed = dispatch.state != state;
            if changed {
                dispatch.state = state;
            }
            if let Some(prompt) = prompt
                && dispatch.prompt != prompt
            {
                dispatch.prompt = prompt;
                changed = true;
            }
            changed
        });
    }

    /// Register the cancellation handle of a running dispatch.
    fn register_dispatch_cancel(&self, id: fl_model::TaskRunId) -> CancellationToken {
        let token = CancellationToken::new();
        self.dispatch_cancels
            .lock()
            .expect("dispatch cancels poisoned")
            .insert(id, token.clone());
        token
    }

    /// Cancel every pending dispatch of a removed worktree.
    fn cancel_dispatches_for(&self, worktree: WorktreeId) {
        let ids: Vec<_> = self
            .dispatches
            .lock()
            .expect("dispatches poisoned")
            .iter()
            .filter(|(_, dispatch)| dispatch.worktree == worktree)
            .map(|(id, _)| *id)
            .collect();
        let tokens: Vec<_> = {
            let mut cancels = self
                .dispatch_cancels
                .lock()
                .expect("dispatch cancels poisoned");
            ids.iter().filter_map(|id| cancels.remove(id)).collect()
        };
        for token in tokens {
            token.cancel();
        }
        for id in ids {
            self.finish_dispatch(
                id,
                TaskDispatchState::Failed {
                    message: "the worktree was removed; the routed task was stopped".into(),
                },
                None,
            );
        }
    }

    /// An idle chat is reusable only when it speaks the route's provider and
    /// exact model, uses the routed permission mode, and has no unsupported
    /// thinking setting. Reusing a differently configured chat would silently
    /// change the routed action's policy.
    async fn idle_chat(&self, worktree: WorktreeId, route: &RouteSpec) -> Option<uuid::Uuid> {
        let sessions = self.chat.list_sessions(worktree).await.ok()?;
        sessions
            .into_iter()
            .rev()
            .find(|session| reusable_for_route(session, route))
            .map(|session| session.id)
    }

    fn spawn_interactive_monitor(
        self: &std::sync::Arc<Self>,
        id: fl_model::TaskRunId,
        session: uuid::Uuid,
        mut notices: broadcast::Receiver<ChatNotice>,
    ) {
        let weak = std::sync::Arc::downgrade(self);
        let cancel = self.register_dispatch_cancel(id);
        tokio::spawn(async move {
            loop {
                let notice = tokio::select! {
                    _ = cancel.cancelled() => {
                        if let Some(service) = weak.upgrade() {
                            service.finish_dispatch(
                                id,
                                TaskDispatchState::Failed {
                                    message: "the worktree was removed; the routed task was stopped"
                                        .into(),
                                },
                                None,
                            );
                        }
                        return;
                    }
                    notice = notices.recv() => notice,
                };
                match notice {
                    Ok(ChatNotice::Event {
                        session: event_session,
                        record,
                    }) if event_session == session => match record.event {
                        ChatEvent::TurnEnd {
                            outcome: TurnOutcome::Completed,
                            ..
                        } => {
                            if let Some(service) = weak.upgrade() {
                                service.finish_dispatch(id, TaskDispatchState::Completed, None);
                            }
                            return;
                        }
                        ChatEvent::TurnEnd { outcome, .. } => {
                            if let Some(service) = weak.upgrade() {
                                service.finish_dispatch(
                                    id,
                                    TaskDispatchState::Failed {
                                        message: format!(
                                            "the provider turn ended with {outcome:?}"
                                        ),
                                    },
                                    None,
                                );
                            }
                            return;
                        }
                        ChatEvent::Error {
                            message,
                            fatal: true,
                            ..
                        } => {
                            if let Some(service) = weak.upgrade() {
                                service.finish_dispatch(
                                    id,
                                    TaskDispatchState::Failed { message },
                                    None,
                                );
                            }
                            return;
                        }
                        ChatEvent::ProcessExited {
                            expected: false, ..
                        } => {
                            if let Some(service) = weak.upgrade() {
                                service.finish_dispatch(
                                    id,
                                    TaskDispatchState::Failed {
                                        message: "the provider exited before the task finished"
                                            .into(),
                                    },
                                    None,
                                );
                            }
                            return;
                        }
                        _ => {}
                    },
                    Ok(ChatNotice::Session(session_state))
                        if session_state.id == session
                            && matches!(
                                session_state.status,
                                SessionState::Closed | SessionState::Failed { .. }
                            ) =>
                    {
                        if let Some(service) = weak.upgrade() {
                            service.finish_dispatch(
                                id,
                                TaskDispatchState::Failed {
                                    message: "the chat session ended before the task finished"
                                        .into(),
                                },
                                None,
                            );
                        }
                        return;
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        if let Some(service) = weak.upgrade() {
                            service.finish_dispatch(
                                id,
                                TaskDispatchState::Failed {
                                    message: "the chat event stream lagged; the outcome is unknown"
                                        .into(),
                                },
                                None,
                            );
                        }
                        return;
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        });
    }

    fn spawn_structured_dispatch(
        self: &std::sync::Arc<Self>,
        dispatch: TaskDispatch,
        route: RouteSpec,
        cwd: String,
        prompt: String,
    ) {
        let weak = std::sync::Arc::downgrade(self);
        let task = dispatch.task;
        let worktree = dispatch.worktree;
        let id = dispatch.id;
        let schema = ChatService::output_schema(task).to_owned();
        let cancel = self.register_dispatch_cancel(id);
        tokio::spawn(async move {
            let Some(service) = weak.upgrade() else {
                return;
            };
            let result = tokio::select! {
                _ = cancel.cancelled() => Err(invalid(
                    "the worktree was removed; the routed task was stopped",
                )),
                result = service.chat.structured_call(
                    &route.provider,
                    route.model.clone(),
                    std::path::PathBuf::from(cwd),
                    prompt,
                    schema,
                ) => result,
            };
            // Do not apply a completed answer into a worktree that is already
            // being removed.
            let result = if cancel.is_cancelled() {
                Err(invalid(
                    "the worktree was removed; the routed task was stopped",
                ))
            } else {
                result
            };
            let outcome = match result {
                Ok(json) => service.apply_structured(task, worktree, &json).await,
                Err(error) => Err(error),
            };
            match outcome {
                Ok(enriched) => service.finish_dispatch(id, TaskDispatchState::Completed, enriched),
                Err(error) => service.finish_dispatch(
                    id,
                    TaskDispatchState::Failed {
                        message: error.message,
                    },
                    None,
                ),
            }
        });
    }

    /// Validate the provider's structured answer, then apply it. No Git or
    /// forge mutation happens before every field passes validation.
    async fn apply_structured(
        self: &std::sync::Arc<Self>,
        task: GitTask,
        worktree: WorktreeId,
        json: &str,
    ) -> Result<Option<String>, RpcError> {
        let value: serde_json::Value = serde_json::from_str(json)
            .map_err(|error| invalid(format!("provider returned invalid JSON: {error}")))?;
        let field = |name: &str| -> Result<String, RpcError> {
            let text = value
                .get(name)
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .ok_or_else(|| invalid(format!("provider answer is missing {name}")))?;
            Ok(text.to_owned())
        };
        match task {
            GitTask::NameWorktree => {
                let branch = field("branch")?;
                let label = field("label")?;
                if branch.len() > 200 || label.len() > 120 {
                    return Err(invalid("provider answer exceeds the name limits"));
                }
                let (worktree_row, project) = self.worktree(worktree).await?;
                let repo = std::path::PathBuf::from(&project.path);
                let branch_for_check = branch.clone();
                blocking(move || fld_git::worktree::validate_branch_name(&repo, &branch_for_check))
                    .await?;
                let path = std::path::PathBuf::from(&worktree_row.path);
                let branch_for_rename = branch.clone();
                blocking(move || fld_git::ops::rename_branch(&path, &branch_for_rename)).await?;
                let _structure = self.structure.lock().await;
                let branch_rev = self
                    .store({
                        let branch = branch.clone();
                        move |store| store.database.set_worktree_branch(worktree, Some(&branch))
                    })
                    .await?;
                let (_, label_rev) = self
                    .store({
                        let label = label.clone();
                        move |store| store.database.set_worktree_label(worktree, &label)
                    })
                    .await?;
                // Either update may be a no-op; publish only a real revision.
                let rev = label_rev.max(branch_rev.unwrap_or(0));
                if rev > 0 {
                    self.publish(DaemonEvent::WorkspaceTreeChanged { rev });
                }
                Ok(None)
            }
            GitTask::Commit | GitTask::CommitPush => {
                let message = field("message")?;
                if message.len() > MAX_STRUCTURED_BODY {
                    return Err(invalid("provider commit message is too long"));
                }
                // Routed commits use the same default as the adaptive Git
                // button's commit dialog: every change in the worktree.
                self.git_commit(worktree, message, true).await?;
                if task == GitTask::CommitPush {
                    self.git_push(worktree).await?;
                }
                Ok(None)
            }
            GitTask::CreatePr => {
                let title = field("title")?;
                let body = value
                    .get("body")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let draft = value
                    .get("draft")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                if title.len() > MAX_STRUCTURED_TITLE || body.len() > MAX_STRUCTURED_BODY {
                    return Err(invalid("provider pull request answer exceeds the limits"));
                }
                self.create_pull_request(worktree, title, body, draft)
                    .await?;
                Ok(None)
            }
            GitTask::EnhancePrompt => {
                let prompt = field("prompt")?;
                if prompt.len() > MAX_STRUCTURED_BODY {
                    return Err(invalid("provider prompt answer is too long"));
                }
                Ok(Some(prompt))
            }
            _ => Err(invalid("not a structured routed task")),
        }
    }
}

/// Keep only the newest in-memory dispatches. The cap includes their prompts,
/// so a long-lived daemon cannot accumulate rendered prompts indefinitely.
fn prune_dispatch_history(
    dispatches: &mut std::collections::HashMap<fl_model::TaskRunId, TaskDispatch>,
) {
    if dispatches.len() <= MAX_DISPATCH_HISTORY {
        return;
    }
    let excess = dispatches.len() - MAX_DISPATCH_HISTORY;
    let mut oldest: Vec<_> = dispatches
        .iter()
        .map(|(id, dispatch)| (*id, dispatch.created_ms))
        .collect();
    oldest.sort_by_key(|(_, created)| *created);
    for (id, _) in oldest.into_iter().take(excess) {
        dispatches.remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_model::chat::StatusSource;

    fn dispatch(created_ms: u64) -> TaskDispatch {
        TaskDispatch {
            id: fl_model::TaskRunId::new(),
            worktree: WorktreeId::new(),
            task: GitTask::Commit,
            mode: DispatchMode::NewSubmit,
            session: None,
            prompt: "prompt".into(),
            state: TaskDispatchState::Prepared,
            created_ms,
        }
    }

    fn session(route: &RouteSpec) -> ChatSession {
        ChatSession {
            id: uuid::Uuid::new_v4(),
            worktree: WorktreeId::new(),
            provider: route.provider.clone(),
            label: "chat".into(),
            status: SessionState::Idle,
            status_source: StatusSource::Structured,
            model: route.model.clone(),
            thinking: route.thinking.clone(),
            permission_mode: PermissionMode::Ask,
            resume: Some(fl_model::chat::ResumeTarget {
                provider_id: route.provider.clone(),
                adapter: fl_model::chat::AdapterKind::ClaudeStream,
                native_id: "native".into(),
                cwd: "/tmp".into(),
            }),
            seq: 0,
            created_ms: 1,
            updated_ms: 1,
        }
    }

    #[test]
    fn routed_reuse_requires_the_exact_route_configuration() {
        let route = RouteSpec::new("claude");
        let matching = session(&route);
        assert!(reusable_for_route(&matching, &route));
        let mut different_model = matching.clone();
        different_model.model = Some("other".into());
        assert!(!reusable_for_route(&different_model, &route));
        let mut read_only = matching.clone();
        read_only.permission_mode = PermissionMode::ReadOnly;
        assert!(!reusable_for_route(&read_only, &route));
        let mut running = matching.clone();
        running.status = SessionState::Running;
        assert!(!reusable_for_route(&running, &route));
        let mut thinking = route.clone();
        thinking.thinking = Some("high".into());
        assert!(!reusable_for_route(&matching, &thinking));
    }

    #[test]
    fn dispatch_history_keeps_only_the_newest_entries() {
        let mut dispatches = std::collections::HashMap::new();
        for created in 0..(MAX_DISPATCH_HISTORY as u64 + 7) {
            let value = dispatch(created);
            dispatches.insert(value.id, value);
            prune_dispatch_history(&mut dispatches);
        }
        assert_eq!(dispatches.len(), MAX_DISPATCH_HISTORY);
        assert!(dispatches.values().all(|item| item.created_ms >= 7));
    }
}

/// Routed reuse is allowed only for a session configured exactly like the
/// route: same provider, model, permission mode and thinking setting.
fn reusable_for_route(session: &ChatSession, route: &RouteSpec) -> bool {
    session.provider == route.provider
        && session.status == SessionState::Idle
        && session.resume.is_some()
        && session.model == route.model
        && session.permission_mode == PermissionMode::Ask
        && session.thinking == route.thinking
}
