//! Selection in grid coordinates (scrollback negative, first live row 0), so
//! it survives local scrolling. Expansion reads cells from the visible frame;
//! the selected text itself comes from the source's `text_between`, which
//! covers scrollback outside the frame.
use fl_term::{Cell, Flags, RenderFrame, TextPoint};

use crate::grid::Side;

/// Read-only access to the cells a selection or link can inspect.
pub trait CellGrid {
    fn columns(&self) -> u16;
    /// `None` outside the available rows.
    fn cell(&self, line: i32, column: u16) -> Option<&Cell>;

    /// The line continues on the next line (soft wrap).
    fn wrapped(&self, line: i32) -> bool {
        self.cell(line, self.columns().saturating_sub(1))
            .is_some_and(|cell| cell.flags.contains(Flags::WRAPLINE))
    }
}

/// The frame's visible rows, addressed by grid line.
pub struct FrameGrid<'a>(pub &'a RenderFrame);

impl FrameGrid<'_> {
    /// Grid line shown on viewport `row`.
    pub fn line_of_row(&self, row: u16) -> i32 {
        row as i32 - self.0.display_offset as i32
    }

    /// Viewport row showing grid `line`, if visible.
    pub fn row_of_line(&self, line: i32) -> Option<u16> {
        let row = line + self.0.display_offset as i32;
        (0..self.0.size.rows as i32)
            .contains(&row)
            .then_some(row as u16)
    }
}

