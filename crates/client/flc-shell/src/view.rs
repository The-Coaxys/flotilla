//! The workspace view: view splits, tab strips and terminal panes.
//!
//! Rendering is driven by the shell model, which mirrors the daemon's
//! projected layout. Terminal panes render `flc-terminal` views; panes whose
//! child is not yet reported by the daemon show a placeholder instead of a
//! blank window.
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use fl_model::layout::{Axis, Layout, SplitNode, SplitTree};
use fl_model::{IncarnationId, PaneId, Size, SplitId, TabId, TerminalId, ViewId};
use flc_sync::{
    ClipboardTarget, LayoutHandle, MirrorEvent, MirrorStatus, Sync, SyncEvent, TerminalNotice,
};
use flc_terminal::{EchoPolicy, TerminalView, TerminalViewOptions};
use flc_ui::input::{TextInput, TextInputEvent};
use flc_ui::theme::{ActiveTheme, Theme};
use gpui::Focusable as _;
use gpui::prelude::*;
use gpui::{
    AsyncApp, Context, Div, Entity, FocusHandle, InteractiveElement, IntoElement, KeyDownEvent,
    ParentElement, Pixels, Render, SharedString, Styled, Subscription, Task, WeakEntity, Window,
    div, px, relative,
};
use tokio::sync::broadcast;

use crate::model::{Focus, ShellModel};
use crate::source::MirrorSource;

/// Presentation settings applied to every terminal pane of the shell
/// (spec §8.2: font and size are set per surface; the terminal palette is an
/// explicit, independent override of the engine's own).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ShellViewOptions {
    pub echo_policy: EchoPolicy,
    /// `None`: the theme's terminal font.
    pub font_family: Option<SharedString>,
    /// `None`: the theme's terminal size.
    pub font_size: Option<Pixels>,
    /// `None`: the palette carried by each frame (the engine's).
    pub palette: Option<Vec<u32>>,
}

/// What the shell asks its host to do outside the layout model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellEvent {
    /// A program asked (OSC 52) to replace the local clipboard. The host owns
    /// the per-host consent decision; the shell never writes it itself.
    ClipboardStore { terminal: TerminalId, text: String },
}

impl gpui::EventEmitter<ShellEvent> for ShellView {}

pub struct ShellView {
    sync: Arc<Sync>,
    layout: LayoutHandle,
    options: ShellViewOptions,
    model: ShellModel,
    sources: HashMap<(TerminalId, IncarnationId), Arc<MirrorSource>>,
    /// One listener per source for titles and live notices.
    listeners: HashMap<(TerminalId, IncarnationId), Task<()>>,
    /// Terminals that rang while their tab was in the background.
    bells: HashSet<TerminalId>,
    terminals: HashMap<(ViewId, TabId, PaneId), Entity<TerminalView>>,
    /// The pane keyboard focus was last moved to; a different focused pane
    /// in the layout moves the keyboard to its terminal.
    keyboard_pane: Option<(ViewId, TabId, PaneId)>,
    focus: FocusHandle,
    rename: Option<(TabId, Entity<TextInput>)>,
    banner: Option<SharedString>,
    /// Link latency published by the supervisor; `None` while disconnected.
    rtt: Option<Duration>,
    /// Whether the first authoritative layout has been seen. A workspace
    /// that is empty at that moment gets its first terminal automatically.
    first_layout_seen: bool,
    _workspace: Task<()>,
    _events: Task<()>,
    _subscriptions: Vec<Subscription>,
}

/// Label of an unnamed tab: the program's title, else the last component
/// of the shell's reported directory (OSC 7 `file://host/path`), else a
/// generic name.
pub(crate) fn tab_label(title: Option<&str>, cwd: Option<&str>) -> String {
    if let Some(title) = title.map(str::trim).filter(|title| !title.is_empty()) {
        return title.to_string();
    }
    cwd.and_then(directory_name)
        .unwrap_or_else(|| "Terminal".to_string())
}

fn directory_name(uri: &str) -> Option<String> {
    let path = uri.strip_prefix("file://")?;
    let path = &path[path.find('/')?..];
    let name = path.trim_end_matches('/').rsplit('/').next()?;
    if name.is_empty() {
        return Some("/".to_string());
    }
    let bytes = name.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let hex = bytes
            .get(index + 1..index + 3)
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match (bytes[index], hex) {
            (b'%', Some(byte)) => {
                decoded.push(byte);
                index += 3;
            }
            (byte, _) => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    Some(String::from_utf8_lossy(&decoded).into_owned())
}

/// Workspace shortcuts, independent of GPUI event plumbing so the whole
/// table is testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Shortcut {
    NewTerminal,
    CloseTab,
    RenameTab,
    SplitPane(Axis),
    SplitView(Axis),
    CycleTab {
        forward: bool,
    },
    FocusPane {
        axis: Axis,
        forward: bool,
    },
    /// `pane` selects the focused pane's split (true) or the view split.
    AdjustRatio {
        pane: bool,
        forward: bool,
    },
    ClosePane,
    CloseView,
    TogglePin,
    ReorderTab {
        forward: bool,
    },
}

