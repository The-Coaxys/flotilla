//! `fl tab …` and `fl layout …`.
use std::io::Read;

use fl_model::layout::{Axis, Layout, LayoutOp, SplitNode, Tab, View};
use fl_model::{PaneId, Size, SplitId, TabId, TerminalId, ViewId};

use super::{apply_layout, current_layout, resolve_worktree, worktree_scope};
use crate::cli::{Cli, LayoutCommand, SplitDirection, TabCommand};
use crate::client::DaemonClient;
use crate::error::CliError;
use crate::output::Reporter;

fn axis(direction: SplitDirection) -> Axis {
    match direction {
        SplitDirection::Right => Axis::Horizontal,
        SplitDirection::Down => Axis::Vertical,
    }
}

fn size(columns: u16, rows: u16) -> Size {
    Size::new(columns, rows)
}

fn view_of(layout: &Layout, view: Option<ViewId>) -> Result<&View, CliError> {
    let id = view.unwrap_or(layout.active);
    layout
        .views
        .iter()
        .find(|view| view.id == id)
        .ok_or_else(|| CliError::not_found(format!("layout view {id} not found")))
}

fn tab_of(view: &View, tab: Option<TabId>) -> Result<&Tab, CliError> {
    let id = tab.or(view.active).ok_or_else(|| {
        CliError::invalid(format!(
            "view {} has no tabs; use `fl tab split` first",
            view.id
        ))
    })?;
    view.tabs
        .iter()
        .find(|tab| tab.id == id)
        .ok_or_else(|| CliError::not_found(format!("tab {id} not found in view {}", view.id)))
}

fn locate_tab(
    layout: &Layout,
    view: Option<ViewId>,
    tab: Option<TabId>,
) -> Result<(&View, &Tab), CliError> {
    match (view, tab) {
        (Some(view_id), Some(tab_id)) => {
            let view = layout
                .views
                .iter()
                .find(|view| view.id == view_id)
                .ok_or_else(|| CliError::not_found(format!("layout view {view_id} not found")))?;
            let tab = tab_of(view, Some(tab_id))?;
            Ok((view, tab))
        }
        (None, Some(tab_id)) => {
            for view in &layout.views {
                if let Some(tab) = view.tabs.iter().find(|tab| tab.id == tab_id) {
                    return Ok((view, tab));
                }
            }
            Err(CliError::not_found(format!("tab {tab_id} not found")))
        }
        (view, None) => {
            let view = view_of(layout, view)?;
            let tab = tab_of(view, None)?;
            Ok((view, tab))
        }
    }
}

pub async fn run_tab(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    command: &TabCommand,
) -> Result<(), CliError> {
    match command {
        TabCommand::Title { view, tab, title } => {
            title_tab(client, reporter, cli, *view, *tab, title).await
        }
        TabCommand::Split {
            view,
            tab,
            direction,
            columns,
            rows,
        } => {
            split_tab(
                client, reporter, cli, *view, *tab, *direction, *columns, *rows,
            )
            .await
        }
        TabCommand::SplitView {
            view,
            direction,
            columns,
            rows,
        } => split_view(client, reporter, cli, *view, *direction, *columns, *rows).await,
    }
}

