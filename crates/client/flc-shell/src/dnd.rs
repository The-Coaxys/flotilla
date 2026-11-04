//! Window-free drag-and-drop planning for the center tab strip.
//!
//! A drag only ever *plans* one [`LayoutOp`]; nothing in the layout changes
//! until the drop is released, so hovering an insertion point can never
//! reorder tabs by itself. The daemon still validates the operation, and a
//! rejected plan never reaches the wire.
//!
//! The layout protocol moves whole tabs ([`LayoutOp::MoveTab`]), not single
//! panes. Dragging a pane therefore moves the tab that owns it; dropping a
//! pane directly onto another pane would need a pane-move operation this
//! protocol does not have, so it is refused as [`DropRefusal::FinalPane`]
//! instead of silently moving tabs under the pointer.
use fl_model::layout::{Layout, LayoutOp};
use fl_model::{PaneId, TabId, ViewId, WorkspaceId};

/// What the pointer is dragging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DragSource {
    pub workspace: WorkspaceId,
    pub view: ViewId,
    pub tab: TabId,
    /// Set when the drag started on a pane grip; the pane's tab still moves.
    pub pane: Option<PaneId>,
}

/// Where the pointer would drop it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DropTarget {
    pub workspace: WorkspaceId,
    pub view: ViewId,
    /// Insertion slot in the target view's current tab order, `0..=len`.
    pub index: usize,
}

/// Why a drop is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropRefusal {
    /// A tab or pane from another worktree cannot enter this layout.
    CrossWorkspace,
    /// The source view or tab disappeared between the drag and the drop.
    SourceGone,
    /// The target view disappeared between the drag and the drop.
    TargetGone,
    /// The plan would leave the tab exactly where it started.
    SamePosition,
    /// Pinned tabs must stay a prefix; this insertion crosses the boundary.
    PinnedBoundary,
    /// A pane cannot be separated from its tab by this protocol.
    FinalPane,
    /// Tabs belong on the tab strip, not on a pane.
    NotATabSlot,
}

impl DropRefusal {
    pub fn message(self) -> &'static str {
        match self {
            DropRefusal::CrossWorkspace => {
                "That tab belongs to another worktree; drags stay inside one workspace."
            }
            DropRefusal::SourceGone => "The dragged tab no longer exists.",
            DropRefusal::TargetGone => "The destination view no longer exists.",
            DropRefusal::SamePosition => "The tab is already there.",
            DropRefusal::PinnedBoundary => {
                "Pinned tabs form a prefix; move it out of the pinned block first."
            }
            DropRefusal::FinalPane => {
                "A pane cannot be separated from its tab in this layout protocol; drag the tab."
            }
            DropRefusal::NotATabSlot => "Drop tabs on the tab strip, not on a pane.",
        }
    }
}

/// The single operation a drop would submit, or why it cannot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropPlan {
    Apply(LayoutOp),
    Refuse(DropRefusal),
}

/// Plan one drop. Pure: `layout` is only read, never changed.
pub fn plan(layout: &Layout, drag: DragSource, target: DropTarget) -> DropPlan {
    if drag.workspace != target.workspace {
        return DropPlan::Refuse(DropRefusal::CrossWorkspace);
    }
    let Some(source_view) = layout.view(drag.view) else {
        return DropPlan::Refuse(DropRefusal::SourceGone);
    };
    let Some(source_index) = source_view.tabs.iter().position(|tab| tab.id == drag.tab) else {
        return DropPlan::Refuse(DropRefusal::SourceGone);
    };
    let Some(target_view) = layout.view(target.view) else {
        return DropPlan::Refuse(DropRefusal::TargetGone);
    };
    let same_view = drag.view == target.view;
    let len = target_view.tabs.len();
    let slot = target.index.min(len);
    let mut index = if same_view {
        if slot > source_index { slot - 1 } else { slot }
    } else {
        slot
    };
    let max = len - usize::from(same_view);
    index = index.min(max);
    if same_view && index == source_index {
        return DropPlan::Refuse(DropRefusal::SamePosition);
    }

    let item = &source_view.tabs[source_index];
    let prefix = target_view.tabs.iter().take_while(|tab| tab.pinned).count()
        - usize::from(same_view && item.pinned);
    if (item.pinned && index > prefix) || (!item.pinned && index < prefix) {
        return DropPlan::Refuse(DropRefusal::PinnedBoundary);
    }
    DropPlan::Apply(LayoutOp::MoveTab {
        view: drag.view,
        tab: drag.tab,
        to: target.view,
        index: index as u16,
    })
}

