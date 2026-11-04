//! A multiline chat composer. Enter submits (or inserts a newline when
//! Shift is held), Escape asks the surface to close the composer rows, and
//! Ctrl+letters are only handled for clipboard editing so they still reach
//! terminals when the composer is not focused.
use std::ops::Range;

use flc_ui::theme::ActiveTheme;
use gpui::prelude::*;
use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, Element, ElementId, ElementInputHandler,
    Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable, KeyDownEvent, LayoutId,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point,
    SharedString, Style, TextAlign, TextRun, UTF16Selection, UnderlineStyle, Window, WrappedLine,
    div, fill, point, px, relative, size,
};
use unicode_segmentation::UnicodeSegmentation;

/// Composer row height. Matches the `.line_height(px(20.))` set in `render`.
const LINE_HEIGHT: Pixels = px(20.);
const PAD_X: Pixels = px(10.);
const PAD_Y: Pixels = px(7.);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposerEvent {
    /// Enter without Shift: send or steer the text.
    Submitted(SharedString),
    /// Escape: close the composer rows.
    Cancelled,
    /// The text or selection changed; embedded editors use this to request a
    /// fresh measurement of their row.
    Changed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposerOptions {
    pub placeholder: SharedString,
    pub min_lines: usize,
    pub max_lines: usize,
    /// False for editors (answers/JSON) where Enter always inserts a newline.
    pub submit_on_enter: bool,
}

impl Default for ComposerOptions {
    fn default() -> Self {
        Self {
            placeholder: "Message…".into(),
            min_lines: 1,
            max_lines: 8,
            submit_on_enter: true,
        }
    }
}

pub struct Composer {
    focus: FocusHandle,
    text: SharedString,
    selection: Range<usize>,
    reversed: bool,
    marked: Option<Range<usize>>,
    options: ComposerOptions,
    last_layout: Option<Vec<WrappedLine>>,
    last_bounds: Option<Bounds<Pixels>>,
    last_scroll: usize,
    selecting: bool,
}

impl EventEmitter<ComposerEvent> for Composer {}

impl Focusable for Composer {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Composer {
    pub fn new(cx: &mut Context<Self>, options: ComposerOptions) -> Self {
        Self {
            focus: cx.focus_handle(),
            text: "".into(),
            selection: 0..0,
            reversed: false,
            marked: None,
            options,
            last_layout: None,
            last_bounds: None,
            last_scroll: 0,
            selecting: false,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.text = "".into();
        self.selection = 0..0;
        self.reversed = false;
        self.marked = None;
        self.last_scroll = 0;
        cx.emit(ComposerEvent::Changed);
        cx.notify();
    }

    fn cursor(&self) -> usize {
        if self.reversed {
            self.selection.start
        } else {
            self.selection.end
        }
    }

    fn move_to(&mut self, offset: usize, select: bool, cx: &mut Context<Self>) {
        if select {
            let anchor = if self.selection.is_empty() {
                self.cursor()
            } else {
                self.cursor_anchor()
            };
            let (start, end) = if anchor <= offset {
                (anchor, offset)
            } else {
                (offset, anchor)
            };
            self.selection = start..end;
            self.reversed = offset < anchor;
        } else {
            self.selection = offset..offset;
            self.reversed = false;
        }
        cx.notify();
    }

    /// The end that is not the moving cursor when a selection exists.
    fn cursor_anchor(&self) -> usize {
        if self.reversed {
            self.selection.end
        } else {
            self.selection.start
        }
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.text
            .grapheme_indices(true)
            .rev()
            .find_map(|(idx, _)| (idx < offset).then_some(idx))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.text
            .grapheme_indices(true)
            .find_map(|(idx, _)| (idx > offset).then_some(idx))
            .unwrap_or(self.text.len())
    }

    fn line_bounds(&self, offset: usize) -> (usize, usize) {
        let start = self.text[..offset]
            .rfind('\n')
            .map(|ix| ix + 1)
            .unwrap_or(0);
        let end = self.text[offset..]
            .find('\n')
            .map(|ix| offset + ix)
            .unwrap_or(self.text.len());
        (start, end)
    }

    fn vertical(&mut self, offset: usize, down: bool, select: bool, cx: &mut Context<Self>) {
        let (start, _) = self.line_bounds(offset);
        let column = self.text[start..offset].chars().count();
        let target = if down {
            let (_, end) = self.line_bounds(offset);
            if end == self.text.len() {
                return;
            }
            let next_start = end + 1;
            let (_, next_end) = self.line_bounds(next_start);
            (next_start, next_end)
        } else {
            if start == 0 {
                return;
            }
            let previous_end = start - 1;
            let previous_start = self.text[..previous_end]
                .rfind('\n')
                .map(|ix| ix + 1)
                .unwrap_or(0);
            (previous_start, previous_end)
        };
        let line = &self.text[target.0..target.1];
        let offset = line
            .char_indices()
            .nth(column)
            .map(|(ix, _)| target.0 + ix)
            .unwrap_or(target.1);
        self.move_to(offset, select, cx);
    }

    fn insert(&mut self, text: &str, cx: &mut Context<Self>) {
        let range = self.selection.clone();
        let mut next = String::with_capacity(self.text.len() + text.len());
        next.push_str(&self.text[..range.start]);
        next.push_str(text);
        next.push_str(&self.text[range.end..]);
        self.text = next.into();
        let caret = range.start + text.len();
        self.selection = caret..caret;
        self.reversed = false;
        self.marked = None;
        cx.emit(ComposerEvent::Changed);
        cx.notify();
    }

    fn delete_range(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        let mut next = String::with_capacity(self.text.len());
        next.push_str(&self.text[..range.start]);
        next.push_str(&self.text[range.end..]);
        self.text = next.into();
        self.selection = range.start..range.start;
        self.reversed = false;
        self.marked = None;
        cx.emit(ComposerEvent::Changed);
        cx.notify();
    }

    fn backspace(&mut self, cx: &mut Context<Self>) {
        if !self.selection.is_empty() {
            self.delete_range(self.selection.clone(), cx);
        } else {
            let caret = self.cursor();
            let previous = self.previous_boundary(caret);
            if previous != caret {
                self.delete_range(previous..caret, cx);
            }
        }
    }

    fn forward_delete(&mut self, cx: &mut Context<Self>) {
        if !self.selection.is_empty() {
            self.delete_range(self.selection.clone(), cx);
        } else {
            let caret = self.cursor();
            let next = self.next_boundary(caret);
            if next != caret {
                self.delete_range(caret..next, cx);
            }
        }
    }

    fn select_all(&mut self, cx: &mut Context<Self>) {
        self.selection = 0..self.text.len();
        self.reversed = false;
        cx.notify();
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        if !self.selection.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.text[self.selection.clone()].to_owned(),
            ));
        }
    }

    fn cut(&mut self, cx: &mut Context<Self>) {
        if !self.selection.is_empty() {
            self.copy(cx);
            let range = self.selection.clone();
            self.delete_range(range, cx);
        }
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            let text = text.replace("\r\n", "\n").replace('\r', "\n");
            self.insert(&text, cx);
        }
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.text.clone();
        if text.trim().is_empty() {
            return;
        }
        cx.emit(ComposerEvent::Submitted(text));
        // Keep focus for the next message; the surface clears on success.
        window.focus(&self.focus);
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let control = keystroke.modifiers.control || keystroke.modifiers.platform;
        let shift = keystroke.modifiers.shift;
        let select = shift && !control;
        if control {
            match keystroke.key.as_str() {
                "a" => self.select_all(cx),
                "c" => self.copy(cx),
                "x" => self.cut(cx),
                "v" => self.paste(cx),
                // Everything else belongs to the application or a terminal.
                _ => {}
            }
            return;
        }
        match keystroke.key.as_str() {
            "enter" => {
                if shift || !self.options.submit_on_enter {
                    self.insert("\n", cx);
                } else {
                    self.submit(window, cx);
                }
            }
            "escape" => {
                cx.emit(ComposerEvent::Cancelled);
                window.blur();
            }
            "backspace" => self.backspace(cx),
            "delete" => self.forward_delete(cx),
            "left" => {
                let caret = self.cursor();
                let target = if self.selection.is_empty() || select {
                    self.previous_boundary(caret)
                } else {
                    self.selection.start
                };
                self.move_to(target, select, cx);
            }
            "right" => {
                let caret = self.cursor();
                let target = if self.selection.is_empty() || select {
                    self.next_boundary(caret)
                } else {
                    self.selection.end
                };
                self.move_to(target, select, cx);
            }
            "home" => {
                let (start, _) = self.line_bounds(self.cursor());
                self.move_to(start, select, cx);
            }
            "end" => {
                let (_, end) = self.line_bounds(self.cursor());
                self.move_to(end, select, cx);
            }
            "up" => self.vertical(self.cursor(), false, select, cx),
            "down" => self.vertical(self.cursor(), true, select, cx),
            _ => {}
        }
    }

    fn index_for_position(&self, position: Point<Pixels>) -> usize {
        let (Some(lines), Some(bounds)) = (self.last_layout.as_ref(), self.last_bounds.as_ref())
        else {
            return self.text.len();
        };
        let local = point(
            (position.x - bounds.left() - PAD_X).max(px(0.)),
            (position.y - bounds.top() - PAD_Y).max(px(0.)),
        );
        let mut offset = 0;
        let mut y = px(0.);
        for line in lines {
            let visual = line.wrap_boundaries().len() + 1;
            let height = LINE_HEIGHT * visual as f32;
            if local.y < y + height {
                let within = point(local.x, local.y - y);
                let index = line
                    .closest_index_for_position(within, LINE_HEIGHT)
                    .unwrap_or(0)
                    .min(line.len());
                return (offset + index).min(self.text.len());
            }
            y += height;
            offset += line.len() + 1;
        }
        self.text.len()
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        let offset = self.index_for_position(event.position);
        if event.modifiers.shift {
            let anchor = if self.selection.is_empty() {
                self.cursor()
            } else {
                self.cursor_anchor()
            };
            let (start, end) = if anchor <= offset {
                (anchor, offset)
            } else {
                (offset, anchor)
            };
            self.selection = start..end;
            self.reversed = offset < anchor;
        } else {
            self.selection = offset..offset;
            self.reversed = false;
            self.selecting = true;
        }
        cx.notify();
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selecting || event.pressed_button != Some(MouseButton::Left) {
            self.selecting = false;
            return;
        }
        let offset = self.index_for_position(event.position);
        let anchor = if self.reversed {
            self.selection.end
        } else {
            self.selection.start
        };
        let (start, end) = if anchor <= offset {
            (anchor, offset)
        } else {
            (offset, anchor)
        };
        self.selection = start..end;
        self.reversed = offset < anchor;
        cx.notify();
    }
}

