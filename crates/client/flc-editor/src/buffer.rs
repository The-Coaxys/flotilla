//! Window-free text buffer: rope storage, selection, editing, undo/redo and
//! line-ending/indentation conventions.
//!
//! Offsets are UTF-8 byte offsets into the document and always sit on a char
//! boundary, never between the `\r` and `\n` of a CRLF pair. The rope keeps the
//! file's bytes verbatim (mixed line endings survive untouched lines); only
//! text the user inserts is normalized to the document's detected ending.
use std::ops::Range;

use ropey::Rope;
use unicode_segmentation::UnicodeSegmentation;

/// The largest file the daemon reads or writes (`fl-proto` validation).
/// Larger files arrive truncated and open read-only; edits that would grow a
/// document past it cannot be saved.
pub const MAX_FILE_BYTES: usize = fl_proto::validation::MAX_FILE_BYTES;

/// Lines longer than this move the caret by chars instead of graphemes, so
/// a minified multi-megabyte line stays responsive.
const GRAPHEME_LINE_LIMIT: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    Lf,
    Crlf,
}

impl LineEnding {
    /// The majority ending of `text`; LF when it has no line breaks.
    pub fn detect(text: &str) -> Self {
        let lf = text.matches('\n').count();
        let crlf = text.matches("\r\n").count();
        if crlf * 2 > lf { Self::Crlf } else { Self::Lf }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::Crlf => "\r\n",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Lf => "LF",
            Self::Crlf => "CRLF",
        }
    }
}

/// What Tab inserts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Indent {
    Tabs,
    Spaces(u8),
}

impl Indent {
    /// Tabs when most indented lines start with a tab; otherwise the most
    /// common indentation step between consecutive lines (2, 3, 4 or 8
    /// spaces), defaulting to 4.
    pub fn detect(text: &str) -> Self {
        let mut tab_lines = 0usize;
        let mut space_lines = 0usize;
        let mut steps = [0usize; 9];
        let mut previous = 0usize;
        for line in text.lines().take(10_000) {
            let content = line.trim_start_matches([' ', '\t']);
            if content.trim().is_empty() {
                continue;
            }
            if line.starts_with('\t') {
                tab_lines += 1;
                continue;
            }
            let spaces = line.len() - line.trim_start_matches(' ').len();
            if spaces > 0 {
                space_lines += 1;
            }
            let step = spaces.abs_diff(previous);
            if (1..=8).contains(&step) {
                steps[step] += 1;
            }
            previous = spaces;
        }
        if tab_lines > space_lines {
            return Self::Tabs;
        }
        let best = [4usize, 2, 8, 3]
            .into_iter()
            .max_by_key(|&w| (steps[w], usize::from(w == 4)))
            .filter(|&w| steps[w] > 0)
            .unwrap_or(4);
        Self::Spaces(best as u8)
    }

    pub fn unit(self) -> String {
        match self {
            Self::Tabs => "\t".into(),
            Self::Spaces(n) => " ".repeat(n as usize),
        }
    }

    pub fn label(self) -> String {
        match self {
            Self::Tabs => "Tabs".into(),
            Self::Spaces(n) => format!("Spaces: {n}"),
        }
    }
}

/// Why a document refuses edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadOnlyReason {
    /// The caller asked for a viewer.
    Requested,
    /// The daemon sent only the first [`MAX_FILE_BYTES`].
    Truncated,
    /// The file is at or above [`MAX_FILE_BYTES`] and could not be written back.
    TooLarge,
    /// The bytes are not UTF-8; saving the lossy decoding would corrupt them.
    InvalidUtf8,
}

impl ReadOnlyReason {
    pub fn label(self) -> &'static str {
        match self {
            Self::Requested => "Read-only",
            Self::Truncated => "Read-only (truncated)",
            Self::TooLarge => "Read-only (too large)",
            Self::InvalidUtf8 => "Read-only (not UTF-8)",
        }
    }
}

/// A selection; `head` is the caret. Empty when `anchor == head`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Selection {
    pub anchor: usize,
    pub head: usize,
}

impl Selection {
    pub fn caret(offset: usize) -> Self {
        Self {
            anchor: offset,
            head: offset,
        }
    }

    pub fn new(anchor: usize, head: usize) -> Self {
        Self { anchor, head }
    }

    pub fn range(&self) -> Range<usize> {
        self.anchor.min(self.head)..self.anchor.max(self.head)
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    pub fn reversed(&self) -> bool {
        self.head < self.anchor
    }
}

/// A zero-based line and char column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub line: usize,
    pub column: usize,
}

/// One applied edit, in the shape tree-sitter's `InputEdit` needs. Points are
/// `(row, byte column)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextChange {
    pub start_byte: usize,
    pub old_end_byte: usize,
    pub new_end_byte: usize,
    pub start_point: (usize, usize),
    pub old_end_point: (usize, usize),
    pub new_end_point: (usize, usize),
}

/// Caret motions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    Left,
    Right,
    WordLeft,
    WordRight,
    Up,
    Down,
    /// First non-blank char, then column 0 on a second press.
    LineStart,
    LineEnd,
    DocStart,
    DocEnd,
    PageUp(usize),
    PageDown(usize),
}

