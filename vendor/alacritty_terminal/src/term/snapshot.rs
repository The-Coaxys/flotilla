// Flotilla patch: complete canonical terminal checkpoints and shared resource policies.
use std::collections::{HashMap, HashSet};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use super::*;
use crate::grid::{Cursor, GridSnapshot, Row};

/// Last confirmed OSC 133 phase, independent of local selection or focus.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum PromptState {
    #[default]
    Unknown,
    Prompt,
    Input,
    Command,
    Finished { exit_status: Option<i32> },
}

/// Cell content with an index into the checkpoint-local hyperlink table.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct SnapshotCell {
    pub cell: Cell,
    pub hyperlink: Option<u32>,
}

fn map_grid<A, B>(grid: &GridSnapshot<A>, mut map: impl FnMut(&A) -> B) -> GridSnapshot<B> {
    let rows = grid.rows.iter().map(|row| {
        Row::from_vec(row.into_iter().map(&mut map).collect(), row.occ)
    }).collect();
    let cursor = Cursor {
        point: grid.cursor.point, template: map(&grid.cursor.template),
        charsets: grid.cursor.charsets, input_needs_wrap: grid.cursor.input_needs_wrap,
    };
    let saved_cursor = Cursor {
        point: grid.saved_cursor.point, template: map(&grid.saved_cursor.template),
        charsets: grid.saved_cursor.charsets, input_needs_wrap: grid.saved_cursor.input_needs_wrap,
    };
    GridSnapshot {
        rows, cursor, saved_cursor, columns: grid.columns, lines: grid.lines,
        max_scroll_limit: grid.max_scroll_limit,
    }
}

/// Complete future-determining terminal state, not a screen replay.
///
/// Active/inactive refer to the current screen, as indicated by `mode`'s ALT_SCREEN bit.
/// Listeners, damage, selection, vi mode, viewport scrolling and focus are local and excluded.
/// Deserializers must also impose a byte/allocation budget before invoking `from_snapshot`.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct TermSnapshot {
    pub grid: GridSnapshot<SnapshotCell>,
    pub inactive_grid: GridSnapshot<SnapshotCell>,
    pub hyperlinks: Vec<cell::Hyperlink>,
    pub active_charset: CharsetIndex,
    pub tabs: Vec<bool>,
    pub mode: u32,
    pub scroll_region: Range<Line>,
    pub colors: Vec<Option<Rgb>>,
    pub cursor_style: Option<CursorStyle>,
    pub title: Option<String>,
    pub title_stack: Vec<Option<String>>,
    pub keyboard_mode_stack: Vec<u8>,
    pub inactive_keyboard_mode_stack: Vec<u8>,
    pub config: Config,
    pub next_hyperlink_id: u64,
    pub hyperlink_bytes: u64,
    pub cwd: Option<String>,
    pub prompt_state: PromptState,
}

pub(super) fn history_limit(requested: usize, columns: usize, lines: usize) -> usize {
    requested.min(MAX_HISTORY).min((MAX_GRID_CELLS / columns).saturating_sub(lines))
}

impl Config {
    pub(super) fn clamp_limits(&mut self) {
        self.scrolling_history = self.scrolling_history.min(MAX_HISTORY);
        if self.semantic_escape_chars.len() > MAX_TEXT_BYTES {
            self.semantic_escape_chars = SEMANTIC_ESCAPE_CHARS.to_owned();
        }
    }
}

impl<T> Term<T> {
    pub fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub fn prompt_state(&self) -> PromptState {
        self.prompt_state
    }

