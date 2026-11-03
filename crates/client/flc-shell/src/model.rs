//! Window-free shell model: what the workspace shows and which layout
//! operations it submits.
//!
//! Every intent previews the operation on a clone of the projected layout, so
//! the UI responds immediately, and returns the [`LayoutOp`] the caller submits
//! to the daemon through `flc_sync::Sync::apply`. The daemon stays
//! authoritative: a rejected operation is replaced by the next published
//! layout, and a rejected-op event is shown in the banner.
use std::collections::HashMap;

use fl_model::layout::{Axis, Layout, LayoutOp, SplitNode, SplitTree, Tab, View};
use fl_model::{IncarnationId, PaneId, Size, SplitId, TabId, TerminalId, ViewId};
use flc_sync::WorkspaceState;

/// Basis-point range the daemon accepts for a split ratio.
pub const MIN_RATIO: u16 = 1000;
pub const MAX_RATIO: u16 = 9000;

/// A window-space rectangle in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    /// Remove `amount` from the leading edge along `axis`.
    fn split(&self, axis: Axis, ratio: u16) -> (Rect, Rect, Rect) {
        let amount = f32::from(ratio) / 10_000.0;
        match axis {
            Axis::Horizontal => {
                let first_w = (self.w * amount).round().clamp(0.0, self.w);
                let divider = 1.0_f32.min(self.w - first_w);
                (
                    Rect::new(self.x, self.y, first_w, self.h),
                    Rect::new(self.x + first_w, self.y, divider, self.h),
                    Rect::new(
                        self.x + first_w + divider,
                        self.y,
                        (self.w - first_w - divider).max(0.0),
                        self.h,
                    ),
                )
            }
            Axis::Vertical => {
                let first_h = (self.h * amount).round().clamp(0.0, self.h);
                let divider = 1.0_f32.min(self.h - first_h);
                (
                    Rect::new(self.x, self.y, self.w, first_h),
                    Rect::new(self.x, self.y + first_h, self.w, divider),
                    Rect::new(
                        self.x,
                        self.y + first_h + divider,
                        self.w,
                        (self.h - first_h - divider).max(0.0),
                    ),
                )
            }
        }
    }
}

/// One pane's place in the workspace, in visual order.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PaneRect<T> {
    pub leaf: T,
    pub rect: Rect,
}

/// A split divider, positioned for dragging.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SplitRect {
    pub split: SplitId,
    pub axis: Axis,
    pub rect: Rect,
    /// The area the divider divides; a drag maps the pointer into it.
    pub container: Rect,
}

/// Panes and dividers for one split tree laid out in `area`.
#[derive(Debug, Clone, PartialEq)]
pub struct SplitRects<T> {
    pub panes: Vec<PaneRect<T>>,
    pub dividers: Vec<SplitRect>,
}

/// Lay out a split tree inside `area`, in visual order.
///
/// Iterative, so a hostile tree cannot recurse; leaves outside the flat node
/// table are skipped rather than panicking.
pub fn split_rects<T: Copy + Eq>(tree: &SplitTree<T>, area: Rect) -> SplitRects<T> {
    let mut out = SplitRects {
        panes: Vec::new(),
        dividers: Vec::new(),
    };
    let mut pending: Vec<(u16, Rect)> = vec![(tree.root, area)];
    // The workspace limits nodes to 127; bound the walk regardless.
    let mut budget = 512;
    while let Some((index, rect)) = pending.pop() {
        if budget == 0 {
            break;
        }
        budget -= 1;
        let Some(node) = tree.nodes.get(usize::from(index)) else {
            continue;
        };
        match node {
            SplitNode::Leaf(leaf) => out.panes.push(PaneRect { leaf: *leaf, rect }),
            SplitNode::Branch {
                id,
                axis,
                ratio,
                first,
                second,
            } => {
                let (first_rect, divider, second_rect) = rect.split(*axis, *ratio);
                out.dividers.push(SplitRect {
                    split: *id,
                    axis: *axis,
                    rect: divider,
                    container: rect,
                });
                // Stack order keeps visual order: first is visited first.
                pending.push((*second, second_rect));
                pending.push((*first, first_rect));
            }
        }
    }
    out
}

