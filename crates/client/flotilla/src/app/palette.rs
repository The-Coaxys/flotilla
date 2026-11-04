//! Command palette: modes, entries, fuzzy ranking and the mode-switch rules.
//!
//! The window-free half lives here so it is testable; the overlay itself and
//! the actions it can run live in `host_workspace/host_palette.rs`, where the
//! selected host's mirrors and model methods are reachable.
use fl_model::workspace::{LayoutScope, ScriptKind};
use fl_model::{ProjectId, TabId, ViewId, WorkspaceId, WorktreeId};
use gpui::SharedString;
use uuid::Uuid;

use super::git_actions::GitAction;

/// Every palette mode, in the order Ctrl+1…6 selects them (spec §8.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Global,
    Commands,
    Files,
    Worktrees,
    Tabs,
    Conversations,
}

pub(crate) const MODES: [Mode; 6] = [
    Mode::Global,
    Mode::Commands,
    Mode::Files,
    Mode::Worktrees,
    Mode::Tabs,
    Mode::Conversations,
];

/// Rows rendered at once; the keyboard never moves past them.
pub(crate) const MAX_VISIBLE: usize = 9;

impl Mode {
    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::Global => "Global",
            Self::Commands => "Commands",
            Self::Files => "Files",
            Self::Worktrees => "Worktrees",
            Self::Tabs => "Tabs",
            Self::Conversations => "Conversations",
        }
    }

    pub(crate) fn placeholder(self) -> &'static str {
        match self {
            Self::Global => "Search commands, worktrees, tabs and conversations",
            Self::Commands => "Run a Flotilla action",
            Self::Files => "Open a file seen in the Files panel",
            Self::Worktrees => "Switch worktree",
            Self::Tabs => "Jump to an open tab",
            Self::Conversations => "Open a conversation",
        }
    }

    pub(crate) fn index(self) -> usize {
        MODES.iter().position(|mode| *mode == self).unwrap_or(0)
    }

    /// Ctrl+1…6, as the palette shows them.
    pub(crate) fn chord(self) -> SharedString {
        format!("Ctrl+{}", self.index() + 1).into()
    }

    pub(crate) fn next(self) -> Self {
        MODES[(self.index() + 1) % MODES.len()]
    }

    pub(crate) fn prev(self) -> Self {
        MODES[(self.index() + MODES.len() - 1) % MODES.len()]
    }

    /// The mode a Ctrl+<digit> chord selects, if any.
    pub(crate) fn from_chord(key: &str) -> Option<Self> {
        let digit: usize = key.parse().ok()?;
        MODES.get(digit.checked_sub(1)?).copied()
    }
}

/// Which panel the "Open … panel" commands reveal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Panel {
    Files,
    Changes,
    Checks,
    Review,
    Chat,
}

/// Everything a palette row can do. Each one routes through an existing
/// `HostWorkspace` method; no action invents a wire call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
    AddProject,
    NewWorktree(ProjectId),
    NewTerminal,
    TogglePanel,
    OpenPanel(Panel),
    SelectScope(LayoutScope),
    /// Open a file by absolute path on the host.
    OpenFile(String),
    /// Show a document that is already open, reopening a file if it was closed.
    ActivateDocument {
        scope: LayoutScope,
        path: String,
        diff: bool,
    },
    OpenChat(Uuid),
    SelectTab {
        view: ViewId,
        tab: TabId,
    },
    CloseTab {
        view: ViewId,
        tab: TabId,
    },
    RunScript {
        worktree: WorktreeId,
        kind: ScriptKind,
    },
    /// Open the scoped custom-command and prompt-template editor.
    OpenCommands,
    /// Settings: theme editor, keyboard editor and notification policy.
    OpenSettings,
    /// Toggle the performance HUD.
    ToggleHud,
    /// Open the sidebar section editor for a workspace.
    OpenSections(WorkspaceId),
    /// Open the routed-action editor for a workspace.
    OpenRoutes(WorkspaceId),
    /// Open the chat inbox.
    OpenInbox,
    /// Start a new chat in the selected worktree.
    NewChat,
    /// Open the selected worktree's branch group diff.
    OpenGroupDiff,
    Git {
        worktree: WorktreeId,
        action: GitAction,
    },
}

/// The source a row came from, shown on the right of the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    Command,
    File,
    Worktree,
    Tab,
    Conversation,
}

impl Source {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::File => "file",
            Self::Worktree => "worktree",
            Self::Tab => "tab",
            Self::Conversation => "chat",
        }
    }
}

/// One palette row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) title: SharedString,
    pub(crate) detail: SharedString,
    pub(crate) source: Source,
    pub(crate) action: Action,
}

/// Ranked rows plus the note explaining a partial source.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Listing {
    pub(crate) entries: Vec<Entry>,
    /// Set when the source cannot be complete from the client's mirrors.
    pub(crate) note: Option<SharedString>,
}

impl Listing {
    pub(crate) fn push(
        &mut self,
        title: impl Into<SharedString>,
        detail: impl Into<SharedString>,
        source: Source,
        action: Action,
    ) {
        self.entries.push(Entry {
            title: title.into(),
            detail: detail.into(),
            source,
            action,
        });
    }
}