/// The shortcut a keystroke means, or `None` when the terminal keeps it:
/// plain Ctrl+letter, Alt+letter and shifted text must reach the shell, not
/// the workspace.
pub(crate) fn shortcut_for(ctrl: bool, shift: bool, alt: bool, key: &str) -> Option<Shortcut> {
    use Shortcut::*;
    Some(match (ctrl, shift, alt, key) {
        (true, true, false, "t") => NewTerminal,
        (true, true, false, "w") => CloseTab,
        (true, true, false, "r") => RenameTab,
        // Ctrl-Tab cycles forward; Ctrl-Shift-Tab cycles back.
        (true, _, false, "tab") => CycleTab { forward: !shift },
        (true, true, false, "left") => ReorderTab { forward: false },
        (true, true, false, "right") => ReorderTab { forward: true },
        // Windows reports Shift+= / Shift+- as "+" / "_" with shift cleared;
        // other platforms keep the unshifted key and the shift flag.
        (false, true, true, "=") | (false, _, true, "+") => SplitPane(Axis::Horizontal),
        (false, true, true, "-") | (false, _, true, "_") => SplitPane(Axis::Vertical),
        (true, true, true, "=") | (true, _, true, "+") => SplitView(Axis::Horizontal),
        (true, true, true, "-") | (true, _, true, "_") => SplitView(Axis::Vertical),
        (false, false, true, "left") => FocusPane {
            axis: Axis::Horizontal,
            forward: false,
        },
        (false, false, true, "right") => FocusPane {
            axis: Axis::Horizontal,
            forward: true,
        },
        (false, false, true, "up") => FocusPane {
            axis: Axis::Vertical,
            forward: false,
        },
        (false, false, true, "down") => FocusPane {
            axis: Axis::Vertical,
            forward: true,
        },
        (false, true, true, "left") => AdjustRatio {
            pane: true,
            forward: false,
        },
        (false, true, true, "right") => AdjustRatio {
            pane: true,
            forward: true,
        },
        (false, true, true, "w") => ClosePane,
        (false, true, true, "v") => CloseView,
        (false, true, true, "p") => TogglePin,
        _ => return None,
    })
}