    pub fn snapshot(&self) -> TermSnapshot {
        let mut hyperlinks = Vec::new();
        let mut indices = HashMap::new();
        let mut encode = |cell: &Cell| {
            let hyperlink = cell.hyperlink().map(|link| {
                *indices.entry(link.clone()).or_insert_with(|| {
                    let index = hyperlinks.len() as u32;
                    hyperlinks.push(link);
                    index
                })
            });
            let mut cell = cell.clone();
            if hyperlink.is_some() {
                cell.set_hyperlink(None);
            }
            SnapshotCell { cell, hyperlink }
        };
        let grid = map_grid(&self.grid.snapshot(), &mut encode);
        let inactive_grid = map_grid(&self.inactive_grid.snapshot(), &mut encode);
        TermSnapshot {
            grid,
            inactive_grid,
            hyperlinks,
            active_charset: self.active_charset,
            tabs: self.tabs.tabs.clone(),
            mode: (self.mode - TermMode::VI).bits(),
            scroll_region: self.scroll_region.clone(),
            colors: (0..color::COUNT).map(|index| self.colors[index]).collect(),
            cursor_style: self.cursor_style,
            title: self.title.clone(),
            title_stack: self.title_stack.clone(),
            keyboard_mode_stack: self.keyboard_mode_stack.iter().map(|mode| mode.bits()).collect(),
            inactive_keyboard_mode_stack: self.inactive_keyboard_mode_stack.iter().map(|mode| mode.bits()).collect(),
            config: self.config.clone(),
            next_hyperlink_id: self.next_hyperlink_id,
            hyperlink_bytes: self.hyperlink_bytes,
            cwd: self.cwd.clone(),
            prompt_state: self.prompt_state,
        }
    }

    /// Direct import; no Handler replay and no externally observable events.
    pub fn from_snapshot(snapshot: TermSnapshot, event_proxy: T) -> Result<Self, String> {
        snapshot.validate()?;
        let decode = |cell: &SnapshotCell| {
            let mut content = cell.cell.clone();
            if let Some(index) = cell.hyperlink {
                content.set_hyperlink(Some(snapshot.hyperlinks[index as usize].clone()));
            }
            content
        };
        let grid = Grid::from_snapshot(map_grid(&snapshot.grid, decode))?;
        let inactive_grid = Grid::from_snapshot(map_grid(&snapshot.inactive_grid, decode))?;
        let damage = TermDamageState::new(grid.columns(), grid.screen_lines());
        let mut colors = Colors::default();
        for (index, color) in snapshot.colors.into_iter().enumerate() {
            colors[index] = color;
        }
        let keyboard_mode_stack = snapshot.keyboard_mode_stack.into_iter()
            .map(KeyboardModes::from_bits_retain).collect();
        let inactive_keyboard_mode_stack = snapshot.inactive_keyboard_mode_stack.into_iter()
            .map(KeyboardModes::from_bits_retain).collect();
        Ok(Self {
            is_focused: false,
            vi_mode_cursor: ViModeCursor::default(),
            selection: None,
            grid,
            inactive_grid,
            active_charset: snapshot.active_charset,
            tabs: TabStops { tabs: snapshot.tabs },
            mode: TermMode::from_bits_retain(snapshot.mode),
            scroll_region: snapshot.scroll_region,
            colors,
            cursor_style: snapshot.cursor_style,
            event_proxy,
            title: snapshot.title,
            title_stack: snapshot.title_stack,
            keyboard_mode_stack,
            inactive_keyboard_mode_stack,
            damage,
            config: snapshot.config,
            next_hyperlink_id: snapshot.next_hyperlink_id,
            hyperlink_bytes: snapshot.hyperlink_bytes,
            cwd: snapshot.cwd,
            prompt_state: snapshot.prompt_state,
        })
    }
}

