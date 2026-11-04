//! Fast/Thorough routes, provider discovery and routed action preparation.
use super::{WorkspaceService, invalid, now_ms, rpc};
use fl_model::WorktreeId;
use fl_model::routing::{
    DispatchMode, GitTask, RoutingMutation, RoutingState, TaskDispatch, TaskDispatchState,
    TemplateContext, render_template,
};
use fl_model::workspace::DaemonEvent;
use fl_proto::{RpcError, RpcErrorCode};
use fld_agent::{ProviderDiscovery, ProviderRegistry};
use tokio::time::{Duration, timeout};

/// Provider discovery runs real CLI handshakes; keep it bounded.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(20);

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
    pub(crate) async fn chat_providers(&self) -> Result<Vec<ProviderDiscovery>, RpcError> {
        let discovered = timeout(DISCOVERY_TIMEOUT, ProviderRegistry::default().discover())
            .await
            .map_err(|_| {
                rpc(
                    RpcErrorCode::Internal,
                    "provider discovery timed out; a provider CLI did not answer its handshake",
                )
            })?;
        Ok(discovered)
    }

    /// Render a routed action. Submit modes need the chat runtime; until it
    /// exists they report exactly that instead of pretending to have run.
    pub(crate) async fn dispatch_git_task(
        self: &std::sync::Arc<Self>,
        id: WorktreeId,
        task: GitTask,
        mode: Option<DispatchMode>,
        alternate: bool,
        context: String,
    ) -> Result<TaskDispatch, RpcError> {
        let (worktree, project) = self.worktree(id).await?;
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
            self.pending_review_bundle(id)
                .await
                .ok()
                .map(|bundle| bundle.prompt)
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
        let prompt = render_template(&template, &template_context);
        if prompt.trim().is_empty() {
            return Err(invalid("the routed prompt is empty"));
        }
        let dispatch = TaskDispatch {
            id: fl_model::TaskRunId::new(),
            worktree: id,
            task,
            mode,
            session: None,
            prompt,
            state: if mode.prepares_only() {
                TaskDispatchState::Prepared
            } else {
                return Err(rpc(
                    RpcErrorCode::Unsupported,
                    "sending a routed action needs the chat runtime, which this build does not include yet; use a prepare mode or the manual dialog",
                ));
            },
            created_ms: now_ms(),
        };
        Ok(dispatch)
    }
}