impl ShellView {
    /// A shell over one layout scope of `sync` (the host's home layout or
    /// a worktree's).
    pub fn new(
        sync: Arc<Sync>,
        layout: LayoutHandle,
        options: ShellViewOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut model = ShellModel::default();
        model.adopt(&layout.workspace());
        let mut view = Self {
            sync: sync.clone(),
            layout: layout.clone(),
            options,
            model,
            sources: HashMap::new(),
            listeners: HashMap::new(),
            bells: HashSet::new(),
            terminals: HashMap::new(),
            keyboard_pane: None,
            focus: cx.focus_handle(),
            rename: None,
            banner: None,
            rtt: None,
            first_layout_seen: false,
            _workspace: Task::ready(()),
            _events: Task::ready(()),
            _subscriptions: Vec::new(),
        };
        view.open_first_terminal(cx);
        view.attach_panes(window, cx);

        let mut changes = layout.workspace_changes();
        view._workspace = cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            loop {
                if changes.changed().await.is_err() {
                    break;
                }
                let alive = this.update(cx, |view, cx| {
                    view.model.adopt(&view.layout.workspace());
                    view.open_first_terminal(cx);
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        });

        let mut events = sync.events();
        view._events = cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            loop {
                let event = match events.recv().await {
                    Ok(event) => event,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                let alive = this.update(cx, |view, cx| {
                    match &event {
                        SyncEvent::Disconnected => {
                            view.banner = Some(
                                "Reconnecting… the terminals below are the last frames received."
                                    .into(),
                            );
                        }
                        SyncEvent::Connected { .. } => view.banner = None,
                        SyncEvent::IncompatibleEngine { daemon } => {
                            view.banner = Some(
                                format!(
                                    "This daemon runs an incompatible terminal engine ({daemon})."
                                )
                                .into(),
                            );
                        }
                        SyncEvent::ChannelsFailed { error } => {
                            view.banner =
                                Some(format!("Terminal streams are unavailable: {error}").into());
                        }
                        SyncEvent::LayoutRejected { error, .. } => {
                            view.banner = Some(
                                format!("The workspace refused a layout change: {error}").into(),
                            );
                        }
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        window.focus(&view.focus);
        view
    }

    /// Keyboard input follows the layout's focused pane: a new tab, a split,
    /// tab cycling or a pane move puts the cursor in that pane's terminal.
    /// Focus held outside the shell (a dialog, the rename editor) is left
    /// alone.
    fn follow_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Focus {
            view,
            tab: Some(tab),
            pane: Some(pane),
        }) = self.model.focus()
        else {
            return;
        };
        let key = (view, tab, pane);
        let Some(terminal) = self.terminals.get(&key) else {
            return;
        };
        if self.rename.is_some() {
            return;
        }
        let handle = terminal.read(cx).focus_handle(cx);
        // The keyboard belongs to the shell when the shell itself, nothing,
        // or any of its terminals holds focus — including a terminal in a tab
        // that is no longer shown, which would otherwise swallow every key.
        // Focus in a dialog or editor elsewhere is left alone.
        let shell_owned = self.focus.is_focused(window)
            || window.focused(cx).is_none()
            || self
                .terminals
                .values()
                .any(|view| view.read(cx).focus_handle(cx).is_focused(window));
        if shell_owned && (self.keyboard_pane != Some(key) || !handle.is_focused(window)) {
            self.keyboard_pane = Some(key);
            window.focus(&handle);
        }
    }

    /// Opening a fresh workspace lands in a shell, not an empty view. Only
    /// the first authoritative layout counts: closing every tab later leaves
    /// the empty view (and its New terminal button) alone.
    fn open_first_terminal(&mut self, cx: &mut Context<Self>) {
        if self.first_layout_seen || self.model.stale() || self.model.pending() > 0 {
            return;
        }
        let Some(layout) = self.model.layout() else {
            return;
        };
        self.first_layout_seen = true;
        if layout.views.iter().all(|view| view.tabs.is_empty()) {
            self.create_terminal(cx);
        }
    }

    /// Publish the current link latency to every pane, including panes the
    /// daemon has not reported yet. `None` while disconnected disables
    /// automatic predictive echo until a measurement returns.
    ///
    /// Panes read the latency when input is typed, so nothing re-renders:
    /// heartbeats update it every few seconds.
    /// Apply new presentation settings to every terminal pane, live.
    pub fn set_presentation(&mut self, options: ShellViewOptions, cx: &mut Context<Self>) {
        if self.options == options {
            return;
        }
        self.options = options;
        let presentation = self.options.clone();
        for terminal in self.terminals.values() {
            terminal.update(cx, |view, cx| {
                let mut next = view.options().clone();
                next.echo_policy = presentation.echo_policy;
                next.font_family = presentation.font_family.clone();
                next.font_size = presentation.font_size;
                next.palette = presentation.palette.clone();
                view.set_options(next, cx);
            });
        }
        cx.notify();
    }

    pub fn set_rtt(&mut self, rtt: Option<Duration>) {
        if self.rtt == rtt {
            return;
        }
        self.rtt = rtt;
        for source in self.sources.values() {
            source.set_rtt(rtt);
        }
    }

    /// Window focus follows the shell; pane focus follows the layout.
    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    pub fn model(&self) -> &ShellModel {
        &self.model
    }

    pub fn sync(&self) -> &Arc<Sync> {
        &self.sync
    }

    /// Create or refresh one terminal view per visible pane whose child the
    /// daemon has reported. Panes without a child stay placeholders.
    fn attach_panes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(layout) = self.model.layout().cloned() else {
            return;
        };
        let mut wanted: Vec<((ViewId, TabId, PaneId), TerminalId, IncarnationId)> = Vec::new();
        for view in &layout.views {
            for tab in &view.tabs {
                for pane in &tab.panes {
                    if let Some(incarnation) = self.model.incarnation(pane.terminal) {
                        wanted.push(((view.id, tab.id, pane.id), pane.terminal, incarnation));
                    }
                }
            }
        }
        for (key, terminal, incarnation) in wanted {
            if self.terminals.contains_key(&key) {
                continue;
            }
            let source_key = (terminal, incarnation);
            let source = match self.sources.get(&source_key) {
                Some(source) => source.clone(),
                None => {
                    let handle = self.sync.attach(terminal, incarnation, key.0);
                    let source = MirrorSource::new(handle);
                    source.set_rtt(self.rtt);
                    self.listeners.insert(source_key, self.listen(&source, cx));
                    self.sources.insert(source_key, source.clone());
                    source
                }
            };
            let options = TerminalViewOptions {
                echo_policy: self.options.echo_policy,
                font_family: self.options.font_family.clone(),
                font_size: self.options.font_size,
                palette: self.options.palette.clone(),
                ..TerminalViewOptions::default()
            };
            let terminal_view = cx.new(|cx| TerminalView::new(source, options, window, cx));
            self.terminals.insert(key, terminal_view);
        }
        // Drop views for panes that no longer exist.
        let live: Vec<(ViewId, TabId, PaneId)> = layout
            .views
            .iter()
            .flat_map(|view| view.tabs.iter().map(move |tab| (view.id, tab.id, tab)))
            .flat_map(|(view, tab, item)| item.panes.iter().map(move |pane| (view, tab, pane.id)))
            .collect();
        self.terminals.retain(|key, _| live.contains(key));
        self.sources.retain(|(terminal, _), _| {
            layout
                .views
                .iter()
                .flat_map(|view| &view.tabs)
                .flat_map(|tab| &tab.panes)
                .any(|entry| entry.terminal == *terminal)
        });
        let sources = &self.sources;
        self.listeners.retain(|key, _| sources.contains_key(key));
        self.bells
            .retain(|terminal| sources.keys().any(|(live, _)| live == terminal));
    }

    /// Follow one mirror's title changes and live notices until the source
    /// is dropped from the layout (dropping the task ends the loop).
    fn listen(&self, source: &MirrorSource, cx: &mut Context<Self>) -> Task<()> {
        let terminal = source.handle().terminal();
        let mut events = source.handle().events();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            loop {
                let event = match events.recv().await {
                    Ok(event) => event,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                let alive = this.update(cx, |view, cx| match event {
                    MirrorEvent::TitleChanged { .. } => cx.notify(),
                    MirrorEvent::Notice {
                        notice: TerminalNotice::Bell,
                        ..
                    } => {
                        if !view.in_active_tab(terminal) && view.bells.insert(terminal) {
                            cx.notify();
                        }
                    }
                    MirrorEvent::Notice {
                        notice:
                            TerminalNotice::ClipboardStore {
                                target: ClipboardTarget::Clipboard,
                                text,
                            },
                        ..
                    } => cx.emit(ShellEvent::ClipboardStore { terminal, text }),
                    // Windows has no primary selection to store into.
                    _ => {}
                });
                if alive.is_err() {
                    break;
                }
            }
        })
    }

    fn in_active_tab(&self, terminal: TerminalId) -> bool {
        self.model.layout().is_some_and(|layout| {
            layout.views.iter().any(|view| {
                view.tabs.iter().any(|tab| {
                    Some(tab.id) == view.active
                        && tab.panes.iter().any(|pane| pane.terminal == terminal)
                })
            })
        })
    }

    fn create_terminal(&mut self, cx: &mut Context<Self>) {
        let terminal = TerminalId::new();
        // The daemon confirms the real grid size once a pane is laid out.
        if let Some(op) = self.model.new_tab(terminal, Size::new(80, 24), None) {
            self.submit(op, cx);
        }
    }

    fn submit(&mut self, op: fl_model::layout::LayoutOp, cx: &mut Context<Self>) {
        let ticket = self.layout.apply(op);
        cx.spawn(
            async move |this: WeakEntity<ShellView>, cx: &mut AsyncApp| match ticket.wait().await {
                Ok(_) => {}
                Err(error) => {
                    let _ = this.update(cx, |view, cx| {
                        view.banner = Some(format!("Layout change failed: {error}").into());
                        cx.notify();
                    });
                }
            },
        )
        .detach();
    }

    fn next_tab(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(layout) = self.model.layout() else {
            return;
        };
        let Some(focus) = self.model.focus() else {
            return;
        };
        let Some(view) = layout.view(focus.view) else {
            return;
        };
        if view.tabs.len() < 2 {
            return;
        }
        let current = view
            .tabs
            .iter()
            .position(|tab| Some(tab.id) == focus.tab)
            .unwrap_or(0);
        let count = view.tabs.len();
        let next = if forward {
            (current + 1) % count
        } else {
            (current + count - 1) % count
        };
        let id = view.tabs[next].id;
        if let Some(op) = self.model.select_tab(id) {
            self.submit(op, cx);
        }
    }

    fn focus_adjacent_pane(&mut self, axis: Axis, forward: bool, cx: &mut Context<Self>) {
        let Some(layout) = self.model.layout() else {
            return;
        };
        let Some(focus) = self.model.focus() else {
            return;
        };
        let Some(tab) = layout.tab(
            focus.view,
            match focus.tab {
                Some(tab) => tab,
                None => return,
            },
        ) else {
            return;
        };
        let Some(current) = focus.pane else {
            return;
        };
        // Visual order of leaves; neighbours along the same axis are best-effort
        // because a split tree has no directional geometry without bounds.
        let leaves: Vec<PaneId> = tab.split.leaves().collect();
        let Some(index) = leaves.iter().position(|pane| *pane == current) else {
            return;
        };
        let next = if forward {
            (index + 1) % leaves.len()
        } else {
            (index + leaves.len() - 1) % leaves.len()
        };
        let _ = axis;
        let target = leaves[next];
        if let Some(op) = self.model.select_pane(target) {
            self.submit(op, cx);
        }
    }

    /// Move the active tab one slot in its view.
    fn reorder_tab(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(focus) = self.model.focus() else {
            return;
        };
        let Some(view) = self.model.view(focus.view) else {
            return;
        };
        let Some(current) = view.tabs.iter().position(|tab| Some(tab.id) == focus.tab) else {
            return;
        };
        let index = if forward {
            (current + 1).min(view.tabs.len() - 1)
        } else {
            current.saturating_sub(1)
        };
        if index == current {
            return;
        }
        if let Some(op) = self.model.reorder_tab(index as u16) {
            self.submit(op, cx);
        }
    }

    fn adjust_ratio(&mut self, pane: bool, forward: bool, cx: &mut Context<Self>) {
        let Some(layout) = self.model.layout() else {
            return;
        };
        let Some(focus) = self.model.focus() else {
            return;
        };
        let split = if pane {
            layout
                .tab(
                    focus.view,
                    match focus.tab {
                        Some(tab) => tab,
                        None => return,
                    },
                )
                .and_then(|tab| first_split(&tab.split, focus.pane))
        } else {
            first_split(&layout.split, Some(focus.view))
        };
        let Some((id, current)) = split else {
            return;
        };
        let step: i32 = if forward { 500 } else { -500 };
        let next = (i32::from(current) + step).clamp(
            i32::from(crate::model::MIN_RATIO),
            i32::from(crate::model::MAX_RATIO),
        ) as u16;
        let op = if pane {
            self.model.set_pane_ratio(id, next)
        } else {
            self.model.set_view_ratio(id, next)
        };
        if let Some(op) = op {
            self.submit(op, cx);
        }
    }

    fn on_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let keystroke = &event.keystroke;
        let modifiers = &keystroke.modifiers;
        let Some(shortcut) = shortcut_for(
            modifiers.control,
            modifiers.shift,
            modifiers.alt,
            &keystroke.key,
        ) else {
            return false;
        };
        match shortcut {
            Shortcut::NewTerminal => self.create_terminal(cx),
            Shortcut::CloseTab => {
                if let Some(op) = self.model.close_tab() {
                    self.submit(op, cx);
                }
            }
            Shortcut::RenameTab => self.begin_rename(cx),
            Shortcut::SplitPane(axis) => {
                let terminal = TerminalId::new();
                if let Some(op) = self.model.split_pane(terminal, axis, Size::new(80, 24)) {
                    self.submit(op, cx);
                }
            }
            Shortcut::SplitView(axis) => {
                let terminal = TerminalId::new();
                if let Some(op) = self.model.split_view(terminal, axis, Size::new(80, 24)) {
                    self.submit(op, cx);
                }
            }
            Shortcut::CycleTab { forward } => self.next_tab(forward, cx),
            Shortcut::FocusPane { axis, forward } => {
                self.focus_adjacent_pane(axis, forward, cx);
            }
            Shortcut::AdjustRatio { pane, forward } => self.adjust_ratio(pane, forward, cx),
            Shortcut::ClosePane => {
                if let Some(op) = self.model.close_pane() {
                    self.submit(op, cx);
                }
            }
            Shortcut::CloseView => {
                if let Some(op) = self.model.close_view() {
                    self.submit(op, cx);
                }
            }
            Shortcut::TogglePin => {
                let pinned = self
                    .model
                    .focus()
                    .and_then(|focus| focus.tab)
                    .and_then(|tab| {
                        let focus = self.model.focus()?;
                        self.model
                            .layout()?
                            .tab(focus.view, tab)
                            .map(|item| item.pinned)
                    })
                    .unwrap_or(false);
                if let Some(op) = self.model.pin_tab(!pinned) {
                    self.submit(op, cx);
                }
            }
            Shortcut::ReorderTab { forward } => self.reorder_tab(forward, cx),
        }
        cx.notify();
        true
    }

    /// Open the inline editor for the focused tab's name.
    fn begin_rename(&mut self, cx: &mut Context<Self>) {
        let Some(focus) = self.model.focus() else {
            return;
        };
        let Some(tab) = focus.tab else {
            return;
        };
        let Some(item) = self
            .model
            .layout()
            .and_then(|layout| layout.tab(focus.view, tab))
        else {
            return;
        };
        let editor = cx.new(|cx| TextInput::new(cx, "Tab name"));
        if let Some(name) = &item.name {
            editor.update(cx, |input, cx| {
                input.set_text(SharedString::from(name.clone()), cx);
            });
        }
        cx.subscribe(&editor, |this, _editor, event: &TextInputEvent, cx| {
            this.rename.take();
            // Enter commits even to an empty name: `None` restores the
            // automatic title. Escape leaves the tab alone.
            let name = match event {
                TextInputEvent::Submitted(name) => {
                    let trimmed = name.trim();
                    (!trimmed.is_empty()).then(|| trimmed.to_string())
                }
                TextInputEvent::Cancelled => {
                    cx.notify();
                    return;
                }
            };
            if let Some(op) = this.model.rename_tab(name) {
                this.submit(op, cx);
            }
            cx.notify();
        })
        .detach();
        self.rename = Some((tab, editor));
        cx.notify();
    }

    fn render_tab_strip(
        &self,
        view: &fl_model::layout::View,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> Div {
        let mut strip = div()
            .flex()
            .overflow_hidden()
            .items_center()
            .h(px(32.))
            .px(px(6.))
            .gap(px(2.))
            .bg(t.chrome_bg)
            .border_b_1()
            .border_color(t.border);
        for tab in &view.tabs {
            let active = Some(tab.id) == view.active;
            let tab_id = tab.id;
            let pinned = tab.pinned;
            let state = tab.panes.first().and_then(|pane| {
                let incarnation = self.model.incarnation(pane.terminal)?;
                Some(
                    self.sources
                        .get(&(pane.terminal, incarnation))
                        .map(|source| source.handle().state()),
                )
            });
            let label = match (&tab.name, &state) {
                (Some(name), _) => name.clone(),
                (None, None) => "Starting…".to_string(),
                (None, Some(state)) => tab_label(
                    state.as_ref().and_then(|s| s.title.as_deref()),
                    state.as_ref().and_then(|s| s.cwd.as_deref()),
                ),
            };
            let rang = !active
                && tab
                    .panes
                    .iter()
                    .any(|pane| self.bells.contains(&pane.terminal));
            let mut item = div()
                .id(SharedString::from(format!("tab-{tab_id}")))
                .flex()
                .flex_shrink()
                .min_w(px(56.))
                .max_w(px(220.))
                .overflow_hidden()
                .items_center()
                .gap(px(6.))
                .px(px(10.))
                .h(px(26.))
                .rounded(t.radius_sm)
                .text_size(t.text_size_sm)
                .text_color(if active {
                    t.text_primary
                } else {
                    t.text_secondary
                })
                .cursor_pointer()
                .when(active, |d| {
                    d.bg(t.row_selected)
                        .border_1()
                        .border_color(t.row_selected_border)
                })
                .hover(move |s| s.bg(t.raised_hover))
                .on_click(cx.listener(move |this, _, window, cx| {
                    if let Some(op) = this.model.select_tab(tab_id) {
                        this.submit(op, cx);
                    }
                    this.focus.focus(window);
                }))
                .child(if pinned {
                    div().text_color(t.accent).child("•")
                } else {
                    div()
                })
                .child(
                    div()
                        .min_w(px(0.))
                        .truncate()
                        .child(SharedString::from(label)),
                )
                .when(rang, |d| d.child(div().text_color(t.warning).child("•")));
            if let Some((id, editor)) = &self.rename
                && *id == tab_id
            {
                item = item.child(div().w(px(180.)).child(editor.clone()));
            }
            strip = strip.child(item);
        }
        strip
            .child(
                div()
                    .id("new-tab")
                    .flex_none()
                    .ml(px(4.))
                    .px(px(8.))
                    .h(px(24.))
                    .rounded(t.radius_sm)
                    .text_color(t.text_secondary)
                    .cursor_pointer()
                    .hover(move |s| s.bg(t.raised_hover))
                    .on_click(cx.listener(|this, _, _, cx| this.create_terminal(cx)))
                    .child("+"),
            )
            .child(
                div()
                    .id("split-view")
                    .flex_none()
                    .px(px(8.))
                    .h(px(24.))
                    .rounded(t.radius_sm)
                    .text_color(t.text_tertiary)
                    .cursor_pointer()
                    .hover(move |s| s.bg(t.raised_hover))
                    .on_click(cx.listener(|this, _, _, cx| {
                        let terminal = TerminalId::new();
                        if let Some(op) =
                            this.model
                                .split_view(terminal, Axis::Horizontal, Size::new(80, 24))
                        {
                            this.submit(op, cx);
                        }
                    }))
                    .child("Split"),
            )
            .child(div().flex_1())
            .when_some(
                self.model.pending().gt(&0).then_some("Saving…"),
                |d, label| d.child(div().mr(px(8.)).text_color(t.text_tertiary).child(label)),
            )
            .when(self.model.stale(), |d| {
                d.child(div().mr(px(8.)).text_color(t.warning).child("Offline copy"))
            })
    }

    fn render_pane_tree(
        &self,
        tree: &SplitTree<PaneId>,
        index: u16,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match tree.nodes.get(usize::from(index)) {
            Some(SplitNode::Leaf(pane)) => self.render_pane(*pane, t, cx),
            Some(SplitNode::Branch {
                id: split,
                axis,
                ratio,
                first,
                second,
            }) => {
                let fraction = f32::from(*ratio) / 10_000.0;
                let divider = div()
                    .id(SharedString::from(format!("pane-divider-{split}")))
                    .when(matches!(axis, Axis::Horizontal), |d| {
                        d.w(px(1.)).h_full().cursor_col_resize()
                    })
                    .when(matches!(axis, Axis::Vertical), |d| {
                        d.h(px(1.)).w_full().cursor_row_resize()
                    })
                    .bg(t.border)
                    .on_click({
                        let split = *split;
                        let (first, second) = (*first, *second);
                        let axis = *axis;
                        let _ = (first, second);
                        cx.listener(move |this, _, _, cx| {
                            // Double-click is not exposed by this handler; a
                            // plain click resets the split to even.
                            let _ = axis;
                            if let Some(op) = this.model.set_pane_ratio(split, 5000) {
                                this.submit(op, cx);
                            }
                        })
                    });
                let direction = if matches!(axis, Axis::Horizontal) {
                    div().flex().flex_row()
                } else {
                    div().flex().flex_col()
                };
                direction
                    .size_full()
                    .child(
                        div()
                            .flex_basis(relative(fraction))
                            .min_w(px(0.))
                            .min_h(px(0.))
                            .child(self.render_pane_tree(tree, *first, t, cx)),
                    )
                    .child(divider)
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .min_h(px(0.))
                            .child(self.render_pane_tree(tree, *second, t, cx)),
                    )
                    .into_any_element()
            }
            _ => div().size_full().bg(t.surface_bg).into_any_element(),
        }
    }

    fn render_pane(&self, pane_id: PaneId, t: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(layout) = self.model.layout() else {
            return div().into_any_element();
        };
        let focused = self
            .model
            .focus()
            .is_some_and(|focus| focus.pane == Some(pane_id));
        let mut found = None;
        for view in &layout.views {
            for tab in &view.tabs {
                if let Some(pane) = tab.panes.iter().find(|pane| pane.id == pane_id) {
                    found = Some((view.id, tab.id, pane.terminal));
                }
            }
        }
        let Some((view_id, tab_id, terminal)) = found else {
            return div().into_any_element();
        };
        let key = (view_id, tab_id, pane_id);
        let body: gpui::AnyElement = match self.terminals.get(&key) {
            Some(terminal_view) => terminal_view.clone().into_any_element(),
            None => {
                let status = if self.model.stale() {
                    "Not connected yet"
                } else {
                    "Starting shell…"
                };
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(6.))
                    .text_color(t.text_secondary)
                    .bg(t.surface_bg)
                    .child(SharedString::from(status))
                    .child(
                        div()
                            .text_size(t.text_size_sm)
                            .text_color(t.text_tertiary)
                            .child(SharedString::from(format!("terminal {terminal}"))),
                    )
                    .into_any_element()
            }
        };
        div()
            .id(SharedString::from(format!("pane-{pane_id}")))
            .size_full()
            .p(px(2.))
            .bg(t.surface_bg)
            .when(focused, |d| {
                d.border_1().border_color(t.accent.opacity(0.4))
            })
            // Capture phase: the terminal inside stops its own mouse-down, so
            // a bubbling handler would never learn the pane was chosen.
            .capture_any_mouse_down(cx.listener(move |this, _, window, cx| {
                if let Some(op) = this.model.select_pane(pane_id) {
                    this.submit(op, cx);
                }
                // The pane's terminal (if any) takes the keyboard on render.
                this.keyboard_pane = None;
                this.focus.focus(window);
            }))
            .child(body)
            .into_any_element()
    }