impl TermSnapshot {
    /// Validate all terminal metadata before trusting internal indices or constructing storage.
    pub fn validate(&self) -> Result<(), String> {
        let columns = self.grid.columns;
        let lines = self.grid.lines;
        let mode_bits = ((1u32 << 23) - 1) & !TermMode::VI.bits();
        if self.mode & !mode_bits != 0
            || self.config.scrolling_history > MAX_HISTORY
            || self.config.semantic_escape_chars.len() > MAX_TEXT_BYTES
            || self.colors.len() != color::COUNT
            || self.tabs.len() != columns
            || self.inactive_grid.columns != columns
            || self.inactive_grid.lines != lines
            || !(MIN_COLUMNS..=MAX_COLUMNS).contains(&columns)
            || !(MIN_SCREEN_LINES..=MAX_SCREEN_LINES).contains(&lines)
            || self.scroll_region.start.0 < 0
            || self.scroll_region.start >= self.scroll_region.end
            || self.scroll_region.end.0 as usize > lines
            || self.title.as_ref().is_some_and(|title| title.len() > MAX_TEXT_BYTES)
            || self.title_stack.len() > TITLE_STACK_MAX_DEPTH
            || self.title_stack.iter().flatten().any(|title| title.len() > MAX_TEXT_BYTES)
            || self.cwd.as_ref().is_some_and(|cwd| cwd.len() > MAX_CWD_BYTES)
            || self.hyperlink_bytes > MAX_HYPERLINK_BYTES
        {
            return Err("invalid checkpoint terminal metadata".into());
        }
        for stack in [&self.keyboard_mode_stack, &self.inactive_keyboard_mode_stack] {
            if stack.len() > KEYBOARD_MODE_STACK_MAX_DEPTH
                || stack.iter().any(|bits| KeyboardModes::from_bits(*bits).is_none())
            {
                return Err("invalid checkpoint keyboard mode stack".into());
            }
        }
        let primary_history = history_limit(self.config.scrolling_history, columns, lines);
        let (active_history, inactive_history) = if self.mode & TermMode::ALT_SCREEN.bits() != 0 {
            (0, primary_history)
        } else {
            (primary_history, 0)
        };
        if self.grid.max_scroll_limit != active_history
            || self.inactive_grid.max_scroll_limit != inactive_history
        {
            return Err("checkpoint history configuration mismatch".into());
        }
        if self.hyperlinks.len() > 2 * MAX_GRID_CELLS + 4 {
            return Err("checkpoint hyperlink table exceeds cell budget".into());
        }
        let mut links = HashSet::new();
        let mut bytes = 0u64;
        for link in &self.hyperlinks {
            if link.id().len() > MAX_URI_BYTES || link.uri().len() > MAX_URI_BYTES
                || !links.insert((link.id(), link.uri()))
            {
                return Err("invalid checkpoint hyperlink table".into());
            }
            bytes += (link.id().len() + link.uri().len()) as u64;
            if bytes > self.hyperlink_bytes {
                return Err("checkpoint hyperlink creation budget mismatch".into());
            }
        }
        let mut seen = vec![false; self.hyperlinks.len()];
        let mut used = 0usize;
        for grid in [&self.grid, &self.inactive_grid] {
            if grid.rows.len() < lines
                || grid.rows.len() > lines + grid.max_scroll_limit
                || grid.rows.iter().any(|row| row.len() != columns || row.occ > columns)
            {
                return Err("invalid checkpoint grid rows".into());
            }
            for cursor in [&grid.cursor, &grid.saved_cursor] {
                if cursor.point.line.0 < 0 || cursor.point.line.0 as usize >= lines
                    || cursor.point.column.0 >= columns
                {
                    return Err("invalid checkpoint cursor".into());
                }
            }
            let cells = grid.rows.iter().flat_map(|row| row.into_iter())
                .chain([&grid.cursor.template, &grid.saved_cursor.template]);
            for cell in cells {
                cell.cell.validate_snapshot()?;
                if cell.cell.hyperlink().is_some()
                    || cell.hyperlink.is_some_and(|index| index as usize >= self.hyperlinks.len())
                {
                    return Err("invalid checkpoint hyperlink reference".into());
                }
                if let Some(index) = cell.hyperlink {
                    let index = index as usize;
                    if !seen[index] {
                        if index != used {
                            return Err("checkpoint hyperlink table is not canonical".into());
                        }
                        seen[index] = true;
                        used += 1;
                    }
                }
            }
        }
        if used != self.hyperlinks.len() {
            return Err("checkpoint hyperlink table has unused entries".into());
        }
        Ok(())
    }
}

#[cfg(all(test, feature = "serde"))]
#[path = "snapshot_tests.rs"]
mod tests;
