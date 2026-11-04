//! Link detection under the pointer: OSC 8 hyperlinks first, then URLs and
//! file paths found in the logical (soft-wrap-joined) line. Paths keep an
//! optional `:line[:column]` or MSVC `(line[,column])` suffix so compiler
//! output opens at the right place.
use fl_term::{Flags, TextPoint};

use crate::selection::CellGrid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkTarget {
    Url(String),
    /// A local or remote path as printed (relative paths resolve against the
    /// terminal's OSC 7 working directory).
    Path {
        path: String,
        line: Option<u32>,
        column: Option<u32>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// Inclusive cell range, wide characters covered whole.
    pub start: TextPoint,
    pub end: TextPoint,
    pub target: LinkTarget,
    /// From an OSC 8 escape rather than text detection.
    pub explicit: bool,
}

const SCHEMES: &[&str] = &[
    "https://", "http://", "file://", "ftp://", "ssh://", "git://", "mailto:",
];

pub fn link_at(grid: &impl CellGrid, point: TextPoint) -> Option<Link> {
    if let Some(link) = hyperlink_at(grid, point) {
        return Some(link);
    }
    let point = base_cell(grid, point)?;
    let (chars, points) = logical_line(grid, point)?;
    let index = points.iter().position(|p| *p == point)?;
    url_at(&chars, index)
        .map(|(range, url)| (range, classify_uri(url)))
        .or_else(|| path_at(&chars, index))
        .map(|(range, target)| Link {
            start: points[range.start],
            end: cover_wide(grid, points[range.end - 1]),
            target,
            explicit: false,
        })
}

fn cover_wide(grid: &impl CellGrid, point: TextPoint) -> TextPoint {
    if grid
        .cell(point.line, point.column)
        .is_some_and(|c| c.flags.contains(Flags::WIDE_CHAR))
    {
        TextPoint {
            column: point.column + 1,
            ..point
        }
    } else {
        point
    }
}

fn hyperlink_at(grid: &impl CellGrid, point: TextPoint) -> Option<Link> {
    let mut point = point;
    let cell = grid.cell(point.line, point.column)?;
    if cell.flags.contains(Flags::WIDE_CHAR_SPACER) && point.column > 0 {
        point.column -= 1;
    }
    let link = grid.cell(point.line, point.column)?.hyperlink()?;
    let same = |p: TextPoint| {
        grid.cell(p.line, p.column)
            .and_then(|c| c.hyperlink())
            .is_some_and(|h| h == link)
    };
    let columns = grid.columns();
    let mut start = point;
    loop {
        let previous = if start.column > 0 {
            TextPoint {
                column: start.column - 1,
                ..start
            }
        } else if grid.wrapped(start.line - 1) {
            TextPoint {
                line: start.line - 1,
                column: columns - 1,
            }
        } else {
            break;
        };
        if !same(previous) {
            break;
        }
        start = previous;
    }
    let mut end = point;
    loop {
        let next = if end.column + 1 < columns {
            TextPoint {
                column: end.column + 1,
                ..end
            }
        } else if grid.wrapped(end.line) {
            TextPoint {
                line: end.line + 1,
                column: 0,
            }
        } else {
            break;
        };
        if !same(next) {
            break;
        }
        end = next;
    }
    Some(Link {
        start,
        end,
        target: classify_uri(link.uri().to_owned()),
        explicit: true,
    })
}

/// The soft-wrapped line containing `point`, one entry per character with
/// the cell it occupies (combining marks share their base cell).
fn logical_line(grid: &impl CellGrid, point: TextPoint) -> Option<(Vec<char>, Vec<TextPoint>)> {
    grid.cell(point.line, point.column)?;
    let mut first = point.line;
    while grid.cell(first - 1, 0).is_some() && grid.wrapped(first - 1) {
        first -= 1;
    }
    let mut last = point.line;
    while grid.wrapped(last) && grid.cell(last + 1, 0).is_some() {
        last += 1;
    }
    let mut chars = Vec::new();
    let mut points = Vec::new();
    for line in first..=last {
        for column in 0..grid.columns() {
            let Some(cell) = grid.cell(line, column) else {
                break;
            };
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            let here = TextPoint { line, column };
            chars.push(cell.c);
            points.push(here);
            for &mark in cell.zerowidth().unwrap_or_default() {
                chars.push(mark);
                points.push(here);
            }
        }
    }
    Some((chars, points))
}

/// The cell holding the character drawn at `point`: a wide character's
/// trailing spacer maps to the character, a leading spacer (a wide
/// character wrapped early) to the next line's first cell.
fn base_cell(grid: &impl CellGrid, point: TextPoint) -> Option<TextPoint> {
    let flags = grid.cell(point.line, point.column)?.flags;
    if flags.contains(Flags::WIDE_CHAR_SPACER) && point.column > 0 {
        Some(TextPoint {
            column: point.column - 1,
            ..point
        })
    } else if flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
        let next = TextPoint {
            line: point.line + 1,
            column: 0,
        };
        grid.cell(next.line, 0).map(|_| next)
    } else {
        Some(point)
    }
}

