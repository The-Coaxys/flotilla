//! Row layout: batched same-style text runs and merged background spans.
//!
//! GPUI shapes a run as one line and, with a forced width, places glyph *n*
//! at `n × cell_width`. That holds only for narrow cells holding exactly one
//! glyph, so those are batched; wide characters and cells carrying combining
//! marks become single-cell clusters positioned explicitly at their column.
//! Blank cells never start a run and are trimmed from run ends, so only real
//! glyphs are shaped. Backgrounds equal to the default background are not
//! emitted; the surface paints that once.
use std::ops::Range;

use fl_term::palette::{cell_colors, resolve};
use fl_term::{Cell, Flags};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Underline {
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunStyle {
    /// Resolved 0xRRGGBB, after bold/dim/inverse/hidden.
    pub fg: u32,
    pub bold: bool,
    pub italic: bool,
    pub underline: Underline,
    /// SGR 58 color, or the foreground.
    pub underline_color: u32,
    pub strikethrough: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunKind {
    /// Narrow single-glyph cells: shape with a forced cell-width advance.
    Grid,
    /// One wide character or a base character with combining marks: shape
    /// naturally and place at the column.
    Cluster,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextRun {
    pub column: u16,
    /// Columns covered.
    pub cells: u16,
    /// Byte range into [`RowLayout::text`].
    pub text: Range<usize>,
    pub style: RunStyle,
    pub kind: RunKind,
}

/// Non-default background over `start..end` columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BgSpan {
    pub start: u16,
    pub end: u16,
    pub color: u32,
}

/// One row's runs; buffers are reused between layouts.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RowLayout {
    pub text: String,
    pub runs: Vec<TextRun>,
    pub backgrounds: Vec<BgSpan>,
}

impl RowLayout {
    pub fn run_text(&self, run: &TextRun) -> &str {
        &self.text[run.text.clone()]
    }
}

/// A block cursor draws its glyph in this color over the cursor quad.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorGlyph {
    pub column: u16,
    pub fg: u32,
}

/// Default (foreground, background) for a palette.
pub fn default_colors(colors: &[u32]) -> (u32, u32) {
    cell_colors(&Cell::default(), colors)
}

fn underline_of(flags: Flags) -> Underline {
    if flags.contains(Flags::UNDERCURL) {
        Underline::Curly
    } else if flags.contains(Flags::DOUBLE_UNDERLINE) {
        Underline::Double
    } else if flags.contains(Flags::DOTTED_UNDERLINE) {
        Underline::Dotted
    } else if flags.contains(Flags::DASHED_UNDERLINE) {
        Underline::Dashed
    } else if flags.contains(Flags::UNDERLINE) {
        Underline::Single
    } else {
        Underline::None
    }
}

fn is_blank(cell: &Cell) -> bool {
    matches!(cell.c, ' ' | '\t' | '\0') && cell.zerowidth().is_none_or(<[char]>::is_empty)
}

struct OpenRun {
    run: TextRun,
    /// Blank cells seen since the run's last glyph.
    pending_blanks: u16,
}

pub fn layout_row(
    cells: &[Cell],
    colors: &[u32],
    cursor: Option<CursorGlyph>,
    out: &mut RowLayout,
) {
    out.text.clear();
    out.runs.clear();
    out.backgrounds.clear();
    let default_bg = default_colors(colors).1;
    let mut open: Option<OpenRun> = None;

    for (column, cell) in cells.iter().enumerate() {
        let column = column as u16;
        let (mut fg, bg) = cell_colors(cell, colors);
        push_background(&mut out.backgrounds, column, bg, default_bg);

        let flags = cell.flags;
        if flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
            // Covered by the wide character (or blank before a wrapped one).
            if let Some(open) = open.as_mut() {
                open.pending_blanks += 1;
            }
            continue;
        }
        let underline = underline_of(flags);
        let strikethrough = flags.contains(Flags::STRIKEOUT);
        let hidden = flags.contains(Flags::HIDDEN);
        let decorated = underline != Underline::None || strikethrough;
        if (is_blank(cell) || hidden) && !decorated {
            if let Some(open) = open.as_mut() {
                open.pending_blanks += 1;
            }
            continue;
        }
        if let Some(c) = cursor
            && c.column == column
        {
            fg = c.fg;
        }
        let style = RunStyle {
            fg,
            bold: flags.contains(Flags::BOLD),
            italic: flags.contains(Flags::ITALIC),
            underline,
            underline_color: cell
                .underline_color()
                .map_or(fg, |color| resolve(color, flags, colors)),
            strikethrough,
        };
        let glyph = if hidden || matches!(cell.c, '\t' | '\0') {
            ' '
        } else {
            cell.c
        };
        let wide = flags.contains(Flags::WIDE_CHAR);
        let combining = cell.zerowidth().is_some_and(|marks| !marks.is_empty());

        if wide || combining {
            close(&mut open, out);
            let start = out.text.len();
            out.text.push(glyph);
            if !hidden {
                for &mark in cell.zerowidth().unwrap_or_default() {
                    out.text.push(mark);
                }
            }
            out.runs.push(TextRun {
                column,
                cells: if wide { 2 } else { 1 },
                text: start..out.text.len(),
                style,
                kind: RunKind::Cluster,
            });
            continue;
        }

        match open.as_mut() {
            Some(current) if current.run.style == style => {
                for _ in 0..current.pending_blanks {
                    out.text.push(' ');
                }
                current.run.cells += current.pending_blanks + 1;
                current.pending_blanks = 0;
                out.text.push(glyph);
                current.run.text.end = out.text.len();
            }
            _ => {
                close(&mut open, out);
                let start = out.text.len();
                out.text.push(glyph);
                open = Some(OpenRun {
                    run: TextRun {
                        column,
                        cells: 1,
                        text: start..out.text.len(),
                        style,
                        kind: RunKind::Grid,
                    },
                    pending_blanks: 0,
                });
            }
        }
    }
    close(&mut open, out);
}