/// Plan dropping onto a pane rather than a tab slot. This protocol cannot
/// move a single pane between tabs, so a valid source is refused explicitly
/// instead of moving the wrong thing.
pub fn plan_pane_drop(layout: &Layout, drag: DragSource, target: DropTarget) -> DropPlan {
    if drag.workspace != target.workspace {
        return DropPlan::Refuse(DropRefusal::CrossWorkspace);
    }
    let Some(view) = layout.view(drag.view) else {
        return DropPlan::Refuse(DropRefusal::SourceGone);
    };
    if !view.tabs.iter().any(|tab| tab.id == drag.tab) {
        return DropPlan::Refuse(DropRefusal::SourceGone);
    }
    if layout.view(target.view).is_none() {
        return DropPlan::Refuse(DropRefusal::TargetGone);
    }
    if drag.pane.is_some() {
        DropPlan::Refuse(DropRefusal::FinalPane)
    } else {
        DropPlan::Refuse(DropRefusal::NotATabSlot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_model::layout::{Axis, LayoutOp, Tab};
    use fl_model::{PaneId, TerminalId, ViewId};

    fn tab(id: TabId, pinned: bool, panes: usize) -> Tab {
        let mut tab = Tab {
            id,
            name: None,
            pinned,
            panes: Vec::new(),
            split: fl_model::layout::SplitTree::leaf(PaneId::new()),
            active: PaneId::new(),
        };
        let first = PaneId::new();
        tab.panes.push(fl_model::layout::Pane {
            id: first,
            terminal: TerminalId::new(),
        });
        tab.split = fl_model::layout::SplitTree::leaf(first);
        tab.active = first;
        for _ in 1..panes {
            let pane = PaneId::new();
            tab.panes.push(fl_model::layout::Pane {
                id: pane,
                terminal: TerminalId::new(),
            });
        }
        tab
    }

    fn layout() -> (Layout, Vec<TabId>) {
        let workspace = WorkspaceId::new();
        let view = ViewId::new();
        let tabs = vec![tab(TabId::new(), false, 1), tab(TabId::new(), false, 1)];
        let mut layout = Layout::new(workspace, view);
        layout.views[0].tabs = tabs.clone();
        layout.views[0].active = Some(tabs[0].id);
        (layout, tabs.iter().map(|tab| tab.id).collect())
    }

    fn source(layout: &Layout, tab: TabId) -> DragSource {
        DragSource {
            workspace: layout.workspace,
            view: layout.views[0].id,
            tab,
            pane: None,
        }
    }

    #[test]
    fn reordering_plans_one_move_with_the_final_index() {
        let (layout, tabs) = layout();
        let before = layout.clone();
        let plan = plan(
            &layout,
            source(&layout, tabs[0]),
            DropTarget {
                workspace: layout.workspace,
                view: layout.views[0].id,
                index: 2,
            },
        );
        assert_eq!(
            plan,
            DropPlan::Apply(LayoutOp::MoveTab {
                view: layout.views[0].id,
                tab: tabs[0],
                to: layout.views[0].id,
                index: 1,
            })
        );
        // Hovering/planning never changes the layout: only the drop does.
        assert_eq!(layout, before);
    }

    #[test]
    fn moving_between_views_keeps_the_insertion_slot() {
        let (mut layout, tabs) = layout();
        let second = ViewId::new();
        layout.views.push(fl_model::layout::View {
            id: second,
            tabs: vec![tab(TabId::new(), false, 1)],
            active: None,
        });
        let first_view = layout.active;
        layout.split = fl_model::layout::SplitTree {
            root: 0,
            nodes: vec![
                fl_model::layout::SplitNode::Branch {
                    id: Default::default(),
                    axis: Axis::Horizontal,
                    ratio: fl_model::layout::DEFAULT_RATIO,
                    first: 1,
                    second: 2,
                },
                fl_model::layout::SplitNode::Leaf(first_view),
                fl_model::layout::SplitNode::Leaf(second),
            ],
        };
        let plan = plan(
            &layout,
            source(&layout, tabs[1]),
            DropTarget {
                workspace: layout.workspace,
                view: second,
                index: 0,
            },
        );
        assert_eq!(
            plan,
            DropPlan::Apply(LayoutOp::MoveTab {
                view: layout.views[0].id,
                tab: tabs[1],
                to: second,
                index: 0,
            })
        );
    }

    #[test]
    fn cross_workspace_and_gone_items_are_refused() {
        let (layout, tabs) = layout();
        let mut drag = source(&layout, tabs[0]);
        drag.workspace = WorkspaceId::new();
        let target = DropTarget {
            workspace: layout.workspace,
            view: layout.views[0].id,
            index: 1,
        };
        assert_eq!(
            plan(&layout, drag, target),
            DropPlan::Refuse(DropRefusal::CrossWorkspace)
        );
        let mut drag = source(&layout, tabs[0]);
        drag.tab = TabId::new();
        assert_eq!(
            plan(&layout, drag, target),
            DropPlan::Refuse(DropRefusal::SourceGone)
        );
        let mut target = target;
        target.view = ViewId::new();
        assert_eq!(
            plan(&layout, source(&layout, tabs[0]), target),
            DropPlan::Refuse(DropRefusal::TargetGone)
        );
    }

    #[test]
    fn the_same_slot_is_a_refusal_not_a_write() {
        let (layout, tabs) = layout();
        let drag = source(&layout, tabs[0]);
        for index in [0, 1] {
            let plan = plan(
                &layout,
                drag,
                DropTarget {
                    workspace: layout.workspace,
                    view: layout.views[0].id,
                    index,
                },
            );
            assert_eq!(plan, DropPlan::Refuse(DropRefusal::SamePosition), "{index}");
        }
    }

    #[test]
    fn pinned_tabs_stay_a_prefix() {
        let workspace = WorkspaceId::new();
        let view = ViewId::new();
        let pinned = tab(TabId::new(), true, 1);
        let plain = tab(TabId::new(), false, 1);
        let mut layout = Layout::new(workspace, view);
        layout.views[0].tabs = vec![pinned.clone(), plain.clone()];
        layout.views[0].active = Some(plain.id);
        let drag = DragSource {
            workspace,
            view,
            tab: pinned.id,
            pane: None,
        };
        assert_eq!(
            plan(
                &layout,
                drag,
                DropTarget {
                    workspace,
                    view,
                    index: 2,
                },
            ),
            DropPlan::Refuse(DropRefusal::PinnedBoundary)
        );
        // A plain tab cannot be inserted before the pinned prefix either.
        let drag = DragSource {
            workspace,
            view,
            tab: plain.id,
            pane: None,
        };
        assert_eq!(
            plan(
                &layout,
                drag,
                DropTarget {
                    workspace,
                    view,
                    index: 0,
                },
            ),
            DropPlan::Refuse(DropRefusal::PinnedBoundary)
        );
    }

    #[test]
    fn a_pane_drag_moves_its_tab_and_pane_targets_are_refused() {
        let (layout, tabs) = layout();
        let mut drag = source(&layout, tabs[0]);
        drag.pane = Some(layout.views[0].tabs[0].active);
        // The drag is still representable as a tab move.
        assert!(matches!(
            plan(
                &layout,
                drag,
                DropTarget {
                    workspace: layout.workspace,
                    view: layout.views[0].id,
                    index: 2,
                },
            ),
            DropPlan::Apply(LayoutOp::MoveTab { .. })
        ));
        // Dropping a pane onto another pane would need a pane move; refuse.
        assert_eq!(
            plan_pane_drop(
                &layout,
                drag,
                DropTarget {
                    workspace: layout.workspace,
                    view: layout.views[0].id,
                    index: 0,
                },
            ),
            DropPlan::Refuse(DropRefusal::FinalPane)
        );
        // A tab dropped on a pane is refused with its own explanation.
        drag.pane = None;
        assert_eq!(
            plan_pane_drop(
                &layout,
                drag,
                DropTarget {
                    workspace: layout.workspace,
                    view: layout.views[0].id,
                    index: 0,
                },
            ),
            DropPlan::Refuse(DropRefusal::NotATabSlot)
        );
    }
}
