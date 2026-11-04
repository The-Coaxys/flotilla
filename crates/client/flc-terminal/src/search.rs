//! Scrollback search over the source's text pages.
//!
//! Text comes from `history_page`, which joins nothing and drops spacer
//! cells, so each hit is mapped back to cells with the engine's own width
//! table (`unicode-width`, as in alacritty) and default 8-column tab stops.
//! Soft-wrapped rows are searched as one logical line.
use fl_term::{HistoryLine, TextPoint};
use unicode_width::UnicodeWidthChar;

/// Lines fetched per `history_page` call (its maximum).
const PAGE: u16 = 1000;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SearchOptions {
    pub case_sensitive: bool,
}

/// Inclusive cell range of one hit, in grid lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchMatch {
    pub start: TextPoint,
    pub end: TextPoint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Toward older output (up).
    Older,
    /// Toward newer output (down).
    Newer,
}

/// Every hit in grid lines `top..=bottom`, ordered top to bottom.
pub fn search(
    query: &str,
    options: SearchOptions,
    top: i32,
    bottom: i32,
    mut page: impl FnMut(i32, u16) -> Vec<HistoryLine>,
    out: &mut Vec<SearchMatch>,
) {
    out.clear();
    let query: Vec<char> = query.chars().collect();
    if query.is_empty() || top > bottom {
        return;
    }
    let mut logical = Logical::default();
    let mut line = top;
    while line <= bottom {
        let count = (bottom - line + 1).min(PAGE as i32) as u16;
        let lines = page(line, count);
        if lines.is_empty() {
            break;
        }
        for history in &lines {
            logical.push(history);
            if !history.wrapped {
                logical.find(&query, options, out);
                logical.clear();
            }
        }
        line = lines.last().map_or(bottom, |l| l.line) + 1;
    }
    logical.find(&query, options, out);
}

#[derive(Default)]
struct Logical {
    chars: Vec<char>,
    /// First and last cell of each character.
    cells: Vec<(TextPoint, TextPoint)>,
}

impl Logical {
    fn clear(&mut self) {
        self.chars.clear();
        self.cells.clear();
    }

    fn push(&mut self, history: &HistoryLine) {
        let mut column: u16 = 0;
        for c in history.text.chars() {
            if c == '\n' {
                continue;
            }
            let width = if c == '\t' {
                8 - column % 8
            } else {
                c.width().unwrap_or(0) as u16
            };
            let start = TextPoint {
                line: history.line,
                column: if width == 0 {
                    column.saturating_sub(1)
                } else {
                    column
                },
            };
            let end = TextPoint {
                line: history.line,
                column: (column + width).saturating_sub(1).max(start.column),
            };
            self.chars.push(c);
            self.cells.push((start, end));
            column = column.saturating_add(width);
        }
    }

    fn find(&self, query: &[char], options: SearchOptions, out: &mut Vec<SearchMatch>) {
        if self.chars.len() < query.len() {
            return;
        }
        let eq = |a: char, b: char| {
            a == b || (!options.case_sensitive && a.to_lowercase().eq(b.to_lowercase()))
        };
        let mut start = 0;
        while start + query.len() <= self.chars.len() {
            let candidate = &self.chars[start..start + query.len()];
            if candidate.iter().zip(query).all(|(&a, &b)| eq(a, b)) {
                out.push(SearchMatch {
                    start: self.cells[start].0,
                    end: self.cells[start + query.len() - 1].1,
                });
                start += query.len();
            } else {
                start += 1;
            }
        }
    }
}

/// Step from the current hit; from none, start at the newest hit.
pub fn step(count: usize, current: Option<usize>, direction: Direction) -> Option<usize> {
    if count == 0 {
        return None;
    }
    Some(match (current, direction) {
        (None, _) => count - 1,
        (Some(i), Direction::Older) => (i.min(count - 1) + count - 1) % count,
        (Some(i), Direction::Newer) => (i.min(count - 1) + 1) % count,
    })
}

/// After results were recomputed, the hit adjacent to the previously
/// current hit's position `from`, wrapping like [`step`].
pub fn step_from(matches: &[SearchMatch], from: TextPoint, direction: Direction) -> Option<usize> {
    let key = |p: TextPoint| (p.line, p.column);
    if matches.is_empty() {
        return None;
    }
    Some(match direction {
        Direction::Older => matches
            .iter()
            .rposition(|m| key(m.start) < key(from))
            .unwrap_or(matches.len() - 1),
        Direction::Newer => matches
            .iter()
            .position(|m| key(m.start) > key(from))
            .unwrap_or(0),
    })
}