    fn render_view_tree(
        &self,
        layout: &Layout,
        index: u16,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match layout.split.nodes.get(usize::from(index)) {
            Some(SplitNode::Leaf(view)) => {
                let Some(model_view) = layout.view(*view) else {
                    return div().into_any_element();
                };
                let focused = layout.active == *view;
                let strip = self.render_tab_strip(model_view, t, cx);
                let body = if model_view.tabs.is_empty() {
                    div()
                        .size_full()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap(px(10.))
                        .text_color(t.text_secondary)
                        .child("No terminals in this view")
                        .child(
                            div()
                                .id("new-terminal")
                                .px(px(14.))
                                .h(px(30.))
                                .rounded(t.radius_md)
                                .bg(t.accent)
                                .text_color(gpui::white())
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| this.create_terminal(cx)))
                                .child("New terminal"),
                        )
                        .into_any_element()
                } else {
                    let tab = model_view
                        .tabs
                        .iter()
                        .find(|tab| Some(tab.id) == model_view.active)
                        .or_else(|| model_view.tabs.first());
                    match tab {
                        Some(tab) => self.render_pane_tree(&tab.split, tab.split.root, t, cx),
                        None => div().into_any_element(),
                    }
                };
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .when(focused, |d| d.border_1().border_color(t.border))
                    .child(strip)
                    .child(div().flex_1().min_h(px(0.)).child(body))
                    .into_any_element()
            }
            Some(SplitNode::Branch {
                id: split,
                axis,
                ratio,
                first,
                second,
            }) => {
                let fraction = f32::from(*ratio) / 10_000.0;
                let divider = div()
                    .id(SharedString::from(format!("view-divider-{split}")))
                    .when(matches!(axis, Axis::Horizontal), |d| {
                        d.w(px(1.)).h_full().cursor_col_resize()
                    })
                    .when(matches!(axis, Axis::Vertical), |d| {
                        d.h(px(1.)).w_full().cursor_row_resize()
                    })
                    .bg(t.border);
                let split = *split;
                let direction = if matches!(axis, Axis::Horizontal) {
                    div().flex().flex_row()
                } else {
                    div().flex().flex_col()
                };
                direction
                    .size_full()
                    .child(
                        div()
                            .flex_basis(relative(fraction))
                            .min_w(px(0.))
                            .min_h(px(0.))
                            .child(self.render_view_tree(layout, *first, t, cx)),
                    )
                    .child(divider.on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(op) = this.model.set_view_ratio(split, 5000) {
                            this.submit(op, cx);
                        }
                    })))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .min_h(px(0.))
                            .child(self.render_view_tree(layout, *second, t, cx)),
                    )
                    .into_any_element()
            }
            _ => div().size_full().bg(t.surface_bg).into_any_element(),
        }
    }
}