/// Characters that start a new word in a path, branch or label.
fn is_boundary(c: char) -> bool {
    matches!(c, '/' | '\\' | '_' | '-' | '.' | ' ' | ':' | '@')
}

/// Fuzzy sub-sequence score: `Some(score)` when every query character appears
/// in `text` in order, larger is better. Case-insensitive; a prefix match and
/// contiguous characters score higher than a scattered subsequence.
pub(crate) fn score(query: &str, text: &str) -> Option<u32> {
    let query: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    let text: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
    if query.is_empty() {
        return Some(0);
    }
    if query.len() > text.len() {
        return None;
    }
    let mut total = 0u32;
    let mut cursor = 0usize;
    let mut first = None;
    let mut last: Option<usize> = None;
    for wanted in &query {
        let found = cursor + text[cursor..].iter().position(|c| c == wanted)?;
        let mut gain = 1;
        if found == 0 || is_boundary(text[found - 1]) {
            gain += 8;
        }
        if last.is_some_and(|previous| previous + 1 == found) {
            gain += 6;
        }
        total += gain;
        first.get_or_insert(found);
        last = Some(found);
        cursor = found + 1;
    }
    // Earlier first matches are better: they keep the row near its source.
    total += 16u32.saturating_sub(first.unwrap_or(0).min(16) as u32);
    if text.starts_with(&query) {
        total += 20;
    }
    Some(total)
}

/// Rank rows for `query`, keeping the source order for equally good matches.
pub(crate) fn rank(query: &str, entries: Vec<Entry>) -> Vec<Entry> {
    let query = query.trim();
    if query.is_empty() {
        return entries;
    }
    let mut scored: Vec<(u32, usize, Entry)> = entries
        .into_iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            let best = score(query, &entry.title)
                .into_iter()
                .chain(score(query, &entry.detail).map(|score| score.saturating_sub(6)))
                .max()?;
            Some((best, index, entry))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, _, entry)| entry).collect()
}

impl super::Workspace {
    /// Open the palette for the selected host on the next frame. The
    /// app-level keybinding has no window, and the palette needs one to build
    /// its text input, so the host opens it from its render.
    pub(super) fn request_palette(&mut self, mode: Mode, cx: &mut gpui::Context<Self>) {
        let Some(host) = self
            .selected
            .and_then(|index| self.hosts.get(index))
            .and_then(|host| host.workspace.clone())
        else {
            return;
        };
        host.update(cx, |host, cx| host.request_palette(mode, cx));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(title: &str, detail: &str) -> Entry {
        Entry {
            title: title.to_string().into(),
            detail: detail.to_string().into(),
            source: Source::Command,
            action: Action::NewTerminal,
        }
    }

    #[test]
    fn modes_switch_in_spec_order_and_wrap() {
        assert_eq!(Mode::Global.index(), 0);
        assert_eq!(Mode::Conversations.index(), 5);
        assert_eq!(Mode::Global.next(), Mode::Commands);
        assert_eq!(Mode::Conversations.next(), Mode::Global);
        assert_eq!(Mode::Global.prev(), Mode::Conversations);
        assert_eq!(Mode::Tabs.prev(), Mode::Worktrees);
        for (index, mode) in MODES.iter().enumerate() {
            assert_eq!(Mode::from_chord(&(index + 1).to_string()), Some(*mode));
            assert_eq!(mode.chord().to_string(), format!("Ctrl+{}", index + 1));
        }
        assert_eq!(Mode::from_chord("0"), None);
        assert_eq!(Mode::from_chord("7"), None);
        assert_eq!(Mode::from_chord("q"), None);
    }

    #[test]
    fn fuzzy_prefers_prefix_and_contiguous_matches() {
        let prefix = score("term", "terminal").unwrap();
        let scattered = score("term", "the remote module").unwrap();
        assert!(prefix > scattered, "{prefix} should beat {scattered}");
        // A word start beats a match inside a word.
        assert!(score("main", "main.rs").unwrap() > score("main", "domain.yml").unwrap());
        // Subsequences are allowed; non-subsequences are not.
        assert!(score("flx", "flotilla-x").is_some());
        assert!(score("xyz", "flotilla").is_none());
        assert_eq!(score("", "anything"), Some(0));
        // Case-insensitive.
        assert!(score("PR", "push the build").is_none());
        assert!(score("pr", "Create PR").is_some());
        assert!(score("PR", "Create pr").is_some());
    }

    #[test]
    fn rank_filters_and_orders_results() {
        let entries = vec![
            entry("Push", "worktree a"),
            entry("Create PR", "push the branch"),
            entry("Add project", "folder or clone"),
            entry("Toggle right panel", "show or hide"),
        ];
        let ranked = rank("push", entries.clone());
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].title.to_string(), "Push");
        assert_eq!(ranked[1].title.to_string(), "Create PR");

        // Empty queries keep the source order untouched.
        let all = rank("  ", entries.clone());
        assert_eq!(all, entries);

        // No match means no rows, not a fallback list.
        assert!(rank("zzzz", entries).is_empty());
    }

    #[test]
    fn rank_matches_details_when_the_title_does_not() {
        let entries = vec![
            entry("Run the tests", "cargo test --workspace"),
            entry("Build", "cargo build"),
        ];
        let ranked = rank("workspace", entries);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].title.to_string(), "Run the tests");
    }
}