impl EntityInputHandler for Composer {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.text[range].to_owned())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selection),
            reversed: self.reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked.as_ref().map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range| self.range_from_utf16(range))
            .or(self.marked.clone())
            .unwrap_or(self.selection.clone());
        self.selection = range;
        self.insert(new_text, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range| self.range_from_utf16(range))
            .or(self.marked.clone())
            .unwrap_or(self.selection.clone());
        self.selection = range.clone();
        let mut next = String::with_capacity(self.text.len() + new_text.len());
        next.push_str(&self.text[..range.start]);
        next.push_str(new_text);
        next.push_str(&self.text[range.end..]);
        self.text = next.into();
        let base = range.start;
        // `new_selected_range_utf16` is relative to the inserted text, so map
        // it inside `new_text` and then rebase onto the buffer.
        let local = |utf16: usize| -> usize {
            let mut bytes = 0;
            let mut count = 0;
            for ch in new_text.chars() {
                if count >= utf16 {
                    break;
                }
                count += ch.len_utf16();
                bytes += ch.len_utf8();
            }
            bytes
        };
        self.selection = new_selected_range_utf16
            .map(|range| base + local(range.start)..base + local(range.end))
            .unwrap_or(base + new_text.len()..base + new_text.len());
        self.marked = (!new_text.is_empty()).then_some(base..base + new_text.len());
        cx.emit(ComposerEvent::Changed);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let lines = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        let start = caret_point(lines, range.start)?;
        let end = caret_point(lines, range.end)?;
        Some(Bounds::from_corners(
            point(
                bounds.left() + PAD_X + start.x,
                bounds.top() + PAD_Y + start.y,
            ),
            point(
                bounds.left() + PAD_X + end.x,
                bounds.top() + PAD_Y + end.y + LINE_HEIGHT,
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.offset_to_utf16(self.index_for_position(point)))
    }
}

