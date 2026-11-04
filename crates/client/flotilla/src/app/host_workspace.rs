//! One connected host's workspace: projects and worktrees in the sidebar,
//! the selected worktree's terminals (or open file/diff documents) in the
//! center, and a right panel with its files, changes and scripts.
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use fl_model::workspace::{
    ChangeKind, DirListing, EntryKind, FileChange, LayoutScope, ProjectKind, ScriptKind,
    ScriptRunState, ScriptState, Worktree,
};
use fl_model::{ViewId, WorktreeId};
use fl_proto::RpcErrorCode;
use flc_editor::diff::Layout as DiffLayout;
use flc_editor::diff_view::DiffView;
use flc_editor::editor::{EditorEvent, EditorView};
use flc_shell::{MirrorSource, ShellEvent, ShellView, ShellViewOptions};
use flc_sync::{CallError, Sync, WorkspacesState};
use flc_terminal::{TerminalView, TerminalViewOptions};
use flc_ui::input::{TextInput, TextInputEvent};
use flc_ui::theme::{ActiveTheme, Theme};
use gpui::prelude::*;
use gpui::{
    AnyElement, AsyncApp, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    SharedString, Subscription, Task, WeakEntity, Window, div, px,
};

use super::host_view::{primary_button, secondary_button};

/// Largest file the editor opens (the daemon's read limit).
const READ_LIMIT: u32 = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostWorkspaceEvent {
    /// A terminal asked (OSC 52) to set the clipboard; consent is the app's.
    ClipboardStore(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Panel {
    Files,
    Changes,
}

enum DocumentKind {
    File {
        editor: Entity<EditorView>,
        /// SHA-256 of the file as last read or written: saves refuse to
        /// overwrite a file that changed on the server since.
        sha256: [u8; 32],
        _events: Subscription,
    },
    Diff(Entity<DiffView>),
}

struct Document {
    /// Absolute path on the host.
    path: String,
    title: SharedString,
    kind: DocumentKind,
}

/// Open documents of one scope; `active == None` shows the terminals.
#[derive(Default)]
struct Documents {
    open: Vec<Document>,
    active: Option<usize>,
}

enum Dialog {
    AddProject {
        path: Entity<TextInput>,
        url: Entity<TextInput>,
        listing: Option<DirListing>,
        error: Option<SharedString>,
        busy: bool,
        _events: [Subscription; 2],
    },
    CreateWorktree {
        project: fl_model::ProjectId,
        branch: Entity<TextInput>,
        error: Option<SharedString>,
        busy: bool,
        _events: Subscription,
    },
    DeleteWorktree {
        worktree: WorktreeId,
        label: SharedString,
        /// The daemon refused a plain delete (dirty or unpushed work).
        needs_force: Option<SharedString>,
        busy: bool,
    },
}

pub struct HostWorkspace {
    sync: Arc<Sync>,
    /// Starting directory for the remote folder picker.
    home_dir: String,
    focus: FocusHandle,
    state: Arc<WorkspacesState>,
    scope: LayoutScope,
    shells: HashMap<LayoutScope, (Entity<ShellView>, Subscription)>,
    documents: HashMap<LayoutScope, Documents>,
    panel_open: bool,
    panel: Panel,
    expanded: HashSet<String>,
    listings: HashMap<String, DirListing>,
    script_tab: ScriptKind,
    script_views: HashMap<(WorktreeId, String), Entity<TerminalView>>,
    dialog: Option<Dialog>,
    banner: Option<SharedString>,
    rtt: Option<Duration>,
    /// A just-added project whose primary checkout opens once the daemon's
    /// tree update arrives (the RPC reply can overtake it).
    pending_select: Option<fl_model::ProjectId>,
    /// Script terminals already asked for in a process refresh.
    refreshed_for: HashSet<fl_model::TerminalId>,
    _follow: Task<()>,
}

impl EventEmitter<HostWorkspaceEvent> for HostWorkspace {}

fn script_key(kind: &ScriptKind) -> String {
    match kind {
        ScriptKind::Setup => "setup".into(),
        ScriptKind::Teardown => "teardown".into(),
        ScriptKind::Run(None) => "run".into(),
        ScriptKind::Run(Some(name)) => format!("run:{name}"),
    }
}

fn script_label(kind: &ScriptKind) -> SharedString {
    match kind {
        ScriptKind::Setup => "Setup".into(),
        ScriptKind::Teardown => "Teardown".into(),
        ScriptKind::Run(None) => "Run".into(),
        ScriptKind::Run(Some(name)) if name == "default" => "Run".into(),
        ScriptKind::Run(Some(name)) => format!("Run: {name}").into(),
    }
}

fn error_text(error: &CallError) -> SharedString {
    match error {
        CallError::Rpc(flc_conn::broker::RpcCallError::Remote(remote)) => {
            remote.message.clone().into()
        }
        other => other.to_string().into(),
    }
}

fn is_conflict(error: &CallError) -> bool {
    matches!(error, CallError::Rpc(e) if e.code() == Some(RpcErrorCode::RevisionConflict))
}

fn change_letter(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Added => "A",
        ChangeKind::Modified => "M",
        ChangeKind::Deleted => "D",
        ChangeKind::Renamed => "R",
        ChangeKind::Copied => "C",
        ChangeKind::TypeChanged => "T",
        ChangeKind::Untracked => "U",
        ChangeKind::Conflicted => "!",
    }
}

fn parent_dir(path: &str) -> String {
    match path.trim_end_matches('/').rfind('/') {
        Some(0) | None => "/".into(),
        Some(index) => path[..index].into(),
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

impl HostWorkspace {
    pub fn new(sync: Arc<Sync>, home_dir: String, cx: &mut Context<Self>) -> Self {
        // Process listings (for attaching script terminals) live in the home
        // layout's state.
        let mut processes = sync.workspace_changes();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            while processes.changed().await.is_ok() {
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        })
        .detach();
        let mut changes = sync.workspaces_changes();
        let follow = cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            loop {
                let state = changes.borrow_and_update().clone();
                if this
                    .update(cx, |view, cx| {
                        let previous = std::mem::replace(&mut view.state, state);
                        view.adopt(&previous, cx);
                        // A deleted worktree leaves nothing to show.
                        if let LayoutScope::Worktree(id) = view.scope
                            && view.state.tree.is_some()
                            && view.state.worktree(id).is_none()
                        {
                            view.scope = LayoutScope::Home;
                        }
                        cx.notify();
                    })
                    .is_err()
                    || changes.changed().await.is_err()
                {
                    break;
                }
            }
        });
        Self {
            state: sync.workspaces(),
            sync,
            home_dir,
            focus: cx.focus_handle(),
            scope: LayoutScope::Home,
            shells: HashMap::new(),
            documents: HashMap::new(),
            panel_open: true,
            panel: Panel::Changes,
            expanded: HashSet::new(),
            listings: HashMap::new(),
            script_tab: ScriptKind::Setup,
            script_views: HashMap::new(),
            dialog: None,
            banner: None,
            rtt: None,
            pending_select: None,
            refreshed_for: HashSet::new(),
            _follow: follow,
        }
    }

    /// React to a new workspace state: open a just-added project, and
    /// reload the file tree when the selected worktree's files changed.
    fn adopt(&mut self, previous: &WorkspacesState, cx: &mut Context<Self>) {
        if let Some(project) = self.pending_select
            && let Some(primary) = self.state.tree.as_ref().and_then(|tree| {
                tree.worktrees
                    .iter()
                    .find(|w| w.project == project && w.is_primary)
                    .map(|w| w.id)
            })
        {
            self.pending_select = None;
            self.select(LayoutScope::Worktree(primary), cx);
        }
        if let LayoutScope::Worktree(id) = self.scope {
            let stamp = |state: &WorkspacesState| state.statuses.get(&id).map(|s| s.at_ms);
            if stamp(previous) != stamp(&self.state) {
                for dir in self.expanded.clone() {
                    if self.listings.contains_key(&dir) {
                        self.load_dir(dir, cx);
                    }
                }
            }
        }
    }

    pub fn set_rtt(&mut self, rtt: Option<Duration>, cx: &mut Context<Self>) {
        self.rtt = rtt;
        for (shell, _) in self.shells.values() {
            shell.update(cx, |shell, _| shell.set_rtt(rtt));
        }
    }

    fn is_folder(&self, worktree: &Worktree) -> bool {
        self.state.tree.as_ref().is_some_and(|tree| {
            tree.projects
                .iter()
                .any(|p| p.id == worktree.project && p.kind == ProjectKind::Folder)
        })
    }

    fn selected_worktree(&self) -> Option<&Worktree> {
        match self.scope {
            LayoutScope::Worktree(id) => self.state.worktree(id),
            LayoutScope::Home => None,
        }
    }

    pub fn select(&mut self, scope: LayoutScope, cx: &mut Context<Self>) {
        if self.scope != scope {
            self.scope = scope;
            self.expanded.clear();
            self.listings.clear();
            if let Some(worktree) = self.selected_worktree() {
                let root = worktree.path.clone();
                self.expanded.insert(root.clone());
                self.load_dir(root, cx);
            }
        }
        cx.notify();
    }

    fn shell(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<ShellView> {
        let scope = self.scope;
        if let Some((shell, _)) = self.shells.get(&scope) {
            return shell.clone();
        }
        let sync = self.sync.clone();
        let layout = sync.layout(scope);
        let shell =
            cx.new(|cx| ShellView::new(sync, layout, ShellViewOptions::default(), window, cx));
        let rtt = self.rtt;
        shell.update(cx, |shell, _| shell.set_rtt(rtt));
        let events = cx.subscribe(&shell, |_, _, event: &ShellEvent, cx| match event {
            ShellEvent::ClipboardStore { text, .. } => {
                cx.emit(HostWorkspaceEvent::ClipboardStore(text.clone()))
            }
        });
        self.shells.insert(scope, (shell.clone(), events));
        shell
    }

    fn show_error(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.banner = Some(message.into());
        cx.notify();
    }

    // ---- files -------------------------------------------------------------

    fn load_dir(&mut self, path: String, cx: &mut Context<Self>) {
        let sync = self.sync.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = sync.list_dir(path.clone()).await;
            let _ = this.update(cx, |view, cx| {
                match result {
                    Ok(listing) => {
                        view.listings.insert(path, listing);
                    }
                    Err(error) => {
                        view.show_error(format!("Cannot list {path}: {}", error_text(&error)), cx)
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn toggle_dir(&mut self, path: String, cx: &mut Context<Self>) {
        if !self.expanded.remove(&path) {
            self.expanded.insert(path.clone());
            self.load_dir(path, cx);
        }
        cx.notify();
    }

    fn documents(&mut self) -> &mut Documents {
        self.documents.entry(self.scope).or_default()
    }

    fn activate_existing(&mut self, path: &str, diff: bool) -> bool {
        let documents = self.documents();
        let found = documents
            .open
            .iter()
            .position(|doc| doc.path == path && matches!(doc.kind, DocumentKind::Diff(_)) == diff);
        if let Some(index) = found {
            documents.active = Some(index);
        }
        found.is_some()
    }

    fn open_file(&mut self, path: String, cx: &mut Context<Self>) {
        if self.activate_existing(&path, false) {
            cx.notify();
            return;
        }
        let sync = self.sync.clone();
        let scope = self.scope;
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = sync.read_file(path.clone(), READ_LIMIT).await;
            let _ = this.update(cx, |view, cx| match result {
                Ok(content) => {
                    if content.binary {
                        view.show_error(format!("{path} is a binary file"), cx);
                        return;
                    }
                    let editor = cx.new(|cx| EditorView::from_content(&content, cx));
                    let saved_path = path.clone();
                    let events =
                        cx.subscribe(&editor, move |view, editor, event: &EditorEvent, cx| {
                            if let EditorEvent::Save(request) = event {
                                view.save(scope, saved_path.clone(), editor, request.clone(), cx);
                            }
                            cx.notify();
                        });
                    let title = path.rsplit('/').next().unwrap_or(&path).to_string().into();
                    let documents = view.documents.entry(scope).or_default();
                    documents.open.push(Document {
                        path,
                        title,
                        kind: DocumentKind::File {
                            editor,
                            sha256: content.sha256,
                            _events: events,
                        },
                    });
                    documents.active = Some(documents.open.len() - 1);
                    cx.notify();
                }
                Err(error) => {
                    view.show_error(format!("Cannot open {path}: {}", error_text(&error)), cx)
                }
            });
        })
        .detach();
    }

    fn save(
        &mut self,
        scope: LayoutScope,
        path: String,
        editor: Entity<EditorView>,
        request: flc_editor::buffer::SaveRequest,
        cx: &mut Context<Self>,
    ) {
        let expected = self
            .documents
            .get(&scope)
            .and_then(|docs| docs.open.iter().find(|doc| doc.path == path))
            .and_then(|doc| match &doc.kind {
                DocumentKind::File { sha256, .. } => Some(*sha256),
                DocumentKind::Diff(_) => None,
            });
        let sync = self.sync.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = sync
                .write_file(path.clone(), request.bytes.clone(), expected)
                .await;
            let _ = this.update(cx, |view, cx| match result {
                Ok(sha) => {
                    editor.update(cx, |editor, cx| editor.mark_saved(request.revision, cx));
                    if let Some(doc) = view
                        .documents
                        .get_mut(&scope)
                        .and_then(|docs| docs.open.iter_mut().find(|doc| doc.path == path))
                        && let DocumentKind::File { sha256, .. } = &mut doc.kind
                    {
                        *sha256 = sha;
                    }
                }
                Err(error) if is_conflict(&error) => view.show_error(
                    format!("{path} changed on the server since it was opened; reopen it to see the new version."),
                    cx,
                ),
                Err(error) => view.show_error(format!("Saving {path} failed: {}", error_text(&error)), cx),
            });
        })
        .detach();
    }

    fn open_diff(&mut self, worktree: WorktreeId, change: FileChange, cx: &mut Context<Self>) {
        let Some(root) = self.state.worktree(worktree).map(|w| w.path.clone()) else {
            return;
        };
        let path = join(&root, &change.path);
        if self.activate_existing(&path, true) {
            cx.notify();
            return;
        }
        let sync = self.sync.clone();
        let scope = self.scope;
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = sync.file_diff(worktree, change.path.clone()).await;
            let _ = this.update(cx, |view, cx| match result {
                Ok(diff) => {
                    let diff_view = cx.new(|cx| DiffView::new(&diff, DiffLayout::Unified, cx));
                    let documents = view.documents.entry(scope).or_default();
                    documents.open.push(Document {
                        path,
                        title: format!(
                            "{} (diff)",
                            change.path.rsplit('/').next().unwrap_or(&change.path)
                        )
                        .into(),
                        kind: DocumentKind::Diff(diff_view),
                    });
                    documents.active = Some(documents.open.len() - 1);
                    cx.notify();
                }
                Err(error) => view.show_error(
                    format!("Cannot diff {}: {}", change.path, error_text(&error)),
                    cx,
                ),
            });
        })
        .detach();
    }

    fn close_document(&mut self, index: usize, cx: &mut Context<Self>) {
        let documents = self.documents();
        if index < documents.open.len() {
            documents.open.remove(index);
            documents.active = match documents.active {
                Some(active) if active == index => None,
                Some(active) if active > index => Some(active - 1),
                other => other,
            };
        }
        cx.notify();
    }

    // ---- scripts -------------------------------------------------------------

    fn script_state(&self, worktree: WorktreeId, kind: &ScriptKind) -> Option<&ScriptState> {
        self.state
            .scripts
            .get(&worktree)?
            .iter()
            .find(|s| &s.kind == kind)
    }

    fn run_script(
        &mut self,
        worktree: WorktreeId,
        kind: ScriptKind,
        stop: bool,
        cx: &mut Context<Self>,
    ) {
        let sync = self.sync.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = if stop {
                sync.stop_script(worktree, kind.clone()).await
            } else {
                sync.run_script(worktree, kind.clone()).await
            };
            // The script's terminal must be listed before a view can attach.
            sync.refresh();
            let _ = this.update(cx, |view, cx| {
                if let Err(error) = result {
                    view.show_error(
                        format!("{}: {}", script_label(&kind), error_text(&error)),
                        cx,
                    );
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn script_view(
        &mut self,
        worktree: WorktreeId,
        state: &ScriptState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<TerminalView>> {
        let terminal = state.terminal?;
        let key = (worktree, format!("{}:{terminal}", script_key(&state.kind)));
        if let Some(view) = self.script_views.get(&key) {
            return Some(view.clone());
        }
        let Some(incarnation) = self
            .sync
            .workspace()
            .processes
            .iter()
            .find(|p| p.terminal == terminal)
            .map(|p| p.incarnation)
        else {
            // Started by the daemon (setup after a create): list it once.
            if self.refreshed_for.insert(terminal) {
                self.sync.refresh();
            }
            return None;
        };
        let handle = self.sync.attach(terminal, incarnation, ViewId::new());
        let source = MirrorSource::new(handle);
        let view =
            cx.new(|cx| TerminalView::new(source, TerminalViewOptions::default(), window, cx));
        self.script_views.insert(key, view.clone());
        Some(view)
    }

    // ---- dialogs ---------------------------------------------------------------

    pub fn open_add_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let path = cx.new(|cx| TextInput::new(cx, "/path/on/the/server"));
        let url = cx.new(|cx| TextInput::new(cx, "https://github.com/owner/repo.git"));
        let home = self.home_dir.clone();
        path.update(cx, |input, cx| input.set_text(home.clone(), cx));
        let path_events = cx.subscribe_in(
            &path,
            window,
            |view, input, event: &TextInputEvent, window, cx| match event {
                TextInputEvent::Submitted(_) => {
                    let path = input.read(cx).text().to_string();
                    view.browse(path, cx);
                }
                TextInputEvent::Cancelled => view.close_dialog(window, cx),
            },
        );
        let url_events = cx.subscribe_in(
            &url,
            window,
            |view, _, event: &TextInputEvent, window, cx| match event {
                TextInputEvent::Submitted(_) => view.clone_project(cx),
                TextInputEvent::Cancelled => view.close_dialog(window, cx),
            },
        );
        path.read(cx).focus_handle(cx).focus(window);
        self.dialog = Some(Dialog::AddProject {
            path,
            url,
            listing: None,
            error: None,
            busy: false,
            _events: [path_events, url_events],
        });
        self.browse(home, cx);
    }

    fn browse(&mut self, path: String, cx: &mut Context<Self>) {
        let sync = self.sync.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = sync.list_dir(path.clone()).await;
            let _ = this.update(cx, |view, cx| {
                if let Some(Dialog::AddProject {
                    path: input,
                    listing,
                    error,
                    ..
                }) = view.dialog.as_mut()
                {
                    match result {
                        Ok(found) => {
                            input.update(cx, |input, cx| input.set_text(found.path.clone(), cx));
                            *listing = Some(found);
                            *error = None;
                        }
                        Err(e) => *error = Some(error_text(&e)),
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn workspace_id(&self) -> Option<fl_model::WorkspaceId> {
        self.state.tree.as_ref()?.workspaces.first().map(|w| w.id)
    }

    fn add_project(&mut self, cx: &mut Context<Self>) {
        let (Some(workspace), Some(Dialog::AddProject { path, busy, .. })) =
            (self.workspace_id(), self.dialog.as_mut())
        else {
            return;
        };
        *busy = true;
        let path = path.read(cx).text().to_string();
        let sync = self.sync.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = sync.add_project(workspace, path).await;
            let _ = this.update(cx, |view, cx| view.project_added(result, cx));
        })
        .detach();
        cx.notify();
    }

    fn clone_project(&mut self, cx: &mut Context<Self>) {
        let (
            Some(workspace),
            Some(Dialog::AddProject {
                path,
                url,
                busy,
                error,
                ..
            }),
        ) = (self.workspace_id(), self.dialog.as_mut())
        else {
            return;
        };
        let url = url.read(cx).text().trim().to_string();
        let name = url
            .trim_end_matches('/')
            .trim_end_matches(".git")
            .rsplit(['/', ':'])
            .next()
            .unwrap_or_default()
            .to_string();
        if name.is_empty() {
            *error = Some("Enter a repository URL".into());
            cx.notify();
            return;
        }
        *busy = true;
        let dest = join(path.read(cx).text(), &name);
        let sync = self.sync.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = sync.clone_project(workspace, url, dest).await;
            let _ = this.update(cx, |view, cx| view.project_added(result, cx));
        })
        .detach();
        cx.notify();
    }

    fn project_added(
        &mut self,
        result: Result<fl_model::workspace::Project, CallError>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(project) => {
                self.dialog = None;
                // Show the project's primary checkout once it is listed.
                self.pending_select = Some(project.id);
                let previous = self.state.clone();
                self.adopt(&previous, cx);
            }
            Err(e) => {
                if let Some(Dialog::AddProject { error, busy, .. }) = self.dialog.as_mut() {
                    *error = Some(error_text(&e));
                    *busy = false;
                }
            }
        }
        cx.notify();
    }

    fn open_create_worktree(
        &mut self,
        project: fl_model::ProjectId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let branch = cx.new(|cx| TextInput::new(cx, "Branch (empty for a temporary name)"));
        let events = cx.subscribe_in(
            &branch,
            window,
            |view, _, event: &TextInputEvent, window, cx| match event {
                TextInputEvent::Submitted(_) => view.create_worktree(cx),
                TextInputEvent::Cancelled => view.close_dialog(window, cx),
            },
        );
        branch.read(cx).focus_handle(cx).focus(window);
        self.dialog = Some(Dialog::CreateWorktree {
            project,
            branch,
            error: None,
            busy: false,
            _events: events,
        });
        cx.notify();
    }

    fn create_worktree(&mut self, cx: &mut Context<Self>) {
        let Some(Dialog::CreateWorktree {
            project,
            branch,
            busy,
            ..
        }) = self.dialog.as_mut()
        else {
            return;
        };
        *busy = true;
        let project = *project;
        let name = branch.read(cx).text().trim().to_string();
        let branch = (!name.is_empty()).then_some(name);
        let sync = self.sync.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = sync.create_worktree(project, branch, None).await;
            let _ = this.update(cx, |view, cx| {
                match result {
                    Ok(worktree) => {
                        view.dialog = None;
                        view.select(LayoutScope::Worktree(worktree.id), cx);
                    }
                    Err(e) => {
                        if let Some(Dialog::CreateWorktree { error, busy, .. }) =
                            view.dialog.as_mut()
                        {
                            *error = Some(error_text(&e));
                            *busy = false;
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn open_delete(&mut self, worktree: WorktreeId, cx: &mut Context<Self>) {
        let label = self
            .state
            .worktree(worktree)
            .map(|w| w.label.clone())
            .unwrap_or_default();
        self.dialog = Some(Dialog::DeleteWorktree {
            worktree,
            label: label.into(),
            needs_force: None,
            busy: false,
        });
        cx.notify();
    }

    fn delete_worktree(&mut self, force: bool, cx: &mut Context<Self>) {
        let Some(Dialog::DeleteWorktree { worktree, busy, .. }) = self.dialog.as_mut() else {
            return;
        };
        *busy = true;
        let worktree = *worktree;
        let sync = self.sync.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = sync.delete_worktree(worktree, force).await;
            let _ = this.update(cx, |view, cx| {
                match result {
                    Ok(()) => {
                        view.dialog = None;
                        if view.scope == LayoutScope::Worktree(worktree) {
                            view.scope = LayoutScope::Home;
                        }
                        view.shells.remove(&LayoutScope::Worktree(worktree));
                        view.documents.remove(&LayoutScope::Worktree(worktree));
                    }
                    Err(e) => {
                        if let Some(Dialog::DeleteWorktree {
                            needs_force, busy, ..
                        }) = view.dialog.as_mut()
                        {
                            *needs_force = Some(error_text(&e));
                            *busy = false;
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn close_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dialog = None;
        window.focus(&self.focus);
        cx.notify();
    }

    // ---- rendering ---------------------------------------------------------------

    /// The workspace part of the app sidebar.
    pub fn sidebar_section(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme().clone();
        let state = self.state.clone();
        let mut column = div().flex().flex_col().gap(px(1.)).child(
            div()
                .px(px(8.))
                .pt(px(12.))
                .pb(px(4.))
                .text_size(t.text_size_sm)
                .text_color(t.text_tertiary)
                .font_weight(FontWeight::SEMIBOLD)
                .child("WORKSPACE"),
        );
        column = column.child(self.sidebar_row(
            "home",
            "Home terminals".into(),
            self.scope == LayoutScope::Home,
            &t,
            cx.listener(|view, _, _, cx| view.select(LayoutScope::Home, cx)),
        ));
        let Some(tree) = state.tree.as_ref() else {
            return column
                .child(
                    div()
                        .px(px(8.))
                        .py(px(6.))
                        .text_size(t.text_size_sm)
                        .text_color(t.text_tertiary)
                        .child(if state.live { "Loading…" } else { "Offline" }),
                )
                .into_any_element();
        };
        let mut projects = tree.projects.clone();
        projects.sort_by_key(|p| p.order);
        for project in projects {
            let project_id = project.id;
            let letter: String = project
                .name
                .chars()
                .next()
                .map(|c| c.to_uppercase().collect())
                .unwrap_or_default();
            let hover = t.raised_hover;
            let mut header = div()
                .id(SharedString::from(format!("project-{project_id}")))
                .mt(px(6.))
                .h(px(28.))
                .px(px(8.))
                .flex()
                .items_center()
                .gap(px(8.))
                .rounded(t.radius_sm)
                .child(
                    div()
                        .size(px(18.))
                        .rounded(px(4.))
                        .bg(t.control_bg)
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(11.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(SharedString::from(letter)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .truncate()
                        .font_weight(FontWeight::MEDIUM)
                        .child(project.name.clone()),
                );
            if project.kind == ProjectKind::Git {
                header = header.child(
                    div()
                        .id(SharedString::from(format!("new-worktree-{project_id}")))
                        .px(px(6.))
                        .rounded(t.radius_sm)
                        .text_color(t.text_secondary)
                        .cursor_pointer()
                        .hover(move |s| s.bg(hover))
                        .child("+")
                        .on_click(cx.listener(move |view, _, window, cx| {
                            view.open_create_worktree(project_id, window, cx)
                        })),
                );
            }
            column = column.child(header);
            let mut worktrees: Vec<&Worktree> = tree
                .worktrees
                .iter()
                .filter(|w| w.project == project_id)
                .collect();
            worktrees.sort_by_key(|w| (!w.is_primary, w.label.clone()));
            for worktree in worktrees {
                column = column.child(self.worktree_row(worktree, &state, &t, cx));
            }
        }
        let hover = t.raised_hover;
        column
            .child(
                div()
                    .id("add-project")
                    .mt(px(6.))
                    .h(px(28.))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .rounded(t.radius_sm)
                    .text_color(t.text_secondary)
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .child("+ Add project")
                    .on_click(cx.listener(|view, _, window, cx| view.open_add_project(window, cx))),
            )
            .into_any_element()
    }

    fn sidebar_row(
        &self,
        id: &'static str,
        label: SharedString,
        selected: bool,
        t: &Theme,
        on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
    ) -> impl IntoElement {
        let hover = t.raised_hover;
        div()
            .id(id)
            .h(px(28.))
            .px(px(8.))
            .flex()
            .items_center()
            .rounded(t.radius_sm)
            .cursor_pointer()
            .when(selected, |d| {
                d.bg(t.row_selected)
                    .border_1()
                    .border_color(t.row_selected_border)
            })
            .when(!selected, |d| d.hover(move |s| s.bg(hover)))
            .child(label)
            .on_click(on_click)
    }

    fn worktree_row(
        &self,
        worktree: &Worktree,
        state: &WorkspacesState,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = worktree.id;
        let selected = self.scope == LayoutScope::Worktree(id);
        let status = state.statuses.get(&id);
        let (added, removed, dirty) = status.map_or((0, 0, false), |s| {
            (s.summary.added, s.summary.removed, s.summary.dirty)
        });
        let hover = t.raised_hover;
        let primary = worktree.is_primary;
        div()
            .id(SharedString::from(format!("worktree-{id}")))
            .group("worktree-row")
            .h(px(28.))
            .pl(px(20.))
            .pr(px(8.))
            .flex()
            .items_center()
            .gap(px(6.))
            .rounded(t.radius_sm)
            .cursor_pointer()
            .when(selected, |d| {
                d.bg(t.row_selected)
                    .border_1()
                    .border_color(t.row_selected_border)
            })
            .when(!selected, |d| d.hover(move |s| s.bg(hover)))
            .on_click(cx.listener(move |view, _, _, cx| view.select(LayoutScope::Worktree(id), cx)))
            .child(div().text_color(t.text_tertiary).child("⎇"))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .truncate()
                    .child(worktree.label.clone()),
            )
            .when(added > 0, |d| {
                d.child(
                    div()
                        .text_size(t.text_size_sm)
                        .text_color(t.success)
                        .child(SharedString::from(format!("+{added}"))),
                )
            })
            .when(removed > 0, |d| {
                d.child(
                    div()
                        .text_size(t.text_size_sm)
                        .text_color(t.danger)
                        .child(SharedString::from(format!("−{removed}"))),
                )
            })
            .when(dirty, |d| {
                d.child(div().size(px(6.)).rounded_full().bg(t.warning))
            })
            .when(primary, |d| d.child(div().text_color(t.warning).child("★")))
            .when(!primary, |d| {
                d.child(
                    div()
                        .id(SharedString::from(format!("delete-{id}")))
                        .invisible()
                        .group_hover("worktree-row", |s| s.visible())
                        .px(px(4.))
                        .text_color(t.text_tertiary)
                        .child("×")
                        .on_click(cx.listener(move |view, _, _, cx| {
                            cx.stop_propagation();
                            view.open_delete(id, cx)
                        })),
                )
            })
    }

    fn render_documents_strip(
        &self,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement + use<>> {
        let documents = self.documents.get(&self.scope)?;
        if documents.open.is_empty() {
            return None;
        }
        let tab = |id: SharedString, label: SharedString, active: bool| {
            div()
                .id(id)
                .h(px(26.))
                .px(px(10.))
                .flex()
                .items_center()
                .gap(px(6.))
                .rounded(t.radius_sm)
                .cursor_pointer()
                .text_size(t.text_size_sm)
                .text_color(if active {
                    t.text_primary
                } else {
                    t.text_secondary
                })
                .when(active, |d| d.bg(t.row_selected))
                .child(label)
        };
        let mut strip = div()
            .h(px(32.))
            .px(px(6.))
            .flex()
            .items_center()
            .gap(px(2.))
            .bg(t.chrome_bg)
            .border_b_1()
            .border_color(t.border)
            .child(
                tab(
                    "doc-terminals".into(),
                    "Terminals".into(),
                    documents.active.is_none(),
                )
                .on_click(cx.listener(|view, _, _, cx| {
                    view.documents().active = None;
                    cx.notify();
                })),
            );
        for (index, doc) in documents.open.iter().enumerate() {
            let dirty = match &doc.kind {
                DocumentKind::File { editor, .. } => editor.read(cx).is_dirty(),
                DocumentKind::Diff(_) => false,
            };
            let label: SharedString = if dirty {
                format!("● {}", doc.title).into()
            } else {
                doc.title.clone()
            };
            strip = strip.child(
                tab(
                    format!("doc-{index}").into(),
                    label,
                    documents.active == Some(index),
                )
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.documents().active = Some(index);
                    cx.notify();
                }))
                .child(
                    div()
                        .id(SharedString::from(format!("doc-close-{index}")))
                        .text_color(t.text_tertiary)
                        .child("×")
                        .on_click(cx.listener(move |view, _, _, cx| {
                            cx.stop_propagation();
                            view.close_document(index, cx)
                        })),
                ),
            );
        }
        Some(strip)
    }

    fn render_bottom_bar(&self, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let worktree = self.selected_worktree();
        let branch = match worktree {
            None => "Home".to_string(),
            Some(w) if self.is_folder(w) => format!("Folder {}", w.label),
            Some(w) => format!(
                "⎇ {}",
                w.branch.clone().unwrap_or_else(|| "detached".into())
            ),
        };
        let summary = worktree
            .and_then(|w| self.state.statuses.get(&w.id))
            .map(|s| s.summary.clone());
        let panel_open = self.panel_open;
        let hover = t.raised_hover;
        div()
            .h(px(28.))
            .px(px(10.))
            .flex()
            .items_center()
            .gap(px(10.))
            .bg(t.chrome_bg)
            .border_t_1()
            .border_color(t.border)
            .text_size(t.text_size_sm)
            .text_color(t.text_secondary)
            .child(SharedString::from(branch))
            .when_some(summary, |d, s| {
                d.when(s.ahead > 0, |d| {
                    d.child(SharedString::from(format!("↑{}", s.ahead)))
                })
                .when(s.behind > 0, |d| {
                    d.child(SharedString::from(format!("↓{}", s.behind)))
                })
                .child(
                    div()
                        .text_color(t.success)
                        .child(SharedString::from(format!("+{}", s.added))),
                )
                .child(
                    div()
                        .text_color(t.danger)
                        .child(SharedString::from(format!("−{}", s.removed))),
                )
            })
            .child(div().flex_1())
            .when(worktree.is_some(), |d| {
                d.child(
                    div()
                        .id("toggle-panel")
                        .px(px(8.))
                        .rounded(t.radius_sm)
                        .cursor_pointer()
                        .hover(move |s| s.bg(hover))
                        .child(if panel_open {
                            "Hide panel"
                        } else {
                            "Show panel"
                        })
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.panel_open = !view.panel_open;
                            cx.notify();
                        })),
                )
            })
    }

    fn render_panel(
        &mut self,
        worktree: &Worktree,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = cx.theme().clone();
        let id = worktree.id;
        // A plain folder has no git changes to list.
        let folder = self.is_folder(worktree);
        if folder {
            self.panel = Panel::Files;
        }
        let changes = self
            .state
            .statuses
            .get(&id)
            .map(|s| s.files.clone())
            .unwrap_or_default();
        let segment = |label: SharedString, panel: Panel, active: bool, cx: &mut Context<Self>| {
            div()
                .id(SharedString::from(format!("panel-{label}")))
                .flex_1()
                .h(px(24.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(t.radius_sm)
                .cursor_pointer()
                .text_size(t.text_size_sm)
                .text_color(if active {
                    t.text_primary
                } else {
                    t.text_secondary
                })
                .when(active, |d| d.bg(t.control_bg))
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.panel = panel;
                    cx.notify();
                }))
                .child(label)
        };
        let header = div()
            .p(px(6.))
            .flex()
            .gap(px(4.))
            .border_b_1()
            .border_color(t.border)
            .child(segment(
                "Files".into(),
                Panel::Files,
                self.panel == Panel::Files,
                cx,
            ))
            .when(!folder, |d| {
                d.child(segment(
                    format!("Changes {}", changes.len()).into(),
                    Panel::Changes,
                    self.panel == Panel::Changes,
                    cx,
                ))
            });
        let body: AnyElement = match self.panel {
            Panel::Files => self
                .render_file_tree(&worktree.path, &t, cx)
                .into_any_element(),
            Panel::Changes => self.render_changes(id, &changes, &t, cx).into_any_element(),
        };
        let scripts = self.render_scripts(id, &t, window, cx);
        div()
            .w(px(320.))
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .bg(t.chrome_bg)
            .border_l_1()
            .border_color(t.border)
            .child(header)
            .child(
                div()
                    .id("panel-body")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .child(body),
            )
            .child(scripts)
    }

    fn render_file_tree(&self, root: &str, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let mut rows = Vec::new();
        let mut stack: Vec<(String, usize)> = vec![(root.to_string(), 0)];
        // Depth-first over expanded directories, in listing order.
        let mut out: Vec<(String, String, EntryKind, bool, usize)> = Vec::new();
        while let Some((dir, depth)) = stack.pop() {
            let Some(listing) = self.listings.get(&dir) else {
                continue;
            };
            let mut children = Vec::new();
            for entry in listing.entries.iter().filter(|e| e.name != ".git") {
                let path = join(&dir, &entry.name);
                children.push((path, entry.name.clone(), entry.kind, entry.ignored, depth));
            }
            // Insert children after their parent row.
            let parent_index = out
                .iter()
                .position(|(p, ..)| *p == dir)
                .map_or(0, |i| i + 1);
            for (offset, child) in children.iter().enumerate() {
                out.insert(parent_index + offset, child.clone());
            }
            for (path, _, kind, _, depth) in children.iter().rev() {
                if *kind == EntryKind::Dir && self.expanded.contains(path) {
                    stack.push((path.clone(), depth + 1));
                }
            }
        }
        let hover = t.raised_hover;
        for (index, (path, name, kind, ignored, depth)) in out.into_iter().enumerate() {
            let is_dir = kind == EntryKind::Dir;
            let open = is_dir && self.expanded.contains(&path);
            let target = path.clone();
            rows.push(
                div()
                    .id(("file-row", index))
                    .h(px(24.))
                    .pl(px(10. + 14. * depth as f32))
                    .pr(px(8.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .text_color(if ignored {
                        t.text_tertiary
                    } else {
                        t.text_primary
                    })
                    .child(
                        div()
                            .w(px(10.))
                            .text_color(t.text_tertiary)
                            .child(if is_dir {
                                if open { "▾" } else { "▸" }
                            } else {
                                ""
                            }),
                    )
                    .child(div().truncate().child(SharedString::from(name)))
                    .on_click(cx.listener(move |view, _, _, cx| {
                        if is_dir {
                            view.toggle_dir(target.clone(), cx);
                        } else {
                            view.open_file(target.clone(), cx);
                        }
                    })),
            );
        }
        if rows.is_empty() {
            return div()
                .p(px(12.))
                .text_color(t.text_tertiary)
                .child(if self.listings.contains_key(root) {
                    "Empty folder"
                } else {
                    "Loading…"
                })
                .into_any_element();
        }
        div()
            .py(px(4.))
            .flex()
            .flex_col()
            .children(rows)
            .into_any_element()
    }

    fn render_changes(
        &self,
        worktree: WorktreeId,
        changes: &[FileChange],
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        if changes.is_empty() {
            return div()
                .p(px(12.))
                .text_color(t.text_tertiary)
                .child("No changes")
                .into_any_element();
        }
        let hover = t.raised_hover;
        div()
            .py(px(4.))
            .flex()
            .flex_col()
            .children(changes.iter().enumerate().map(|(index, change)| {
                let item = change.clone();
                let color = match change.kind {
                    ChangeKind::Added | ChangeKind::Untracked => t.success,
                    ChangeKind::Deleted | ChangeKind::Conflicted => t.danger,
                    _ => t.warning,
                };
                div()
                    .id(("change", index))
                    .h(px(24.))
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .child(
                        div()
                            .w(px(10.))
                            .text_color(color)
                            .child(change_letter(change.kind)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .child(SharedString::from(change.path.clone())),
                    )
                    .when_some(change.added, |d, n| {
                        d.child(
                            div()
                                .text_size(t.text_size_sm)
                                .text_color(t.success)
                                .child(SharedString::from(format!("+{n}"))),
                        )
                    })
                    .when_some(change.removed.filter(|n| *n > 0), |d, n| {
                        d.child(
                            div()
                                .text_size(t.text_size_sm)
                                .text_color(t.danger)
                                .child(SharedString::from(format!("−{n}"))),
                        )
                    })
                    .on_click(
                        cx.listener(move |view, _, _, cx| {
                            view.open_diff(worktree, item.clone(), cx)
                        }),
                    )
            }))
            .into_any_element()
    }

    fn render_scripts(
        &mut self,
        worktree: WorktreeId,
        t: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // Setup first, then the run targets the daemon reports for this
        // worktree's config (a plain default run when it reports none).
        let mut kinds = vec![ScriptKind::Setup];
        if let Some(list) = self.state.scripts.get(&worktree) {
            for state in list {
                if !kinds.contains(&state.kind)
                    && !matches!(state.kind, ScriptKind::Teardown | ScriptKind::Setup)
                {
                    kinds.push(state.kind.clone());
                }
            }
        }
        if !kinds.iter().any(|kind| matches!(kind, ScriptKind::Run(_))) {
            kinds.push(ScriptKind::Run(None));
        }
        if !kinds.contains(&self.script_tab) {
            self.script_tab = ScriptKind::Setup;
        }
        let current = self.script_tab.clone();
        let state = self.script_state(worktree, &current).cloned();
        let running = state
            .as_ref()
            .is_some_and(|s| s.state == ScriptRunState::Running);
        let view = state
            .as_ref()
            .and_then(|state| self.script_view(worktree, state, window, cx));
        let hover = t.raised_hover;
        let tabs = div()
            .h(px(30.))
            .px(px(6.))
            .flex()
            .items_center()
            .gap(px(2.))
            .border_t_1()
            .border_b_1()
            .border_color(t.border)
            .children(kinds.into_iter().map(|kind| {
                let active = kind == current;
                let label = script_label(&kind);
                div()
                    .id(SharedString::from(format!(
                        "script-tab-{}",
                        script_key(&kind)
                    )))
                    .px(px(8.))
                    .h(px(22.))
                    .flex()
                    .items_center()
                    .rounded(t.radius_sm)
                    .cursor_pointer()
                    .text_size(t.text_size_sm)
                    .text_color(if active {
                        t.text_primary
                    } else {
                        t.text_secondary
                    })
                    .when(active, |d| d.bg(t.control_bg))
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.script_tab = kind.clone();
                        cx.notify();
                    }))
                    .child(label)
            }))
            .child(div().flex_1())
            .child(
                div()
                    .id("script-action")
                    .px(px(8.))
                    .h(px(22.))
                    .flex()
                    .items_center()
                    .rounded(t.radius_sm)
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .text_size(t.text_size_sm)
                    .text_color(if running { t.danger } else { t.accent })
                    .child(if running { "■ Stop" } else { "▶ Run" })
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.run_script(worktree, current.clone(), running, cx)
                    })),
            );
        let status: SharedString = match state.as_ref().map(|s| s.state) {
            None => format!(
                "No {} output",
                script_label(&self.script_tab).to_lowercase()
            )
            .into(),
            Some(ScriptRunState::Idle) => "Not started".into(),
            Some(ScriptRunState::Running) => "Running".into(),
            Some(ScriptRunState::Exited { code: Some(code) }) => {
                format!("Exited with {code}").into()
            }
            Some(ScriptRunState::Exited { code: None }) => "Stopped".into(),
        };
        div()
            .h(px(240.))
            .flex_none()
            .flex()
            .flex_col()
            .child(tabs)
            .child(
                div()
                    .px(px(10.))
                    .py(px(2.))
                    .text_size(t.text_size_sm)
                    .text_color(t.text_tertiary)
                    .child(status),
            )
            .child(match view {
                Some(view) => div().flex_1().min_h(px(0.)).child(view).into_any_element(),
                None => div().flex_1().into_any_element(),
            })
    }

    fn render_dialog(&self, t: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let dialog = self.dialog.as_ref()?;
        let card = div()
            .w(px(520.))
            .p(px(20.))
            .flex()
            .flex_col()
            .gap(px(12.))
            .rounded(t.radius_lg)
            .bg(t.surface_raised)
            .border_1()
            .border_color(t.border)
            .shadow_lg();
        let title = |text: &'static str| {
            div()
                .text_size(px(16.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(text)
        };
        let error_row = |error: &Option<SharedString>| {
            error.clone().map(|message| {
                div()
                    .text_size(t.text_size_sm)
                    .text_color(t.danger)
                    .child(message)
            })
        };
        let content = match dialog {
            Dialog::AddProject {
                path,
                url,
                listing,
                error,
                busy,
                ..
            } => {
                let hover = t.raised_hover;
                let current = listing.as_ref().map(|l| l.path.clone()).unwrap_or_default();
                let up = parent_dir(&current);
                let dirs: Vec<String> = listing
                    .as_ref()
                    .map(|l| {
                        l.entries
                            .iter()
                            .filter(|e| e.kind == EntryKind::Dir && !e.name.starts_with('.'))
                            .map(|e| e.name.clone())
                            .collect()
                    })
                    .unwrap_or_default();
                let busy = *busy;
                card.child(title("Add a project"))
                    .child(
                        div()
                            .text_size(t.text_size_sm)
                            .text_color(t.text_secondary)
                            .child("Choose a folder on the server. Git repositories bring their worktrees; other folders open as files and terminals."),
                    )
                    .child(path.clone())
                    .child(
                        div()
                            .id("dir-list")
                            .h(px(220.))
                            .overflow_y_scroll()
                            .rounded(t.radius_md)
                            .bg(t.surface_bg)
                            .border_1()
                            .border_color(t.border)
                            .child(
                                div()
                                    .id("dir-up")
                                    .h(px(24.))
                                    .px(px(10.))
                                    .flex()
                                    .items_center()
                                    .cursor_pointer()
                                    .hover(move |s| s.bg(hover))
                                    .text_color(t.text_secondary)
                                    .child("..")
                                    .on_click(cx.listener(move |view, _, _, cx| view.browse(up.clone(), cx))),
                            )
                            .children(dirs.into_iter().enumerate().map(|(index, name)| {
                                let target = join(&current, &name);
                                div()
                                    .id(("dir", index))
                                    .h(px(24.))
                                    .px(px(10.))
                                    .flex()
                                    .items_center()
                                    .gap(px(6.))
                                    .cursor_pointer()
                                    .hover(move |s| s.bg(hover))
                                    .child(div().text_color(t.text_tertiary).child("▸"))
                                    .child(SharedString::from(name))
                                    .on_click(cx.listener(move |view, _, _, cx| view.browse(target.clone(), cx)))
                            })),
                    )
                    .children(error_row(error))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(8.))
                            .child(secondary_button("cancel-dialog", "Cancel", t).on_click(
                                cx.listener(|view, _, window, cx| view.close_dialog(window, cx)),
                            ))
                            .child(
                                primary_button("add-folder", if busy { "Adding…" } else { "Add this folder" }, t)
                                    .on_click(cx.listener(|view, _, _, cx| view.add_project(cx))),
                            ),
                    )
                    .child(div().h(px(1.)).bg(t.border))
                    .child(
                        div()
                            .text_size(t.text_size_sm)
                            .text_color(t.text_secondary)
                            .child("Or clone a repository into the folder above:"),
                    )
                    .child(url.clone())
                    .child(
                        div().flex().justify_end().child(
                            secondary_button("clone", "Clone", t)
                                .on_click(cx.listener(|view, _, _, cx| view.clone_project(cx))),
                        ),
                    )
            }
            Dialog::CreateWorktree {
                branch, error, busy, ..
            } => card
                .child(title("New worktree"))
                .child(
                    div()
                        .text_size(t.text_size_sm)
                        .text_color(t.text_secondary)
                        .child("A separate checkout for one task, branched from the project's default branch."),
                )
                .child(branch.clone())
                .children(error_row(error))
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .gap(px(8.))
                        .child(secondary_button("cancel-dialog", "Cancel", t).on_click(
                            cx.listener(|view, _, window, cx| view.close_dialog(window, cx)),
                        ))
                        .child(
                            primary_button("create-worktree", if *busy { "Creating…" } else { "Create" }, t)
                                .on_click(cx.listener(|view, _, _, cx| view.create_worktree(cx))),
                        ),
                ),
            Dialog::DeleteWorktree {
                label,
                needs_force,
                busy,
                ..
            } => {
                let force = needs_force.is_some();
                card.child(title("Delete worktree"))
                    .child(SharedString::from(format!(
                        "Remove the worktree “{label}” and its terminals from the server? Teardown scripts run first."
                    )))
                    .children(needs_force.clone().map(|reason| {
                        div()
                            .text_size(t.text_size_sm)
                            .text_color(t.danger)
                            .child(SharedString::from(format!("{reason} Deleting anyway discards that work.")))
                    }))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(8.))
                            .child(secondary_button("cancel-dialog", "Cancel", t).on_click(
                                cx.listener(|view, _, window, cx| view.close_dialog(window, cx)),
                            ))
                            .child(
                                primary_button(
                                    "confirm-delete",
                                    match (*busy, force) {
                                        (true, _) => "Deleting…",
                                        (false, true) => "Delete anyway",
                                        (false, false) => "Delete",
                                    },
                                    t,
                                )
                                .on_click(cx.listener(move |view, _, _, cx| view.delete_worktree(force, cx))),
                            ),
                    )
            }
        };
        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui::black().opacity(0.45))
                .child(content)
                .into_any_element(),
        )
    }
}

impl Render for HostWorkspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme().clone();
        let strip = self
            .render_documents_strip(&t, cx)
            .map(IntoElement::into_any_element);
        let active_document = self
            .documents
            .get(&self.scope)
            .and_then(|docs| docs.active.and_then(|i| docs.open.get(i)))
            .map(|doc| match &doc.kind {
                DocumentKind::File { editor, .. } => editor.clone().into_any_element(),
                DocumentKind::Diff(diff) => diff.clone().into_any_element(),
            });
        let content = match active_document {
            Some(document) => document,
            None => self.shell(window, cx).into_any_element(),
        };
        let bottom = self.render_bottom_bar(&t, cx);
        let panel = match self.selected_worktree().cloned() {
            Some(worktree) if self.panel_open => {
                Some(self.render_panel(&worktree, window, cx).into_any_element())
            }
            _ => None,
        };
        let banner = self.banner.clone();
        let dialog = self.render_dialog(&t, cx);
        div()
            .track_focus(&self.focus)
            .relative()
            .size_full()
            .flex()
            .flex_row()
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .h_full()
                    .flex()
                    .flex_col()
                    .when_some(banner, |d, message| {
                        d.child(
                            div()
                                .id("workspace-banner")
                                .px(px(12.))
                                .py(px(6.))
                                .bg(t.danger.opacity(0.15))
                                .cursor_pointer()
                                .child(message)
                                .on_click(cx.listener(|view, _, _, cx| {
                                    view.banner = None;
                                    cx.notify();
                                })),
                        )
                    })
                    .children(strip)
                    .child(div().flex_1().min_h(px(0.)).child(content))
                    .child(bottom),
            )
            .children(panel)
            .children(dialog)
    }
}
