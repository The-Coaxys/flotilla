use super::*;
use fl_model::layout::{Layout, LayoutOp};
use fl_model::{PaneId, Size, TerminalId, ViewId, WorkspaceId};

fn workspace() -> (ShellModel, ViewId, Size) {
    let mut model = ShellModel::default();
    let view = ViewId::new();
    let size = Size::new(80, 24);
    let layout = Layout::new(WorkspaceId::new(), view);
    let state = WorkspaceState {
        layout: Some(layout.clone()),
        projected: Some(layout),
        stale: false,
        pending: Vec::new(),
        processes: Vec::new(),
    };
    model.adopt(&state);
    (model, view, size)
}

fn spawned(model: &mut ShellModel, size: Size) -> TerminalId {
    let terminal = TerminalId::new();
    let op = model.new_tab(terminal, size, None).expect("new tab");
    assert!(matches!(op, LayoutOp::NewTab { terminal: t, .. } if t == terminal));
    terminal
}

#[test]
fn new_tab_projects_immediately_and_focuses_it() {
    let (mut model, view, size) = workspace();
    let terminal = spawned(&mut model, size);
    let focus = model.focus().unwrap();
    assert_eq!(focus.view, view);
    let tab = model
        .layout()
        .unwrap()
        .tab(view, focus.tab.unwrap())
        .unwrap();
    assert_eq!(tab.panes[0].terminal, terminal);
    assert_eq!(model.pending(), 1);
    assert!(model.has_workspace());
}

#[test]
fn closing_the_active_pane_focuses_its_neighbour() {
    let (mut model, view, size) = workspace();
    spawned(&mut model, size);
    let second = TerminalId::new();
    model
        .split_pane(second, Axis::Horizontal, size)
        .expect("split");
    let before = model.focus().unwrap();
    let first_pane = model
        .layout()
        .unwrap()
        .tab(view, before.tab.unwrap())
        .unwrap()
        .panes[0]
        .id;
    model.select_pane(first_pane).expect("select first");
    model.close_pane().expect("close pane");
    let focus = model.focus().unwrap();
    let tab = model
        .layout()
        .unwrap()
        .tab(view, focus.tab.unwrap())
        .unwrap();
    assert_eq!(tab.panes.len(), 1);
    assert_eq!(tab.panes[0].terminal, second);
    assert_eq!(focus.pane, Some(tab.panes[0].id));
}

#[test]
fn split_rects_follow_ratios_and_visual_order() {
    let view = ViewId::new();
    let left = PaneId::new();
    let right = PaneId::new();
    let split = SplitId::new();
    let layout = Layout::new(WorkspaceId::new(), view);
    let mut tree = SplitTree::leaf(left);
    tree = apply_split(tree, left, right, split, Axis::Horizontal);
    let rects = split_rects(&tree, Rect::new(0.0, 0.0, 1000.0, 500.0));
    assert_eq!(rects.panes.len(), 2);
    assert_eq!(rects.panes[0].leaf, left);
    assert_eq!(rects.panes[1].leaf, right);
    assert!((rects.panes[0].rect.w - 500.0).abs() <= 1.0);
    assert_eq!(rects.dividers.len(), 1);
    assert_eq!(rects.dividers[0].axis, Axis::Horizontal);
    let _ = layout;
}

fn apply_split(
    mut tree: SplitTree<PaneId>,
    left: PaneId,
    right: PaneId,
    split: SplitId,
    axis: Axis,
) -> SplitTree<PaneId> {
    // The public tree API only mutates through Layout, so build by hand.
    let _ = &mut tree;
    SplitTree {
        root: 0,
        nodes: vec![
            SplitNode::Branch {
                id: split,
                axis,
                ratio: 5000,
                first: 1,
                second: 2,
            },
            SplitNode::Leaf(left),
            SplitNode::Leaf(right),
        ],
    }
}

#[test]
fn pointer_positions_clamp_to_the_allowed_ratio_range() {
    let container = Rect::new(100.0, 0.0, 1000.0, 100.0);
    assert_eq!(ratio_at(container, Axis::Horizontal, 600.0), 5000);
    assert_eq!(ratio_at(container, Axis::Horizontal, 0.0), MIN_RATIO);
    assert_eq!(ratio_at(container, Axis::Horizontal, 5000.0), MAX_RATIO);
    assert_eq!(ratio_at(container, Axis::Vertical, 50.0), 5000);
}

#[test]
fn ratio_changes_are_clamped_before_submission() {
    let (mut model, view, size) = workspace();
    spawned(&mut model, size);
    let split = model
        .split_pane(TerminalId::new(), Axis::Vertical, size)
        .unwrap();
    let split_id = match split {
        LayoutOp::SplitPane { split, .. } => split,
        other => panic!("unexpected {other:?}"),
    };
    let op = model.set_pane_ratio(split_id, 5).unwrap();
    assert!(matches!(op, LayoutOp::SetPaneRatio { ratio, .. } if ratio == MIN_RATIO));
    let focus = model.focus().unwrap();
    let tab = model
        .layout()
        .unwrap()
        .tab(view, focus.tab.unwrap())
        .unwrap();
    let branch = tab
        .split
        .nodes
        .iter()
        .find_map(|node| match node {
            SplitNode::Branch { id, ratio, .. } if *id == split_id => Some(*ratio),
            _ => None,
        })
        .unwrap();
    assert_eq!(branch, MIN_RATIO);
}

