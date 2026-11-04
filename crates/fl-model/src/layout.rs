//! Canonical workspace layout: a view split tree, ordered tabs in each view,
//! and a pane split tree in each tab. This module never performs terminal I/O.
//!
//! Deserialization bounds every collection before allocating and validates a
//! complete layout before returning it. Public fields support rendering and
//! persistence; `apply` also validates its starting state, so direct edits cannot
//! make a subsequent operation partially commit.

use crate::{InvalidSize, PaneId, Size, SplitId, TabId, TerminalId, ViewId, WorkspaceId};
use serde::de::{self, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use smallvec::SmallVec;
use std::fmt;
use std::marker::PhantomData;

pub const MAX_VIEWS: usize = 16;
pub const MAX_TABS: usize = 64;
pub const MAX_PANES_PER_TAB: usize = 32;
pub const MAX_TERMINALS: usize = 128;
pub const MAX_SPLIT_NODES: usize = 127;
/// Maximum number of edges from the root to a leaf (the root has depth zero).
pub const MAX_SPLIT_DEPTH: usize = 8;
pub const MAX_TAB_NAME_BYTES: usize = 128;
pub const MIN_RATIO: u16 = 1000;
pub const MAX_RATIO: u16 = 9000;
pub const DEFAULT_RATIO: u16 = 5000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Axis {
    /// First child on the left, second child on the right.
    Horizontal,
    /// First child above the second child.
    Vertical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SplitNode<T> {
    Leaf(T),
    Branch {
        id: SplitId,
        axis: Axis,
        /// First child's share, in basis points.
        ratio: u16,
        first: u16,
        second: u16,
    },
}

/// Flat, compact tree; child references are indices into `nodes`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
pub struct SplitTree<T> {
    pub root: u16,
    #[serde(deserialize_with = "deserialize_nodes")]
    pub nodes: Vec<SplitNode<T>>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SplitTreeError {
    #[error("a split tree must have 1..={MAX_SPLIT_NODES} nodes")]
    NodeCount,
    #[error("split node index {0} is outside the tree")]
    NodeIndex(u16),
    #[error("split node {0} is cyclic or has multiple parents")]
    ReusedNode(u16),
    #[error("split node {0} is unreachable from the root")]
    UnreachableNode(u16),
    #[error("split tree exceeds depth {MAX_SPLIT_DEPTH}")]
    Depth,
    #[error("split tree repeats a leaf")]
    DuplicateLeaf,
    #[error("split tree repeats split ID {0}")]
    DuplicateSplit(SplitId),
    #[error("split ratio {0} is outside {MIN_RATIO}..={MAX_RATIO}")]
    Ratio(u16),
    #[error("split leaves do not match the corresponding entities")]
    LeavesMismatch,
}

impl<T: Copy + Eq> SplitTree<T> {
    pub fn leaf(value: T) -> Self {
        Self {
            root: 0,
            nodes: vec![SplitNode::Leaf(value)],
        }
    }

    pub fn root_node(&self) -> Option<&SplitNode<T>> {
        self.node(self.root)
    }

    pub fn node(&self, index: u16) -> Option<&SplitNode<T>> {
        self.nodes.get(usize::from(index))
    }

    pub fn leaf_index(&self, value: T) -> Option<u16> {
        self.nodes
            .iter()
            .position(|node| matches!(node, SplitNode::Leaf(id) if *id == value))
            .and_then(|index| u16::try_from(index).ok())
    }

    pub fn branch_index(&self, id: SplitId) -> Option<u16> {
        self.nodes
            .iter()
            .position(|node| matches!(node, SplitNode::Branch { id: found, .. } if *found == id))
            .and_then(|index| u16::try_from(index).ok())
    }

    /// Leaves in visual order: left before right, top before bottom. Even a
    /// directly edited invalid tree cannot cause this iterator to loop forever.
    pub fn leaves(&self) -> Leaves<'_, T> {
        Leaves {
            tree: self,
            pending: smallvec::smallvec![self.root],
            remaining: MAX_SPLIT_NODES,
        }
    }

    pub fn validate(&self) -> Result<(), SplitTreeError> {
        if self.nodes.is_empty() || self.nodes.len() > MAX_SPLIT_NODES {
            return Err(SplitTreeError::NodeCount);
        }
        let mut visited = [false; MAX_SPLIT_NODES];
        let mut pending: SmallVec<[(u16, usize); 16]> = smallvec::smallvec![(self.root, 0)];
        while let Some((index, depth)) = pending.pop() {
            let node = self.node(index).ok_or(SplitTreeError::NodeIndex(index))?;
            if visited[usize::from(index)] {
                return Err(SplitTreeError::ReusedNode(index));
            }
            if depth > MAX_SPLIT_DEPTH {
                return Err(SplitTreeError::Depth);
            }
            visited[usize::from(index)] = true;
            match node {
                SplitNode::Leaf(value) => {
                    if self.nodes[..usize::from(index)]
                        .iter()
                        .any(|other| matches!(other, SplitNode::Leaf(id) if id == value))
                    {
                        return Err(SplitTreeError::DuplicateLeaf);
                    }
                }
                SplitNode::Branch {
                    id,
                    ratio,
                    first,
                    second,
                    ..
                } => {
                    if !(MIN_RATIO..=MAX_RATIO).contains(ratio) {
                        return Err(SplitTreeError::Ratio(*ratio));
                    }
                    if self.nodes[..usize::from(index)].iter().any(
                        |other| matches!(other, SplitNode::Branch { id: found, .. } if found == id),
                    ) {
                        return Err(SplitTreeError::DuplicateSplit(*id));
                    }
                    pending.push((*second, depth + 1));
                    pending.push((*first, depth + 1));
                }
            }
        }
        if let Some(index) = visited[..self.nodes.len()].iter().position(|seen| !seen) {
            return Err(SplitTreeError::UnreachableNode(index as u16));
        }
        Ok(())
    }

    fn matches(
        &self,
        count: usize,
        mut contains: impl FnMut(T) -> bool,
    ) -> Result<(), SplitTreeError> {
        let mut leaves = 0;
        for node in &self.nodes {
            if let SplitNode::Leaf(value) = node {
                if !contains(*value) {
                    return Err(SplitTreeError::LeavesMismatch);
                }
                leaves += 1;
            }
        }
        if leaves != count {
            return Err(SplitTreeError::LeavesMismatch);
        }
        Ok(())
    }

    fn check_split(&self, source: T) -> Result<(), SplitTreeError> {
        if self.nodes.len() + 2 > MAX_SPLIT_NODES {
            return Err(SplitTreeError::NodeCount);
        }
        let mut pending: SmallVec<[(u16, usize); 16]> = smallvec::smallvec![(self.root, 0)];
        while let Some((index, depth)) = pending.pop() {
            match self.nodes[usize::from(index)] {
                SplitNode::Leaf(value) if value == source => {
                    return if depth < MAX_SPLIT_DEPTH {
                        Ok(())
                    } else {
                        Err(SplitTreeError::Depth)
                    };
                }
                SplitNode::Branch { first, second, .. } => {
                    pending.push((second, depth + 1));
                    pending.push((first, depth + 1));
                }
                SplitNode::Leaf(_) => {}
            }
        }
        Err(SplitTreeError::LeavesMismatch)
    }

    // These mutators only run after the complete operation has been checked.
    fn split_leaf(&mut self, source: T, value: T, id: SplitId, axis: Axis) {
        let index = usize::from(self.leaf_index(source).expect("prevalidated split leaf"));
        let first = self.nodes.len() as u16;
        self.nodes[index] = SplitNode::Branch {
            id,
            axis,
            ratio: DEFAULT_RATIO,
            first,
            second: first + 1,
        };
        self.nodes.push(SplitNode::Leaf(source));
        self.nodes.push(SplitNode::Leaf(value));
    }

    fn neighbor(&self, value: T) -> Option<T> {
        let mut leaves = self.leaves();
        let mut previous = None;
        while let Some(leaf) = leaves.next() {
            if leaf == value {
                return leaves.next().or(previous);
            }
            previous = Some(leaf);
        }
        None
    }

    fn remove_leaf(&mut self, value: T) {
        let leaf = self.leaf_index(value).expect("prevalidated removed leaf");
        let (parent, sibling) = self
            .nodes
            .iter()
            .enumerate()
            .find_map(|(index, node)| match node {
                SplitNode::Branch { first, second, .. } if *first == leaf => Some((index, *second)),
                SplitNode::Branch { first, second, .. } if *second == leaf => Some((index, *first)),
                _ => None,
            })
            .expect("non-final leaf has a parent");
        self.nodes[parent] = self.nodes[usize::from(sibling)];
        // Remove both obsolete nodes while compacting every child reference.
        // The destination never exceeds the source: this works in place.
        let mut remap = [0_u16; MAX_SPLIT_NODES];
        let mut length = 0;
        for (index, destination) in remap.iter_mut().enumerate().take(self.nodes.len()) {
            if index != usize::from(leaf) && index != usize::from(sibling) {
                *destination = length;
                length += 1;
            }
        }
        for index in 0..self.nodes.len() {
            if index == usize::from(leaf) || index == usize::from(sibling) {
                continue;
            }
            let mut node = self.nodes[index];
            if let SplitNode::Branch { first, second, .. } = &mut node {
                *first = remap[usize::from(*first)];
                *second = remap[usize::from(*second)];
            }
            self.nodes[usize::from(remap[index])] = node;
        }
        self.root = remap[usize::from(self.root)];
        self.nodes.truncate(usize::from(length));
    }

    fn set_ratio(&mut self, id: SplitId, value: u16) {
        let index = usize::from(self.branch_index(id).expect("prevalidated split ID"));
        if let SplitNode::Branch { ratio, .. } = &mut self.nodes[index] {
            *ratio = value;
        }
    }
}

pub struct Leaves<'a, T> {
    tree: &'a SplitTree<T>,
    pending: SmallVec<[u16; 16]>,
    remaining: usize,
}

impl<T: Copy> Iterator for Leaves<'_, T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        while self.remaining > 0 {
            let index = self.pending.pop()?;
            self.remaining -= 1;
            match self.tree.nodes.get(usize::from(index))? {
                SplitNode::Leaf(value) => return Some(*value),
                SplitNode::Branch { first, second, .. } => {
                    self.pending.push(*second);
                    self.pending.push(*first);
                }
            }
        }
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pane {
    pub id: PaneId,
    pub terminal: TerminalId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tab {
    pub id: TabId,
    #[serde(deserialize_with = "deserialize_name")]
    pub name: Option<String>,
    pub pinned: bool,
    #[serde(deserialize_with = "deserialize_panes")]
    pub panes: Vec<Pane>,
    pub split: SplitTree<PaneId>,
    pub active: PaneId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct View {
    pub id: ViewId,
    #[serde(deserialize_with = "deserialize_tabs")]
    pub tabs: Vec<Tab>,
    pub active: Option<TabId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Layout {
    pub workspace: WorkspaceId,
    pub rev: u64,
    pub views: Vec<View>,
    pub split: SplitTree<ViewId>,
    pub active: ViewId,
}

impl<'de> Deserialize<'de> for Layout {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct WireLayout {
            workspace: WorkspaceId,
            rev: u64,
            #[serde(deserialize_with = "deserialize_views")]
            views: Vec<View>,
            split: SplitTree<ViewId>,
            active: ViewId,
        }
        let wire = WireLayout::deserialize(deserializer)?;
        let layout = Self {
            workspace: wire.workspace,
            rev: wire.rev,
            views: wire.views,
            split: wire.split,
            active: wire.active,
        };
        layout.validate().map_err(de::Error::custom)?;
        Ok(layout)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LayoutOp {
    NewTab {
        view: ViewId,
        tab: TabId,
        pane: PaneId,
        terminal: TerminalId,
        #[serde(deserialize_with = "deserialize_name")]
        name: Option<String>,
        size: Size,
    },
    CloseTab {
        view: ViewId,
        tab: TabId,
    },
    RenameTab {
        view: ViewId,
        tab: TabId,
        #[serde(deserialize_with = "deserialize_name")]
        name: Option<String>,
    },
    PinTab {
        view: ViewId,
        tab: TabId,
        pinned: bool,
    },
    /// `index` is the final destination index, after removing the source tab.
    /// The moved tab is selected and its destination view becomes active.
    MoveTab {
        view: ViewId,
        tab: TabId,
        to: ViewId,
        index: u16,
    },
    SelectTab {
        view: ViewId,
        tab: TabId,
    },
    SplitView {
        source: ViewId,
        view: ViewId,
        tab: TabId,
        pane: PaneId,
        terminal: TerminalId,
        split: SplitId,
        axis: Axis,
        size: Size,
    },
    CloseView {
        view: ViewId,
    },
    SelectView {
        view: ViewId,
    },
    SplitPane {
        view: ViewId,
        tab: TabId,
        source: PaneId,
        pane: PaneId,
        terminal: TerminalId,
        split: SplitId,
        axis: Axis,
        size: Size,
    },
    ClosePane {
        view: ViewId,
        tab: TabId,
        pane: PaneId,
    },
    SelectPane {
        view: ViewId,
        tab: TabId,
        pane: PaneId,
    },
    SetViewRatio {
        split: SplitId,
        ratio: u16,
    },
    SetPaneRatio {
        view: ViewId,
        tab: TabId,
        split: SplitId,
        ratio: u16,
    },
}

impl LayoutOp {
    /// Checks payload bounds without needing a layout. Entity existence,
    /// capacity, order and revision checks are performed atomically by `apply`.
    pub fn validate(&self) -> Result<(), LayoutError> {
        match self {
            Self::NewTab { name, size, .. } => {
                validate_name(name.as_deref())?;
                size.validate()?;
            }
            Self::RenameTab { name, .. } => validate_name(name.as_deref())?,
            Self::SplitView { size, .. } | Self::SplitPane { size, .. } => {
                size.validate()?;
            }
            Self::SetViewRatio { ratio, .. } | Self::SetPaneRatio { ratio, .. }
                if !(MIN_RATIO..=MAX_RATIO).contains(ratio) =>
            {
                return Err(LayoutError::InvalidRatio(*ratio));
            }
            _ => {}
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnTerminal {
    pub terminal: TerminalId,
    pub size: Size,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LayoutEffect {
    pub spawn: Option<SpawnTerminal>,
    /// Exactly the removed terminals, in tab order and visual pane order.
    pub close: SmallVec<[TerminalId; 4]>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LayoutError {
    #[error("layout must contain at least one view")]
    EmptyViews,
    #[error("layout exceeds {MAX_VIEWS} views")]
    TooManyViews,
    #[error("layout exceeds {MAX_TABS} tabs")]
    TooManyTabs,
    #[error("tab exceeds {MAX_PANES_PER_TAB} panes")]
    TooManyPanes,
    #[error("layout exceeds {MAX_TERMINALS} terminals")]
    TooManyTerminals,
    #[error("view {0} does not exist")]
    ViewNotFound(ViewId),
    #[error("tab {tab} does not exist in view {view}")]
    TabNotFound { view: ViewId, tab: TabId },
    #[error("pane {pane} does not exist in tab {tab}")]
    PaneNotFound { tab: TabId, pane: PaneId },
    #[error("split {0} does not exist in the requested tree")]
    SplitNotFound(SplitId),
    #[error("view ID {0} already exists")]
    DuplicateView(ViewId),
    #[error("tab ID {0} already exists")]
    DuplicateTab(TabId),
    #[error("pane ID {0} already exists")]
    DuplicatePane(PaneId),
    #[error("terminal ID {0} already exists")]
    DuplicateTerminal(TerminalId),
    #[error("split ID {0} already exists")]
    DuplicateSplit(SplitId),
    #[error("active view {0} does not exist")]
    ActiveView(ViewId),
    #[error("view {0} must select one of its tabs, or None only when empty")]
    ActiveTab(ViewId),
    #[error("tab {0} must select one of its panes")]
    ActivePane(TabId),
    #[error("tab {0} has no panes")]
    EmptyTab(TabId),
    #[error("pinned tabs must form a prefix")]
    PinnedOrder,
    #[error("tab index {index} exceeds final insertion bound {max}")]
    TabIndex { index: u16, max: usize },
    #[error("the final view cannot be closed")]
    FinalView,
    #[error(
        "tab name must be nonblank, contain no control characters and fit in {MAX_TAB_NAME_BYTES} UTF-8 bytes"
    )]
    InvalidName,
    #[error(transparent)]
    InvalidSize(#[from] InvalidSize),
    #[error("split ratio {0} is outside {MIN_RATIO}..={MAX_RATIO}")]
    InvalidRatio(u16),
    #[error(transparent)]
    InvalidTree(#[from] SplitTreeError),
    #[error("layout revision has reached u64::MAX")]
    RevisionOverflow,
}

impl Layout {
    pub fn new(workspace: WorkspaceId, view: ViewId) -> Self {
        Self {
            workspace,
            rev: 0,
            views: vec![View {
                id: view,
                tabs: Vec::new(),
                active: None,
            }],
            split: SplitTree::leaf(view),
            active: view,
        }
    }

    pub fn view(&self, id: ViewId) -> Option<&View> {
        self.views.iter().find(|view| view.id == id)
    }

    pub fn tab(&self, view: ViewId, tab: TabId) -> Option<&Tab> {
        self.view(view)?.tabs.iter().find(|item| item.id == tab)
    }

    pub fn pane(&self, view: ViewId, tab: TabId, pane: PaneId) -> Option<&Pane> {
        self.tab(view, tab)?
            .panes
            .iter()
            .find(|item| item.id == pane)
    }

    pub fn validate(&self) -> Result<(), LayoutError> {
        if self.views.is_empty() {
            return Err(LayoutError::EmptyViews);
        }
        if self.views.len() > MAX_VIEWS {
            return Err(LayoutError::TooManyViews);
        }
        self.split.validate()?;
        self.split.matches(self.views.len(), |id| {
            self.views.iter().any(|view| view.id == id)
        })?;
        if self.view(self.active).is_none() {
            return Err(LayoutError::ActiveView(self.active));
        }
        // The fixed inline capacities cover every valid layout. Validation does
        // not allocate a hash table or make a copy of the tree.
        let mut views: SmallVec<[ViewId; 16]> = SmallVec::new();
        let mut tabs: SmallVec<[TabId; 64]> = SmallVec::new();
        let mut panes: SmallVec<[PaneId; 128]> = SmallVec::new();
        let mut terminals: SmallVec<[TerminalId; 128]> = SmallVec::new();
        let mut splits: SmallVec<[SplitId; 256]> = SmallVec::new();
        collect_splits(&self.split, &mut splits)?;
        for view in &self.views {
            if views.contains(&view.id) {
                return Err(LayoutError::DuplicateView(view.id));
            }
            views.push(view.id);
            if view.tabs.len() > MAX_TABS - tabs.len() {
                return Err(LayoutError::TooManyTabs);
            }
            if view.tabs.is_empty() {
                if view.active.is_some() {
                    return Err(LayoutError::ActiveTab(view.id));
                }
            } else if !view.tabs.iter().any(|tab| Some(tab.id) == view.active) {
                return Err(LayoutError::ActiveTab(view.id));
            }
            let mut unpinned = false;
            for tab in &view.tabs {
                if tabs.contains(&tab.id) {
                    return Err(LayoutError::DuplicateTab(tab.id));
                }
                tabs.push(tab.id);
                if tab.pinned && unpinned {
                    return Err(LayoutError::PinnedOrder);
                }
                unpinned |= !tab.pinned;
                validate_name(tab.name.as_deref())?;
                if tab.panes.is_empty() {
                    return Err(LayoutError::EmptyTab(tab.id));
                }
                if tab.panes.len() > MAX_PANES_PER_TAB {
                    return Err(LayoutError::TooManyPanes);
                }
                if tab.panes.len() > MAX_TERMINALS - panes.len() {
                    return Err(LayoutError::TooManyTerminals);
                }
                tab.split.validate()?;
                tab.split.matches(tab.panes.len(), |id| {
                    tab.panes.iter().any(|pane| pane.id == id)
                })?;
                collect_splits(&tab.split, &mut splits)?;
                if !tab.panes.iter().any(|pane| pane.id == tab.active) {
                    return Err(LayoutError::ActivePane(tab.id));
                }
                for pane in &tab.panes {
                    if panes.contains(&pane.id) {
                        return Err(LayoutError::DuplicatePane(pane.id));
                    }
                    if terminals.contains(&pane.terminal) {
                        return Err(LayoutError::DuplicateTerminal(pane.terminal));
                    }
                    panes.push(pane.id);
                    terminals.push(pane.terminal);
                }
            }
        }
        Ok(())
    }

    /// All fallible checks precede the first mutation. A successful operation,
    /// including an already-selected item, advances the revision exactly once.
    /// Closing an active item selects its next neighbor, or the previous one at
    /// the end. View/pane neighbors follow visual tree order; tabs follow their
    /// list order. Explicit closes also close pinned tabs.
    pub fn apply(&mut self, op: LayoutOp) -> Result<LayoutEffect, LayoutError> {
        self.validate()?;
        op.validate()?;
        let revision = self
            .rev
            .checked_add(1)
            .ok_or(LayoutError::RevisionOverflow)?;
        let mut effect = LayoutEffect::default();
        match op {
            LayoutOp::NewTab {
                view,
                tab,
                pane,
                terminal,
                name,
                size,
            } => {
                let v = self.view_index(view)?;
                self.check_new_tab(tab)?;
                self.check_new_pane(pane, terminal)?;
                self.views[v].tabs.push(new_tab(tab, pane, terminal, name));
                self.views[v].active = Some(tab);
                self.active = view;
                effect.spawn = Some(SpawnTerminal { terminal, size });
            }
            LayoutOp::CloseTab { view, tab } => {
                let (v, t) = self.tab_index(view, tab)?;
                collect_terminals(&self.views[v].tabs[t], &mut effect.close);
                remove_tab(&mut self.views[v], t);
            }
            LayoutOp::RenameTab { view, tab, name } => {
                let (v, t) = self.tab_index(view, tab)?;
                self.views[v].tabs[t].name = name;
            }
            LayoutOp::PinTab { view, tab, pinned } => {
                let (v, t) = self.tab_index(view, tab)?;
                if self.views[v].tabs[t].pinned != pinned {
                    let mut item = self.views[v].tabs.remove(t);
                    item.pinned = pinned;
                    let index = pinned_count(&self.views[v]);
                    self.views[v].tabs.insert(index, item);
                }
            }
            LayoutOp::MoveTab {
                view,
                tab,
                to,
                index,
            } => {
                let (v, t) = self.tab_index(view, tab)?;
                let destination = self.view_index(to)?;
                let item = &self.views[v].tabs[t];
                let max = self.views[destination].tabs.len() - usize::from(v == destination);
                if usize::from(index) > max {
                    return Err(LayoutError::TabIndex { index, max });
                }
                let prefix = pinned_count(&self.views[destination])
                    - usize::from(v == destination && item.pinned);
                if (item.pinned && usize::from(index) > prefix)
                    || (!item.pinned && usize::from(index) < prefix)
                {
                    return Err(LayoutError::PinnedOrder);
                }
                let item = remove_tab(&mut self.views[v], t);
                self.views[destination]
                    .tabs
                    .insert(usize::from(index), item);
                self.views[destination].active = Some(tab);
                self.active = to;
            }
            LayoutOp::SelectTab { view, tab } => {
                let (v, _) = self.tab_index(view, tab)?;
                self.views[v].active = Some(tab);
                self.active = view;
            }
            LayoutOp::SplitView {
                source,
                view,
                tab,
                pane,
                terminal,
                split,
                axis,
                size,
            } => {
                self.view_index(source)?;
                if self.view(view).is_some() {
                    return Err(LayoutError::DuplicateView(view));
                }
                if self.views.len() == MAX_VIEWS {
                    return Err(LayoutError::TooManyViews);
                }
                self.check_new_tab(tab)?;
                self.check_new_pane(pane, terminal)?;
                self.check_new_split(split)?;
                self.split.check_split(source)?;
                self.split.split_leaf(source, view, split, axis);
                self.views.push(View {
                    id: view,
                    tabs: vec![new_tab(tab, pane, terminal, None)],
                    active: Some(tab),
                });
                self.active = view;
                effect.spawn = Some(SpawnTerminal { terminal, size });
            }
            LayoutOp::CloseView { view } => {
                let v = self.view_index(view)?;
                if self.views.len() == 1 {
                    return Err(LayoutError::FinalView);
                }
                let neighbor = self
                    .split
                    .neighbor(view)
                    .expect("non-final view has neighbor");
                for tab in &self.views[v].tabs {
                    collect_terminals(tab, &mut effect.close);
                }
                self.split.remove_leaf(view);
                self.views.remove(v);
                if self.active == view {
                    self.active = neighbor;
                }
            }
            LayoutOp::SelectView { view } => {
                self.view_index(view)?;
                self.active = view;
            }
            LayoutOp::SplitPane {
                view,
                tab,
                source,
                pane,
                terminal,
                split,
                axis,
                size,
            } => {
                let (v, t, _) = self.pane_index(view, tab, source)?;
                self.check_new_pane(pane, terminal)?;
                self.check_new_split(split)?;
                let item = &self.views[v].tabs[t];
                if item.panes.len() == MAX_PANES_PER_TAB {
                    return Err(LayoutError::TooManyPanes);
                }
                item.split.check_split(source)?;
                let item = &mut self.views[v].tabs[t];
                item.split.split_leaf(source, pane, split, axis);
                item.panes.push(Pane { id: pane, terminal });
                item.active = pane;
                self.views[v].active = Some(tab);
                self.active = view;
                effect.spawn = Some(SpawnTerminal { terminal, size });
            }
            LayoutOp::ClosePane { view, tab, pane } => {
                let (v, t, p) = self.pane_index(view, tab, pane)?;
                effect.close.push(self.views[v].tabs[t].panes[p].terminal);
                if self.views[v].tabs[t].panes.len() == 1 {
                    remove_tab(&mut self.views[v], t);
                } else {
                    let item = &mut self.views[v].tabs[t];
                    let neighbor = item
                        .split
                        .neighbor(pane)
                        .expect("non-final pane has neighbor");
                    item.split.remove_leaf(pane);
                    item.panes.remove(p);
                    if item.active == pane {
                        item.active = neighbor;
                    }
                }
            }
            LayoutOp::SelectPane { view, tab, pane } => {
                let (v, t, _) = self.pane_index(view, tab, pane)?;
                self.views[v].tabs[t].active = pane;
                self.views[v].active = Some(tab);
                self.active = view;
            }
            LayoutOp::SetViewRatio { split, ratio } => {
                if self.split.branch_index(split).is_none() {
                    return Err(LayoutError::SplitNotFound(split));
                }
                self.split.set_ratio(split, ratio);
            }
            LayoutOp::SetPaneRatio {
                view,
                tab,
                split,
                ratio,
            } => {
                let (v, t) = self.tab_index(view, tab)?;
                if self.views[v].tabs[t].split.branch_index(split).is_none() {
                    return Err(LayoutError::SplitNotFound(split));
                }
                self.views[v].tabs[t].split.set_ratio(split, ratio);
            }
        }
        self.rev = revision;
        Ok(effect)
    }

    fn view_index(&self, view: ViewId) -> Result<usize, LayoutError> {
        self.views
            .iter()
            .position(|item| item.id == view)
            .ok_or(LayoutError::ViewNotFound(view))
    }

    fn tab_index(&self, view: ViewId, tab: TabId) -> Result<(usize, usize), LayoutError> {
        let v = self.view_index(view)?;
        let t = self.views[v]
            .tabs
            .iter()
            .position(|item| item.id == tab)
            .ok_or(LayoutError::TabNotFound { view, tab })?;
        Ok((v, t))
    }

    fn pane_index(
        &self,
        view: ViewId,
        tab: TabId,
        pane: PaneId,
    ) -> Result<(usize, usize, usize), LayoutError> {
        let (v, t) = self.tab_index(view, tab)?;
        let p = self.views[v].tabs[t]
            .panes
            .iter()
            .position(|item| item.id == pane)
            .ok_or(LayoutError::PaneNotFound { tab, pane })?;
        Ok((v, t, p))
    }

    fn check_new_tab(&self, id: TabId) -> Result<(), LayoutError> {
        let mut count = 0;
        for view in &self.views {
            for tab in &view.tabs {
                if tab.id == id {
                    return Err(LayoutError::DuplicateTab(id));
                }
                count += 1;
            }
        }
        if count == MAX_TABS {
            return Err(LayoutError::TooManyTabs);
        }
        Ok(())
    }

    fn check_new_pane(&self, id: PaneId, terminal: TerminalId) -> Result<(), LayoutError> {
        let mut count = 0;
        for view in &self.views {
            for tab in &view.tabs {
                for pane in &tab.panes {
                    if pane.id == id {
                        return Err(LayoutError::DuplicatePane(id));
                    }
                    if pane.terminal == terminal {
                        return Err(LayoutError::DuplicateTerminal(terminal));
                    }
                    count += 1;
                }
            }
        }
        if count == MAX_TERMINALS {
            return Err(LayoutError::TooManyTerminals);
        }
        Ok(())
    }

    fn check_new_split(&self, id: SplitId) -> Result<(), LayoutError> {
        if self.split.branch_index(id).is_some()
            || self
                .views
                .iter()
                .flat_map(|view| &view.tabs)
                .any(|tab| tab.split.branch_index(id).is_some())
        {
            return Err(LayoutError::DuplicateSplit(id));
        }
        Ok(())
    }
}

fn new_tab(id: TabId, pane: PaneId, terminal: TerminalId, name: Option<String>) -> Tab {
    Tab {
        id,
        name,
        pinned: false,
        panes: vec![Pane { id: pane, terminal }],
        split: SplitTree::leaf(pane),
        active: pane,
    }
}

fn pinned_count(view: &View) -> usize {
    view.tabs.iter().take_while(|tab| tab.pinned).count()
}

fn remove_tab(view: &mut View, index: usize) -> Tab {
    let tab = view.tabs.remove(index);
    if view.active == Some(tab.id) {
        view.active = view
            .tabs
            .get(index)
            .or_else(|| view.tabs.last())
            .map(|item| item.id);
    }
    tab
}

fn collect_terminals(tab: &Tab, terminals: &mut SmallVec<[TerminalId; 4]>) {
    for id in tab.split.leaves() {
        terminals.push(
            tab.panes
                .iter()
                .find(|pane| pane.id == id)
                .expect("validated pane leaf")
                .terminal,
        );
    }
}

fn collect_splits<T>(
    tree: &SplitTree<T>,
    splits: &mut SmallVec<[SplitId; 256]>,
) -> Result<(), LayoutError> {
    for node in &tree.nodes {
        if let SplitNode::Branch { id, .. } = node {
            if splits.contains(id) {
                return Err(LayoutError::DuplicateSplit(*id));
            }
            splits.push(*id);
        }
    }
    Ok(())
}

fn validate_name(name: Option<&str>) -> Result<(), LayoutError> {
    if let Some(name) = name
        && (name.len() > MAX_TAB_NAME_BYTES
            || name.trim().is_empty()
            || name.chars().any(char::is_control))
    {
        return Err(LayoutError::InvalidName);
    }
    Ok(())
}

fn deserialize_bounded_vec<'de, D, T, const MAX: usize>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Bounded<T, const MAX: usize>(PhantomData<T>);
    impl<'de, T: Deserialize<'de>, const MAX: usize> Visitor<'de> for Bounded<T, MAX> {
        type Value = Vec<T>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "at most {MAX} items")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<T>, A::Error> {
            if seq.size_hint().is_some_and(|size| size > MAX) {
                return Err(de::Error::custom(format_args!(
                    "collection exceeds {MAX} items"
                )));
            }
            let mut values = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(MAX));
            while let Some(value) = seq.next_element()? {
                if values.len() == MAX {
                    return Err(de::Error::custom(format_args!(
                        "collection exceeds {MAX} items"
                    )));
                }
                values.push(value);
            }
            Ok(values)
        }
    }
    deserializer.deserialize_seq(Bounded::<T, MAX>(PhantomData))
}

fn deserialize_nodes<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Vec<SplitNode<T>>, D::Error> {
    deserialize_bounded_vec::<D, SplitNode<T>, MAX_SPLIT_NODES>(d)
}
fn deserialize_views<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<View>, D::Error> {
    deserialize_bounded_vec::<D, View, MAX_VIEWS>(d)
}
fn deserialize_tabs<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Tab>, D::Error> {
    deserialize_bounded_vec::<D, Tab, MAX_TABS>(d)
}
fn deserialize_panes<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Pane>, D::Error> {
    deserialize_bounded_vec::<D, Pane, MAX_PANES_PER_TAB>(d)
}

fn deserialize_name<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    struct OptionalName;
    struct Name;
    impl<'de> Visitor<'de> for Name {
        type Value = String;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a bounded, nonblank tab name")
        }
        fn visit_str<E: de::Error>(self, value: &str) -> Result<String, E> {
            validate_name(Some(value)).map_err(de::Error::custom)?;
            Ok(value.to_owned())
        }
        fn visit_string<E: de::Error>(self, value: String) -> Result<String, E> {
            validate_name(Some(&value)).map_err(de::Error::custom)?;
            Ok(value)
        }
    }
    impl<'de> Visitor<'de> for OptionalName {
        type Value = Option<String>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("an optional bounded tab name")
        }
        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
            d.deserialize_str(Name).map(Some)
        }
    }
    d.deserialize_option(OptionalName)
}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! ids {
        ($($function:ident: $ty:ty),+ $(,)?) => {$(
            fn $function(number: u16) -> $ty {
                let mut bytes = [0; 16];
                bytes[..2].copy_from_slice(&number.to_le_bytes());
                <$ty>::from_bytes(bytes)
            }
        )+};
    }
    ids!(workspace: WorkspaceId, view: ViewId, tab: TabId, pane: PaneId, terminal: TerminalId, split: SplitId);

    fn add_tab(view_number: u16, number: u16) -> LayoutOp {
        LayoutOp::NewTab {
            view: view(view_number),
            tab: tab(number),
            pane: pane(number),
            terminal: terminal(number),
            name: None,
            size: Size::new(80, 24),
        }
    }

    fn split_pane(source: u16, number: u16, split_number: u16) -> LayoutOp {
        LayoutOp::SplitPane {
            view: view(1),
            tab: tab(1),
            source: pane(source),
            pane: pane(number),
            terminal: terminal(number),
            split: split(split_number),
            axis: Axis::Horizontal,
            size: Size::new(90, 30),
        }
    }

    fn split_view(number: u16, split_number: u16) -> LayoutOp {
        LayoutOp::SplitView {
            source: view(1),
            view: view(number),
            tab: tab(number),
            pane: pane(number),
            terminal: terminal(number),
            split: split(split_number),
            axis: Axis::Vertical,
            size: Size::new(100, 40),
        }
    }

    fn populated() -> Layout {
        let mut layout = Layout::new(workspace(1), view(1));
        layout.apply(add_tab(1, 1)).unwrap();
        layout
    }

    fn rejected(layout: &mut Layout, op: LayoutOp, expected: LayoutError) {
        let before = layout.clone();
        assert_eq!(layout.apply(op), Err(expected));
        assert_eq!(*layout, before, "rejection changed canonical state");
    }

    fn assert_valid(layout: &Layout) {
        layout.validate().unwrap();
        let bytes = postcard::to_allocvec(layout).unwrap();
        assert_eq!(postcard::from_bytes::<Layout>(&bytes).unwrap(), *layout);
    }

    #[test]
    fn view_splits_contain_independent_ordered_tabs_and_pane_trees() {
        let mut layout = populated();
        layout.apply(split_pane(1, 3, 1)).unwrap();
        layout.apply(add_tab(1, 4)).unwrap();
        let original_tabs = layout.views[0].tabs.clone();
        let effect = layout.apply(split_view(2, 2)).unwrap();
        assert_eq!(
            effect.spawn,
            Some(SpawnTerminal {
                terminal: terminal(2),
                size: Size::new(100, 40)
            })
        );
        assert!(effect.close.is_empty());
        assert_eq!(
            layout.split.leaves().collect::<Vec<_>>(),
            [view(1), view(2)]
        );
        assert_eq!(layout.views[0].tabs, original_tabs);
        assert_eq!(layout.active, view(2));
        assert_eq!(layout.views[1].active, Some(tab(2)));
        assert_eq!(layout.views[1].tabs[0].active, pane(2));

        layout
            .apply(LayoutOp::SelectPane {
                view: view(1),
                tab: tab(1),
                pane: pane(1),
            })
            .unwrap();
        assert_eq!(layout.active, view(1));
        assert_eq!(layout.views[0].active, Some(tab(1)));
        assert_eq!(layout.views[0].tabs[0].active, pane(1));
        layout
            .apply(LayoutOp::SetViewRatio {
                split: split(2),
                ratio: 3500,
            })
            .unwrap();
        layout
            .apply(LayoutOp::SetPaneRatio {
                view: view(1),
                tab: tab(1),
                split: split(1),
                ratio: 6500,
            })
            .unwrap();
        assert!(matches!(
            layout.split.root_node(),
            Some(SplitNode::Branch {
                ratio: 3500,
                axis: Axis::Vertical,
                ..
            })
        ));
        assert!(matches!(
            layout.views[0].tabs[0].split.root_node(),
            Some(SplitNode::Branch {
                ratio: 6500,
                axis: Axis::Horizontal,
                ..
            })
        ));
        assert_valid(&layout);
    }

    #[test]
    fn collapsing_a_pane_promotes_a_branch_and_compacts_all_indices() {
        let mut layout = populated();
        layout.apply(split_pane(1, 2, 1)).unwrap();
        layout.apply(split_pane(1, 3, 2)).unwrap();
        assert_eq!(
            layout.views[0].tabs[0].split.leaves().collect::<Vec<_>>(),
            [pane(1), pane(3), pane(2)]
        );
        let effect = layout
            .apply(LayoutOp::ClosePane {
                view: view(1),
                tab: tab(1),
                pane: pane(2),
            })
            .unwrap();
        assert_eq!(effect.close.as_slice(), [terminal(2)]);
        assert_eq!(effect.spawn, None);
        let item = &layout.views[0].tabs[0];
        assert_eq!(item.split.nodes.len(), 3);
        assert_eq!(item.split.leaves().collect::<Vec<_>>(), [pane(1), pane(3)]);
        assert_eq!(item.active, pane(3));
        assert_eq!(item.split.branch_index(split(1)), None);
        assert!(item.split.branch_index(split(2)).is_some());
        assert_valid(&layout);

        layout
            .apply(LayoutOp::ClosePane {
                view: view(1),
                tab: tab(1),
                pane: pane(3),
            })
            .unwrap();
        assert_eq!(layout.views[0].tabs[0].split, SplitTree::leaf(pane(1)));
        assert_eq!(layout.views[0].tabs[0].active, pane(1));
        let effect = layout
            .apply(LayoutOp::ClosePane {
                view: view(1),
                tab: tab(1),
                pane: pane(1),
            })
            .unwrap();
        assert_eq!(effect.close.as_slice(), [terminal(1)]);
        assert!(layout.views[0].tabs.is_empty());
        assert_eq!(layout.views[0].active, None);
        assert_eq!(layout.active, view(1));
        assert_eq!(layout.rev, 6);
        assert_valid(&layout);
    }

    #[test]
    fn active_neighbors_follow_visual_order_not_entity_storage_order() {
        let mut layout = populated();
        layout.apply(split_pane(1, 2, 1)).unwrap();
        layout.apply(split_pane(1, 3, 2)).unwrap();
        layout
            .apply(LayoutOp::SelectPane {
                view: view(1),
                tab: tab(1),
                pane: pane(1),
            })
            .unwrap();
        layout
            .apply(LayoutOp::ClosePane {
                view: view(1),
                tab: tab(1),
                pane: pane(1),
            })
            .unwrap();
        assert_eq!(layout.views[0].tabs[0].active, pane(3));
        assert_eq!(
            layout.views[0].tabs[0].split.leaves().collect::<Vec<_>>(),
            [pane(3), pane(2)]
        );
        layout
            .apply(LayoutOp::ClosePane {
                view: view(1),
                tab: tab(1),
                pane: pane(3),
            })
            .unwrap();
        assert_eq!(layout.views[0].tabs[0].active, pane(2));

        layout.apply(split_view(4, 3)).unwrap();
        layout.apply(split_view(5, 4)).unwrap();
        assert_eq!(
            layout.split.leaves().collect::<Vec<_>>(),
            [view(1), view(5), view(4)]
        );
        layout
            .apply(LayoutOp::SelectView { view: view(1) })
            .unwrap();
        layout.apply(LayoutOp::CloseView { view: view(1) }).unwrap();
        assert_eq!(layout.active, view(5));
        layout.apply(LayoutOp::CloseView { view: view(4) }).unwrap();
        assert_eq!(layout.active, view(5));
        assert_eq!(layout.split, SplitTree::leaf(view(5)));
        assert_valid(&layout);
    }

    #[test]
    fn closing_a_view_emits_each_terminal_once_in_tab_and_visual_pane_order() {
        let mut layout = populated();
        layout.apply(split_pane(1, 2, 1)).unwrap();
        layout.apply(split_pane(1, 3, 2)).unwrap();
        layout.apply(add_tab(1, 4)).unwrap();
        layout
            .apply(LayoutOp::PinTab {
                view: view(1),
                tab: tab(4),
                pinned: true,
            })
            .unwrap();
        layout.apply(split_view(5, 3)).unwrap();
        layout
            .apply(LayoutOp::SelectView { view: view(1) })
            .unwrap();
        let effect = layout.apply(LayoutOp::CloseView { view: view(1) }).unwrap();
        assert_eq!(
            effect.close.as_slice(),
            [terminal(4), terminal(1), terminal(3), terminal(2)]
        );
        assert_eq!(effect.spawn, None);
        assert_eq!(layout.active, view(5));
        assert_eq!(layout.views.len(), 1);
        assert_eq!(layout.split, SplitTree::leaf(view(5)));
        assert_valid(&layout);
        rejected(
            &mut layout,
            LayoutOp::CloseView { view: view(5) },
            LayoutError::FinalView,
        );
    }

    #[test]
    fn pins_reorder_but_do_not_protect_tabs_from_explicit_close() {
        let mut layout = populated();
        layout.apply(add_tab(1, 2)).unwrap();
        layout.apply(add_tab(1, 3)).unwrap();
        layout
            .apply(LayoutOp::PinTab {
                view: view(1),
                tab: tab(3),
                pinned: true,
            })
            .unwrap();
        layout
            .apply(LayoutOp::PinTab {
                view: view(1),
                tab: tab(2),
                pinned: true,
            })
            .unwrap();
        assert_eq!(
            layout.views[0]
                .tabs
                .iter()
                .map(|item| item.id)
                .collect::<Vec<_>>(),
            [tab(3), tab(2), tab(1)]
        );
        layout
            .apply(LayoutOp::PinTab {
                view: view(1),
                tab: tab(3),
                pinned: false,
            })
            .unwrap();
        assert_eq!(
            layout.views[0]
                .tabs
                .iter()
                .map(|item| item.id)
                .collect::<Vec<_>>(),
            [tab(2), tab(3), tab(1)]
        );
        layout
            .apply(LayoutOp::SelectTab {
                view: view(1),
                tab: tab(2),
            })
            .unwrap();
        let effect = layout
            .apply(LayoutOp::CloseTab {
                view: view(1),
                tab: tab(2),
            })
            .unwrap();
        assert_eq!(effect.close.as_slice(), [terminal(2)]);
        assert_eq!(layout.views[0].active, Some(tab(3)));
        layout
            .apply(LayoutOp::RenameTab {
                view: view(1),
                tab: tab(3),
                name: Some("部署 — server".into()),
            })
            .unwrap();
        assert_eq!(
            layout.tab(view(1), tab(3)).unwrap().name.as_deref(),
            Some("部署 — server")
        );
        layout
            .apply(LayoutOp::RenameTab {
                view: view(1),
                tab: tab(3),
                name: None,
            })
            .unwrap();
        assert_eq!(layout.tab(view(1), tab(3)).unwrap().name, None);
        assert_valid(&layout);
    }

    #[test]
    fn tab_moves_use_final_indices_and_preserve_both_views_selection() {
        let mut layout = populated();
        layout.apply(add_tab(1, 2)).unwrap();
        layout.apply(add_tab(1, 3)).unwrap();
        layout
            .apply(LayoutOp::PinTab {
                view: view(1),
                tab: tab(1),
                pinned: true,
            })
            .unwrap();
        layout
            .apply(LayoutOp::MoveTab {
                view: view(1),
                tab: tab(2),
                to: view(1),
                index: 2,
            })
            .unwrap();
        assert_eq!(
            layout.views[0]
                .tabs
                .iter()
                .map(|item| item.id)
                .collect::<Vec<_>>(),
            [tab(1), tab(3), tab(2)]
        );
        assert_eq!(layout.views[0].active, Some(tab(2)));
        rejected(
            &mut layout,
            LayoutOp::MoveTab {
                view: view(1),
                tab: tab(2),
                to: view(1),
                index: 0,
            },
            LayoutError::PinnedOrder,
        );
        rejected(
            &mut layout,
            LayoutOp::MoveTab {
                view: view(1),
                tab: tab(1),
                to: view(1),
                index: 2,
            },
            LayoutError::PinnedOrder,
        );
        rejected(
            &mut layout,
            LayoutOp::MoveTab {
                view: view(1),
                tab: tab(2),
                to: view(1),
                index: 3,
            },
            LayoutError::TabIndex { index: 3, max: 2 },
        );

        layout.apply(split_view(4, 1)).unwrap();
        layout
            .apply(LayoutOp::PinTab {
                view: view(4),
                tab: tab(4),
                pinned: true,
            })
            .unwrap();
        rejected(
            &mut layout,
            LayoutOp::MoveTab {
                view: view(1),
                tab: tab(2),
                to: view(4),
                index: 0,
            },
            LayoutError::PinnedOrder,
        );
        let effect = layout
            .apply(LayoutOp::MoveTab {
                view: view(1),
                tab: tab(2),
                to: view(4),
                index: 1,
            })
            .unwrap();
        assert_eq!(effect, LayoutEffect::default());
        assert_eq!(layout.views[0].active, Some(tab(3)));
        assert_eq!(layout.active, view(4));
        assert_eq!(layout.views[1].active, Some(tab(2)));
        layout
            .apply(LayoutOp::MoveTab {
                view: view(1),
                tab: tab(1),
                to: view(4),
                index: 1,
            })
            .unwrap();
        assert_eq!(
            layout.views[1]
                .tabs
                .iter()
                .map(|item| item.id)
                .collect::<Vec<_>>(),
            [tab(4), tab(1), tab(2)]
        );
        layout
            .apply(LayoutOp::MoveTab {
                view: view(1),
                tab: tab(3),
                to: view(4),
                index: 3,
            })
            .unwrap();
        assert!(layout.views[0].tabs.is_empty());
        assert_eq!(layout.views[0].active, None);
        assert_eq!(layout.views[1].active, Some(tab(3)));
        assert_valid(&layout);
    }

    #[test]
    fn stale_scoped_ids_duplicate_ids_and_invalid_payloads_never_mutate() {
        let mut layout = populated();
        layout.apply(split_pane(1, 2, 1)).unwrap();
        layout.apply(split_view(3, 2)).unwrap();
        let cases = [
            (
                LayoutOp::SelectView { view: view(99) },
                LayoutError::ViewNotFound(view(99)),
            ),
            (
                LayoutOp::CloseTab {
                    view: view(1),
                    tab: tab(99),
                },
                LayoutError::TabNotFound {
                    view: view(1),
                    tab: tab(99),
                },
            ),
            (
                LayoutOp::SelectTab {
                    view: view(3),
                    tab: tab(1),
                },
                LayoutError::TabNotFound {
                    view: view(3),
                    tab: tab(1),
                },
            ),
            (
                LayoutOp::SelectPane {
                    view: view(3),
                    tab: tab(3),
                    pane: pane(1),
                },
                LayoutError::PaneNotFound {
                    tab: tab(3),
                    pane: pane(1),
                },
            ),
            (
                LayoutOp::ClosePane {
                    view: view(1),
                    tab: tab(1),
                    pane: pane(99),
                },
                LayoutError::PaneNotFound {
                    tab: tab(1),
                    pane: pane(99),
                },
            ),
            (
                LayoutOp::MoveTab {
                    view: view(1),
                    tab: tab(1),
                    to: view(99),
                    index: 0,
                },
                LayoutError::ViewNotFound(view(99)),
            ),
            (
                LayoutOp::SetViewRatio {
                    split: split(1),
                    ratio: 5000,
                },
                LayoutError::SplitNotFound(split(1)),
            ),
            (
                LayoutOp::SetPaneRatio {
                    view: view(1),
                    tab: tab(1),
                    split: split(2),
                    ratio: 5000,
                },
                LayoutError::SplitNotFound(split(2)),
            ),
            (
                LayoutOp::SetViewRatio {
                    split: split(2),
                    ratio: 999,
                },
                LayoutError::InvalidRatio(999),
            ),
            (
                LayoutOp::RenameTab {
                    view: view(1),
                    tab: tab(1),
                    name: Some(" \t ".into()),
                },
                LayoutError::InvalidName,
            ),
            (
                LayoutOp::RenameTab {
                    view: view(1),
                    tab: tab(1),
                    name: Some("bad\nname".into()),
                },
                LayoutError::InvalidName,
            ),
            (
                LayoutOp::RenameTab {
                    view: view(1),
                    tab: tab(1),
                    name: Some("界".repeat(43)),
                },
                LayoutError::InvalidName,
            ),
            (add_tab(1, 1), LayoutError::DuplicateTab(tab(1))),
            (split_pane(1, 2, 3), LayoutError::DuplicatePane(pane(2))),
            (split_pane(1, 4, 2), LayoutError::DuplicateSplit(split(2))),
            (split_view(3, 3), LayoutError::DuplicateView(view(3))),
            (
                LayoutOp::NewTab {
                    view: view(1),
                    tab: tab(4),
                    pane: pane(4),
                    terminal: terminal(1),
                    name: None,
                    size: Size::new(80, 24),
                },
                LayoutError::DuplicateTerminal(terminal(1)),
            ),
            (
                LayoutOp::NewTab {
                    view: view(1),
                    tab: tab(4),
                    pane: pane(4),
                    terminal: terminal(4),
                    name: None,
                    size: Size::new(1, 24),
                },
                LayoutError::InvalidSize(InvalidSize),
            ),
        ];
        for (op, error) in cases {
            rejected(&mut layout, op, error);
        }
        layout
            .apply(LayoutOp::CloseTab {
                view: view(1),
                tab: tab(1),
            })
            .unwrap();
        rejected(
            &mut layout,
            LayoutOp::SelectTab {
                view: view(1),
                tab: tab(1),
            },
            LayoutError::TabNotFound {
                view: view(1),
                tab: tab(1),
            },
        );
        assert_valid(&layout);
    }

    #[test]
    fn revision_exhaustion_rejects_creates_closes_and_selection_without_effects() {
        let mut layout = populated();
        layout.rev = u64::MAX;
        for op in [
            add_tab(1, 2),
            split_pane(1, 2, 1),
            LayoutOp::ClosePane {
                view: view(1),
                tab: tab(1),
                pane: pane(1),
            },
            LayoutOp::SelectTab {
                view: view(1),
                tab: tab(1),
            },
            LayoutOp::PinTab {
                view: view(1),
                tab: tab(1),
                pinned: true,
            },
        ] {
            rejected(&mut layout, op, LayoutError::RevisionOverflow);
        }
        layout.rev = u64::MAX - 1;
        layout
            .apply(LayoutOp::SelectTab {
                view: view(1),
                tab: tab(1),
            })
            .unwrap();
        assert_eq!(layout.rev, u64::MAX);
        assert_valid(&layout);
    }

    fn branch(id: u16, first: u16, second: u16) -> SplitNode<PaneId> {
        SplitNode::Branch {
            id: split(id),
            axis: Axis::Horizontal,
            ratio: DEFAULT_RATIO,
            first,
            second,
        }
    }

    #[test]
    fn flat_tree_validation_rejects_cycles_aliases_unreachable_and_duplicate_nodes() {
        let cases = [
            (
                SplitTree {
                    root: 0,
                    nodes: Vec::new(),
                },
                SplitTreeError::NodeCount,
            ),
            (
                SplitTree {
                    root: 1,
                    nodes: vec![SplitNode::Leaf(pane(1))],
                },
                SplitTreeError::NodeIndex(1),
            ),
            (
                SplitTree {
                    root: 0,
                    nodes: vec![branch(1, 1, 9), SplitNode::Leaf(pane(1))],
                },
                SplitTreeError::NodeIndex(9),
            ),
            (
                SplitTree {
                    root: 0,
                    nodes: vec![branch(1, 0, 1), SplitNode::Leaf(pane(1))],
                },
                SplitTreeError::ReusedNode(0),
            ),
            (
                SplitTree {
                    root: 0,
                    nodes: vec![branch(1, 1, 1), SplitNode::Leaf(pane(1))],
                },
                SplitTreeError::ReusedNode(1),
            ),
            (
                SplitTree {
                    root: 0,
                    nodes: vec![SplitNode::Leaf(pane(1)), SplitNode::Leaf(pane(2))],
                },
                SplitTreeError::UnreachableNode(1),
            ),
            (
                SplitTree {
                    root: 0,
                    nodes: vec![
                        branch(1, 1, 2),
                        SplitNode::Leaf(pane(1)),
                        SplitNode::Leaf(pane(1)),
                    ],
                },
                SplitTreeError::DuplicateLeaf,
            ),
            (
                SplitTree {
                    root: 0,
                    nodes: vec![
                        branch(1, 1, 2),
                        SplitNode::Leaf(pane(1)),
                        branch(1, 3, 4),
                        SplitNode::Leaf(pane(2)),
                        SplitNode::Leaf(pane(3)),
                    ],
                },
                SplitTreeError::DuplicateSplit(split(1)),
            ),
        ];
        for (tree, error) in cases {
            assert_eq!(tree.validate(), Err(error));
        }
        let tree = SplitTree {
            root: 0,
            nodes: vec![
                SplitNode::Branch {
                    id: split(1),
                    axis: Axis::Vertical,
                    ratio: 9001,
                    first: 1,
                    second: 2,
                },
                SplitNode::Leaf(pane(1)),
                SplitNode::Leaf(pane(2)),
            ],
        };
        assert_eq!(tree.validate(), Err(SplitTreeError::Ratio(9001)));
    }

    #[test]
    fn split_depth_limit_is_checked_before_any_terminal_is_added() {
        let mut layout = populated();
        for number in 2..=9 {
            layout.apply(split_pane(1, number, number - 1)).unwrap();
        }
        assert_valid(&layout);
        rejected(
            &mut layout,
            split_pane(1, 10, 9),
            LayoutError::InvalidTree(SplitTreeError::Depth),
        );
        let mut malformed = layout.clone();
        malformed.views[0].tabs[0]
            .split
            .split_leaf(pane(1), pane(10), split(9), Axis::Vertical);
        malformed.views[0].tabs[0].panes.push(Pane {
            id: pane(10),
            terminal: terminal(10),
        });
        assert_eq!(
            malformed.validate(),
            Err(LayoutError::InvalidTree(SplitTreeError::Depth))
        );
        let bytes = postcard::to_allocvec(&malformed).unwrap();
        assert!(postcard::from_bytes::<Layout>(&bytes).is_err());
    }

    #[test]
    fn malformed_persisted_layouts_are_rejected_before_consumers_observe_them() {
        let base = populated();
        let mut malformed = Vec::new();
        let mut layout = base.clone();
        layout.views[0].tabs[0].panes[0].id = pane(2);
        malformed.push(layout);
        let mut layout = base.clone();
        layout.views[0].active = None;
        malformed.push(layout);
        let mut layout = base.clone();
        layout.views[0].tabs[0].active = pane(2);
        malformed.push(layout);
        let mut layout = base.clone();
        layout.active = view(2);
        malformed.push(layout);
        let mut layout = base.clone();
        layout.views.clear();
        malformed.push(layout);
        let mut layout = base.clone();
        layout.views[0].tabs[0].panes.clear();
        malformed.push(layout);
        let mut layout = base.clone();
        layout.apply(add_tab(1, 2)).unwrap();
        layout.views[0].tabs[1].pinned = true;
        malformed.push(layout);
        let mut layout = base.clone();
        layout.apply(add_tab(1, 2)).unwrap();
        layout.views[0].tabs[1].panes[0].terminal = terminal(1);
        malformed.push(layout);
        let mut layout = base.clone();
        layout.apply(split_pane(1, 2, 1)).unwrap();
        layout.apply(split_view(3, 2)).unwrap();
        if let SplitNode::Branch { id, .. } = &mut layout.split.nodes[0] {
            *id = split(1);
        }
        malformed.push(layout);
        for mut layout in malformed {
            assert!(layout.validate().is_err());
            let bytes = postcard::to_allocvec(&layout).unwrap();
            assert!(postcard::from_bytes::<Layout>(&bytes).is_err());
            let before = layout.clone();
            assert!(
                layout
                    .apply(LayoutOp::SelectView { view: view(1) })
                    .is_err()
            );
            assert_eq!(layout, before);
        }
    }

    #[test]
    fn bounded_deserialization_rejects_oversized_collections_and_names() {
        let tree = SplitTree {
            root: 0,
            nodes: vec![SplitNode::Leaf(pane(1)); MAX_SPLIT_NODES + 1],
        };
        let bytes = postcard::to_allocvec(&tree).unwrap();
        assert!(postcard::from_bytes::<SplitTree<PaneId>>(&bytes).is_err());
        // A claimed length must be rejected without reserving that length or
        // requiring any elements to appear in the frame.
        let bytes = postcard::to_allocvec(&(0_u16, u64::MAX)).unwrap();
        assert!(postcard::from_bytes::<SplitTree<PaneId>>(&bytes).is_err());
        let mut layout = populated();
        layout.views[0].tabs[0].name = Some("x".repeat(MAX_TAB_NAME_BYTES + 1));
        let bytes = postcard::to_allocvec(&layout).unwrap();
        assert!(postcard::from_bytes::<Layout>(&bytes).is_err());
        let mut layout = Layout::new(workspace(1), view(1));
        layout.views = vec![layout.views[0].clone(); MAX_VIEWS + 1];
        let bytes = postcard::to_allocvec(&layout).unwrap();
        assert!(postcard::from_bytes::<Layout>(&bytes).is_err());
    }

    #[test]
    fn closing_from_an_arbitrary_valid_node_order_keeps_the_tree_compact() {
        let mut layout = populated();
        let item = &mut layout.views[0].tabs[0];
        item.panes.push(Pane {
            id: pane(2),
            terminal: terminal(2),
        });
        item.panes.push(Pane {
            id: pane(3),
            terminal: terminal(3),
        });
        item.split = SplitTree {
            root: 4,
            nodes: vec![
                SplitNode::Leaf(pane(1)),
                branch(2, 0, 3),
                SplitNode::Leaf(pane(2)),
                SplitNode::Leaf(pane(3)),
                branch(1, 1, 2),
            ],
        };
        assert_valid(&layout);
        layout
            .apply(LayoutOp::ClosePane {
                view: view(1),
                tab: tab(1),
                pane: pane(2),
            })
            .unwrap();
        assert_eq!(
            layout.views[0].tabs[0].split.leaves().collect::<Vec<_>>(),
            [pane(1), pane(3)]
        );
        assert_eq!(layout.views[0].tabs[0].split.nodes.len(), 3);
        assert_valid(&layout);
        layout
            .apply(LayoutOp::ClosePane {
                view: view(1),
                tab: tab(1),
                pane: pane(1),
            })
            .unwrap();
        assert_eq!(layout.views[0].tabs[0].split, SplitTree::leaf(pane(3)));
        assert_valid(&layout);
    }
}
