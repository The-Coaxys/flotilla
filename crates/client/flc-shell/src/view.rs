//! The workspace view: view splits, tab strips and terminal panes.
//!
//! Rendering is driven by the shell model, which mirrors the daemon's
//! projected layout. Terminal panes render `flc-terminal` views; panes whose
//! child is not yet reported by the daemon show a placeholder instead of a
//! blank window.
use std::collections::HashMap;
use std::sync::Arc;

use fl_model::layout::{Axis, Layout, SplitNode, SplitTree};
use fl_model::{IncarnationId, PaneId, Size, SplitId, TabId, TerminalId, ViewId};
use flc_sync::{MirrorStatus, Sync, SyncEvent};
use flc_terminal::{EchoPolicy, TerminalView, TerminalViewOptions};
use flc_ui::input::{TextInput, TextInputEvent};
use flc_ui::theme::{ActiveTheme, Theme};
use gpui::prelude::*;
use gpui::{
    AsyncApp, Context, Div, Entity, FocusHandle, InteractiveElement, IntoElement, KeyDownEvent,
    ParentElement, Render, SharedString, Styled, Subscription, Task, WeakEntity, Window, div, px,
    relative,
};

use crate::model::ShellModel;
use crate::source::MirrorSource;

#[derive(Debug, Clone, Default)]
pub struct ShellViewOptions {
    pub echo_policy: EchoPolicy,
}

/// What the shell asks its host to do outside the layout model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellEvent {
    /// The connection is gone; the shell keeps showing its last frames.
    Disconnected,
    /// The daemon speaks a different terminal engine.
    IncompatibleEngine { daemon: String },
}

