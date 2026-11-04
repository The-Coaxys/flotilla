//! Line diffs for the Changes view: hunks with context, collapsible
//! unchanged regions, unified and side-by-side rows, and word-level
//! highlights for modified line pairs.
use std::collections::HashSet;
use std::ops::Range;

use fl_model::workspace::{FileContent, FileDiff};
use imara_diff::{Algorithm, Diff, InternedInput};

/// Unchanged lines shown around each change.
pub const DEFAULT_CONTEXT: usize = 3;
/// Above this many bytes per side the diff is not computed.
pub const MAX_DIFF_BYTES: usize = 4 * 1024 * 1024;
/// Lines longer than this get no word-level highlights.
const MAX_INTRALINE_BYTES: usize = 2000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    /// 0-based line numbers on each side.
    pub old: Option<usize>,
    pub new: Option<usize>,
    /// The line without its terminator.
    pub text: String,
    /// The last line of its side has no trailing newline.
    pub no_newline: bool,
    /// Byte ranges of `text` that differ from the paired line.
    pub changed: Vec<Range<usize>>,
}

/// The aligned whole-file diff of two texts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextDiff {
    lines: Vec<DiffLine>,
    pub added: usize,
    pub removed: usize,
}

/// What a file comparison can show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffContent {
    Text(TextDiff),
    /// Either side is binary.
    Binary,
    /// A side was truncated by the daemon or exceeds [`MAX_DIFF_BYTES`].
    TooLarge,
    /// Neither side exists (the file was created and removed again).
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// Hidden unchanged lines; `gap` identifies it for [`Rows::expand`].
    Gap {
        gap: usize,
        hidden: usize,
    },
    Unified(DiffLine),
    /// Side by side: removed/added runs are paired line by line.
    Split {
        old: Option<DiffLine>,
        new: Option<DiffLine>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    Unified,
    SideBySide,
}

/// Split into lines, keeping each terminator on its line.
fn split_lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

fn strip_terminator(line: &str) -> (&str, bool) {
    if let Some(body) = line.strip_suffix("\r\n") {
        (body, true)
    } else if let Some(body) = line.strip_suffix('\n') {
        (body, true)
    } else {
        (line, false)
    }
}

impl TextDiff {
    pub fn new(old: &str, new: &str) -> Self {
        let input = InternedInput::new(old, new);
        let mut diff = Diff::compute(Algorithm::Histogram, &input);
        diff.postprocess_lines(&input);
        let old_lines = split_lines(old);
        let new_lines = split_lines(new);
        let line = |kind, old_index: Option<usize>, new_index: Option<usize>, raw: &str| {
            let (text, terminated) = strip_terminator(raw);
            DiffLine {
                kind,
                old: old_index,
                new: new_index,
                text: text.to_owned(),
                no_newline: !terminated,
                changed: Vec::new(),
            }
        };
        let mut lines = Vec::with_capacity(old_lines.len().max(new_lines.len()));
        let (mut o, mut n) = (0usize, 0usize);
        let (mut added, mut removed) = (0, 0);
        for hunk in diff.hunks() {
            let (before, after) = (
                hunk.before.start as usize..hunk.before.end as usize,
                hunk.after.start as usize..hunk.after.end as usize,
            );
            while o < before.start {
                lines.push(line(LineKind::Context, Some(o), Some(n), old_lines[o]));
                o += 1;
                n += 1;
            }
            let first_removed = lines.len();
            for index in before.clone() {
                lines.push(line(LineKind::Removed, Some(index), None, old_lines[index]));
            }
            let first_added = lines.len();
            for index in after.clone() {
                lines.push(line(LineKind::Added, None, Some(index), new_lines[index]));
            }
            removed += before.len();
            added += after.len();
            // Word-level highlights for modified pairs.
            for pair in 0..before.len().min(after.len()) {
                let (left, right) = (first_removed + pair, first_added + pair);
                let (old_changed, new_changed) = intraline(&lines[left].text, &lines[right].text);
                lines[left].changed = old_changed;
                lines[right].changed = new_changed;
            }
            o = before.end;
            n = after.end;
        }
        while o < old_lines.len() {
            lines.push(line(LineKind::Context, Some(o), Some(n), old_lines[o]));
            o += 1;
            n += 1;
        }
        Self {
            lines,
            added,
            removed,
        }
    }

    pub fn lines(&self) -> &[DiffLine] {
        &self.lines
    }