impl CellGrid for FrameGrid<'_> {
    fn columns(&self) -> u16 {
        self.0.size.columns
    }

    fn cell(&self, line: i32, column: u16) -> Option<&Cell> {
        let row = self.row_of_line(line)?;
        let columns = self.0.size.columns as usize;
        if column as usize >= columns {
            return None;
        }
        self.0.cells.get(row as usize * columns + column as usize)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionKind {
    /// Character-wise, following the pointer.
    Simple,
    /// Double click: whole words.
    Word,
    /// Triple click: whole logical lines, across soft wraps.
    Line,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Anchor {
    pub point: TextPoint,
    pub side: Side,
}

/// Inclusive cell range, `start <= end`, both in grid lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionRange {
    pub start: TextPoint,
    pub end: TextPoint,
}

impl SelectionRange {
    /// Highlighted columns on `line` (inclusive), if any.
    pub fn columns_on(&self, line: i32, columns: u16) -> Option<(u16, u16)> {
        if line < self.start.line || line > self.end.line || columns == 0 {
            return None;
        }
        let first = if line == self.start.line {
            self.start.column
        } else {
            0
        };
        let last = if line == self.end.line {
            self.end.column
        } else {
            columns - 1
        };
        (first <= last).then_some((first, last.min(columns - 1)))
    }

    pub fn contains(&self, point: TextPoint) -> bool {
        key(point) >= key(self.start) && key(point) <= key(self.end)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub kind: SelectionKind,
    pub anchor: Anchor,
    pub head: Anchor,
}

/// Default word separators (Alacritty's semantic escape characters).
const SEPARATORS: &str = ",│`|:\"' ()[]{}<>\t";

fn key(point: TextPoint) -> (i32, u16) {
    (point.line, point.column)
}

impl Selection {
    pub fn new(kind: SelectionKind, anchor: Anchor) -> Self {
        Self {
            kind,
            anchor,
            head: anchor,
        }
    }

    pub fn update(&mut self, head: Anchor) {
        self.head = head;
    }

    /// Content moved up by `lines` (new output pushed it into scrollback).
    pub fn scroll_content(&mut self, lines: i32) {
        self.anchor.point.line -= lines;
        self.head.point.line -= lines;
    }

    /// The selected cells, or `None` for an empty character selection (a
    /// click without crossing a cell midpoint).
    pub fn range(&self, grid: &impl CellGrid) -> Option<SelectionRange> {
        let columns = grid.columns();
        if columns == 0 {
            return None;
        }
        let (first, second) = if (key(self.anchor.point), self.anchor.side)
            <= (key(self.head.point), self.head.side)
        {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        };
        let range = match self.kind {
            SelectionKind::Simple => {
                let start = if first.side == Side::Right {
                    step_right(first.point, columns)
                } else {
                    first.point
                };
                let end = if second.side == Side::Left {
                    step_left(second.point, columns)?
                } else {
                    second.point
                };
                if key(start) > key(end) {
                    return None;
                }
                SelectionRange { start, end }
            }
            SelectionKind::Word => SelectionRange {
                start: word_start(grid, first.point),
                end: word_end(grid, second.point),
            },
            SelectionKind::Line => SelectionRange {
                start: TextPoint {
                    line: logical_first(grid, first.point.line),
                    column: 0,
                },
                end: TextPoint {
                    line: logical_last(grid, second.point.line),
                    column: columns - 1,
                },
            },
        };
        Some(snap_wide(grid, range))
    }
}

fn step_right(point: TextPoint, columns: u16) -> TextPoint {
    if point.column + 1 >= columns {
        TextPoint {
            line: point.line + 1,
            column: 0,
        }
    } else {
        TextPoint {
            column: point.column + 1,
            ..point
        }
    }
}

fn step_left(point: TextPoint, columns: u16) -> Option<TextPoint> {
    if point.column == 0 {
        Some(TextPoint {
            line: point.line.checked_sub(1)?,
            column: columns - 1,
        })
    } else {
        Some(TextPoint {
            column: point.column - 1,
            ..point
        })
    }
}

/// A wide character and its spacer are selected together.
fn snap_wide(grid: &impl CellGrid, mut range: SelectionRange) -> SelectionRange {
    if range.start.column > 0
        && grid
            .cell(range.start.line, range.start.column)
            .is_some_and(|c| c.flags.contains(Flags::WIDE_CHAR_SPACER))
    {
        range.start.column -= 1;
    }
    if range.end.column + 1 < grid.columns()
        && grid
            .cell(range.end.line, range.end.column)
            .is_some_and(|c| c.flags.contains(Flags::WIDE_CHAR))
    {
        range.end.column += 1;
    }
    range
}

/// The character a cell contributes to word classification; spacers belong
/// to their wide character or the wrapped wide character that follows.
fn word_char(grid: &impl CellGrid, point: TextPoint) -> Option<char> {
    let cell = grid.cell(point.line, point.column)?;
    if cell.flags.contains(Flags::WIDE_CHAR_SPACER) && point.column > 0 {
        return grid.cell(point.line, point.column - 1).map(|c| c.c);
    }
    if cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
        return grid.cell(point.line + 1, 0).map(|c| c.c);
    }
    Some(cell.c)
}

fn is_separator(c: char) -> bool {
    c == '\0' || SEPARATORS.contains(c)
}

fn prev_point(grid: &impl CellGrid, point: TextPoint) -> Option<TextPoint> {
    if point.column > 0 {
        return Some(TextPoint {
            column: point.column - 1,
            ..point
        });
    }
    let previous = point.line - 1;
    grid.wrapped(previous).then(|| TextPoint {
        line: previous,
        column: grid.columns() - 1,
    })
}

fn next_point(grid: &impl CellGrid, point: TextPoint) -> Option<TextPoint> {
    if point.column + 1 < grid.columns() {
        return Some(TextPoint {
            column: point.column + 1,
            ..point
        });
    }
    grid.wrapped(point.line).then(|| TextPoint {
        line: point.line + 1,
        column: 0,
    })
}

fn word_start(grid: &impl CellGrid, point: TextPoint) -> TextPoint {
    if word_char(grid, point).is_none_or(is_separator) {
        return point;
    }
    let mut start = point;
    while let Some(previous) = prev_point(grid, start) {
        match word_char(grid, previous) {
            Some(c) if !is_separator(c) => start = previous,
            _ => break,
        }
    }
    start
}

fn word_end(grid: &impl CellGrid, point: TextPoint) -> TextPoint {
    if word_char(grid, point).is_none_or(is_separator) {
        return point;
    }
    let mut end = point;
    while let Some(next) = next_point(grid, end) {
        match word_char(grid, next) {
            Some(c) if !is_separator(c) => end = next,
            _ => break,
        }
    }
    end
}

fn logical_first(grid: &impl CellGrid, mut line: i32) -> i32 {
    while grid.cell(line - 1, 0).is_some() && grid.wrapped(line - 1) {
        line -= 1;
    }
    line
}

fn logical_last(grid: &impl CellGrid, mut line: i32) -> i32 {
    while grid.wrapped(line) && grid.cell(line + 1, 0).is_some() {
        line += 1;
    }
    line
}

/// How far existing content moved up between two frames of one terminal,
/// so grid-anchored state (selection, search hits) can follow it. `None`
/// when line identities were lost: a resize reflow, switching screens, or
/// cleared history. Once history is at its retention limit, growth no
/// longer shows in `history_lines`; anchored state then stays put.
pub fn content_shift(old: &RenderFrame, new: &RenderFrame) -> Option<i32> {
    let alternate = |f: &RenderFrame| f.modes.contains(fl_term::TermMode::ALT_SCREEN);
    if (old.size.columns, old.size.rows) != (new.size.columns, new.size.rows)
        || alternate(old) != alternate(new)
        || new.history_lines < old.history_lines
    {
        return None;
    }
    Some((new.history_lines - old.history_lines) as i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_term::{EffectMode, Engine, Size};

    fn engine(columns: u16, rows: u16, bytes: &[u8]) -> Engine {
        let mut engine = Engine::new(Size::new(columns, rows), EffectMode::Mirror).unwrap();
        engine.feed(0, bytes).unwrap();
        engine
    }

    fn frame(engine: &Engine) -> RenderFrame {
        let mut frame = RenderFrame::default();
        engine.render_into(&mut frame);
        frame
    }

    fn at(line: i32, column: u16, side: Side) -> Anchor {
        Anchor {
            point: TextPoint { line, column },
            side,
        }
    }

    fn text(engine: &Engine, range: SelectionRange) -> String {
        engine.text_between(range.start, range.end)
    }

    #[test]
    fn character_selection_respects_cell_midpoints() {
        let e = engine(20, 3, b"hello world");
        let f = frame(&e);
        let grid = FrameGrid(&f);
        // Click without crossing a midpoint selects nothing.
        let click = Selection::new(SelectionKind::Simple, at(0, 2, Side::Left));
        assert_eq!(click.range(&grid), None);
        // Drag from the right half of 'h' to the right half of 'o'.
        let mut s = Selection::new(SelectionKind::Simple, at(0, 0, Side::Right));
        s.update(at(0, 4, Side::Right));
        assert_eq!(text(&e, s.range(&grid).unwrap()), "ello");
        // Dragging backwards gives the same cells.
        let mut back = Selection::new(SelectionKind::Simple, at(0, 4, Side::Right));
        back.update(at(0, 0, Side::Right));
        assert_eq!(back.range(&grid), s.range(&grid));
    }

    #[test]
    fn selection_across_a_soft_wrap_joins_without_newline() {
        // 10 columns: "abcdefghij" wraps into "klm".
        let e = engine(10, 4, b"abcdefghijklm\r\nnext");
        let f = frame(&e);
        let grid = FrameGrid(&f);
        assert!(grid.wrapped(0));
        let mut s = Selection::new(SelectionKind::Simple, at(0, 7, Side::Left));
        s.update(at(1, 1, Side::Right));
        assert_eq!(text(&e, s.range(&grid).unwrap()), "hijkl");
        // A triple click on the continuation selects the whole logical line.
        let line = Selection::new(SelectionKind::Line, at(1, 0, Side::Left));
        let range = line.range(&grid).unwrap();
        assert_eq!((range.start.line, range.end.line), (0, 1));
        assert_eq!(text(&e, range), "abcdefghijklm");
        // A word split by the wrap is one word.
        let word = Selection::new(SelectionKind::Word, at(1, 2, Side::Left));
        assert_eq!(text(&e, word.range(&grid).unwrap()), "abcdefghijklm");
    }

    #[test]
    fn wide_cells_are_selected_whole() {
        // 界 occupies columns 2-3.
        let e = engine(20, 2, "ab界cd".as_bytes());
        let f = frame(&e);
        let grid = FrameGrid(&f);
        // Starting on the spacer's left half includes the wide character.
        let mut s = Selection::new(SelectionKind::Simple, at(0, 3, Side::Left));
        s.update(at(0, 4, Side::Right));
        let range = s.range(&grid).unwrap();
        assert_eq!(range.start.column, 2);
        assert_eq!(text(&e, range), "界c");
        // Ending on the wide character extends over its spacer.
        let mut s = Selection::new(SelectionKind::Simple, at(0, 0, Side::Left));
        s.update(at(0, 2, Side::Right));
        let range = s.range(&grid).unwrap();
        assert_eq!(range.end.column, 3);
        assert_eq!(text(&e, range), "ab界");
        assert_eq!(range.columns_on(0, 20), Some((0, 3)));
    }

    #[test]
    fn word_selection_stops_at_separators_and_includes_wide_words() {
        let e = engine(30, 2, "ls 'src/main.rs' 日本語!".as_bytes());
        let f = frame(&e);
        let grid = FrameGrid(&f);
        let word = Selection::new(SelectionKind::Word, at(0, 6, Side::Left));
        assert_eq!(text(&e, word.range(&grid).unwrap()), "src/main.rs");
        // Clicking the spacer half of 本.
        let wide = Selection::new(SelectionKind::Word, at(0, 20, Side::Left));
        assert_eq!(text(&e, wide.range(&grid).unwrap()), "日本語!");
    }

    #[test]
    fn selection_reaches_into_scrollback_and_follows_new_output() {
        let mut e = engine(10, 3, b"one\r\ntwo\r\nthree\r\nfour\r\nfive");
        // "one" and "two" scrolled into history.
        e.scroll(2);
        let f = frame(&e);
        let grid = FrameGrid(&f);
        assert_eq!(f.display_offset, 2);
        assert_eq!(grid.line_of_row(0), -2);
        let mut s = Selection::new(SelectionKind::Simple, at(-2, 0, Side::Left));
        s.update(at(-1, 2, Side::Right));
        let range = s.range(&grid).unwrap();
        assert_eq!(text(&e, range), "one\ntwo");
        // Output while scrolled back pushes one more line into history.
        let before = f.history_lines;
        let offset = e.offset();
        e.feed(offset, b"\r\nsix").unwrap();
        let after = frame(&e).history_lines;
        s.scroll_content((after - before) as i32);
        let f = frame(&e);
        let range = s.range(&FrameGrid(&f)).unwrap();
        assert_eq!(text(&e, range), "one\ntwo");
    }

    #[test]
    fn line_selection_in_scrollback_outside_the_viewport_still_extracts() {
        let e = engine(10, 2, b"alpha\r\nbeta\r\ngamma");
        let f = frame(&e);
        let grid = FrameGrid(&f);
        assert_eq!(grid.cell(-1, 0), None, "history is outside the frame");
        let mut s = Selection::new(SelectionKind::Simple, at(-1, 0, Side::Left));
        s.update(at(1, 4, Side::Right));
        assert_eq!(text(&e, s.range(&grid).unwrap()), "alpha\nbeta\ngamma");
    }

    #[test]
    fn content_shift_tracks_output_and_detects_lost_line_identity() {
        let mut e = engine(10, 3, b"a\r\nb\r\nc");
        let before = frame(&e);
        let offset = e.offset();
        e.feed(offset, b"\r\nd\r\ne").unwrap();
        let after = frame(&e);
        assert_eq!(content_shift(&before, &after), Some(2));
        // Local scrolling moves the viewport, not the content.
        e.scroll(1);
        assert_eq!(content_shift(&after, &frame(&e)), Some(0));
        // Entering the alternate screen (vim/less).
        let offset = e.offset();
        e.feed(offset, b"\x1b[?1049h").unwrap();
        let alternate = frame(&e);
        assert_eq!(content_shift(&after, &alternate), None);
        // Leaving it, then clearing scrollback (`clear` sends CSI 3 J).
        let offset = e.offset();
        e.feed(offset, b"\x1b[?1049l").unwrap();
        let back = frame(&e);
        let offset = e.offset();
        e.feed(offset, b"\x1b[3J").unwrap();
        assert_eq!(content_shift(&back, &frame(&e)), None);
        // A resize reflows lines.
        let resized_from = frame(&e);
        e.resize(Size::new(12, 3)).unwrap();
        assert_eq!(content_shift(&resized_from, &frame(&e)), None);
    }
}