/// The nearest split enclosing `leaf`, with its current ratio.
fn first_split<T>(tree: &SplitTree<T>, leaf: Option<T>) -> Option<(SplitId, u16)>
where
    T: Copy + Eq,
{
    let leaf = leaf?;
    let mut focus = tree.leaf_index(leaf)?;
    loop {
        let parent = tree
            .nodes
            .iter()
            .enumerate()
            .find_map(|(index, node)| match node {
                SplitNode::Branch {
                    id,
                    ratio,
                    first,
                    second,
                    ..
                } if *first == focus || *second == focus => Some((index as u16, *id, *ratio)),
                _ => None,
            })?;
        if parent.0 == tree.root {
            return Some((parent.1, parent.2));
        }
        focus = parent.0;
    }
}

impl Render for ShellView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.attach_panes(window, cx);
        self.follow_focus(window, cx);
        // Showing a tab acknowledges its bell.
        let bells = std::mem::take(&mut self.bells);
        self.bells = bells
            .into_iter()
            .filter(|terminal| !self.in_active_tab(*terminal))
            .collect();
        let t = cx.theme().clone();
        let content: gpui::AnyElement = match self.model.layout() {
            Some(layout) => self.render_view_tree(layout, layout.split.root, &t, cx),
            None => div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(10.))
                .bg(t.surface_bg)
                .text_color(t.text_secondary)
                .child("Waiting for the workspace layout…")
                .into_any_element(),
        };
        let banner = self.banner.clone();
        div()
            .id("shell")
            .key_context("Shell")
            .track_focus(&self.focus)
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.surface_bg)
            .text_color(t.text_primary)
            .font_family(t.font_ui.clone())
            .text_size(t.text_size)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if this.on_key(event, cx) {
                    cx.stop_propagation();
                }
            }))
            .when_some(banner, |d, message| {
                d.child(
                    div()
                        .id("shell-banner")
                        .px(px(12.))
                        .py(px(6.))
                        .bg(t.surface_raised)
                        .text_color(t.warning)
                        .child(message),
                )
            })
            .child(content)
    }
}