impl Composer {
    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;
        for ch in self.text.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }
        utf8_offset
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;
        for ch in self.text.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }
        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range.start)..self.offset_from_utf16(range.end)
    }
}

/// Position of `index` inside the shaped text, in wrapped-line coordinates.
fn caret_point(lines: &[WrappedLine], index: usize) -> Option<Point<Pixels>> {
    let mut offset = 0;
    let mut y = px(0.);
    for line in lines {
        let end = offset + line.len();
        if index <= end {
            let local = line.position_for_index(index - offset, LINE_HEIGHT)?;
            return Some(point(local.x, y + local.y));
        }
        offset = end + 1;
        y += LINE_HEIGHT * (line.wrap_boundaries().len() + 1) as f32;
    }
    None
}

fn total_lines(lines: &[WrappedLine]) -> usize {
    lines
        .iter()
        .map(|line| line.wrap_boundaries().len() + 1)
        .sum::<usize>()
        .max(1)
}

struct ComposerElement {
    input: Entity<Composer>,
    rows: usize,
}

struct PrepaintState {
    lines: Vec<WrappedLine>,
    caret: Option<Point<Pixels>>,
    selection: Option<PaintQuad>,
    scroll: usize,
}

impl IntoElement for ComposerElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for ComposerElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = (LINE_HEIGHT * self.rows as f32).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let (text, selection, caret_index, marked, scroll, placeholder, accent, tertiary) = {
            let input = self.input.read(cx);
            let theme = cx.theme();
            (
                input.text.clone(),
                input.selection.clone(),
                input.cursor(),
                input.marked.clone(),
                input.last_scroll,
                input.options.placeholder.clone(),
                theme.accent,
                theme.text_tertiary,
            )
        };
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let wrap = (bounds.size.width - PAD_X * 2.).max(px(20.));
        let (display, color) = if text.is_empty() {
            (placeholder, tertiary)
        } else {
            (text, style.color)
        };
        let run = TextRun {
            len: display.len(),
            font: style.font(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs = if let Some(marked) = marked.as_ref() {
            vec![
                TextRun {
                    len: marked.start,
                    ..run.clone()
                },
                TextRun {
                    len: marked.end - marked.start,
                    underline: Some(UnderlineStyle {
                        color: Some(run.color),
                        thickness: px(1.0),
                        wavy: false,
                    }),
                    ..run.clone()
                },
                TextRun {
                    len: display.len() - marked.end,
                    ..run
                },
            ]
            .into_iter()
            .filter(|run| run.len > 0)
            .collect()
        } else {
            vec![run]
        };
        let lines = window
            .text_system()
            .shape_text(display, font_size, &runs, Some(wrap), None)
            .unwrap_or_default();
        let visible = self.rows.max(1);
        let total = total_lines(&lines);
        let caret = caret_point(&lines, caret_index);
        let mut scroll = scroll.min(total.saturating_sub(1));
        if let Some(caret) = caret {
            let line = (caret.y / LINE_HEIGHT) as usize;
            if line < scroll {
                scroll = line;
            } else if line >= scroll + visible {
                scroll = line + 1 - visible;
            }
        }
        let selection = (!selection.is_empty()).then(|| {
            let start = caret_point(&lines, selection.start).unwrap_or_default();
            let end = caret_point(&lines, selection.end).unwrap_or_default();
            let y = start.y.min(end.y) - LINE_HEIGHT * scroll as f32;
            fill(
                Bounds::from_corners(
                    point(bounds.left() + PAD_X + start.x, bounds.top() + PAD_Y + y),
                    point(
                        bounds.left() + PAD_X + end.x.max(start.x),
                        bounds.top() + PAD_Y + y + LINE_HEIGHT,
                    ),
                ),
                accent.opacity(0.3),
            )
        });
        PrepaintState {
            lines: lines.into_iter().collect(),
            caret,
            selection,
            scroll,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus = self.input.read(cx).focus.clone();
        let accent = cx.theme().accent;
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        if let Some(selection) = prepaint.selection.take() {
            window.paint_quad(selection);
        }
        let offset = LINE_HEIGHT * prepaint.scroll as f32;
        let wrap = bounds.size.width - PAD_X * 2.;
        let mask = gpui::ContentMask {
            bounds: Bounds::new(
                point(bounds.left() + PAD_X, bounds.top() + PAD_Y),
                size(wrap.max(px(1.)), bounds.size.height - PAD_Y * 2.),
            ),
        };
        window.with_content_mask(Some(mask), |window| {
            let mut y = px(0.);
            for line in &prepaint.lines {
                let origin = point(bounds.left() + PAD_X, bounds.top() + PAD_Y + y - offset);
                let _ = line.paint(
                    origin,
                    LINE_HEIGHT,
                    TextAlign::Left,
                    Some(Bounds::new(origin, size(wrap.max(px(1.)), LINE_HEIGHT))),
                    window,
                    cx,
                );
                y += LINE_HEIGHT * (line.wrap_boundaries().len() + 1) as f32;
            }
        });
        let focused = focus.is_focused(window);
        if focused && let Some(caret) = prepaint.caret.take() {
            let caret = point(caret.x, caret.y - offset);
            if caret.y >= px(0.) && caret.y < bounds.size.height {
                window.paint_quad(fill(
                    Bounds::new(
                        point(
                            bounds.left() + PAD_X + caret.x,
                            bounds.top() + PAD_Y + caret.y,
                        ),
                        size(px(2.), LINE_HEIGHT),
                    ),
                    accent,
                ));
            }
        }
        let scroll = prepaint.scroll;
        self.input.update(cx, |input, _| {
            input.last_layout = Some(std::mem::take(&mut prepaint.lines));
            input.last_bounds = Some(bounds);
            input.last_scroll = scroll;
        });
    }
}

impl Render for Composer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let focused = self.focus.is_focused(window);
        let logical = self.text.split('\n').count();
        let rows = logical.clamp(self.options.min_lines.max(1), self.options.max_lines.max(1));
        div()
            .key_context("ChatComposer")
            .track_focus(&self.focus)
            .cursor(CursorStyle::IBeam)
            .on_key_down(cx.listener(|composer, event: &KeyDownEvent, window, cx| {
                composer.key_down(event, window, cx)
            }))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _window, cx| {
                    this.selecting = false;
                    cx.notify()
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _window, cx| {
                    this.selecting = false;
                    cx.notify()
                }),
            )
            .on_mouse_move(cx.listener(Self::mouse_move))
            .w_full()
            .min_h(px(34.))
            .rounded(theme.radius_sm)
            .border_1()
            .border_color(if focused { theme.accent } else { theme.border })
            .bg(theme.control_bg)
            .py(PAD_Y)
            .px(PAD_X)
            .text_size(theme.text_size)
            .line_height(LINE_HEIGHT)
            .text_color(theme.text_primary)
            .font_family(theme.font_ui.clone())
            .child(ComposerElement {
                input: cx.entity(),
                rows,
            })
    }
}

/// Creates a composer bound to a chat surface or a question card.
pub fn composer(cx: &mut gpui::App, options: ComposerOptions) -> Entity<Composer> {
    cx.new(|cx| Composer::new(cx, options))
}

/// The theme token used by answer editors; exported so callers do not need to
/// re-derive monospace options.
pub fn mono_options(placeholder: impl Into<SharedString>) -> ComposerOptions {
    ComposerOptions {
        placeholder: placeholder.into(),
        min_lines: 2,
        max_lines: 10,
        submit_on_enter: false,
    }
}