fn close(open: &mut Option<OpenRun>, out: &mut RowLayout) {
    if let Some(open) = open.take() {
        out.runs.push(open.run);
    }
}

fn push_background(spans: &mut Vec<BgSpan>, column: u16, color: u32, default_bg: u32) {
    if color == default_bg {
        return;
    }
    if let Some(last) = spans.last_mut()
        && last.end == column
        && last.color == color
    {
        last.end += 1;
        return;
    }
    spans.push(BgSpan {
        start: column,
        end: column + 1,
        color,
    });
}

/// A background rectangle in cells: rows `rows`, columns `columns`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BgQuad {
    pub rows: Range<u16>,
    pub columns: Range<u16>,
    pub color: u32,
}

/// Merge each row's spans with identical spans on the row above, so a
/// full-screen colored region is one quad instead of one per row.
pub fn merge_backgrounds<'a>(
    rows: impl IntoIterator<Item = (u16, &'a [BgSpan])>,
    out: &mut Vec<BgQuad>,
    scratch: &mut Vec<usize>,
) {
    out.clear();
    scratch.clear();
    // `scratch` holds indices of quads ending at the previous row.
    let mut previous_row = None;
    for (row, spans) in rows {
        let contiguous = previous_row.is_some_and(|p: u16| p + 1 == row);
        let active_end = scratch.len();
        if !contiguous {
            scratch.clear();
        }
        let carried = if contiguous { active_end } else { 0 };
        for span in spans {
            let extended = scratch[..carried].iter().copied().find(|&index| {
                let quad = &out[index];
                quad.rows.end == row
                    && quad.columns == (span.start..span.end)
                    && quad.color == span.color
            });
            match extended {
                Some(index) => {
                    out[index].rows.end = row + 1;
                    scratch.push(index);
                }
                None => {
                    out.push(BgQuad {
                        rows: row..row + 1,
                        columns: span.start..span.end,
                        color: span.color,
                    });
                    scratch.push(out.len() - 1);
                }
            }
        }
        scratch.drain(..carried);
        previous_row = Some(row);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_term::{EffectMode, Engine, NamedColor, RenderFrame, Size};

    fn frame(columns: u16, rows: u16, bytes: &[u8]) -> RenderFrame {
        let mut engine = Engine::new(Size::new(columns, rows), EffectMode::Mirror).unwrap();
        engine.feed(0, bytes).unwrap();
        let mut frame = RenderFrame::default();
        engine.render_into(&mut frame);
        frame
    }

    fn row(frame: &RenderFrame, row: usize) -> &[Cell] {
        let columns = frame.size.columns as usize;
        &frame.cells[row * columns..(row + 1) * columns]
    }

    fn layout(frame: &RenderFrame, index: usize) -> RowLayout {
        let mut out = RowLayout::default();
        layout_row(row(frame, index), &frame.colors, None, &mut out);
        out
    }

    fn runs(layout: &RowLayout) -> Vec<(u16, u16, &str, RunKind)> {
        layout
            .runs
            .iter()
            .map(|r| (r.column, r.cells, layout.run_text(r), r.kind))
            .collect()
    }

    #[test]
    fn same_style_cells_batch_across_spaces_and_blanks_are_trimmed() {
        let f = frame(20, 1, b"  hello world   ");
        let l = layout(&f, 0);
        assert_eq!(runs(&l), vec![(2, 11, "hello world", RunKind::Grid)]);
        assert!(
            l.backgrounds.is_empty(),
            "default background is not emitted"
        );
    }

    #[test]
    fn style_changes_split_runs_with_resolved_truecolor_and_palette() {
        // Red (palette 1), then 24-bit orange bold italic, then default.
        let f = frame(20, 1, b"\x1b[31mab\x1b[38;2;255;128;0;1;3mcd\x1b[0mef");
        let l = layout(&f, 0);
        assert_eq!(
            runs(&l).iter().map(|r| (r.0, r.1, r.2)).collect::<Vec<_>>(),
            vec![(0, 2, "ab"), (2, 2, "cd"), (4, 2, "ef")]
        );
        assert_eq!(l.runs[0].style.fg, f.colors[1]);
        assert_eq!(l.runs[1].style.fg, 0xff8000);
        assert!(l.runs[1].style.bold && l.runs[1].style.italic);
        assert_eq!(l.runs[2].style.fg, default_colors(&f.colors).0);
    }

    #[test]
    fn wide_and_combining_cells_are_positioned_clusters() {
        // e + U+0301 combining acute, then a wide CJK character.
        let f = frame(20, 1, "ab e\u{301}x界yz".as_bytes());
        let l = layout(&f, 0);
        assert_eq!(
            runs(&l),
            vec![
                (0, 2, "ab", RunKind::Grid),
                (3, 1, "e\u{301}", RunKind::Cluster),
                (4, 1, "x", RunKind::Grid),
                (5, 2, "界", RunKind::Cluster),
                (7, 2, "yz", RunKind::Grid),
            ]
        );
    }

    #[test]
    fn inverse_dim_hidden_and_decorations() {
        let f = frame(
            30,
            1,
            b"\x1b[7mI\x1b[0m\x1b[2mD\x1b[0m\x1b[8mH\x1b[0m\x1b[4mU \x1b[0m\x1b[9mS\x1b[0m\x1b[4:3mC\x1b[0m",
        );
        let l = layout(&f, 0);
        let (fg, bg) = default_colors(&f.colors);
        // Inverse swaps colors and paints a background.
        assert_eq!(l.runs[0].style.fg, bg);
        assert_eq!(
            l.backgrounds[0],
            BgSpan {
                start: 0,
                end: 1,
                color: fg
            }
        );
        // Dim uses the dim palette entry.
        assert_eq!(
            l.runs[1].style.fg,
            f.colors[NamedColor::DimForeground as usize]
        );
        // Hidden text is not shaped.
        assert!(!l.text.contains('H'));
        // The underlined trailing space is drawn (its underline is visible).
        let underlined = &l.runs[2];
        assert_eq!((underlined.column, underlined.cells), (3, 2));
        assert_eq!(underlined.style.underline, Underline::Single);
        assert!(l.runs[3].style.strikethrough);
        assert_eq!(l.runs[4].style.underline, Underline::Curly);
    }

    #[test]
    fn colored_underline_and_block_cursor_glyph() {
        let f = frame(10, 1, b"\x1b[4;58;2;1;2;3mabc");
        let mut l = RowLayout::default();
        layout_row(
            row(&f, 0),
            &f.colors,
            Some(CursorGlyph {
                column: 1,
                fg: 0x123456,
            }),
            &mut l,
        );
        assert_eq!(l.runs[0].style.underline_color, 0x010203);
        // The cursor cell becomes its own run in the cursor text color.
        assert_eq!(
            runs(&l).iter().map(|r| (r.0, r.2)).collect::<Vec<_>>(),
            vec![(0, "a"), (1, "b"), (2, "c")]
        );
        assert_eq!(l.runs[1].style.fg, 0x123456);
    }

    #[test]
    fn backgrounds_merge_horizontally_and_vertically() {
        // A 4x3 blue block at columns 2..6 on rows 0-2, plus a red cell on row 1.
        let f = frame(
            10,
            4,
            b"\x1b[44m\x1b[1;3H    \x1b[2;3H    \x1b[3;3H    \x1b[0m\x1b[2;9H\x1b[41m \x1b[0m",
        );
        let layouts: Vec<_> = (0..4).map(|r| layout(&f, r)).collect();
        assert_eq!(layouts[0].backgrounds.len(), 1, "one span for four cells");
        let mut quads = Vec::new();
        let mut scratch = Vec::new();
        merge_backgrounds(
            layouts
                .iter()
                .enumerate()
                .map(|(r, l)| (r as u16, l.backgrounds.as_slice())),
            &mut quads,
            &mut scratch,
        );
        assert_eq!(
            quads,
            vec![
                BgQuad {
                    rows: 0..3,
                    columns: 2..6,
                    color: f.colors[4]
                },
                BgQuad {
                    rows: 1..2,
                    columns: 8..9,
                    color: f.colors[1]
                },
            ]
        );
    }

    #[test]
    fn non_adjacent_rows_do_not_merge() {
        let span = [BgSpan {
            start: 0,
            end: 3,
            color: 7,
        }];
        let mut quads = Vec::new();
        let mut scratch = Vec::new();
        merge_backgrounds(
            [(0u16, &span[..]), (2, &span[..])],
            &mut quads,
            &mut scratch,
        );
        assert_eq!(quads.len(), 2);
    }
}