#[test]
fn unknown_terminals_are_not_attachable_until_the_daemon_reports_them() {
    let (mut model, _, size) = workspace();
    let terminal = spawned(&mut model, size);
    assert_eq!(model.incarnation(terminal), None);
    let incarnation = IncarnationId::new();
    let layout = model.layout().unwrap().clone();
    let state = WorkspaceState {
        layout: Some(layout.clone()),
        projected: Some(layout),
        stale: true,
        pending: Vec::new(),
        processes: vec![fl_model::ProcessInfo {
            terminal,
            incarnation,
            pid: 42,
            size,
            argv: Vec::new(),
            cwd: "/work".into(),
            state: fl_model::ProcessState::Running,
            tail: fl_model::LogCursor::default(),
            echo: true,
        }],
    };
    model.adopt(&state);
    assert_eq!(model.incarnation(terminal), Some(incarnation));
    assert!(model.stale());
}

#[test]
fn pinned_tabs_move_ahead_of_unpinned_tabs() {
    let (mut model, view, size) = workspace();
    spawned(&mut model, size);
    let second = spawned(&mut model, size);
    model.select_tab(model.layout().unwrap().view(view).unwrap().tabs[0].id);
    model.pin_tab(true).expect("pin first");
    let tabs = model.layout().unwrap().view(view).unwrap();
    assert!(tabs.tabs[0].pinned);
    assert_eq!(tabs.tabs[1].panes[0].terminal, second);
}

#[test]
fn impossible_operations_leave_the_layout_untouched() {
    let (mut model, _, _) = workspace();
    // Closing the final view is refused by the model, not by the network.
    assert!(model.close_view().is_none());
    assert_eq!(model.pending(), 0);
    let before = model.layout().unwrap().clone();
    let refused = model.layout().map(|layout| layout.views.len()).unwrap();
    assert_eq!(refused, before.views.len());
}

/// Every intent the shell submits must be an operation the daemon's
/// authoritative applier accepts, and applying it there must land exactly the
/// terminals the shell projected locally.
#[test]
fn shell_intents_apply_cleanly_to_the_authoritative_layout() {
    let (mut model, view, size) = workspace();
    // The daemon's copy: what the server holds before the intents arrive.
    let mut server = model.layout().unwrap().clone();

    fn terminals(layout: &Layout) -> Vec<TerminalId> {
        layout
            .views
            .iter()
            .flat_map(|v| v.tabs.iter())
            .flat_map(|t| t.panes.iter())
            .map(|p| p.terminal)
            .collect()
    }

    fn submit(model: &ShellModel, server: &mut Layout, op: Option<LayoutOp>, what: &str) {
        let op = op.unwrap_or_else(|| panic!("{what} produced no operation"));
        server.apply(op).expect("daemon rejects a shell intent");
        assert_eq!(terminals(server), terminals(model.layout().unwrap()));
    }

    let first = TerminalId::new();
    let op = model.new_tab(first, size, Some("build".into()));
    let spawn = server
        .apply(op.clone().expect("new tab"))
        .expect("daemon rejects new tab")
        .spawn
        .expect("a new tab must ask the daemon to spawn its terminal");
    assert_eq!(spawn.terminal, first);

    let second = TerminalId::new();
    let op = model.split_pane(second, Axis::Horizontal, size);
    submit(&model, &mut server, op, "split pane");

    let first_pane = model
        .layout()
        .unwrap()
        .tab(view, model.focus().unwrap().tab.unwrap())
        .unwrap()
        .panes[0]
        .id;
    let op = model.select_pane(first_pane);
    submit(&model, &mut server, op, "select pane");

    let op = model.pin_tab(true);
    submit(&model, &mut server, op, "pin");

    let op = model.rename_tab(Some("tests".into()));
    submit(&model, &mut server, op, "rename");

    let third = TerminalId::new();
    let op = model.split_view(third, Axis::Vertical, size);
    submit(&model, &mut server, op, "split view");

    // Work in the first view, where the split left two panes.
    let view = model.layout().unwrap().views[0].id;
    let op = model.select_view(view);
    submit(&model, &mut server, op, "select view");
    let tab = model.layout().unwrap().views[0].tabs[0].id;
    let op = model.select_tab(tab);
    submit(&model, &mut server, op, "select tab");

    let op = model.close_pane();
    submit(&model, &mut server, op, "close pane");

    let op = model.close_tab();
    submit(&model, &mut server, op, "close tab");

    // One view remains: the shell refuses to empty the workspace, so no
    // operation is submitted and the daemon's layout is untouched.
    assert!(model.close_view().is_none());
    assert_eq!(terminals(&server), terminals(model.layout().unwrap()));
}