impl ShellView {
    /// Which mirrors are live, for the status row.
    pub fn live_terminals(&self) -> Vec<(TerminalId, MirrorStatus)> {
        self.sources
            .values()
            .map(|source| {
                let handle = source.handle();
                (handle.terminal(), handle.state().status.clone())
            })
            .collect()
    }
}

#[cfg(test)]
mod shortcut_tests {
    use super::{Axis, Shortcut, shortcut_for};

    fn chord(ctrl: bool, shift: bool, alt: bool, key: &str) -> Option<Shortcut> {
        shortcut_for(ctrl, shift, alt, key)
    }

    #[test]
    fn spec_chords_map_to_their_workspace_actions() {
        use Shortcut::*;
        assert_eq!(chord(true, true, false, "t"), Some(NewTerminal));
        assert_eq!(chord(true, true, false, "w"), Some(CloseTab));
        assert_eq!(chord(true, true, false, "r"), Some(RenameTab));
        assert_eq!(
            chord(false, true, true, "="),
            Some(SplitPane(Axis::Horizontal))
        );
        assert_eq!(
            chord(false, true, true, "-"),
            Some(SplitPane(Axis::Vertical))
        );
        assert_eq!(
            chord(true, true, true, "="),
            Some(SplitView(Axis::Horizontal))
        );
        assert_eq!(
            chord(true, true, true, "-"),
            Some(SplitView(Axis::Vertical))
        );
        assert_eq!(
            chord(false, false, true, "right"),
            Some(FocusPane {
                axis: Axis::Horizontal,
                forward: true
            })
        );
        assert_eq!(
            chord(false, true, true, "left"),
            Some(AdjustRatio {
                pane: true,
                forward: false
            })
        );
    }