async fn title_tab(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    view: Option<ViewId>,
    tab: Option<TabId>,
    title: &str,
) -> Result<(), CliError> {
    let (worktree, _project) = resolve_worktree(client, cli).await?;
    let layout = current_layout(client, worktree_scope(&worktree)).await?;
    let (view, tab) = locate_tab(&layout, view, tab)?;
    let name = if title.trim().is_empty() {
        None
    } else {
        Some(title.to_owned())
    };
    let layout = apply_layout(
        client,
        worktree_scope(&worktree),
        LayoutOp::RenameTab {
            view: view.id,
            tab: tab.id,
            name,
        },
    )
    .await?;
    if reporter.json {
        return reporter.value(&serde_json::json!({
            "worktree": worktree,
            "tab": tab.id,
            "layout": layout,
        }));
    }
    reporter.text(format!("renamed tab {} in view {}", tab.id, view.id));
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn split_tab(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    view: Option<ViewId>,
    tab: Option<TabId>,
    direction: SplitDirection,
    columns: u16,
    rows: u16,
) -> Result<(), CliError> {
    let (worktree, _project) = resolve_worktree(client, cli).await?;
    let layout = current_layout(client, worktree_scope(&worktree)).await?;
    let view = if tab.is_some() {
        locate_tab(&layout, view, tab)?.0
    } else {
        view_of(&layout, view)?
    };
    let terminal = TerminalId::new();
    let op = match (tab, view.active) {
        (None, None) => LayoutOp::NewTab {
            view: view.id,
            tab: TabId::new(),
            pane: PaneId::new(),
            terminal,
            name: None,
            size: size(columns, rows),
        },
        (tab, _) => {
            let tab = tab_of(view, tab)?;
            LayoutOp::SplitPane {
                view: view.id,
                tab: tab.id,
                source: tab.active,
                pane: PaneId::new(),
                terminal,
                split: SplitId::new(),
                axis: axis(direction),
                size: size(columns, rows),
            }
        }
    };
    let layout = apply_layout(client, worktree_scope(&worktree), op).await?;
    if reporter.json {
        return reporter.value(&serde_json::json!({
            "worktree": worktree,
            "terminal": terminal,
            "layout": layout,
        }));
    }
    reporter.text(format!(
        "split tab in view {} with terminal {}",
        view.id, terminal
    ));
    Ok(())
}

async fn split_view(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    view: Option<ViewId>,
    direction: SplitDirection,
    columns: u16,
    rows: u16,
) -> Result<(), CliError> {
    let (worktree, _project) = resolve_worktree(client, cli).await?;
    let layout = current_layout(client, worktree_scope(&worktree)).await?;
    let source = view_of(&layout, view)?.id;
    let terminal = TerminalId::new();
    let op = LayoutOp::SplitView {
        source,
        view: ViewId::new(),
        tab: TabId::new(),
        pane: PaneId::new(),
        terminal,
        split: SplitId::new(),
        axis: axis(direction),
        size: size(columns, rows),
    };
    let layout = apply_layout(client, worktree_scope(&worktree), op).await?;
    if reporter.json {
        return reporter.value(&serde_json::json!({
            "worktree": worktree,
            "terminal": terminal,
            "layout": layout,
        }));
    }
    reporter.text(format!("split view {} with terminal {}", source, terminal));
    Ok(())
}

pub async fn run_layout(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    command: &LayoutCommand,
) -> Result<(), CliError> {
    match command {
        LayoutCommand::Views => views(client, reporter, cli).await,
        LayoutCommand::Set { view, ratio } => set(client, reporter, cli, *view, *ratio).await,
        LayoutCommand::Close { view } => close(client, reporter, cli, *view).await,
        LayoutCommand::Compose { dry_run, file } => {
            compose(client, reporter, cli, *dry_run, file).await
        }
    }
}

async fn views(client: &mut DaemonClient, reporter: &Reporter, cli: &Cli) -> Result<(), CliError> {
    let (worktree, _project) = resolve_worktree(client, cli).await?;
    let layout = current_layout(client, worktree_scope(&worktree)).await?;
    if reporter.json {
        return reporter.value(&serde_json::json!({
            "worktree": worktree,
            "layout": layout,
        }));
    }
    reporter.text(format!(
        "layout rev {} active view {}",
        layout.rev, layout.active
    ));
    for view in &layout.views {
        reporter.text(format!(
            "view {}{}",
            view.id,
            if view.id == layout.active {
                " (active)"
            } else {
                ""
            }
        ));
        for tab in &view.tabs {
            reporter.text(format!(
                "  tab {} {:?}{}{}",
                tab.id,
                tab.name.as_deref().unwrap_or(""),
                if tab.pinned { " (pinned)" } else { "" },
                if Some(tab.id) == view.active {
                    " (active)"
                } else {
                    ""
                }
            ));
            for pane in &tab.panes {
                reporter.text(format!(
                    "    pane {} terminal {}{}",
                    pane.id,
                    pane.terminal,
                    if pane.id == tab.active {
                        " (active)"
                    } else {
                        ""
                    }
                ));
            }
        }
    }
    for node in &layout.split.nodes {
        if let SplitNode::Branch {
            id, axis, ratio, ..
        } = node
        {
            reporter.text(format!("split {id} {:?} ratio {ratio}", axis));
        }
    }
    Ok(())
}

/// The split whose direct child is the leaf holding `view`.
fn split_parent(tree: &fl_model::layout::SplitTree<ViewId>, view: ViewId) -> Option<SplitId> {
    let leaf = |index: u16| match tree.nodes.get(index as usize) {
        Some(SplitNode::Leaf(candidate)) => *candidate == view,
        _ => false,
    };
    tree.nodes.iter().find_map(|node| match node {
        SplitNode::Branch {
            id, first, second, ..
        } if leaf(*first) || leaf(*second) => Some(*id),
        _ => None,
    })
}

async fn set(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    view: ViewId,
    ratio: Option<u16>,
) -> Result<(), CliError> {
    let (worktree, _project) = resolve_worktree(client, cli).await?;
    let layout = current_layout(client, worktree_scope(&worktree)).await?;
    if !layout.views.iter().any(|candidate| candidate.id == view) {
        return Err(CliError::not_found(format!("layout view {view} not found")));
    }
    let op = match ratio {
        Some(ratio) => {
            if !(1000..=9000).contains(&ratio) {
                return Err(CliError::invalid(
                    "split ratio must be between 1000 and 9000 basis points",
                ));
            }
            let split = split_parent(&layout.split, view).ok_or_else(|| {
                CliError::invalid(format!(
                    "view {view} is not part of a split; nothing to ratio"
                ))
            })?;
            LayoutOp::SetViewRatio { split, ratio }
        }
        None => LayoutOp::SelectView { view },
    };
    let layout = apply_layout(client, worktree_scope(&worktree), op).await?;
    if reporter.json {
        return reporter.value(&serde_json::json!({
            "worktree": worktree,
            "layout": layout,
        }));
    }
    match ratio {
        Some(ratio) => reporter.text(format!("set split ratio for view {view} to {ratio}")),
        None => reporter.text(format!("selected view {view}")),
    }
    Ok(())
}

async fn close(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    view: ViewId,
) -> Result<(), CliError> {
    let (worktree, _project) = resolve_worktree(client, cli).await?;
    let layout = current_layout(client, worktree_scope(&worktree)).await?;
    if !layout.views.iter().any(|candidate| candidate.id == view) {
        return Err(CliError::not_found(format!("layout view {view} not found")));
    }
    let layout = apply_layout(
        client,
        worktree_scope(&worktree),
        LayoutOp::CloseView { view },
    )
    .await?;
    if reporter.json {
        return reporter.value(&serde_json::json!({
            "worktree": worktree,
            "layout": layout,
        }));
    }
    reporter.text(format!("closed view {view}"));
    Ok(())
}

async fn compose(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    _dry_run: bool,
    file: &str,
) -> Result<(), CliError> {
    let (worktree, _project) = resolve_worktree(client, cli).await?;
    let layout = current_layout(client, worktree_scope(&worktree)).await?;
    let text = if file == "-" {
        let mut text = String::new();
        std::io::stdin()
            .take(1024 * 1024 + 1)
            .read_to_string(&mut text)
            .map_err(|error| CliError::invalid(format!("reading stdin: {error}")))?;
        if text.len() > 1024 * 1024 {
            return Err(CliError::invalid("layout op file exceeds 1 MiB"));
        }
        text
    } else {
        std::fs::read_to_string(file)
            .map_err(|error| CliError::invalid(format!("reading {file}: {error}")))?
    };
    let value: serde_json::Value = serde_json::from_str(&text)?;
    let ops: Vec<LayoutOp> = if value.is_array() {
        serde_json::from_value(value)?
    } else {
        let ops = value
            .get("ops")
            .cloned()
            .ok_or_else(|| CliError::invalid("expected a JSON array or an object with `ops`"))?;
        serde_json::from_value(ops)?
    };
    if ops.len() > 128 {
        return Err(CliError::invalid(
            "at most 128 layout operations are accepted",
        ));
    }
    let base_rev = layout.rev;
    let mut simulated = layout;
    let mut spawned = Vec::new();
    let mut closed = Vec::new();
    for (index, op) in ops.iter().enumerate() {
        let effect = simulated
            .apply(op.clone())
            .map_err(|error| CliError::invalid(format!("operation {index}: {error}")))?;
        if let Some(spawn) = effect.spawn {
            spawned.push(serde_json::json!({
                "terminal": spawn.terminal,
                "size": spawn.size,
            }));
        }
        closed.extend(effect.close);
    }
    let spawn_count = spawned.len();
    let close_count = closed.len();
    if reporter.json {
        return reporter.value(&serde_json::json!({
            "worktree": worktree,
            "dry_run": true,
            "base_rev": base_rev,
            "final_rev": simulated.rev,
            "ops": ops,
            "spawn": spawned,
            "close": closed,
        }));
    }
    reporter.text(format!(
        "dry run: {} operation(s), rev {} -> {}; {spawn_count} spawn(s), {close_count} close(s)",
        ops.len(),
        base_rev,
        simulated.rev,
    ));
    for (index, op) in ops.iter().enumerate() {
        reporter.text(format!("  {index}: {op:?}"));
    }
    Ok(())
}