/// Ratio that puts the divider under `pointer` inside `container`.
pub fn ratio_at(container: Rect, axis: Axis, pointer: f32) -> u16 {
    let (start, length) = match axis {
        Axis::Horizontal => (container.x, container.w),
        Axis::Vertical => (container.y, container.h),
    };
    if length <= 0.0 {
        return MIN_RATIO;
    }
    let fraction = ((pointer - start) / length).clamp(0.0, 1.0);
    let basis_points = (fraction * 10_000.0).round() as u32;
    basis_points.clamp(u32::from(MIN_RATIO), u32::from(MAX_RATIO)) as u16
}

/// Which view, tab and pane receives keyboard input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Focus {
    pub view: ViewId,
    pub tab: Option<TabId>,
    pub pane: Option<PaneId>,
}

/// The shell's view of the workspace.
#[derive(Debug, Default)]
pub struct ShellModel {
    layout: Option<Layout>,
    stale: bool,
    pending: usize,
    focus: Option<Focus>,
    processes: HashMap<TerminalId, IncarnationId>,
    banner: Option<String>,
    /// Cells already requested per terminal, so a resize is not re-sent.
    sizes: HashMap<TerminalId, Size>,
}

impl ShellModel {
    /// Adopt a published workspace snapshot.
    ///
    /// The projected layout wins while operations are outstanding; the
    /// confirmed layout is used once the daemon has caught up.
    pub fn adopt(&mut self, state: &WorkspaceState) {
        self.pending = state.pending.len();
        self.stale = state.stale;
        self.processes = state
            .processes
            .iter()
            .map(|process| (process.terminal, process.incarnation))
            .collect();
        self.layout = state.projected.clone().or_else(|| state.layout.clone());
        self.repair_focus();
    }

    pub fn layout(&self) -> Option<&Layout> {
        self.layout.as_ref()
    }

    pub fn focus(&self) -> Option<Focus> {
        self.focus
    }

    pub fn stale(&self) -> bool {
        self.stale
    }

    /// Unacknowledged operations; the UI marks the workspace busy.
    pub fn pending(&self) -> usize {
        self.pending
    }

    pub fn banner(&self) -> Option<&str> {
        self.banner.as_deref()
    }

    pub fn set_banner(&mut self, message: Option<String>) {
        self.banner = message;
    }

    /// The live child for a terminal, once the daemon has reported it.
    pub fn incarnation(&self, terminal: TerminalId) -> Option<IncarnationId> {
        self.processes.get(&terminal).copied()
    }

    /// Terminal of the focused pane.
    pub fn focused_terminal(&self) -> Option<TerminalId> {
        let focus = self.focus?;
        let tab = self.layout.as_ref()?.tab(focus.view, focus.tab?)?;
        tab.panes
            .iter()
            .find(|pane| Some(pane.id) == focus.pane)
            .map(|pane| pane.terminal)
    }

    pub fn view(&self, id: ViewId) -> Option<&View> {
        self.layout.as_ref()?.view(id)
    }

    /// Tabs in display order, pinned prefix included.
    pub fn tabs(&self, view: ViewId) -> &[Tab] {
        self.view(view).map_or(&[], |view| view.tabs.as_slice())
    }

    /// An empty workspace can only host a first tab.
    pub fn has_workspace(&self) -> bool {
        self.layout.is_some()
    }

    /// Preview `op` locally and hand it back for submission.
    fn submit(&mut self, op: LayoutOp) -> Option<LayoutOp> {
        let layout = self.layout.as_mut()?;
        match layout.apply(op.clone()) {
            Ok(_) => {
                self.pending += 1;
                self.repair_focus();
                Some(op)
            }
            Err(error) => {
                self.banner = Some(format!("That layout change is not possible: {error}"));
                None
            }
        }
    }

    fn active(&self) -> Option<(ViewId, TabId, PaneId)> {
        let layout = self.layout.as_ref()?;
        let view = layout.view(layout.active)?;
        let tab = view.tabs.iter().find(|tab| Some(tab.id) == view.active)?;
        Some((view.id, tab.id, tab.active))
    }

    /// Keep the focus inside the current layout, following the active items.
    fn repair_focus(&mut self) {
        let Some(layout) = self.layout.as_ref() else {
            self.focus = None;
            return;
        };
        let view = layout.view(layout.active);
        let tab = view.and_then(|view| {
            view.tabs
                .iter()
                .find(|tab| Some(tab.id) == view.active)
                .or_else(|| view.tabs.first())
        });
        let focus = Focus {
            view: layout.active,
            tab: tab.map(|tab| tab.id),
            pane: tab.map(|tab| tab.active),
        };
        self.focus = Some(focus);
    }

