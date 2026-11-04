//! The Changes diff view: unified or side-by-side rows with syntax colors,
//! word-level change highlights, expandable unchanged regions, and line
//! range selection on the new side (for review comments).
use std::collections::HashSet;
use std::ops::Range;
use std::sync::Arc;

use fl_model::workspace::FileDiff;
use flc_ui::theme::{ActiveTheme, Theme};
use gpui::prelude::*;
use gpui::{
    AnyElement, Context, EventEmitter, FocusHandle, Focusable, HighlightStyle, MouseButton,
    SharedString, StyledText, UniformListScrollHandle, Window, div, px, uniform_list,
};

use crate::diff::{DEFAULT_CONTEXT, DiffContent, DiffLine, Layout, LineKind, Row};
use crate::display::{DisplayLine, flatten};
use crate::editor::{class_color, mono_font};
use crate::syntax::{HighlightSpan, Language, highlight_text};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffEvent {
    /// New-side lines (0-based, inclusive) chosen for a review comment.
    LinesSelected {
        path: SharedString,
        lines: Range<usize>,
    },
}

pub struct DiffView {
    focus: FocusHandle,
    path: SharedString,
    content: DiffContent,
    layout: Layout,
    expanded: HashSet<usize>,
    rows: Arc<Vec<Row>>,
    old_spans: Arc<Vec<Vec<HighlightSpan>>>,
    new_spans: Arc<Vec<Vec<HighlightSpan>>>,
    scroll: UniformListScrollHandle,
    /// New-side line where a drag selection started, and where it is now.
    selection: Option<(usize, usize)>,
}

impl EventEmitter<DiffEvent> for DiffView {}

impl Focusable for DiffView {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

fn side_text(side: &Option<fl_model::workspace::FileContent>) -> String {
    side.as_ref()
        .map(|content| String::from_utf8_lossy(&content.bytes).into_owned())
        .unwrap_or_default()
}

impl DiffView {
    pub fn new(diff: &FileDiff, layout: Layout, cx: &mut Context<Self>) -> Self {
        let content = DiffContent::from_file_diff(diff);
        let language = Language::detect(&diff.path, "");
        let (old_spans, new_spans) = match &content {
            DiffContent::Text(_) => (
                highlight_text(language, &side_text(&diff.old)),
                highlight_text(language, &side_text(&diff.new)),
            ),
            _ => (Vec::new(), Vec::new()),
        };
        let mut view = Self {
            focus: cx.focus_handle(),
            path: diff.path.clone().into(),
            content,
            layout,
            expanded: HashSet::new(),
            rows: Arc::new(Vec::new()),
            old_spans: Arc::new(old_spans),
            new_spans: Arc::new(new_spans),
            scroll: UniformListScrollHandle::new(),
            selection: None,
        };
        view.rebuild();
        view
    }

    pub fn path(&self) -> &SharedString {
        &self.path
    }

    pub fn content(&self) -> &DiffContent {
        &self.content
    }

    pub fn set_layout(&mut self, layout: Layout, cx: &mut Context<Self>) {
        self.layout = layout;
        self.rebuild();
        cx.notify();
    }

    fn rebuild(&mut self) {
        self.rows = Arc::new(match &self.content {
            DiffContent::Text(diff) => diff.rows(self.layout, DEFAULT_CONTEXT, &self.expanded),
            _ => Vec::new(),
        });
    }

    fn expand(&mut self, gap: usize, cx: &mut Context<Self>) {
        self.expanded.insert(gap);
        self.rebuild();
        cx.notify();
    }

    fn select_line(&mut self, line: usize, extend: bool, cx: &mut Context<Self>) {
        self.selection = match self.selection {
            Some((anchor, _)) if extend => Some((anchor, line)),
            _ => Some((line, line)),
        };
        cx.notify();
    }

    fn finish_selection(&mut self, cx: &mut Context<Self>) {
        if let Some((a, b)) = self.selection {
            cx.emit(DiffEvent::LinesSelected {
                path: self.path.clone(),
                lines: a.min(b)..a.max(b),
            });
        }
    }

    fn selected(&self, new_line: Option<usize>) -> bool {
        match (self.selection, new_line) {
            (Some((a, b)), Some(line)) => (a.min(b)..=a.max(b)).contains(&line),
            _ => false,
        }
    }

