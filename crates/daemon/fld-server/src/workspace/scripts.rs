//! Setup/run/teardown scripts from `.flotilla/config.json`. Each run is a
//! daemon terminal (`/bin/sh -lc '<commands joined with &&>'` in the
//! worktree) that belongs to no layout but is attachable like any terminal.
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fl_model::workspace::{
    DaemonEvent, Project, ProjectScriptCheck, ScriptKind, ScriptRunState, ScriptState, Worktree,
};
use fl_model::{ProcessState, Size, WorktreeId};
use fl_proto::RpcError;
use fl_proto::validation::{MAX_SCRIPTS, MAX_TEXT_BYTES};
use fld_git::config::RepoConfig;
use fld_store::TerminalKey;

use super::{WorkspaceService, internal, invalid};
use crate::terminal::bounded;

const SCRIPT_SIZE: Size = Size::new(120, 32);
/// Teardown must finish within this before the worktree is removed anyway.
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(120);
const STOP_COMMANDS_TIMEOUT: Duration = Duration::from_secs(30);
/// Upper bound for a terminated script to report its exit.
const STOP_WAIT: Duration = Duration::from_secs(10);

/// Durable script identity within a worktree.
fn kind_key(kind: &ScriptKind) -> String {
    match kind {
        ScriptKind::Setup => "setup".into(),
        ScriptKind::Teardown => "teardown".into(),
        ScriptKind::Run(None) => "run".into(),
        ScriptKind::Run(Some(name)) => format!("run:{name}"),
    }
}

fn parse_kind(key: &str) -> Option<ScriptKind> {
    match key {
        "setup" => Some(ScriptKind::Setup),
        "teardown" => Some(ScriptKind::Teardown),
        "run" => Some(ScriptKind::Run(None)),
        _ => key
            .strip_prefix("run:")
            .map(|name| ScriptKind::Run(Some(name.to_owned()))),
    }
}

fn run_state(state: Option<ProcessState>) -> ScriptRunState {
    match state {
        Some(
            ProcessState::Running | ProcessState::Backpressured { .. } | ProcessState::Draining(_),
        ) => ScriptRunState::Running,
        Some(ProcessState::Exited(status)) => ScriptRunState::Exited { code: status.code },
        // Lost, never launched or no longer attached.
        Some(ProcessState::Lost) | None => ScriptRunState::Exited { code: None },
    }
}

/// Commands and stop commands configured for `kind`.
fn commands(config: &RepoConfig, kind: &ScriptKind) -> Option<(Vec<String>, Vec<String>)> {
    match kind {
        ScriptKind::Setup => Some((config.setup.clone(), Vec::new())),
        ScriptKind::Teardown => Some((config.teardown.clone(), Vec::new())),
        ScriptKind::Run(name) => config
            .run_target(name.as_deref())
            .map(|(_, target)| (target.commands.clone(), target.stop_commands.clone())),
    }
    .filter(|(commands, _)| !commands.is_empty())
}

fn describe(kind: &ScriptKind) -> String {
    match kind {
        ScriptKind::Setup => "setup".into(),
        ScriptKind::Teardown => "teardown".into(),
        ScriptKind::Run(None) => "default run".into(),
        ScriptKind::Run(Some(name)) => format!("run target {name:?}"),
    }
}

async fn load_config(worktree: &Worktree) -> Result<Option<RepoConfig>, RpcError> {
    let root = PathBuf::from(&worktree.path);
    tokio::task::spawn_blocking(move || fld_git::config::load_config(&root))
        .await
        .map_err(|error| internal(format!("background task failed: {error}")))?
        .map_err(|error| invalid(format!("invalid .flotilla/config.json: {error}")))
}

impl WorkspaceService {
    async fn script_key(
        &self,
        worktree: WorktreeId,
        kind: &ScriptKind,
    ) -> Result<Option<TerminalKey>, RpcError> {
        let key = kind_key(kind);
        Ok(self
            .terminals
            .scripts()
            .await?
            .into_iter()
            .find(|script| script.worktree == worktree && script.kind == key)
            .map(|script| script.key))
    }

    fn state_of(
        &self,
        worktree: WorktreeId,
        kind: ScriptKind,
        key: Option<TerminalKey>,
    ) -> ScriptState {
        ScriptState {
            worktree,
            kind,
            terminal: key.map(|key| key.terminal),
            state: match key {
                Some(key) => run_state(self.terminals.process_state(key)),
                None => ScriptRunState::Idle,
            },
        }
    }

    pub(crate) async fn run_script(
        self: &Arc<Self>,
        id: WorktreeId,
        kind: ScriptKind,
    ) -> Result<ScriptState, RpcError> {
        let (worktree, project) = self.worktree(id).await?;
        self.clone().run_configured(&worktree, &project, kind).await
    }