    pub fn is_empty(&self) -> bool {
        self.added == 0 && self.removed == 0
    }

    /// Rows with `context` unchanged lines around changes; other unchanged
    /// runs collapse into gaps unless listed in `expanded`.
    pub fn rows(&self, layout: Layout, context: usize, expanded: &HashSet<usize>) -> Vec<Row> {
        let changed: Vec<usize> = self
            .lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line.kind != LineKind::Context)
            .map(|(index, _)| index)
            .collect();
        // Visible: within `context` of a change (by index distance).
        let mut visible = vec![changed.is_empty(); self.lines.len()];
        for &index in &changed {
            let start = index.saturating_sub(context);
            let end = (index + context + 1).min(self.lines.len());
            for flag in &mut visible[start..end] {
                *flag = true;
            }
        }
        let mut rows = Vec::new();
        let mut gap = 0;
        let mut index = 0;
        while index < self.lines.len() {
            if visible[index] {
                let end = (index..self.lines.len())
                    .find(|&i| !visible[i])
                    .unwrap_or(self.lines.len());
                self.push_rows(layout, index..end, &mut rows);
                index = end;
            } else {
                let end = (index..self.lines.len())
                    .find(|&i| visible[i])
                    .unwrap_or(self.lines.len());
                if expanded.contains(&gap) {
                    self.push_rows(layout, index..end, &mut rows);
                } else {
                    rows.push(Row::Gap {
                        gap,
                        hidden: end - index,
                    });
                }
                gap += 1;
                index = end;
            }
        }
        rows
    }

    fn push_rows(&self, layout: Layout, range: Range<usize>, rows: &mut Vec<Row>) {
        let lines = &self.lines[range];
        match layout {
            Layout::Unified => rows.extend(lines.iter().cloned().map(Row::Unified)),
            Layout::SideBySide => {
                let mut index = 0;
                while index < lines.len() {
                    if lines[index].kind == LineKind::Context {
                        rows.push(Row::Split {
                            old: Some(lines[index].clone()),
                            new: Some(lines[index].clone()),
                        });
                        index += 1;
                        continue;
                    }
                    let removed_end = (index..lines.len())
                        .find(|&i| lines[i].kind != LineKind::Removed)
                        .unwrap_or(lines.len());
                    let added_end = (removed_end..lines.len())
                        .find(|&i| lines[i].kind != LineKind::Added)
                        .unwrap_or(lines.len());
                    let removed = &lines[index..removed_end];
                    let added = &lines[removed_end..added_end];
                    for pair in 0..removed.len().max(added.len()) {
                        rows.push(Row::Split {
                            old: removed.get(pair).cloned(),
                            new: added.get(pair).cloned(),
                        });
                    }
                    index = added_end;
                }
            }
        }
    }
}