/// Viewport delta (positive = into scrollback) that brings grid `line` into
/// a `rows`-high viewport currently `display_offset` lines back.
pub fn reveal(line: i32, display_offset: u32, rows: u16) -> i32 {
    let offset = display_offset as i32;
    let top = -offset;
    let bottom = top + rows as i32 - 1;
    if line < top {
        top - line
    } else if line > bottom {
        bottom - line
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_term::{EffectMode, Engine, RenderFrame, Size};

    fn engine(columns: u16, rows: u16, bytes: &[u8]) -> Engine {
        let mut engine = Engine::new(Size::new(columns, rows), EffectMode::Mirror).unwrap();
        engine.feed(0, bytes).unwrap();
        engine
    }

    fn run(engine: &Engine, query: &str, options: SearchOptions) -> Vec<SearchMatch> {
        let mut frame = RenderFrame::default();
        engine.render_into(&mut frame);
        let mut out = Vec::new();
        search(
            query,
            options,
            -(frame.history_lines as i32),
            frame.size.rows as i32 - 1,
            |start, count| engine.history_page(start, count),
            &mut out,
        );
        out
    }

    fn found(engine: &Engine, m: &SearchMatch) -> String {
        engine.text_between(m.start, m.end)
    }

    #[test]
    fn finds_hits_in_scrollback_and_screen_in_order() {
        let mut bytes = Vec::new();
        for i in 0..2500 {
            bytes.extend_from_slice(format!("line {i}\r\n").as_bytes());
        }
        bytes.extend_from_slice(b"needle here");
        let mut e = Engine::new(Size::new(20, 5), EffectMode::Mirror).unwrap();
        for chunk in bytes.chunks(16 * 1024) {
            let offset = e.offset();
            e.feed(offset, chunk).unwrap();
        }
        // Spans three history pages.
        let hits = run(&e, "line 12", SearchOptions::default());
        let texts: Vec<_> = hits.iter().map(|m| found(&e, m)).collect();
        assert!(texts.iter().all(|t| t == "line 12"));
        // "line 12", "line 120".."line 129", "line 1200".."line 1299".
        assert_eq!(hits.len(), 1 + 10 + 100);
        assert!(hits.windows(2).all(|w| w[0].start.line < w[1].start.line));
        let last = run(&e, "needle", SearchOptions::default());
        assert_eq!(last.len(), 1);
        assert_eq!(last[0].start, TextPoint { line: 4, column: 0 });
    }

    #[test]
    fn matches_across_soft_wraps_and_wide_cells_map_to_cells() {
        // 10 columns: "abc 界面 h" wraps before "ello" (界 4-5, 面 6-7).
        let e = engine(10, 3, "abc 界面 hello\r\n".as_bytes());
        let hits = run(&e, "面 hel", SearchOptions::default());
        assert_eq!(hits.len(), 1);
        let hit = hits[0];
        assert_eq!(hit.start, TextPoint { line: 0, column: 6 });
        assert_eq!(hit.end, TextPoint { line: 1, column: 1 });
        assert_eq!(found(&e, &hit), "面 hel");
        let wide = run(&e, "界", SearchOptions::default());
        assert_eq!((wide[0].start.column, wide[0].end.column), (4, 5));
    }

    #[test]
    fn case_folding_tabs_and_combining_marks() {
        let e = engine(40, 3, "a\tCafe\u{301} CAFÉ".as_bytes());
        let insensitive = run(&e, "café", SearchOptions::default());
        // The decomposed é does not equal precomposed É; the precomposed one matches.
        assert_eq!(insensitive.len(), 1);
        assert_eq!(insensitive[0].start.column, 13);
        // The tab advances to column 8.
        let cafe = run(
            &e,
            "Cafe",
            SearchOptions {
                case_sensitive: true,
            },
        );
        assert_eq!(cafe[0].start.column, 8);
        assert_eq!(cafe[0].end.column, 11);
        assert!(
            run(
                &e,
                "cafe",
                SearchOptions {
                    case_sensitive: true
                }
            )
            .is_empty()
        );
    }

    #[test]
    fn stepping_wraps_and_starts_at_the_newest_hit() {
        assert_eq!(step(0, None, Direction::Older), None);
        assert_eq!(step(3, None, Direction::Older), Some(2));
        assert_eq!(step(3, Some(2), Direction::Older), Some(1));
        assert_eq!(step(3, Some(0), Direction::Older), Some(2));
        assert_eq!(step(3, Some(2), Direction::Newer), Some(0));
        // A stale index past a shrunken result set is clamped.
        assert_eq!(step(2, Some(7), Direction::Newer), Some(0));
    }

    #[test]
    fn stepping_resumes_from_the_old_position_after_new_output() {
        let mut e = engine(20, 3, b"x1\r\nx2\r\nx3");
        let hits = run(&e, "x", SearchOptions::default());
        let current = step(hits.len(), None, Direction::Older).unwrap();
        let older = step(hits.len(), Some(current), Direction::Older).unwrap();
        assert_eq!(found(&e, &hits[older]), "x");
        let from = hits[older].start;
        // New output scrolls the hits up by two lines; positions are
        // re-found and the next older hit is the one above the old one.
        let offset = e.offset();
        e.feed(offset, b"\r\nx4\r\nx5").unwrap();
        let shifted = TextPoint {
            line: from.line - 2,
            ..from
        };
        let hits = run(&e, "x", SearchOptions::default());
        let next = step_from(&hits, shifted, Direction::Older).unwrap();
        assert_eq!(hits[next].start.line, shifted.line - 1);
        let newer = step_from(&hits, shifted, Direction::Newer).unwrap();
        assert_eq!(hits[newer].start.line, shifted.line + 1);
        // Wraps past the oldest hit.
        let oldest = hits[0].start;
        assert_eq!(
            step_from(&hits, oldest, Direction::Older),
            Some(hits.len() - 1)
        );
    }

    #[test]
    fn reveal_scrolls_just_enough() {
        // 24 rows at the bottom: lines 0..=23 visible.
        assert_eq!(reveal(5, 0, 24), 0);
        assert_eq!(reveal(-10, 0, 24), 10);
        // Scrolled back 30: lines -30..=-7 visible.
        assert_eq!(reveal(-40, 30, 24), 10);
        assert_eq!(reveal(2, 30, 24), -9);
    }
}