    /// GPUI on Windows turns Shift+= / Shift+- into "+" / "_" and clears the
    /// shift flag; the split chords must still fire.
    #[test]
    fn split_chords_accept_the_windows_shifted_spelling() {
        use Shortcut::*;
        assert_eq!(
            chord(false, false, true, "+"),
            Some(SplitPane(Axis::Horizontal))
        );
        assert_eq!(
            chord(false, false, true, "_"),
            Some(SplitPane(Axis::Vertical))
        );
        assert_eq!(
            chord(true, false, true, "+"),
            Some(SplitView(Axis::Horizontal))
        );
        assert_eq!(
            chord(true, false, true, "_"),
            Some(SplitView(Axis::Vertical))
        );
        // Without Alt these are typed characters.
        assert_eq!(chord(false, false, false, "+"), None);
        assert_eq!(chord(false, false, false, "_"), None);
    }

    /// Ctrl-Tab cycles forward, Ctrl-Shift-Tab must cycle back instead of
    /// falling through to no action.
    #[test]
    fn tab_cycling_honours_the_shift_modifier() {
        assert_eq!(
            chord(true, false, false, "tab"),
            Some(Shortcut::CycleTab { forward: true })
        );
        assert_eq!(
            chord(true, true, false, "tab"),
            Some(Shortcut::CycleTab { forward: false })
        );
    }