/// The bytes to write for a save, and the revision they represent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveRequest {
    pub bytes: Vec<u8>,
    pub revision: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveError {
    ReadOnly(ReadOnlyReason),
    /// The document grew past what the daemon accepts.
    TooLarge(usize),
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReadOnly(reason) => f.write_str(reason.label()),
            Self::TooLarge(n) => write!(
                f,
                "{n} bytes exceeds the {} byte file limit",
                MAX_FILE_BYTES
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditKind {
    Insert,
    DeleteBack,
    DeleteForward,
    Other,
}

#[derive(Debug, Clone)]
struct EditRecord {
    start: usize,
    deleted: String,
    inserted: String,
}

#[derive(Debug, Clone)]
struct Transaction {
    id: u64,
    kind: EditKind,
    edits: Vec<EditRecord>,
    before: Selection,
    after: Selection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CharClass {
    Space,
    Newline,
    Word,
    Punct,
}

fn class_of(c: char) -> CharClass {
    if c == '\n' || c == '\r' {
        CharClass::Newline
    } else if c.is_whitespace() {
        CharClass::Space
    } else if c.is_alphanumeric() || c == '_' {
        CharClass::Word
    } else {
        CharClass::Punct
    }
}

pub struct TextBuffer {
    rope: Rope,
    line_ending: LineEnding,
    indent: Indent,
    tab_width: usize,
    selection: Selection,
    /// Display column kept across vertical moves.
    goal_column: Option<usize>,
    undo: Vec<Transaction>,
    redo: Vec<Transaction>,
    next_id: u64,
    saved_id: u64,
    /// Whether the next compatible edit may merge into the top transaction.
    group_open: bool,
    version: u64,
    changes: Vec<TextChange>,
    read_only: Option<ReadOnlyReason>,
}

impl TextBuffer {
    pub fn new(text: &str) -> Self {
        Self {
            rope: Rope::from_str(text),
            line_ending: LineEnding::detect(text),
            indent: Indent::detect(text),
            tab_width: match Indent::detect(text) {
                Indent::Spaces(n) => n as usize,
                Indent::Tabs => 4,
            },
            selection: Selection::default(),
            goal_column: None,
            undo: Vec::new(),
            redo: Vec::new(),
            next_id: 1,
            saved_id: 0,
            group_open: false,
            version: 0,
            changes: Vec::new(),
            read_only: None,
        }
    }

    /// Decode file bytes. Invalid UTF-8 is decoded lossily and the buffer is
    /// made read-only so the replacement chars can never be written back.
    pub fn from_bytes(bytes: &[u8]) -> Self {
        match std::str::from_utf8(bytes) {
            Ok(text) => Self::new(text),
            Err(_) => {
                let mut buffer = Self::new(&String::from_utf8_lossy(bytes));
                buffer.read_only = Some(ReadOnlyReason::InvalidUtf8);
                buffer
            }
        }
    }

    // ---- content -------------------------------------------------------

    pub fn rope(&self) -> &Rope {
        &self.rope
    }

    pub fn text(&self) -> String {
        self.rope.to_string()
    }

    pub fn len(&self) -> usize {
        self.rope.len_bytes()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Lines, counting the empty line after a trailing newline.
    pub fn line_count(&self) -> usize {
        self.rope.len_lines()
    }

    /// Byte range of a line's content, without its terminator.
    pub fn line_range(&self, line: usize) -> Range<usize> {
        let line = line.min(self.line_count() - 1);
        let start = self.rope.line_to_byte(line);
        let mut end = if line + 1 < self.line_count() {
            self.rope.line_to_byte(line + 1) - 1
        } else {
            self.len()
        };
        if end > start && line + 1 < self.line_count() && self.rope.byte(end - 1) == b'\r' {
            end -= 1;
        }
        start..end
    }

    /// A line's content, without its terminator.
    pub fn line(&self, line: usize) -> String {
        self.slice(self.line_range(line))
    }

    pub fn slice(&self, range: Range<usize>) -> String {
        self.rope.byte_slice(range).to_string()
    }

    pub fn line_of(&self, offset: usize) -> usize {
        self.rope.byte_to_line(offset.min(self.len()))
    }

    /// Zero-based line and char column of an offset.
    pub fn position(&self, offset: usize) -> Position {
        let offset = offset.min(self.len());
        let line = self.line_of(offset);
        let start = self.rope.line_to_byte(line);
        let column = self.rope.byte_to_char(offset) - self.rope.byte_to_char(start);
        Position { line, column }
    }

    /// Offset of a zero-based line and char column, clamped to the line.
    pub fn offset(&self, position: Position) -> usize {
        let range = self.line_range(position.line);
        let start_char = self.rope.byte_to_char(range.start);
        let end_char = self.rope.byte_to_char(range.end);
        self.rope
            .char_to_byte((start_char + position.column).min(end_char))
    }

    pub fn line_ending(&self) -> LineEnding {
        self.line_ending
    }

    pub fn indent(&self) -> Indent {
        self.indent
    }

    pub fn tab_width(&self) -> usize {
        self.tab_width
    }

    /// Bumped on every content change.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Edits since the last call, oldest first (for incremental reparsing).
    pub fn take_changes(&mut self) -> Vec<TextChange> {
        std::mem::take(&mut self.changes)
    }

    pub fn read_only(&self) -> Option<ReadOnlyReason> {
        self.read_only
    }

    /// Make the buffer read-only, or editable again. A buffer decoded from
    /// invalid UTF-8 stays read-only.
    pub fn set_read_only(&mut self, reason: Option<ReadOnlyReason>) {
        if self.read_only != Some(ReadOnlyReason::InvalidUtf8) {
            self.read_only = reason;
        }
    }

    // ---- display columns -------------------------------------------------

    /// Display column of `offset` in its line, expanding tabs.
    pub fn display_column(&self, offset: usize) -> usize {
        let range = self.line_range(self.line_of(offset));
        let mut column = 0;
        for c in self
            .rope
            .byte_slice(range.start..offset.clamp(range.start, range.end))
            .chars()
        {
            column = self.advance_column(column, c);
        }
        column
    }

    /// The offset in `line` closest to display column `goal`.
    pub fn offset_at_display_column(&self, line: usize, goal: usize) -> usize {
        let range = self.line_range(line);
        let mut column = 0;
        let mut offset = range.start;
        for c in self.rope.byte_slice(range.clone()).chars() {
            let next = self.advance_column(column, c);
            if next > goal {
                if goal - column > next - goal {
                    offset += c.len_utf8();
                }
                return offset;
            }
            column = next;
            offset += c.len_utf8();
        }
        range.end
    }

    fn advance_column(&self, column: usize, c: char) -> usize {
        if c == '\t' {
            (column / self.tab_width + 1) * self.tab_width
        } else {
            column + 1
        }
    }

    // ---- selection -------------------------------------------------------

    pub fn selection(&self) -> Selection {
        self.selection
    }

    pub fn cursor(&self) -> usize {
        self.selection.head
    }

    pub fn set_selection(&mut self, selection: Selection) {
        self.selection = Selection::new(self.clamp(selection.anchor), self.clamp(selection.head));
        self.goal_column = None;
        self.group_open = false;
    }

    pub fn select_all(&mut self) {
        self.set_selection(Selection::new(0, self.len()));
    }

    pub fn selected_text(&self) -> String {
        self.slice(self.selection.range())
    }

    /// Snap an offset to a char boundary outside any CRLF pair.
    pub fn clamp(&self, offset: usize) -> usize {
        let mut offset = offset.min(self.len());
        while offset > 0 && offset < self.len() && (self.rope.byte(offset) & 0xC0) == 0x80 {
            offset -= 1;
        }
        if offset > 0
            && offset < self.len()
            && self.rope.byte(offset) == b'\n'
            && self.rope.byte(offset - 1) == b'\r'
        {
            offset -= 1;
        }
        offset
    }

    /// Select the word (or run of punctuation/space) around `offset`.
    pub fn select_word_at(&mut self, offset: usize) {
        let range = self.word_range_at(offset);
        self.set_selection(Selection::new(range.start, range.end));
    }

    pub fn word_range_at(&self, offset: usize) -> Range<usize> {
        let offset = self.clamp(offset);
        let line = self.line_range(self.line_of(offset));
        let text = self.slice(line.clone());
        let local = offset - line.start;
        let class = text[local..]
            .chars()
            .next()
            .or_else(|| text[..local].chars().next_back())
            .map(class_of);
        let Some(class) = class else {
            return offset..offset;
        };
        let start = text[..local]
            .char_indices()
            .rev()
            .take_while(|&(_, c)| class_of(c) == class)
            .last()
            .map_or(local, |(i, _)| i);
        let end = text[local..]
            .char_indices()
            .find(|&(_, c)| class_of(c) != class)
            .map_or(text.len(), |(i, _)| local + i);
        line.start + start..line.start + end
    }

    /// Select whole lines `first..=last`, including the last line's terminator.
    pub fn select_lines(&mut self, first: usize, last: usize) {
        let start = self.line_range(first).start;
        let end = if last + 1 < self.line_count() {
            self.rope.line_to_byte(last + 1)
        } else {
            self.len()
        };
        self.set_selection(Selection::new(start, end));
    }

    /// Move the caret; with `extend` the anchor stays put.
    pub fn move_caret(&mut self, motion: Motion, extend: bool) {
        let selection = self.selection;
        let goal = match motion {
            Motion::Up | Motion::Down | Motion::PageUp(_) | Motion::PageDown(_) => Some(
                self.goal_column
                    .unwrap_or_else(|| self.display_column(selection.head)),
            ),
            _ => None,
        };
        let head = if !extend && !selection.is_empty() {
            match motion {
                Motion::Left => selection.range().start,
                Motion::Right => selection.range().end,
                _ => self.motion_target(selection.head, motion, goal),
            }
        } else {
            self.motion_target(selection.head, motion, goal)
        };
        let anchor = if extend { selection.anchor } else { head };
        self.set_selection(Selection::new(anchor, head));
        self.goal_column = goal;
    }

    fn motion_target(&self, from: usize, motion: Motion, goal: Option<usize>) -> usize {
        let line = self.line_of(from);
        match motion {
            Motion::Left => self.prev_boundary(from),
            Motion::Right => self.next_boundary(from),
            Motion::WordLeft => self.word_left(from),
            Motion::WordRight => self.word_right(from),
            Motion::Up if line == 0 => 0,
            Motion::Up => self.offset_at_display_column(line - 1, goal.unwrap_or(0)),
            Motion::Down if line + 1 >= self.line_count() => self.len(),
            Motion::Down => self.offset_at_display_column(line + 1, goal.unwrap_or(0)),
            Motion::PageUp(n) => {
                self.offset_at_display_column(line.saturating_sub(n.max(1)), goal.unwrap_or(0))
            }
            Motion::PageDown(n) => {
                let target = line + n.max(1);
                if target >= self.line_count() {
                    self.len()
                } else {
                    self.offset_at_display_column(target, goal.unwrap_or(0))
                }
            }
            Motion::LineStart => {
                let range = self.line_range(line);
                let text = self.slice(range.clone());
                let first = range.start + (text.len() - text.trim_start_matches([' ', '\t']).len());
                if from == first { range.start } else { first }
            }
            Motion::LineEnd => self.line_range(line).end,
            Motion::DocStart => 0,
            Motion::DocEnd => self.len(),
        }
    }

    /// Previous grapheme boundary; a line break counts as one step.
    pub fn prev_boundary(&self, offset: usize) -> usize {
        let offset = self.clamp(offset);
        let line = self.line_of(offset);
        let range = self.line_range(line);
        if offset <= range.start {
            return if line == 0 {
                0
            } else {
                self.line_range(line - 1).end
            };
        }
        if range.len() > GRAPHEME_LINE_LIMIT {
            return self.clamp(offset - 1);
        }
        let text = self.slice(range.start..offset);
        range.start
            + text
                .grapheme_indices(true)
                .next_back()
                .map_or(0, |(i, _)| i)
    }

    /// Next grapheme boundary; a line break counts as one step.
    pub fn next_boundary(&self, offset: usize) -> usize {
        let offset = self.clamp(offset);
        let line = self.line_of(offset);
        let range = self.line_range(line);
        if offset >= range.end {
            return if line + 1 < self.line_count() {
                self.rope.line_to_byte(line + 1)
            } else {
                self.len()
            };
        }
        if range.len() > GRAPHEME_LINE_LIMIT {
            let c = self.rope.byte_slice(offset..range.end).chars().next();
            return offset + c.map_or(0, char::len_utf8);
        }
        let text = self.slice(offset..range.end);
        offset
            + text
                .grapheme_indices(true)
                .nth(1)
                .map_or(text.len(), |(i, _)| i)
    }

    /// Start of the previous word: skip spaces, then one run of a class.
    /// A line break is its own stop.
    pub fn word_left(&self, offset: usize) -> usize {
        let offset = self.clamp(offset);
        let mut chars = self.rope.chars_at(self.rope.byte_to_char(offset));
        let mut pos = offset;
        let mut class = None;
        while let Some(c) = chars.prev() {
            let k = class_of(c);
            match class {
                None if k == CharClass::Space => {}
                None if k == CharClass::Newline => {
                    pos -= c.len_utf8();
                    if c == '\n'
                        && let Some(p) = chars.prev()
                    {
                        if p == '\r' {
                            pos -= 1;
                        } else {
                            chars.next();
                        }
                    }
                    return pos;
                }
                None => class = Some(k),
                Some(prev) if prev != k => break,
                Some(_) => {}
            }
            pos -= c.len_utf8();
        }
        pos
    }

    /// End of the next word: skip spaces, then one run of a class.
    pub fn word_right(&self, offset: usize) -> usize {
        let offset = self.clamp(offset);
        let mut pos = offset;
        let mut class = None;
        for c in self.rope.chars_at(self.rope.byte_to_char(offset)) {
            let k = class_of(c);
            match class {
                None if k == CharClass::Space => {}
                None if k == CharClass::Newline => {
                    pos += c.len_utf8();
                    if c == '\r' && pos < self.len() && self.rope.byte(pos) == b'\n' {
                        pos += 1;
                    }
                    return pos;
                }
                None => class = Some(k),
                Some(prev) if prev != k => break,
                Some(_) => {}
            }
            pos += c.len_utf8();
        }
        pos
    }

    // ---- editing ---------------------------------------------------------

    /// Normalize `text`'s line breaks to the document's ending.
    pub fn normalize_newlines(&self, text: &str) -> String {
        let unix = text.replace("\r\n", "\n").replace('\r', "\n");
        match self.line_ending {
            LineEnding::Lf => unix,
            LineEnding::Crlf => unix.replace('\n', "\r\n"),
        }
    }

    /// Type `text` over the selection. Consecutive typing groups into one
    /// undo step until the caret moves or a word ends.
    pub fn insert(&mut self, text: &str) -> bool {
        let range = self.selection.range();
        self.replace_range(range, text)
    }

    /// Replace `range` as typing would (IME composition and typed text).
    pub fn replace_range(&mut self, range: Range<usize>, text: &str) -> bool {
        let text = self.normalize_newlines(text);
        let kind = if text.contains('\n') || text.chars().count() > 1 {
            EditKind::Other
        } else {
            EditKind::Insert
        };
        let range = self.clamp(range.start)..self.clamp(range.end);
        // A word boundary ends a typing group.
        if kind == EditKind::Insert
            && text.chars().all(char::is_whitespace)
            && range.start > 0
            && !self
                .slice(self.prev_boundary(range.start)..range.start)
                .chars()
                .all(char::is_whitespace)
        {
            self.group_open = false;
        }
        let end = range.start + text.len();
        self.commit(
            kind,
            vec![EditRecord {
                start: range.start,
                deleted: self.slice(range),
                inserted: text,
            }],
            Selection::caret(end),
        )
    }

    /// Insert pasted text over the selection as its own undo step.
    pub fn paste(&mut self, text: &str) -> bool {
        let text = self.normalize_newlines(text);
        let range = self.selection.range();
        let end = range.start + text.len();
        self.group_open = false;
        let changed = self.commit(
            EditKind::Other,
            vec![EditRecord {
                start: range.start,
                deleted: self.slice(range),
                inserted: text,
            }],
            Selection::caret(end),
        );
        self.group_open = false;
        changed
    }

    /// Delete the selection, or the grapheme before the caret.
    pub fn backspace(&mut self) -> bool {
        let selection = self.selection;
        let range = if selection.is_empty() {
            self.prev_boundary(selection.head)..selection.head
        } else {
            selection.range()
        };
        self.delete(range, EditKind::DeleteBack)
    }

    /// Delete the selection, or the grapheme after the caret.
    pub fn delete_forward(&mut self) -> bool {
        let selection = self.selection;
        let range = if selection.is_empty() {
            selection.head..self.next_boundary(selection.head)
        } else {
            selection.range()
        };
        self.delete(range, EditKind::DeleteForward)
    }

    pub fn delete_word_back(&mut self) -> bool {
        let selection = self.selection;
        let range = if selection.is_empty() {
            self.word_left(selection.head)..selection.head
        } else {
            selection.range()
        };
        self.group_open = false;
        self.delete(range, EditKind::Other)
    }

    pub fn delete_word_forward(&mut self) -> bool {
        let selection = self.selection;
        let range = if selection.is_empty() {
            selection.head..self.word_right(selection.head)
        } else {
            selection.range()
        };
        self.group_open = false;
        self.delete(range, EditKind::Other)
    }

    fn delete(&mut self, range: Range<usize>, kind: EditKind) -> bool {
        if range.is_empty() {
            return false;
        }
        let start = range.start;
        self.commit(
            kind,
            vec![EditRecord {
                start,
                deleted: self.slice(range),
                inserted: String::new(),
            }],
            Selection::caret(start),
        )
    }

    /// Break the line at the caret, keeping the current indentation and
    /// adding one level after an opening bracket.
    pub fn newline(&mut self) -> bool {
        let range = self.selection.range();
        let line = self.line_range(self.line_of(range.start));
        let before = self.slice(line.start..range.start.max(line.start));
        let mut indent: String = before
            .chars()
            .take_while(|&c| c == ' ' || c == '\t')
            .collect();
        if before.trim_end().ends_with(['{', '[', '(']) {
            indent.push_str(&self.indent.unit());
        }
        let text = format!("{}{indent}", self.line_ending.as_str());
        let end = range.start + text.len();
        self.group_open = false;
        let changed = self.commit(
            EditKind::Other,
            vec![EditRecord {
                start: range.start,
                deleted: self.slice(range),
                inserted: text,
            }],
            Selection::caret(end),
        );
        self.group_open = false;
        changed
    }

    /// Tab: indent every selected line when the selection spans lines,
    /// otherwise insert one indent (spaces to the next stop).
    pub fn tab(&mut self) -> bool {
        let range = self.selection.range();
        if self.line_of(range.start) != self.line_of(range.end) {
            return self.indent_lines();
        }
        let text = match self.indent {
            Indent::Tabs => "\t".to_string(),
            Indent::Spaces(n) => {
                let n = n as usize;
                let column = self.display_column(range.start);
                " ".repeat(n - column % n)
            }
        };
        self.group_open = false;
        let end = range.start + text.len();
        let changed = self.commit(
            EditKind::Other,
            vec![EditRecord {
                start: range.start,
                deleted: self.slice(range),
                inserted: text,
            }],
            Selection::caret(end),
        );
        self.group_open = false;
        changed
    }

    fn selected_lines(&self) -> Range<usize> {
        let range = self.selection.range();
        let first = self.line_of(range.start);
        let mut last = self.line_of(range.end);
        if last > first && range.end == self.line_range(last).start {
            last -= 1;
        }
        first..last + 1
    }

    /// Indent every line touched by the selection by one level.
    pub fn indent_lines(&mut self) -> bool {
        let unit = self.indent.unit();
        let edits: Vec<EditRecord> = self
            .selected_lines()
            .rev()
            .filter(|&line| !self.line_range(line).is_empty())
            .map(|line| EditRecord {
                start: self.line_range(line).start,
                deleted: String::new(),
                inserted: unit.clone(),
            })
            .collect();
        self.commit_mapped(edits)
    }

    /// Remove up to one level of indentation from every touched line.
    pub fn outdent_lines(&mut self) -> bool {
        let width = match self.indent {
            Indent::Tabs => self.tab_width,
            Indent::Spaces(n) => n as usize,
        };
        let edits: Vec<EditRecord> = self
            .selected_lines()
            .rev()
            .filter_map(|line| {
                let range = self.line_range(line);
                let text = self.slice(range.clone());
                let remove = if text.starts_with('\t') {
                    1
                } else {
                    text.chars().take(width).take_while(|&c| c == ' ').count()
                };
                (remove > 0).then(|| EditRecord {
                    start: range.start,
                    deleted: text[..remove].to_string(),
                    inserted: String::new(),
                })
            })
            .collect();
        self.commit_mapped(edits)
    }

    /// Commit several edits (applied in order) and carry the selection
    /// through them.
    fn commit_mapped(&mut self, edits: Vec<EditRecord>) -> bool {
        if edits.is_empty() {
            return false;
        }
        let mut selection = self.selection;
        for edit in &edits {
            selection.anchor = map_offset(selection.anchor, edit);
            selection.head = map_offset(selection.head, edit);
        }
        self.group_open = false;
        let changed = self.commit(EditKind::Other, edits, selection);
        self.group_open = false;
        changed
    }

    fn commit(&mut self, kind: EditKind, edits: Vec<EditRecord>, after: Selection) -> bool {
        if self.read_only.is_some() {
            return false;
        }
        let edits: Vec<EditRecord> = edits
            .into_iter()
            .filter(|e| e.deleted != e.inserted)
            .collect();
        if edits.is_empty() {
            return false;
        }
        let before = self.selection;
        for edit in &edits {
            self.apply(edit.start..edit.start + edit.deleted.len(), &edit.inserted);
        }
        self.redo.clear();
        let merge = self.group_open
            && kind != EditKind::Other
            && self.undo.last().is_some_and(|top| {
                top.kind == kind && edits.len() == 1 && {
                    let last = top.edits.last().expect("transactions are never empty");
                    let edit = &edits[0];
                    match kind {
                        EditKind::Insert => {
                            edit.start == last.start + last.inserted.len()
                                && edit.deleted.is_empty()
                        }
                        EditKind::DeleteBack => edit.start + edit.deleted.len() == last.start,
                        EditKind::DeleteForward => edit.start == last.start,
                        EditKind::Other => false,
                    }
                }
            });
        if merge {
            let top = self.undo.last_mut().expect("checked above");
            top.edits.extend(edits);
            top.after = after;
        } else {
            let id = self.next_id;
            self.next_id += 1;
            self.undo.push(Transaction {
                id,
                kind,
                edits,
                before,
                after,
            });
        }
        self.group_open = kind != EditKind::Other;
        self.selection = Selection::new(self.clamp(after.anchor), self.clamp(after.head));
        self.goal_column = None;
        true
    }

    /// The single primitive that changes the rope.
    fn apply(&mut self, range: Range<usize>, text: &str) {
        let point = |rope: &Rope, byte: usize| {
            let row = rope.byte_to_line(byte);
            (row, byte - rope.line_to_byte(row))
        };
        let start_point = point(&self.rope, range.start);
        let old_end_point = point(&self.rope, range.end);
        let start_char = self.rope.byte_to_char(range.start);
        let end_char = self.rope.byte_to_char(range.end);
        self.rope.remove(start_char..end_char);
        self.rope.insert(start_char, text);
        let new_end = range.start + text.len();
        self.changes.push(TextChange {
            start_byte: range.start,
            old_end_byte: range.end,
            new_end_byte: new_end,
            start_point,
            old_end_point,
            new_end_point: point(&self.rope, new_end),
        });
        self.version += 1;
    }

    // ---- history ---------------------------------------------------------

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn undo(&mut self) -> bool {
        if self.read_only.is_some() {
            return false;
        }
        let Some(transaction) = self.undo.pop() else {
            return false;
        };
        for edit in transaction.edits.iter().rev() {
            self.apply(edit.start..edit.start + edit.inserted.len(), &edit.deleted);
        }
        self.selection = transaction.before;
        self.redo.push(transaction);
        self.group_open = false;
        self.goal_column = None;
        true
    }

    pub fn redo(&mut self) -> bool {
        if self.read_only.is_some() {
            return false;
        }
        let Some(transaction) = self.redo.pop() else {
            return false;
        };
        for edit in &transaction.edits {
            self.apply(edit.start..edit.start + edit.deleted.len(), &edit.inserted);
        }
        self.selection = transaction.after;
        self.undo.push(transaction);
        self.group_open = false;
        self.goal_column = None;
        true
    }

    // ---- saving ----------------------------------------------------------

    /// Identifies the current content state in the undo history.
    pub fn revision(&self) -> u64 {
        self.undo.last().map_or(0, |t| t.id)
    }

    pub fn is_dirty(&self) -> bool {
        self.revision() != self.saved_id
    }

    /// The bytes to write. The buffer stays dirty until [`Self::mark_saved`]
    /// confirms the write of this revision.
    pub fn save(&mut self) -> Result<SaveRequest, SaveError> {
        if let Some(reason) = self.read_only {
            return Err(SaveError::ReadOnly(reason));
        }
        if self.len() > MAX_FILE_BYTES {
            return Err(SaveError::TooLarge(self.len()));
        }
        self.group_open = false;
        let mut bytes = Vec::with_capacity(self.len());
        for chunk in self.rope.chunks() {
            bytes.extend_from_slice(chunk.as_bytes());
        }
        Ok(SaveRequest {
            bytes,
            revision: self.revision(),
        })
    }

    /// The write of `revision` succeeded.
    pub fn mark_saved(&mut self, revision: u64) {
        self.saved_id = revision;
        self.group_open = false;
    }
}

/// Where `offset` lands after `edit`; an offset at an insertion point moves
/// past the inserted text.
fn map_offset(offset: usize, edit: &EditRecord) -> usize {
    let end = edit.start + edit.deleted.len();
    if offset >= end {
        offset - edit.deleted.len() + edit.inserted.len()
    } else if offset >= edit.start {
        edit.start + edit.inserted.len().min(offset - edit.start)
    } else {
        offset
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(buffer: &mut TextBuffer, text: &str) {
        for c in text.chars() {
            assert!(buffer.insert(&c.to_string()));
        }
    }

    #[test]
    fn line_ending_detection_and_line_ranges() {
        assert_eq!(LineEnding::detect("a\nb\n"), LineEnding::Lf);
        assert_eq!(LineEnding::detect("a\r\nb\r\nc\n"), LineEnding::Crlf);
        assert_eq!(LineEnding::detect("no breaks"), LineEnding::Lf);

        let buffer = TextBuffer::new("one\r\ntwo\r\n\r\nlast");
        assert_eq!(buffer.line_ending(), LineEnding::Crlf);
        assert_eq!(buffer.line_count(), 4);
        assert_eq!(buffer.line(0), "one");
        assert_eq!(buffer.line(1), "two");
        assert_eq!(buffer.line(2), "");
        assert_eq!(buffer.line(3), "last");

        let trailing = TextBuffer::new("x\n");
        assert_eq!(trailing.line_count(), 2);
        assert_eq!(trailing.line(1), "");
    }

    #[test]
    fn crlf_is_preserved_and_new_lines_use_the_document_ending() {
        let mut buffer = TextBuffer::new("fn a() {\r\n}\r\nlegacy\nline\r\n");
        buffer.set_selection(Selection::caret(buffer.line_range(0).end));
        assert!(buffer.newline());
        typed(&mut buffer, "x");
        // Pasted LF text is normalized; the lone-LF line elsewhere survives.
        buffer.set_selection(Selection::caret(buffer.len()));
        assert!(buffer.paste("p1\np2\r\n"));
        let saved = buffer.save().unwrap();
        assert_eq!(
            String::from_utf8(saved.bytes).unwrap(),
            "fn a() {\r\n    x\r\n}\r\nlegacy\nline\r\np1\r\np2\r\n"
        );
    }

    #[test]
    fn caret_never_splits_crlf_or_multibyte_chars() {
        let buffer = TextBuffer::new("é\r\nb");
        assert_eq!(buffer.clamp(1), 0);
        assert_eq!(buffer.clamp(3), 2);
        assert_eq!(buffer.next_boundary(0), 2);
        assert_eq!(buffer.next_boundary(2), 4);
        assert_eq!(buffer.prev_boundary(4), 2);
        assert_eq!(buffer.prev_boundary(2), 0);
        // A combining sequence is one grapheme.
        let combined = TextBuffer::new("e\u{301}x");
        assert_eq!(combined.next_boundary(0), 3);
        assert_eq!(combined.prev_boundary(3), 0);
    }

    #[test]
    fn insert_delete_and_replace_selection() {
        let mut buffer = TextBuffer::new("hello world");
        buffer.set_selection(Selection::new(6, 11));
        assert_eq!(buffer.selected_text(), "world");
        typed(&mut buffer, "rope");
        assert_eq!(buffer.text(), "hello rope");
        assert!(buffer.backspace());
        assert_eq!(buffer.text(), "hello rop");
        buffer.set_selection(Selection::caret(0));
        assert!(buffer.delete_forward());
        assert_eq!(buffer.text(), "ello rop");
        assert!(!TextBuffer::new("").backspace());
        buffer.set_selection(Selection::caret(buffer.len()));
        assert!(buffer.delete_word_back());
        assert_eq!(buffer.text(), "ello ");
        buffer.set_selection(Selection::caret(0));
        assert!(buffer.delete_word_forward());
        assert_eq!(buffer.text(), " ");
    }

    #[test]
    fn multi_line_selection_replaced_as_one_edit() {
        let mut buffer = TextBuffer::new("alpha\nbeta\ngamma\n");
        buffer.set_selection(Selection::new(13, 2));
        assert!(buffer.selection().reversed());
        assert_eq!(buffer.selected_text(), "pha\nbeta\nga");
        buffer.paste("X");
        assert_eq!(buffer.text(), "alXmma\n");
        assert!(buffer.undo());
        assert_eq!(buffer.text(), "alpha\nbeta\ngamma\n");
        assert_eq!(buffer.selection(), Selection::new(13, 2));
    }

    #[test]
    fn motions_cover_words_lines_and_document() {
        let mut buffer = TextBuffer::new("    let x_1 = foo(bar);\nnext line");
        buffer.set_selection(Selection::caret(4));
        buffer.move_caret(Motion::WordRight, false);
        assert_eq!(buffer.cursor(), 7);
        buffer.move_caret(Motion::WordRight, false);
        assert_eq!(buffer.cursor(), 11);
        buffer.move_caret(Motion::WordRight, false);
        assert_eq!(buffer.cursor(), 13);
        buffer.move_caret(Motion::WordLeft, false);
        assert_eq!(buffer.cursor(), 12);
        buffer.move_caret(Motion::WordLeft, false);
        assert_eq!(buffer.cursor(), 8);
        buffer.move_caret(Motion::LineEnd, false);
        assert_eq!(buffer.cursor(), 23);
        // A line break is its own word stop.
        buffer.move_caret(Motion::WordRight, false);
        assert_eq!(buffer.cursor(), 24);
        buffer.move_caret(Motion::WordLeft, false);
        assert_eq!(buffer.cursor(), 23);
        buffer.move_caret(Motion::LineStart, false);
        assert_eq!(buffer.cursor(), 4, "smart home: first non-blank");
        buffer.move_caret(Motion::LineStart, false);
        assert_eq!(buffer.cursor(), 0, "second press: column 0");
        buffer.move_caret(Motion::DocEnd, true);
        assert_eq!(buffer.selection(), Selection::new(0, buffer.len()));
        buffer.move_caret(Motion::Left, false);
        assert_eq!(buffer.cursor(), 0, "Left collapses to the selection start");
        buffer.move_caret(Motion::Right, true);
        buffer.move_caret(Motion::Right, true);
        assert_eq!(buffer.selected_text(), "  ");
    }

    #[test]
    fn vertical_motion_keeps_goal_column_across_short_lines_and_tabs() {
        let mut buffer = TextBuffer::new("abcdefgh\nab\n\tcdefgh\nxy");
        buffer.set_selection(Selection::caret(6));
        buffer.move_caret(Motion::Down, false);
        assert_eq!(
            buffer.position(buffer.cursor()),
            Position { line: 1, column: 2 }
        );
        buffer.move_caret(Motion::Down, false);
        // Tab expands to 4 columns: display column 6 is after "\tcd".
        assert_eq!(
            buffer.position(buffer.cursor()),
            Position { line: 2, column: 3 }
        );
        buffer.move_caret(Motion::Up, false);
        buffer.move_caret(Motion::Up, false);
        assert_eq!(buffer.cursor(), 6);
        buffer.move_caret(Motion::PageDown(10), false);
        assert_eq!(buffer.cursor(), buffer.len());
        buffer.move_caret(Motion::PageUp(10), false);
        assert_eq!(buffer.position(buffer.cursor()).line, 0);
    }

    #[test]
    fn word_and_line_selection() {
        let mut buffer = TextBuffer::new("foo_bar  baz\nsecond\n");
        buffer.select_word_at(2);
        assert_eq!(buffer.selected_text(), "foo_bar");
        buffer.select_word_at(8);
        assert_eq!(buffer.selected_text(), "  ");
        buffer.select_lines(0, 0);
        assert_eq!(buffer.selected_text(), "foo_bar  baz\n");
        buffer.select_lines(1, 2);
        assert_eq!(buffer.selected_text(), "second\n");
    }

    #[test]
    fn typing_groups_by_word_and_undo_redo_restore_selection() {
        let mut buffer = TextBuffer::new("");
        typed(&mut buffer, "hello world");
        assert_eq!(buffer.text(), "hello world");
        assert!(buffer.undo());
        assert_eq!(buffer.text(), "hello");
        assert!(buffer.undo());
        assert_eq!(buffer.text(), "");
        assert!(!buffer.undo());
        assert!(buffer.redo());
        assert_eq!(buffer.text(), "hello");
        assert_eq!(buffer.cursor(), 5);
        assert!(buffer.redo());
        assert_eq!(buffer.text(), "hello world");
        assert!(!buffer.redo());

        // Moving the caret ends a group.
        typed(&mut buffer, "ab");
        buffer.move_caret(Motion::Left, false);
        buffer.move_caret(Motion::Right, false);
        typed(&mut buffer, "cd");
        buffer.undo();
        assert_eq!(buffer.text(), "hello worldab");

        // Backspaces group; a new edit clears redo.
        let mut buffer = TextBuffer::new("abcdef");
        buffer.set_selection(Selection::caret(6));
        for _ in 0..3 {
            buffer.backspace();
        }
        assert_eq!(buffer.text(), "abc");
        buffer.undo();
        assert_eq!(buffer.text(), "abcdef");
        buffer.redo();
        buffer.undo();
        buffer.insert("Z");
        assert!(!buffer.can_redo());
        assert_eq!(buffer.text(), "abcdefZ");
    }

    #[test]
    fn dirty_tracking_follows_the_saved_revision_through_undo() {
        let mut buffer = TextBuffer::new("a");
        assert!(!buffer.is_dirty());
        buffer.set_selection(Selection::caret(1));
        typed(&mut buffer, "b");
        assert!(buffer.is_dirty());
        let save = buffer.save().unwrap();
        assert_eq!(save.bytes, b"ab");
        assert!(buffer.is_dirty(), "dirty until the write is confirmed");
        buffer.mark_saved(save.revision);
        assert!(!buffer.is_dirty());
        // Typing after a save never merges into the saved transaction.
        typed(&mut buffer, "c");
        assert!(buffer.is_dirty());
        buffer.undo();
        assert!(!buffer.is_dirty());
        assert_eq!(buffer.text(), "ab");
        buffer.undo();
        assert!(buffer.is_dirty());
        buffer.redo();
        assert!(!buffer.is_dirty());
        // A save confirmed for an older revision leaves later edits dirty.
        let stale = buffer.save().unwrap();
        typed(&mut buffer, "d");
        buffer.mark_saved(stale.revision);
        assert!(buffer.is_dirty());
    }

    #[test]
    fn indentation_detection_and_tab_handling() {
        assert_eq!(
            Indent::detect("fn a() {\n  b();\n  if c {\n    d();\n  }\n}\n"),
            Indent::Spaces(2)
        );
        assert_eq!(
            Indent::detect("def a():\n    b()\n    if c:\n        d()\n"),
            Indent::Spaces(4)
        );
        assert_eq!(
            Indent::detect("func a() {\n\tb()\n\tif c {\n\t\td()\n\t}\n}\n"),
            Indent::Tabs
        );
        assert_eq!(Indent::detect("plain\ntext\n"), Indent::Spaces(4));

        let mut buffer = TextBuffer::new("a {\n  b\n}\n");
        buffer.set_selection(Selection::caret(1));
        assert!(buffer.tab());
        assert_eq!(buffer.line(0), "a  {", "spaces to the next 2-column stop");
        buffer.set_selection(Selection::caret(buffer.line_range(1).end));
        assert!(buffer.newline());
        assert_eq!(buffer.line(2), "  ", "newline keeps indentation");

        let mut tabs = TextBuffer::new("x\n\ty\n\tz\n");
        tabs.set_selection(Selection::caret(0));
        tabs.tab();
        assert_eq!(tabs.line(0), "\tx");
    }

    #[test]
    fn indent_and_outdent_selected_lines_as_one_undo_step() {
        let mut buffer = TextBuffer::new("one\ntwo\n\nthree\n");
        buffer.set_selection(Selection::new(1, buffer.line_range(3).start + 2));
        assert!(buffer.tab());
        assert_eq!(buffer.text(), "    one\n    two\n\n    three\n");
        assert_eq!(buffer.selected_text(), "ne\n    two\n\n    th");
        assert!(buffer.outdent_lines());
        assert_eq!(buffer.text(), "one\ntwo\n\nthree\n");
        buffer.undo();
        buffer.undo();
        assert_eq!(buffer.text(), "one\ntwo\n\nthree\n");
        assert!(!buffer.can_undo());
        // A selection ending at column 0 does not indent that line.
        buffer.set_selection(Selection::new(0, buffer.line_range(1).start));
        buffer.tab();
        assert_eq!(buffer.text(), "    one\ntwo\n\nthree\n");
    }

    #[test]
    fn newline_after_opening_bracket_adds_a_level() {
        let mut buffer = TextBuffer::new("fn main() {\n    x\n}\n");
        buffer.set_selection(Selection::caret(11));
        buffer.newline();
        assert_eq!(buffer.line(1), "    ");
        assert_eq!(
            buffer.position(buffer.cursor()),
            Position { line: 1, column: 4 }
        );
    }

    #[test]
    fn read_only_buffers_refuse_edits_and_saves() {
        let mut buffer = TextBuffer::new("x");
        buffer.set_read_only(Some(ReadOnlyReason::Truncated));
        assert!(!buffer.insert("y"));
        assert!(!buffer.paste("y"));
        assert!(!buffer.backspace());
        assert_eq!(
            buffer.save(),
            Err(SaveError::ReadOnly(ReadOnlyReason::Truncated))
        );
        buffer.set_read_only(None);
        assert!(buffer.insert("y"));

        let mut invalid = TextBuffer::from_bytes(b"ok\xffbad");
        assert_eq!(invalid.read_only(), Some(ReadOnlyReason::InvalidUtf8));
        invalid.set_read_only(None);
        assert_eq!(invalid.read_only(), Some(ReadOnlyReason::InvalidUtf8));
        assert!(invalid.text().contains('\u{FFFD}'));
    }

    #[test]
    fn oversized_documents_cannot_be_saved() {
        let mut buffer = TextBuffer::new(&"x".repeat(MAX_FILE_BYTES));
        assert!(buffer.save().is_ok());
        buffer.set_selection(Selection::caret(0));
        buffer.insert("y");
        assert_eq!(buffer.save(), Err(SaveError::TooLarge(MAX_FILE_BYTES + 1)));
    }

    #[test]
    fn changes_describe_edits_for_incremental_parsing() {
        let mut buffer = TextBuffer::new("ab\ncd\n");
        buffer.set_selection(Selection::new(1, 4));
        buffer.insert("XY");
        assert_eq!(
            buffer.take_changes(),
            vec![TextChange {
                start_byte: 1,
                old_end_byte: 4,
                new_end_byte: 3,
                start_point: (0, 1),
                old_end_point: (1, 1),
                new_end_point: (0, 3),
            }]
        );
        assert_eq!(buffer.text(), "aXYd\n");
        buffer.undo();
        let undo = buffer.take_changes();
        assert_eq!(undo.len(), 1);
        assert_eq!(
            (
                undo[0].start_byte,
                undo[0].old_end_byte,
                undo[0].new_end_byte
            ),
            (1, 3, 4)
        );
        assert!(buffer.take_changes().is_empty());
        assert_eq!(buffer.version(), 2);
    }

    #[test]
    fn display_columns_expand_tabs() {
        let buffer = TextBuffer::new("\tab\tc");
        assert_eq!(buffer.display_column(1), 4);
        assert_eq!(buffer.display_column(4), 8);
        assert_eq!(buffer.offset_at_display_column(0, 5), 2);
        assert_eq!(buffer.offset_at_display_column(0, 100), 5);
        assert_eq!(buffer.offset(Position { line: 0, column: 3 }), 3);
    }
}