    fn line_cell(
        &self,
        line: Option<&DiffLine>,
        old_side: bool,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(line) = line else {
            return div()
                .flex_1()
                .min_w(px(0.))
                .h_full()
                .bg(t.surface_raised)
                .into_any_element();
        };
        let number = if old_side {
            line.old
        } else {
            line.new.or(line.old)
        };
        let (background, marker) = match line.kind {
            LineKind::Added => (Some(t.success.opacity(0.12)), "+"),
            LineKind::Removed => (Some(t.danger.opacity(0.12)), "-"),
            LineKind::Context => (None, " "),
        };
        let word = match line.kind {
            LineKind::Added => t.success.opacity(0.35),
            _ => t.danger.opacity(0.35),
        };
        let display = DisplayLine::new(&line.text, 4);
        let source_line = if line.kind == LineKind::Removed || old_side {
            line.old
        } else {
            line.new
        };
        let source_spans = source_line
            .and_then(|n| {
                if line.kind == LineKind::Removed || (old_side && line.kind == LineKind::Context) {
                    self.old_spans.get(n)
                } else {
                    self.new_spans.get(n)
                }
            })
            .map(Vec::as_slice)
            .unwrap_or_default();
        let syntax: Vec<(Range<usize>, HighlightStyle)> = source_spans
            .iter()
            .filter(|span| span.range.end <= line.text.len())
            .map(|span| {
                (
                    display.range_to_display(span.range.clone()),
                    HighlightStyle {
                        color: Some(class_color(span.class, t)),
                        ..Default::default()
                    },
                )
            })
            .collect();
        let words: Vec<(Range<usize>, HighlightStyle)> = line
            .changed
            .iter()
            .filter(|r| r.end <= line.text.len())
            .map(|r| {
                (
                    display.range_to_display(r.clone()),
                    HighlightStyle {
                        background_color: Some(word),
                        ..Default::default()
                    },
                )
            })
            .collect();
        let mut runs = flatten(display.text.len(), &[&syntax]);
        for (range, style) in words {
            runs = merge_background(runs, range, style.background_color.unwrap_or(word));
        }
        let selectable = !old_side && line.new.is_some();
        let selected = selectable && self.selected(line.new);
        let new_line = line.new;
        div()
            .flex_1()
            .min_w(px(0.))
            .h_full()
            .flex()
            .flex_row()
            .when_some(background, |d, bg| d.bg(bg))
            .when(selected, |d| d.bg(t.accent.opacity(0.25)))
            .child(
                div()
                    .id(SharedString::from(format!(
                        "n-{}-{}-{}",
                        old_side as u8,
                        line.old.map_or(-1, |n| n as i64),
                        line.new.map_or(-1, |n| n as i64)
                    )))
                    .w(px(52.))
                    .flex_none()
                    .pr(px(8.))
                    .flex()
                    .justify_end()
                    .text_color(t.text_tertiary)
                    .when(selectable, |d| {
                        d.cursor_pointer()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                                    if let Some(line) = new_line {
                                        this.select_line(line, event.modifiers.shift, cx);
                                    }
                                }),
                            )
                            .on_mouse_move(cx.listener(
                                move |this, event: &gpui::MouseMoveEvent, _, cx| {
                                    if event.pressed_button == Some(MouseButton::Left)
                                        && let Some(line) = new_line
                                        && this.selection.is_some()
                                    {
                                        this.select_line(line, true, cx);
                                    }
                                },
                            ))
                            .on_mouse_up(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| this.finish_selection(cx)),
                            )
                    })
                    .child(SharedString::from(
                        number.map_or(String::new(), |n| (n + 1).to_string()),
                    )),
            )
            .child(
                div()
                    .w(px(14.))
                    .flex_none()
                    .text_color(t.text_tertiary)
                    .child(marker),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(StyledText::new(display.text.clone()).with_highlights(runs))
                    .when(line.no_newline, |d| {
                        d.child(div().ml(px(6.)).text_color(t.text_tertiary).child("⏎̸"))
                    }),
            )
            .into_any_element()
    }

    fn render_row(&self, row: &Row, index: usize, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let base = div()
            .id(("diff-row", index))
            .h(px(20.))
            .w_full()
            .flex()
            .flex_row();
        match row {
            Row::Gap { gap, hidden } => {
                let gap = *gap;
                base.items_center()
                    .px(px(12.))
                    .bg(t.chrome_bg)
                    .text_color(t.text_secondary)
                    .cursor_pointer()
                    .hover(|s| s.bg(t.raised_hover))
                    .on_click(cx.listener(move |this, _, _, cx| this.expand(gap, cx)))
                    .child(SharedString::from(format!(
                        "⋯ {hidden} unchanged line{}",
                        if *hidden == 1 { "" } else { "s" }
                    )))
                    .into_any_element()
            }
            Row::Unified(line) => base
                .child(self.line_cell(Some(line), false, t, cx))
                .into_any_element(),
            Row::Split { old, new } => base
                .child(self.line_cell(old.as_ref(), true, t, cx))
                .child(div().w(px(1.)).h_full().bg(t.border))
                .child(self.line_cell(new.as_ref(), false, t, cx))
                .into_any_element(),
        }
    }

    fn header(&self, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let (added, removed) = match &self.content {
            DiffContent::Text(diff) => (diff.added, diff.removed),
            _ => (0, 0),
        };
        let toggle = |label: &'static str, layout: Layout, cx: &mut Context<Self>| {
            let active = self.layout == layout;
            div()
                .id(label)
                .px(px(8.))
                .h(px(22.))
                .flex()
                .items_center()
                .rounded(t.radius_sm)
                .cursor_pointer()
                .text_color(if active {
                    t.text_primary
                } else {
                    t.text_secondary
                })
                .when(active, |d| d.bg(t.control_bg))
                .on_click(cx.listener(move |this, _, _, cx| this.set_layout(layout, cx)))
                .child(label)
        };
        div()
            .h(px(32.))
            .px(px(12.))
            .flex()
            .items_center()
            .gap(px(10.))
            .bg(t.chrome_bg)
            .border_b_1()
            .border_color(t.border)
            .child(div().flex_1().truncate().child(self.path.clone()))
            .child(
                div()
                    .text_color(t.success)
                    .child(SharedString::from(format!("+{added}"))),
            )
            .child(
                div()
                    .text_color(t.danger)
                    .child(SharedString::from(format!("−{removed}"))),
            )
            .child(toggle("Unified", Layout::Unified, cx))
            .child(toggle("Split", Layout::SideBySide, cx))
    }
}