    /// Plain Ctrl+letter and typed text belong to the terminal.
    #[test]
    fn terminal_chords_are_not_workspace_shortcuts() {
        for key in ["a", "c", "d", "l", "v", "z", "t", "w", "r"] {
            assert_eq!(chord(true, false, false, key), None, "ctrl-{key}");
            assert_eq!(chord(false, false, false, key), None, "{key}");
            assert_eq!(chord(false, true, false, key), None, "shift-{key}");
        }
        assert_eq!(chord(false, false, false, "tab"), None);
        assert_eq!(chord(false, true, false, "tab"), None);
    }
}

#[cfg(test)]
mod label_tests {
    use super::tab_label;

    #[test]
    fn the_program_title_wins_then_the_reported_directory() {
        assert_eq!(
            tab_label(Some("vim notes.md"), Some("file://box/srv")),
            "vim notes.md"
        );
        assert_eq!(tab_label(Some("  "), Some("file://box/srv/app/")), "app");
        assert_eq!(tab_label(None, Some("file://box/srv/my%20app")), "my app");
        assert_eq!(tab_label(None, Some("file:///home/ü%C3%A9")), "üé");
        assert_eq!(tab_label(None, Some("file://box/")), "/");
    }

    /// Malformed reports never panic and fall back to a generic name.
    #[test]
    fn malformed_directories_fall_back() {
        assert_eq!(tab_label(None, None), "Terminal");
        assert_eq!(tab_label(None, Some("http://box/srv")), "Terminal");
        assert_eq!(tab_label(None, Some("file://box")), "Terminal");
        assert_eq!(tab_label(None, Some("file://box/a%2")), "a%2");
        assert_eq!(tab_label(None, Some("file://box/%FF")), "\u{FFFD}");
    }
}