    /// Start `kind` from the worktree's config (restarting a previous run).
    pub(super) async fn run_configured(
        self: Arc<Self>,
        worktree: &Worktree,
        project: &Project,
        kind: ScriptKind,
    ) -> Result<ScriptState, RpcError> {
        let config = load_config(worktree)
            .await?
            .ok_or_else(|| invalid(format!("{} has no .flotilla/config.json", worktree.path)))?;
        let (commands, _) = commands(&config, &kind).ok_or_else(|| {
            invalid(format!(
                "no {} script is configured in .flotilla/config.json",
                describe(&kind)
            ))
        })?;
        let argv = vec![
            "/bin/sh".to_owned(),
            "-lc".to_owned(),
            commands.join(" && "),
        ];
        let key = self
            .terminals
            .start_script(
                worktree.id,
                kind_key(&kind),
                argv,
                self.worktree_defaults(worktree, project),
                SCRIPT_SIZE,
            )
            .await?;
        let state = self.state_of(worktree.id, kind.clone(), Some(key));
        self.publish(DaemonEvent::ScriptChanged(state.clone()));
        self.monitor(worktree.id, kind, key);
        Ok(state)
    }

    /// Publish this run's state transitions until it exits or is replaced.
    fn monitor(self: &Arc<Self>, worktree: WorktreeId, kind: ScriptKind, key: TerminalKey) {
        let Some(mut states) = self.terminals.watch_state(key) else {
            return;
        };
        let service = self.clone();
        let cancel = self.cancel.clone();
        tokio::spawn(async move {
            let mut last = run_state(Some(states.borrow_and_update().clone()));
            loop {
                if matches!(last, ScriptRunState::Exited { .. }) {
                    return;
                }
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    changed = states.changed() => if changed.is_err() {
                        return;
                    },
                }
                let now = run_state(Some(states.borrow_and_update().clone()));
                if now != last {
                    last = now;
                    service.publish(DaemonEvent::ScriptChanged(ScriptState {
                        worktree,
                        kind: kind.clone(),
                        terminal: Some(key.terminal),
                        state: now,
                    }));
                }
            }
        });
    }

    /// Resume monitoring scripts that were running before a daemon restart.
    pub(super) async fn resume_scripts(self: Arc<Self>) -> Result<(), RpcError> {
        for script in self.terminals.scripts().await? {
            if let Some(kind) = parse_kind(&script.kind) {
                self.monitor(script.worktree, kind, script.key);
            }
        }
        Ok(())
    }

    /// Run the configured stop commands (if any), then terminate the run.
    /// The terminal and its output remain; the state becomes `Exited`.
    pub(crate) async fn stop_script(
        &self,
        id: WorktreeId,
        kind: ScriptKind,
    ) -> Result<ScriptState, RpcError> {
        let (worktree, project) = self.worktree(id).await?;
        let Some(key) = self.script_key(id, &kind).await? else {
            return Ok(self.state_of(id, kind, None));
        };
        if run_state(self.terminals.process_state(key)) != ScriptRunState::Running {
            return Ok(self.state_of(id, kind, Some(key)));
        }
        let stop = match load_config(&worktree).await {
            Ok(Some(config)) => commands(&config, &kind)
                .map(|(_, stop)| stop)
                .unwrap_or_default(),
            Ok(None) => Vec::new(),
            Err(error) => {
                tracing::warn!("not running stop commands: {}", error.message);
                Vec::new()
            }
        };
        if !stop.is_empty() {
            self.run_stop_commands(&worktree, &project, &stop).await?;
        }
        let mut states = self.terminals.watch_state(key);
        self.terminals.terminate(key).await?;
        if let Some(states) = states.as_mut() {
            let _ = tokio::time::timeout(
                STOP_WAIT,
                states.wait_for(|state| {
                    matches!(state, ProcessState::Exited(_) | ProcessState::Lost)
                }),
            )
            .await;
        }
        Ok(self.state_of(id, kind, Some(key)))
    }

    async fn run_stop_commands(
        &self,
        worktree: &Worktree,
        project: &Project,
        stop: &[String],
    ) -> Result<(), RpcError> {
        let defaults = self.worktree_defaults(worktree, project);
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .arg("-lc")
            .arg(stop.join(" && "))
            .current_dir(Path::new(&worktree.path))
            .envs(&defaults.env)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let child = command
            .spawn()
            .map_err(|error| internal(format!("starting stop commands: {error}")))?;
        match tokio::time::timeout(STOP_COMMANDS_TIMEOUT, child.wait_with_output()).await {
            Ok(Ok(output)) if output.status.success() => Ok(()),
            Ok(Ok(output)) => {
                // The run is still terminated; report the failure in the log.
                tracing::warn!(
                    status = %output.status,
                    stderr = %String::from_utf8_lossy(&output.stderr),
                    "stop commands failed"
                );
                Ok(())
            }
            Ok(Err(error)) => Err(internal(format!("running stop commands: {error}"))),
            Err(_) => {
                tracing::warn!("stop commands timed out after {STOP_COMMANDS_TIMEOUT:?}");
                Ok(())
            }
        }
    }

    /// Configured scripts and every recorded run, with their state.
    pub(crate) async fn list_scripts(&self, id: WorktreeId) -> Result<Vec<ScriptState>, RpcError> {
        let (worktree, _) = self.worktree(id).await?;
        let mut kinds: Vec<ScriptKind> = Vec::new();
        match load_config(&worktree).await {
            Ok(Some(config)) => {
                if !config.setup.is_empty() {
                    kinds.push(ScriptKind::Setup);
                }
                for (name, target) in &config.run {
                    if !target.commands.is_empty() {
                        kinds.push(ScriptKind::Run(Some(name.clone())));
                    }
                }
                if !config.teardown.is_empty() {
                    kinds.push(ScriptKind::Teardown);
                }
            }
            Ok(None) => {}
            Err(error) => tracing::debug!("listing scripts without config: {}", error.message),
        }
        let recorded: Vec<_> = self
            .terminals
            .scripts()
            .await?
            .into_iter()
            .filter(|script| script.worktree == id)
            .filter_map(|script| parse_kind(&script.kind).map(|kind| (kind, script.key)))
            .collect();
        for (kind, _) in &recorded {
            if !kinds.contains(kind) {
                kinds.push(kind.clone());
            }
        }
        kinds.truncate(MAX_SCRIPTS);
        Ok(kinds
            .into_iter()
            .map(|kind| {
                let key = recorded
                    .iter()
                    .find(|(recorded, _)| *recorded == kind)
                    .map(|(_, key)| *key);
                self.state_of(id, kind, key)
            })
            .collect())
    }

    /// Parse the worktree's config and report its configured scripts without
    /// starting anything. A missing file is valid and empty.
    pub(crate) async fn check_project_scripts(
        &self,
        id: WorktreeId,
    ) -> Result<ProjectScriptCheck, RpcError> {
        let (worktree, _) = self.worktree(id).await?;
        let root = PathBuf::from(&worktree.path);
        let config_path = super::utf8(&root.join(fld_git::config::CONFIG_PATH))?;
        let loaded = tokio::task::spawn_blocking(move || fld_git::config::load_config(&root))
            .await
            .map_err(|error| internal(format!("background task failed: {error}")))?;
        let (error, setup, teardown, runs) = match loaded {
            Ok(Some(config)) => (
                None,
                !config.setup.is_empty(),
                !config.teardown.is_empty(),
                config
                    .run
                    .iter()
                    .filter(|(name, target)| {
                        !target.commands.is_empty()
                            && name.len() <= MAX_TEXT_BYTES
                            && !name.contains('\0')
                    })
                    .map(|(name, _)| name.clone())
                    .collect::<Vec<_>>(),
            ),
            Ok(None) => (None, false, false, Vec::new()),
            Err(error) => (Some(bounded(error.to_string())), false, false, Vec::new()),
        };
        let mut runs = runs;
        runs.truncate(MAX_SCRIPTS);
        Ok(ProjectScriptCheck {
            worktree: id,
            config_path,
            error,
            setup,
            teardown,
            runs,
        })
    }

    /// Run the configured teardown and wait (bounded) for it to finish.
    pub(super) async fn teardown(self: Arc<Self>, worktree: &Worktree, project: &Project) {
        match load_config(worktree).await {
            Ok(Some(config)) if !config.teardown.is_empty() => {}
            Ok(_) => return,
            Err(error) => {
                tracing::warn!(path = %worktree.path, "skipping teardown: {}", error.message);
                return;
            }
        }
        let started = self
            .clone()
            .run_configured(worktree, project, ScriptKind::Teardown)
            .await;
        if let Err(error) = started {
            tracing::warn!(path = %worktree.path, "teardown failed to start: {}", error.message);
            return;
        }
        let Ok(Some(key)) = self.script_key(worktree.id, &ScriptKind::Teardown).await else {
            return;
        };
        let Some(mut states) = self.terminals.watch_state(key) else {
            return;
        };
        let timed_out = tokio::time::timeout(
            TEARDOWN_TIMEOUT,
            states.wait_for(|state| matches!(state, ProcessState::Exited(_) | ProcessState::Lost)),
        )
        .await
        .is_err();
        if timed_out {
            tracing::warn!(path = %worktree.path, "teardown timed out; terminating it");
            if let Err(error) = self.terminals.terminate(key).await {
                tracing::warn!("terminating teardown: {}", error.message);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_kinds_round_trip_through_their_durable_keys() {
        for kind in [
            ScriptKind::Setup,
            ScriptKind::Teardown,
            ScriptKind::Run(None),
            ScriptKind::Run(Some("web".into())),
            ScriptKind::Run(Some("a:b".into())),
        ] {
            assert_eq!(parse_kind(&kind_key(&kind)), Some(kind));
        }
        assert_eq!(parse_kind("unknown"), None);
    }

    #[test]
    fn process_states_map_to_script_states() {
        use fl_model::ExitStatus;
        assert_eq!(
            run_state(Some(ProcessState::Running)),
            ScriptRunState::Running
        );
        assert_eq!(
            run_state(Some(ProcessState::Exited(ExitStatus {
                code: Some(3),
                signal: None
            }))),
            ScriptRunState::Exited { code: Some(3) }
        );
        assert_eq!(run_state(None), ScriptRunState::Exited { code: None });
    }
}