fn is_url_char(c: char) -> bool {
    !c.is_whitespace()
        && !c.is_control()
        && !matches!(c, '<' | '>' | '"' | '`' | '{' | '}' | '|' | '\\' | '^')
}

fn starts_with_ignore_case(chars: &[char], prefix: &str) -> bool {
    let mut it = chars.iter();
    prefix
        .chars()
        .all(|p| it.next().is_some_and(|c| c.eq_ignore_ascii_case(&p)))
}

/// Trim trailing sentence punctuation and closing brackets that have no
/// opening partner inside the candidate.
fn trim_end(chars: &[char], start: usize, mut end: usize) -> usize {
    loop {
        let Some(&last) = chars[start..end].last() else {
            return end;
        };
        let unbalanced = |open: char, close: char| {
            let slice = &chars[start..end];
            slice.iter().filter(|&&c| c == close).count()
                > slice.iter().filter(|&&c| c == open).count()
        };
        let trim = match last {
            '.' | ',' | ':' | ';' | '!' | '?' | '\'' => true,
            ')' => unbalanced('(', ')'),
            ']' => unbalanced('[', ']'),
            _ => false,
        };
        if !trim {
            return end;
        }
        end -= 1;
    }
}

fn url_at(chars: &[char], index: usize) -> Option<(std::ops::Range<usize>, String)> {
    let mut start = 0;
    while start < chars.len() {
        let boundary = start == 0 || !chars[start - 1].is_alphanumeric();
        let scheme = boundary
            .then(|| {
                SCHEMES
                    .iter()
                    .find(|s| starts_with_ignore_case(&chars[start..], s))
            })
            .flatten();
        let Some(scheme) = scheme else {
            start += 1;
            continue;
        };
        let mut end = start + scheme.len();
        while end < chars.len() && is_url_char(chars[end]) {
            end += 1;
        }
        let end = trim_end(chars, start, end);
        if end > start + scheme.len() && (start..end).contains(&index) {
            return Some((start..end, chars[start..end].iter().collect()));
        }
        start = end.max(start + 1);
    }
    None
}

fn is_path_delimiter(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '"' | '\'' | '`' | '<' | '>' | '|' | ';' | '{' | '}' | '[' | ']' | '\0'
        )
}

fn path_at(chars: &[char], index: usize) -> Option<(std::ops::Range<usize>, LinkTarget)> {
    if is_path_delimiter(chars[index]) {
        return None;
    }
    let mut start = index;
    while start > 0 && !is_path_delimiter(chars[start - 1]) {
        start -= 1;
    }
    let mut end = index + 1;
    while end < chars.len() && !is_path_delimiter(chars[end]) {
        end += 1;
    }
    // An opening parenthesis wrapping the path: "(see src/a.rs)".
    while start < end && chars[start] == '(' {
        start += 1;
    }
    let msvc = parse_msvc_suffix(&chars[start..end]);
    let (path_end, line, column) = match msvc {
        Some((len, line, column)) => (start + len, Some(line), column),
        None => {
            let end2 = trim_end(chars, start, end);
            let (len, line, column) = parse_colon_suffix(&chars[start..end2]);
            end = end2;
            (start + len, line, column)
        }
    };
    if msvc.is_some() {
        end = path_end + chars[path_end..end].iter().position(|&c| c == ')')? + 1;
    }
    if !(start..end).contains(&index) || path_end <= start {
        return None;
    }
    let path: String = chars[start..path_end].iter().collect();
    looks_like_path(&path, line.is_some())
        .then_some((start..end, LinkTarget::Path { path, line, column }))
}