/// Add a background over `range` keeping existing colors.
fn merge_background(
    runs: Vec<(Range<usize>, HighlightStyle)>,
    range: Range<usize>,
    background: gpui::Hsla,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let len = runs
        .iter()
        .map(|(r, _)| r.end)
        .max()
        .unwrap_or(0)
        .max(range.end);
    let mut slots: Vec<Option<HighlightStyle>> = vec![None; len];
    for (r, style) in &runs {
        for slot in &mut slots[r.clone()] {
            *slot = Some(*style);
        }
    }
    for slot in &mut slots[range] {
        let mut style = slot.unwrap_or_default();
        style.background_color = Some(background);
        *slot = Some(style);
    }
    let layered: Vec<(Range<usize>, HighlightStyle)> = slots
        .iter()
        .enumerate()
        .filter_map(|(i, s)| s.map(|s| (i..i + 1, s)))
        .collect();
    flatten(len, &[&layered])
}

impl Render for DiffView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme().clone();
        let header = self.header(&t, cx);
        let body: AnyElement = match &self.content {
            DiffContent::Text(diff) if diff.is_empty() => message("No changes", &t),
            DiffContent::Text(_) => {
                let rows = self.rows.clone();
                let entity = cx.entity();
                let theme = t.clone();
                let count = rows.len();
                uniform_list("diff-rows", count, move |range, _, cx| {
                    entity.update(cx, |this, cx| {
                        range
                            .map(|index| this.render_row(&rows[index], index, &theme, cx))
                            .collect::<Vec<_>>()
                    })
                })
                .track_scroll(self.scroll.clone())
                .size_full()
                .into_any_element()
            }
            DiffContent::Binary => message("Binary file — not shown", &t),
            DiffContent::TooLarge => message("File too large to diff", &t),
            DiffContent::Missing => message("The file no longer exists on either side", &t),
        };
        div()
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .bg(t.surface_bg)
            .text_color(t.text_primary)
            .text_size(t.editor_size)
            .line_height(px(20.))
            .font(mono_font(&t))
            .child(header)
            .child(div().flex_1().min_h(px(0.)).child(body))
    }
}

fn message(text: &'static str, t: &Theme) -> AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .text_color(t.text_secondary)
        .child(text)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_backgrounds_keep_syntax_colors() {
        let color = HighlightStyle {
            color: Some(gpui::rgb(0x112233).into()),
            ..Default::default()
        };
        let bg: gpui::Hsla = gpui::rgb(0x00ff00).into();
        let runs = merge_background(vec![(0..4, color)], 2..6, bg);
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0], (0..2, color));
        assert_eq!(runs[1].0, 2..4);
        assert_eq!(runs[1].1.color, color.color);
        assert_eq!(runs[1].1.background_color, Some(bg));
        assert_eq!(runs[2].0, 4..6);
        assert_eq!(runs[2].1.color, None);
    }
}