/// Word-level token boundaries: runs of word characters, runs of
/// whitespace, and single other characters.
fn tokens(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let class = |c: char| {
        if c.is_alphanumeric() || c == '_' {
            0
        } else if c.is_whitespace() {
            1
        } else {
            2
        }
    };
    let mut previous: Option<u8> = None;
    for (index, c) in text.char_indices() {
        let k = class(c);
        if let Some(p) = previous
            && (p != k || k == 2)
        {
            out.push(&text[start..index]);
            start = index;
        }
        previous = Some(k);
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

/// Byte ranges that differ between a removed and an added line.
fn intraline(old: &str, new: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    if old.len() > MAX_INTRALINE_BYTES || new.len() > MAX_INTRALINE_BYTES {
        return (Vec::new(), Vec::new());
    }
    let (old_tokens, new_tokens) = (tokens(old), tokens(new));
    let mut input = InternedInput::default();
    input.update_before(old_tokens.iter().copied());
    input.update_after(new_tokens.iter().copied());
    let diff = Diff::compute(Algorithm::Histogram, &input);
    let offsets = |tokens: &[&str]| {
        let mut at = 0;
        let mut out = Vec::with_capacity(tokens.len() + 1);
        for token in tokens {
            out.push(at);
            at += token.len();
        }
        out.push(at);
        out
    };
    let (old_at, new_at) = (offsets(&old_tokens), offsets(&new_tokens));
    let mut old_ranges = Vec::new();
    let mut new_ranges = Vec::new();
    for hunk in diff.hunks() {
        if !hunk.before.is_empty() {
            old_ranges.push(old_at[hunk.before.start as usize]..old_at[hunk.before.end as usize]);
        }
        if !hunk.after.is_empty() {
            new_ranges.push(new_at[hunk.after.start as usize]..new_at[hunk.after.end as usize]);
        }
    }
    // A line that changed entirely is shown as plain added/removed.
    let whole = |ranges: &[Range<usize>], len: usize| ranges.len() == 1 && ranges[0] == (0..len);
    if whole(&old_ranges, old.len()) || whole(&new_ranges, new.len()) {
        return (Vec::new(), Vec::new());
    }
    (old_ranges, new_ranges)
}

fn side_text(side: &FileContent) -> Result<&str, DiffContent> {
    if side.binary {
        return Err(DiffContent::Binary);
    }
    if side.truncated || side.bytes.len() > MAX_DIFF_BYTES {
        return Err(DiffContent::TooLarge);
    }
    std::str::from_utf8(&side.bytes).map_err(|_| DiffContent::Binary)
}

impl DiffContent {
    /// Compare the daemon's two sides of a changed file. A missing side is
    /// empty: an added file is all additions, a deleted one all removals.
    pub fn from_file_diff(diff: &FileDiff) -> Self {
        if diff.old.is_none() && diff.new.is_none() {
            return Self::Missing;
        }
        fn side(side: &Option<FileContent>) -> Result<&str, DiffContent> {
            match side {
                Some(content) => side_text(content),
                None => Ok(""),
            }
        }
        match (side(&diff.old), side(&diff.new)) {
            (Ok(old), Ok(new)) => Self::Text(TextDiff::new(old, new)),
            (Err(Self::Binary), _) | (_, Err(Self::Binary)) => Self::Binary,
            (Err(other), _) | (_, Err(other)) => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(diff: &TextDiff) -> Vec<(LineKind, Option<usize>, Option<usize>, &str)> {
        diff.lines()
            .iter()
            .map(|l| (l.kind, l.old, l.new, l.text.as_str()))
            .collect()
    }

    use LineKind::{Added as A, Context as C, Removed as R};

    #[test]
    fn modification_insertion_and_deletion_align_line_numbers() {
        let diff = TextDiff::new("a\nb\nc\nd\n", "a\nB\nc\nx\nd\n");
        assert_eq!(
            kinds(&diff),
            vec![
                (C, Some(0), Some(0), "a"),
                (R, Some(1), None, "b"),
                (A, None, Some(1), "B"),
                (C, Some(2), Some(2), "c"),
                (A, None, Some(3), "x"),
                (C, Some(3), Some(4), "d"),
            ]
        );
        assert_eq!((diff.added, diff.removed), (2, 1));
        let deleted = TextDiff::new("a\nb\nc\n", "a\nc\n");
        assert_eq!(
            kinds(&deleted),
            vec![
                (C, Some(0), Some(0), "a"),
                (R, Some(1), None, "b"),
                (C, Some(2), Some(1), "c"),
            ]
        );
    }

    #[test]
    fn moved_blocks_show_as_removal_and_insertion() {
        let diff = TextDiff::new("one\ntwo\nthree\n", "three\none\ntwo\n");
        assert_eq!((diff.added, diff.removed), (1, 1));
        let changed: Vec<(LineKind, &str)> = diff
            .lines()
            .iter()
            .filter(|l| l.kind != C)
            .map(|l| (l.kind, l.text.as_str()))
            .collect();
        assert!(changed.contains(&(A, "three")) && changed.contains(&(R, "three")));
    }

    #[test]
    fn line_endings_and_missing_final_newline_are_changes() {
        let crlf = TextDiff::new("a\r\nb\r\n", "a\nb\r\n");
        assert_eq!(
            kinds(&crlf),
            vec![
                (R, Some(0), None, "a"),
                (A, None, Some(0), "a"),
                (C, Some(1), Some(1), "b")
            ]
        );
        let eol = TextDiff::new("a\nb\n", "a\nb");
        let last = eol.lines().last().unwrap();
        assert_eq!((last.kind, last.no_newline), (A, true));
        assert!(eol.lines().iter().any(|l| l.kind == R && !l.no_newline));
        assert!(TextDiff::new("same\n", "same\n").is_empty());
    }

    #[test]
    fn empty_sides_are_whole_file_additions_or_removals() {
        let added = TextDiff::new("", "x\ny\n");
        assert_eq!(
            kinds(&added),
            vec![(A, None, Some(0), "x"), (A, None, Some(1), "y")]
        );
        let removed = TextDiff::new("x\n", "");
        assert_eq!(kinds(&removed), vec![(R, Some(0), None, "x")]);
        assert!(TextDiff::new("", "").lines().is_empty());
    }

    #[test]
    fn word_level_highlights_mark_only_the_changed_words() {
        let diff = TextDiff::new("let total = a + b;\n", "let sum = a + b;\n");
        let old = &diff.lines()[0];
        let new = &diff.lines()[1];
        assert_eq!(&old.text[old.changed[0].clone()], "total");
        assert_eq!(&new.text[new.changed[0].clone()], "sum");
        assert_eq!((old.changed.len(), new.changed.len()), (1, 1));
        // A completely different line gets no word highlights.
        let replaced = TextDiff::new("alpha\n", "omega\n");
        assert!(replaced.lines().iter().all(|l| l.changed.is_empty()));
    }

    fn numbered(count: usize) -> String {
        (0..count).map(|i| format!("line {i}\n")).collect()
    }

    #[test]
    fn unchanged_runs_collapse_into_expandable_gaps() {
        let old = numbered(30);
        let new = old.replace("line 15\n", "LINE 15\n");
        let diff = TextDiff::new(&old, &new);
        let rows = diff.rows(Layout::Unified, 3, &HashSet::new());
        assert_eq!(rows.first(), Some(&Row::Gap { gap: 0, hidden: 12 }));
        assert_eq!(rows.last(), Some(&Row::Gap { gap: 1, hidden: 11 }));
        // 3 context + removed + added + 3 context.
        assert_eq!(rows.len(), 2 + 8);
        let expanded = diff.rows(Layout::Unified, 3, &HashSet::from([0]));
        assert!(matches!(expanded.first(), Some(Row::Unified(l)) if l.text == "line 0"));
        assert_eq!(expanded.len(), 12 + 8 + 1);
        // No changes: everything is shown, nothing collapses.
        let same = TextDiff::new(&old, &old);
        assert_eq!(same.rows(Layout::Unified, 3, &HashSet::new()).len(), 30);
    }

    #[test]
    fn side_by_side_pairs_removed_and_added_runs() {
        let diff = TextDiff::new("a\nb\nc\nz\n", "a\nB\nC\nD\nz\n");
        let rows = diff.rows(Layout::SideBySide, 3, &HashSet::new());
        let pairs: Vec<(Option<&str>, Option<&str>)> = rows
            .iter()
            .map(|row| match row {
                Row::Split { old, new } => (
                    old.as_ref().map(|l| l.text.as_str()),
                    new.as_ref().map(|l| l.text.as_str()),
                ),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(
            pairs,
            vec![
                (Some("a"), Some("a")),
                (Some("b"), Some("B")),
                (Some("c"), Some("C")),
                (None, Some("D")),
                (Some("z"), Some("z")),
            ]
        );
    }

    fn content(bytes: &[u8]) -> FileContent {
        FileContent {
            path: "/w/f".into(),
            bytes: bytes.to_vec(),
            sha256: [0; 32],
            size: bytes.len() as u64,
            truncated: false,
            binary: bytes.contains(&0),
        }
    }

    #[test]
    fn file_diffs_handle_added_deleted_binary_and_truncated_sides() {
        let file = |old: Option<FileContent>, new: Option<FileContent>| FileDiff {
            worktree: fl_model::WorktreeId::new(),
            path: "f".into(),
            old,
            new,
        };
        let DiffContent::Text(added) =
            DiffContent::from_file_diff(&file(None, Some(content(b"x\n"))))
        else {
            panic!("text")
        };
        assert_eq!((added.added, added.removed), (1, 0));
        let DiffContent::Text(deleted) =
            DiffContent::from_file_diff(&file(Some(content(b"x\ny\n")), None))
        else {
            panic!("text")
        };
        assert_eq!((deleted.added, deleted.removed), (0, 2));
        assert_eq!(
            DiffContent::from_file_diff(&file(Some(content(b"a\0b")), Some(content(b"a\n")))),
            DiffContent::Binary
        );
        let mut big = content(b"partial");
        big.truncated = true;
        assert_eq!(
            DiffContent::from_file_diff(&file(Some(content(b"a\n")), Some(big))),
            DiffContent::TooLarge
        );
        assert_eq!(
            DiffContent::from_file_diff(&file(Some(content(&[0xff, 0xfe])), None)),
            DiffContent::Binary
        );
        assert_eq!(
            DiffContent::from_file_diff(&file(None, None)),
            DiffContent::Missing
        );
    }
}