fn digits(chars: &[char]) -> Option<u32> {
    if chars.is_empty() || !chars.iter().all(char::is_ascii_digit) {
        return None;
    }
    chars.iter().collect::<String>().parse().ok()
}

/// `name(12)` / `name(12,5)` at the end of the token: (path length, line, column).
fn parse_msvc_suffix(chars: &[char]) -> Option<(usize, u32, Option<u32>)> {
    let close = chars.iter().rposition(|&c| c == ')')?;
    let open = chars[..close].iter().rposition(|&c| c == '(')?;
    let inner = &chars[open + 1..close];
    let rest = &chars[close + 1..];
    if !rest.iter().all(|c| matches!(c, ':' | '.' | ',')) {
        return None;
    }
    let (line, column) = match inner.iter().position(|&c| c == ',') {
        Some(comma) => (digits(&inner[..comma])?, Some(digits(&inner[comma + 1..])?)),
        None => (digits(inner)?, None),
    };
    (open > 0).then_some((open, line, column))
}

/// `name:12` / `name:12:5`: (path length, line, column).
fn parse_colon_suffix(chars: &[char]) -> (usize, Option<u32>, Option<u32>) {
    let parts: Vec<usize> = chars
        .iter()
        .enumerate()
        .filter(|(_, c)| **c == ':')
        .map(|(i, _)| i)
        .collect();
    let number = |from: usize, to: usize| digits(&chars[from..to]);
    match parts.as_slice() {
        [.., a, b] if *a > 0 => {
            if let (Some(line), Some(column)) = (number(a + 1, *b), number(b + 1, chars.len())) {
                return (*a, Some(line), Some(column));
            }
            if let Some(line) = number(b + 1, chars.len()) {
                return (*b, Some(line), None);
            }
            (chars.len(), None, None)
        }
        [.., b] if *b > 0 => match number(b + 1, chars.len()) {
            Some(line) => (*b, Some(line), None),
            None => (chars.len(), None, None),
        },
        _ => (chars.len(), None, None),
    }
}

fn looks_like_path(path: &str, has_position: bool) -> bool {
    if path.contains("://") || !path.chars().any(char::is_alphabetic) {
        return false;
    }
    let bytes = path.as_bytes();
    let drive = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/');
    let anchored = path.starts_with('/')
        || path.starts_with("./")
        || path.starts_with("../")
        || path.starts_with("~/")
        || path.starts_with("\\\\")
        || path.starts_with(".\\")
        || drive;
    let separated = (path.contains('/') || path.contains('\\'))
        && !path.ends_with('/')
        && !path.starts_with("//");
    // `main.rs:12` style compiler output needs an extension.
    let positioned_file = has_position
        && path.rsplit_once('.').is_some_and(|(stem, ext)| {
            !stem.is_empty() && ext.chars().all(char::is_alphanumeric) && !ext.is_empty()
        });
    anchored || separated || positioned_file
}