pub struct ShellView {
    sync: Arc<Sync>,
    options: ShellViewOptions,
    model: ShellModel,
    sources: HashMap<(TerminalId, IncarnationId), Arc<MirrorSource>>,
    terminals: HashMap<(ViewId, TabId, PaneId), Entity<TerminalView>>,
    focus: FocusHandle,
    rename: Option<(TabId, Entity<TextInput>)>,
    banner: Option<SharedString>,
    _workspace: Task<()>,
    _events: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl ShellView {
    pub fn new(
        sync: Arc<Sync>,
        options: ShellViewOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut model = ShellModel::default();
        model.adopt(&sync.workspace());
        let mut view = Self {
            sync: sync.clone(),
            options,
            model,
            sources: HashMap::new(),
            terminals: HashMap::new(),
            focus: cx.focus_handle(),
            rename: None,
            banner: None,
            _workspace: Task::ready(()),
            _events: Task::ready(()),
            _subscriptions: Vec::new(),
        };
        view.attach_panes(window, cx);

        let mut changes = sync.workspace_changes();
        view._workspace = cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            loop {
                if changes.changed().await.is_err() {
                    break;
                }
                let alive = this.update(cx, |view, cx| {
                    view.model.adopt(&view.sync.workspace());
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
                let Ok(event) = events.recv().await else {
                    continue;
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
                    self.sources.insert(source_key, source.clone());
                    source
                }
            };
            let options = TerminalViewOptions {
                echo_policy: self.options.echo_policy,
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
    }

    fn create_terminal(&mut self, cx: &mut Context<Self>) {
        let terminal = TerminalId::new();
        // The daemon confirms the real grid size once a pane is laid out.
        if let Some(op) = self.model.new_tab(terminal, Size::new(80, 24), None) {
            self.submit(op, cx);
        }
    }

    fn submit(&mut self, op: fl_model::layout::LayoutOp, cx: &mut Context<Self>) {
        let ticket = self.sync.apply(op);
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
        let key = keystroke.key.as_str();
        let (ctrl, shift, alt) = (modifiers.control, modifiers.shift, modifiers.alt);
        match (ctrl, shift, alt, key) {
            (true, true, false, "t") => self.create_terminal(cx),
            (true, true, false, "w") => {
                if let Some(op) = self.model.close_tab() {
                    self.submit(op, cx);
                }
            }
            (true, true, false, "r") => {
                if let Some(focus) = self.model.focus()
                    && let Some(tab) = focus.tab
                    && let Some(item) = self
                        .model
                        .layout()
                        .and_then(|layout| layout.tab(focus.view, tab))
                {
                    let editor = cx.new(|cx| TextInput::new(cx, "Tab name"));
                    editor.update(cx, |input, cx| {
                        if let Some(name) = &item.name {
                            input.set_text(SharedString::from(name.clone()), cx);
                        }
                    });
                    cx.subscribe(&editor, |this, _editor, event: &TextInputEvent, cx| {
                        this.rename.take();
                        // Enter commits even to an empty name: `None` restores
                        // the automatic title. Escape leaves the tab alone.
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
            }
            (false, true, true, "=") | (false, true, true, "+") => {
                let terminal = TerminalId::new();
                if let Some(op) =
                    self.model
                        .split_pane(terminal, Axis::Horizontal, Size::new(80, 24))
                {
                    self.submit(op, cx);
                }
            }
            (false, true, true, "-") => {
                let terminal = TerminalId::new();
                if let Some(op) = self
                    .model
                    .split_pane(terminal, Axis::Vertical, Size::new(80, 24))
                {
                    self.submit(op, cx);
                }
            }
            (true, true, true, "=") | (true, true, true, "+") => {
                let terminal = TerminalId::new();
                if let Some(op) =
                    self.model
                        .split_view(terminal, Axis::Horizontal, Size::new(80, 24))
                {
                    self.submit(op, cx);
                }
            }
            (true, true, true, "-") => {
                let terminal = TerminalId::new();
                if let Some(op) = self
                    .model
                    .split_view(terminal, Axis::Vertical, Size::new(80, 24))
                {
                    self.submit(op, cx);
                }
            }
            (true, false, false, "tab") => self.next_tab(!shift, cx),
            (false, false, true, "left") => self.focus_adjacent_pane(Axis::Horizontal, false, cx),
            (false, false, true, "right") => self.focus_adjacent_pane(Axis::Horizontal, true, cx),
            (false, false, true, "up") => self.focus_adjacent_pane(Axis::Vertical, false, cx),
            (false, false, true, "down") => self.focus_adjacent_pane(Axis::Vertical, true, cx),
            (false, true, true, "left") => self.adjust_ratio(true, false, cx),
            (false, true, true, "right") => self.adjust_ratio(true, true, cx),
            (false, false, false, _) => return false,
            _ => return false,
        }
        cx.notify();
        true
    }

    fn render_tab_strip(
        &self,
        view: &fl_model::layout::View,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> Div {
        let view_id = view.id;
        let mut strip = div()
            .flex()
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
            let label = tab
                .name
                .clone()
                .or_else(|| {
                    tab.panes
                        .first()
                        .and_then(|pane| self.model.incarnation(pane.terminal))
                        .map(|_| "Terminal".to_string())
                })
                .unwrap_or_else(|| "Starting…".to_string());
            let mut item = div()
                .id(SharedString::from(format!("tab-{tab_id}")))
                .flex()
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
                .child(SharedString::from(label));
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
            .child(div().w(px(0.)))
            .child(div().flex_1().min_w(px(0.)))
            .child(
                div()
                    .mr(px(8.))
                    .text_color(t.text_tertiary)
                    .child(SharedString::from(
                        view_id.to_string().chars().take(4).collect::<String>(),
                    )),
            )
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
                    div().flex_row()
                } else {
                    div().flex_col()
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
            .on_click(cx.listener(move |this, _, window, cx| {
                if let Some(op) = this.model.select_pane(pane_id) {
                    this.submit(op, cx);
                }
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
                    div().flex_row()
                } else {
                    div().flex_col()
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