    // ---- intents -------------------------------------------------------

    pub fn new_tab(
        &mut self,
        terminal: TerminalId,
        size: Size,
        name: Option<String>,
    ) -> Option<LayoutOp> {
        let view = self.layout.as_ref()?.active;
        self.sizes.insert(terminal, size);
        self.submit(LayoutOp::NewTab {
            view,
            tab: TabId::new(),
            pane: PaneId::new(),
            terminal,
            name,
            size,
        })
    }

    pub fn close_tab(&mut self) -> Option<LayoutOp> {
        let (view, tab, _) = self.active()?;
        self.submit(LayoutOp::CloseTab { view, tab })
    }

    pub fn close_pane(&mut self) -> Option<LayoutOp> {
        let (view, tab, pane) = self.active()?;
        self.submit(LayoutOp::ClosePane { view, tab, pane })
    }

    pub fn close_view(&mut self) -> Option<LayoutOp> {
        let (view, _, _) = self.active()?;
        self.submit(LayoutOp::CloseView { view })
    }

    pub fn rename_tab(&mut self, name: Option<String>) -> Option<LayoutOp> {
        let (view, tab, _) = self.active()?;
        self.submit(LayoutOp::RenameTab { view, tab, name })
    }

    pub fn pin_tab(&mut self, pinned: bool) -> Option<LayoutOp> {
        let (view, tab, _) = self.active()?;
        self.submit(LayoutOp::PinTab { view, tab, pinned })
    }

    /// Move the active tab to `index` inside its view (pinned prefix respected).
    pub fn reorder_tab(&mut self, index: u16) -> Option<LayoutOp> {
        let (view, tab, _) = self.active()?;
        self.submit(LayoutOp::MoveTab {
            view,
            tab,
            to: view,
            index,
        })
    }

    pub fn select_tab(&mut self, tab: TabId) -> Option<LayoutOp> {
        let view = self.layout.as_ref()?.active;
        self.submit(LayoutOp::SelectTab { view, tab })
    }

    pub fn select_view(&mut self, view: ViewId) -> Option<LayoutOp> {
        self.submit(LayoutOp::SelectView { view })
    }

    pub fn select_pane(&mut self, pane: PaneId) -> Option<LayoutOp> {
        let (view, tab, _) = self.active()?;
        self.submit(LayoutOp::SelectPane { view, tab, pane })
    }

    pub fn split_pane(&mut self, terminal: TerminalId, axis: Axis, size: Size) -> Option<LayoutOp> {
        let (view, tab, source) = self.active()?;
        self.sizes.insert(terminal, size);
        self.submit(LayoutOp::SplitPane {
            view,
            tab,
            source,
            pane: PaneId::new(),
            terminal,
            split: SplitId::new(),
            axis,
            size,
        })
    }

    /// Split off a new view containing a new terminal.
    pub fn split_view(&mut self, terminal: TerminalId, axis: Axis, size: Size) -> Option<LayoutOp> {
        let source = self.layout.as_ref()?.active;
        self.sizes.insert(terminal, size);
        self.submit(LayoutOp::SplitView {
            source,
            view: ViewId::new(),
            tab: TabId::new(),
            pane: PaneId::new(),
            terminal,
            split: SplitId::new(),
            axis,
            size,
        })
    }

    pub fn set_view_ratio(&mut self, split: SplitId, ratio: u16) -> Option<LayoutOp> {
        let ratio = ratio.clamp(MIN_RATIO, MAX_RATIO);
        self.submit(LayoutOp::SetViewRatio { split, ratio })
    }

    pub fn set_pane_ratio(&mut self, split: SplitId, ratio: u16) -> Option<LayoutOp> {
        let (view, tab, _) = self.active()?;
        let ratio = ratio.clamp(MIN_RATIO, MAX_RATIO);
        self.submit(LayoutOp::SetPaneRatio {
            view,
            tab,
            split,
            ratio,
        })
    }

    /// Size last requested for a terminal, remembered across re-attachment.
    pub fn size(&self, terminal: TerminalId) -> Option<Size> {
        self.sizes.get(&terminal).copied()
    }

    pub fn remember_size(&mut self, terminal: TerminalId, size: Size) {
        self.sizes.insert(terminal, size);
    }
}

#[cfg(test)]
#[path = "model/tests.rs"]
mod tests;