fn classify_uri(uri: String) -> LinkTarget {
    let Some(rest) = uri
        .get(..7)
        .filter(|scheme| scheme.eq_ignore_ascii_case("file://"))
        .map(|_| &uri[7..])
    else {
        return LinkTarget::Url(uri);
    };
    // file://host/path → /path (the host is the terminal's machine).
    let path = match rest.find('/') {
        Some(slash) => &rest[slash..],
        None => return LinkTarget::Url(uri),
    };
    LinkTarget::Path {
        path: percent_decode(path),
        line: None,
        column: None,
    }
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push(h << 4 | l);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(byte: u8) -> Option<u8> {
    (byte as char).to_digit(16).map(|d| d as u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selection::FrameGrid;
    use fl_term::{EffectMode, Engine, RenderFrame, Size};

    fn frame(columns: u16, rows: u16, bytes: &[u8]) -> RenderFrame {
        let mut engine = Engine::new(Size::new(columns, rows), EffectMode::Mirror).unwrap();
        engine.feed(0, bytes).unwrap();
        let mut frame = RenderFrame::default();
        engine.render_into(&mut frame);
        frame
    }

    fn at(line: i32, column: u16) -> TextPoint {
        TextPoint { line, column }
    }

    fn path(path: &str, line: Option<u32>, column: Option<u32>) -> LinkTarget {
        LinkTarget::Path {
            path: path.into(),
            line,
            column,
        }
    }

    #[test]
    fn osc8_hyperlink_spans_its_cells_across_a_wrap() {
        // 10 columns; the link text "click here now" wraps.
        let f = frame(
            10,
            3,
            b"go \x1b]8;id=a;https://example.com/x\x1b\\click here now\x1b]8;;\x1b\\ end",
        );
        let grid = FrameGrid(&f);
        let link = link_at(&grid, at(1, 2)).unwrap();
        assert!(link.explicit);
        assert_eq!(link.target, LinkTarget::Url("https://example.com/x".into()));
        assert_eq!((link.start, link.end), (at(0, 3), at(1, 6)));
        // Plain text next to it is not part of the link.
        assert_eq!(link_at(&grid, at(0, 0)), None);
    }

    #[test]
    fn osc8_file_uri_becomes_a_decoded_path() {
        let f = frame(
            40,
            2,
            b"\x1b]8;;file://box/srv/my%20dir/a.rs\x1b\\a.rs\x1b]8;;\x1b\\",
        );
        let link = link_at(&FrameGrid(&f), at(0, 1)).unwrap();
        assert_eq!(link.target, path("/srv/my dir/a.rs", None, None));
    }

    #[test]
    fn urls_are_detected_with_trailing_punctuation_trimmed() {
        let f = frame(80, 2, b"see (https://example.com/a_(b)?q=1&x=2). Thanks");
        let grid = FrameGrid(&f);
        let link = link_at(&grid, at(0, 10)).unwrap();
        assert_eq!(
            link.target,
            LinkTarget::Url("https://example.com/a_(b)?q=1&x=2".into())
        );
        assert_eq!((link.start.column, link.end.column), (5, 37));
        assert_eq!(link_at(&grid, at(0, 42)), None, "on 'Thanks'");
    }

    #[test]
    fn a_url_wrapped_across_rows_is_one_link() {
        let f = frame(12, 3, b"x https://example.org/long/path y");
        let grid = FrameGrid(&f);
        let link = link_at(&grid, at(1, 3)).unwrap();
        assert_eq!(
            link.target,
            LinkTarget::Url("https://example.org/long/path".into())
        );
        assert_eq!((link.start, link.end), (at(0, 2), at(2, 6)));
    }

    #[test]
    fn compiler_style_paths_carry_line_and_column() {
        let f = frame(
            80,
            4,
            b"error: src/main.rs:12:5: oops\r\n --> ./lib/x.rs:7\r\nC:\\proj\\a.cpp(42,3): warning\r\nmain.rs:9 and 1.5 and v2",
        );
        let grid = FrameGrid(&f);
        assert_eq!(
            link_at(&grid, at(0, 9)).unwrap().target,
            path("src/main.rs", Some(12), Some(5))
        );
        let lib = link_at(&grid, at(1, 6)).unwrap();
        assert_eq!(lib.target, path("./lib/x.rs", Some(7), None));
        assert_eq!((lib.start.column, lib.end.column), (5, 16));
        assert_eq!(
            link_at(&grid, at(2, 3)).unwrap().target,
            path("C:\\proj\\a.cpp", Some(42), Some(3))
        );
        assert_eq!(
            link_at(&grid, at(3, 2)).unwrap().target,
            path("main.rs", Some(9), None)
        );
        // Numbers and bare words are not paths.
        assert_eq!(link_at(&grid, at(3, 15)), None, "1.5");
        assert_eq!(link_at(&grid, at(3, 22)), None, "v2");
    }

    #[test]
    fn wide_characters_in_paths_cover_both_cells() {
        let f = frame(40, 2, "cat ~/文書/メモ.txt".as_bytes());
        let grid = FrameGrid(&f);
        // Hover the spacer half of 書.
        let link = link_at(&grid, at(0, 9)).unwrap();
        assert_eq!(link.target, path("~/文書/メモ.txt", None, None));
        assert_eq!((link.start.column, link.end.column), (4, 18));
    }
}
