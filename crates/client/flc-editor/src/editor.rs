//! The file viewer/editor: a virtualized list of highlighted lines with a
//! gutter, caret, selection, mouse selection, IME, find bar and status line.
//! Text is monospace, so columns map to x by the font's advance.
use std::collections::HashMap;
use std::ops::Range;

use fl_model::workspace::FileContent;
use flc_ui::input::{TextInput, TextInputEvent};
use flc_ui::theme::{ActiveTheme, Theme};
use gpui::prelude::*;
use gpui::{
    App, Bounds, ClipboardItem, Context, ElementInputHandler, Entity, EntityInputHandler,
    EventEmitter, FocusHandle, Focusable, Font, FontFeatures, FontStyle, FontWeight,
    HighlightStyle, Hsla, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, Point,
    ScrollStrategy, ScrollWheelEvent, SharedString, StyledText, Subscription, UTF16Selection,
    UniformListScrollHandle, Window, canvas, div, point, px, size, uniform_list,
};

use crate::buffer::{Motion, ReadOnlyReason, SaveRequest, Selection, TextBuffer};
use crate::display::{DisplayLine, flatten};
use crate::syntax::{HighlightClass, HighlightSpan, Language, Syntax};

/// Rows scrolled by PageUp/PageDown when the viewport is unknown.
const DEFAULT_PAGE: usize = 30;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorEvent {
    /// Ctrl+S on a dirty, writable buffer: write these bytes, then call
    /// [`EditorView::mark_saved`] with the revision.
    Save(SaveRequest),
    /// The buffer became dirty or clean.
    DirtyChanged(bool),
}

/// Theme color for a highlight class (dark palette).
pub fn class_color(class: HighlightClass, t: &Theme) -> Hsla {
    let rgb = |hex: u32| -> Hsla { gpui::rgb(hex).into() };
    match class {
        HighlightClass::Plain => t.text_primary,
        HighlightClass::Keyword => rgb(0xC792EA),
        HighlightClass::Function => rgb(0x82AAFF),
        HighlightClass::Type => rgb(0xFFCB6B),
        HighlightClass::String => rgb(0xC3E88D),
        HighlightClass::Escape => rgb(0x89DDFF),
        HighlightClass::Constant => rgb(0xF78C6C),
        HighlightClass::Comment => rgb(0x6C7086),
        HighlightClass::Property => rgb(0x9CDCFE),
        HighlightClass::VariableBuiltin => rgb(0xFF5370),
        HighlightClass::Operator => rgb(0x89DDFF),
        HighlightClass::Punctuation => rgb(0xA6ACCD),
        HighlightClass::Tag => rgb(0xF07178),
        HighlightClass::Attribute => rgb(0xFFCB6B),
        HighlightClass::Label => rgb(0xF78C6C),
        HighlightClass::Heading => rgb(0x82AAFF),
        HighlightClass::Literal => rgb(0xC3E88D),
        HighlightClass::Link => rgb(0x80CBC4),
        HighlightClass::Emphasis => rgb(0xC792EA),
    }
}

pub(crate) fn mono_font(t: &Theme) -> Font {
    Font {
        family: t.font_mono.clone(),
        features: FontFeatures::disable_ligatures(),
        fallbacks: None,
        weight: FontWeight::NORMAL,
        style: FontStyle::Normal,
    }
}

struct Find {
    input: Entity<TextInput>,
    query: String,
    matches: Vec<Range<usize>>,
    current: usize,
    case_sensitive: bool,
    _subscriptions: [Subscription; 2],
}

pub struct EditorView {
    focus: FocusHandle,
    path: SharedString,
    buffer: TextBuffer,
    syntax: Option<Syntax>,
    language: Option<Language>,
    /// Highlight spans per line for the buffer version they were computed at.
    highlights: HashMap<usize, Vec<HighlightSpan>>,
    highlights_version: u64,
    scroll: UniformListScrollHandle,
    /// Text area bounds (right of the gutter) from the last paint.
    text_bounds: Option<Bounds<Pixels>>,
    advance: Pixels,
    line_height: Pixels,
    selecting: bool,
    marked: Option<Range<usize>>,
    find: Option<Find>,
    was_dirty: bool,
}

impl EventEmitter<EditorEvent> for EditorView {}

impl Focusable for EditorView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl EditorView {
    /// Open file bytes. `read_only` forces a viewer; invalid UTF-8 always is.
    pub fn new(
        path: impl Into<SharedString>,
        bytes: &[u8],
        read_only: Option<ReadOnlyReason>,
        cx: &mut Context<Self>,
    ) -> Self {
        let path = path.into();
        let mut buffer = TextBuffer::from_bytes(bytes);
        if read_only.is_some() {
            buffer.set_read_only(read_only);
        }
        let first_line = buffer.line(0);
        let language = Language::detect(&path, &first_line);
        let mut syntax = language.and_then(Syntax::new);
        if let Some(syntax) = syntax.as_mut() {
            syntax.parse(buffer.rope());
        }
        Self {
            focus: cx.focus_handle(),
            path,
            buffer,
            syntax,
            language,
            highlights: HashMap::new(),
            highlights_version: u64::MAX,
            scroll: UniformListScrollHandle::new(),
            text_bounds: None,
            advance: px(7.5),
            line_height: px(19.),
            selecting: false,
            marked: None,
            find: None,
            was_dirty: false,
        }
    }

    /// Open a file read by the daemon. Truncated or oversized files are
    /// read-only: saving them would cut the file.
    pub fn from_content(content: &FileContent, cx: &mut Context<Self>) -> Self {
        let reason = if content.truncated {
            Some(ReadOnlyReason::Truncated)
        } else if content.bytes.len() >= crate::buffer::MAX_FILE_BYTES {
            Some(ReadOnlyReason::TooLarge)
        } else {
            None
        };
        Self::new(content.path.clone(), &content.bytes, reason, cx)
    }

    pub fn path(&self) -> &SharedString {
        &self.path
    }

    pub fn buffer(&self) -> &TextBuffer {
        &self.buffer
    }

    pub fn language(&self) -> Option<Language> {
        self.language
    }

    pub fn is_dirty(&self) -> bool {
        self.buffer.is_dirty()
    }

    /// The save of `revision` reached the disk.
    pub fn mark_saved(&mut self, revision: u64, cx: &mut Context<Self>) {
        self.buffer.mark_saved(revision);
        self.after_edit(cx);
    }

    /// Move the caret to a line (0-based) and scroll it into view.
    pub fn go_to_line(&mut self, line: usize, cx: &mut Context<Self>) {
        let start = self.buffer.line_range(line).start;
        self.buffer.set_selection(Selection::caret(start));
        self.scroll
            .scroll_to_item(self.buffer.line_of(start), ScrollStrategy::Center);
        cx.notify();
    }

    fn after_edit(&mut self, cx: &mut Context<Self>) {
        let changes = self.buffer.take_changes();
        if let Some(syntax) = self.syntax.as_mut()
            && !changes.is_empty()
        {
            syntax.apply_changes(&changes, self.buffer.rope());
        }
        if self.find.is_some() {
            self.refresh_find(false);
        }
        let dirty = self.buffer.is_dirty();
        if dirty != self.was_dirty {
            self.was_dirty = dirty;
            cx.emit(EditorEvent::DirtyChanged(dirty));
        }
        self.reveal_caret();
        cx.notify();
    }

    fn reveal_caret(&self) {
        let line = self.buffer.line_of(self.buffer.cursor());
        let state = self.scroll.0.borrow();
        let handle = &state.base_handle;
        let viewport = handle.bounds().size.height;
        let top = -handle.offset().y;
        let y = self.line_height * line as f32;
        drop(state);
        if viewport > px(0.) && (y < top || y + self.line_height > top + viewport) {
            self.scroll.scroll_to_item(line, ScrollStrategy::Center);
        }
    }

    fn page_lines(&self) -> usize {
        let viewport = self.scroll.0.borrow().base_handle.bounds().size.height;
        if viewport > px(0.) {
            ((viewport / self.line_height) as usize)
                .saturating_sub(1)
                .max(1)
        } else {
            DEFAULT_PAGE
        }
    }

    // ---- input -----------------------------------------------------------

    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let m = &keystroke.modifiers;
        let ctrl = m.control || m.platform;
        let shift = m.shift;
        let key = keystroke.key.as_str();
        let handled = match (ctrl, m.alt, key) {
            (true, false, "s") => {
                self.save(cx);
                true
            }
            (true, false, "f") => {
                self.open_find(window, cx);
                true
            }
            (true, false, "a") => {
                self.buffer.select_all();
                cx.notify();
                true
            }
            (true, false, "c") => {
                self.copy(cx);
                true
            }
            (true, false, "x") => {
                self.copy(cx);
                self.edit(cx, |b| b.insert(""));
                true
            }
            (true, false, "v") => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    self.edit(cx, |b| b.paste(&text));
                }
                true
            }
            (true, false, "z") if shift => {
                self.edit(cx, TextBuffer::redo);
                true
            }
            (true, false, "z") => {
                self.edit(cx, TextBuffer::undo);
                true
            }
            (true, false, "y") => {
                self.edit(cx, TextBuffer::redo);
                true
            }
            (true, false, "backspace") => {
                self.edit(cx, TextBuffer::delete_word_back);
                true
            }
            (true, false, "delete") => {
                self.edit(cx, TextBuffer::delete_word_forward);
                true
            }
            (_, false, "left") => self.motion(
                if ctrl { Motion::WordLeft } else { Motion::Left },
                shift,
                cx,
            ),
            (_, false, "right") => self.motion(
                if ctrl {
                    Motion::WordRight
                } else {
                    Motion::Right
                },
                shift,
                cx,
            ),
            (false, false, "up") => self.motion(Motion::Up, shift, cx),
            (false, false, "down") => self.motion(Motion::Down, shift, cx),
            (_, false, "home") => self.motion(
                if ctrl {
                    Motion::DocStart
                } else {
                    Motion::LineStart
                },
                shift,
                cx,
            ),
            (_, false, "end") => self.motion(
                if ctrl {
                    Motion::DocEnd
                } else {
                    Motion::LineEnd
                },
                shift,
                cx,
            ),
            (false, false, "pageup") => self.motion(Motion::PageUp(self.page_lines()), shift, cx),
            (false, false, "pagedown") => {
                self.motion(Motion::PageDown(self.page_lines()), shift, cx)
            }
            (false, false, "backspace") => {
                self.edit(cx, TextBuffer::backspace);
                true
            }
            (false, false, "delete") => {
                self.edit(cx, TextBuffer::delete_forward);
                true
            }
            (false, false, "enter") => {
                self.edit(cx, TextBuffer::newline);
                true
            }
            (false, false, "tab") if shift => {
                self.edit(cx, TextBuffer::outdent_lines);
                true
            }
            (false, false, "tab") => {
                self.edit(cx, TextBuffer::tab);
                true
            }
            (false, false, "escape") if self.find.is_some() => {
                self.close_find(window, cx);
                true
            }
            _ => false,
        };
        if handled {
            cx.stop_propagation();
        }
    }

    fn motion(&mut self, motion: Motion, extend: bool, cx: &mut Context<Self>) -> bool {
        self.buffer.move_caret(motion, extend);
        self.reveal_caret();
        cx.notify();
        true
    }

    fn edit(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut TextBuffer) -> bool) {
        if f(&mut self.buffer) {
            self.after_edit(cx);
        }
    }

    fn copy(&self, cx: &mut Context<Self>) {
        let text = self.buffer.selected_text();
        if !text.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        if !self.buffer.is_dirty() {
            return;
        }
        if let Ok(request) = self.buffer.save() {
            cx.emit(EditorEvent::Save(request));
        }
    }

    /// Document offset under a window position.
    fn offset_at(&self, position: Point<Pixels>) -> usize {
        let Some(bounds) = self.text_bounds else {
            return self.buffer.cursor();
        };
        let line = ((position.y - bounds.top()) / self.line_height)
            .floor()
            .max(0.) as usize;
        let line = line.min(self.buffer.line_count() - 1);
        let column = ((position.x - bounds.left()) / self.advance)
            .round()
            .max(0.) as usize;
        let range = self.buffer.line_range(line);
        let display = DisplayLine::new(&self.buffer.slice(range.clone()), self.buffer.tab_width());
        range.start + display.source_for_column(column)
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        let offset = self.offset_at(event.position);
        match event.click_count {
            2 => self.buffer.select_word_at(offset),
            n if n >= 3 => {
                let line = self.buffer.line_of(offset);
                self.buffer.select_lines(line, line);
            }
            _ if event.modifiers.shift => {
                let anchor = self.buffer.selection().anchor;
                self.buffer.set_selection(Selection::new(anchor, offset));
            }
            _ => self.buffer.set_selection(Selection::caret(offset)),
        }
        self.selecting = true;
        cx.notify();
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selecting || event.pressed_button != Some(MouseButton::Left) {
            self.selecting = false;
            return;
        }
        let offset = self.offset_at(event.position);
        let anchor = self.buffer.selection().anchor;
        self.buffer.set_selection(Selection::new(anchor, offset));
        cx.notify();
    }

    // ---- find --------------------------------------------------------------

    fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let selected = self.buffer.selected_text();
        let input = match &self.find {
            Some(find) => find.input.clone(),
            None => {
                let input = cx.new(|cx| TextInput::new(cx, "Find"));
                let events = cx.subscribe_in(
                    &input,
                    window,
                    |this, _, event: &TextInputEvent, window, cx| match event {
                        TextInputEvent::Submitted(_) => this.next_match(cx),
                        TextInputEvent::Cancelled => this.close_find(window, cx),
                    },
                );
                // Search as the query is typed.
                let typing = cx.observe(&input, |this, input, cx| {
                    let query = input.read(cx).text().to_string();
                    this.set_query(query, cx);
                });
                self.find = Some(Find {
                    input: input.clone(),
                    query: String::new(),
                    matches: Vec::new(),
                    current: 0,
                    case_sensitive: false,
                    _subscriptions: [events, typing],
                });
                input
            }
        };
        // A short single-line selection seeds the query.
        if !selected.is_empty() && !selected.contains('\n') && selected.len() <= 80 {
            input.update(cx, |input, cx| input.set_text(selected.clone(), cx));
            self.set_query(selected, cx);
        }
        input.read(cx).focus_handle(cx).focus(window);
        cx.notify();
    }

    fn close_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.find = None;
        window.focus(&self.focus);
        cx.notify();
    }

    fn set_query(&mut self, query: String, cx: &mut Context<Self>) {
        let Some(find) = self.find.as_mut() else {
            return;
        };
        if find.query == query {
            return;
        }
        find.query = query;
        self.refresh_find(true);
        cx.notify();
    }

    fn refresh_find(&mut self, jump: bool) {
        let Some(find) = self.find.as_mut() else {
            return;
        };
        find.matches = self.buffer.find_all(&find.query, find.case_sensitive);
        let cursor = self.buffer.selection().range().start;
        find.current = find
            .matches
            .iter()
            .position(|m| m.start >= cursor)
            .unwrap_or(0);
        if jump && let Some(range) = find.matches.get(find.current).cloned() {
            self.buffer
                .set_selection(Selection::new(range.start, range.end));
            self.reveal_caret();
        }
    }

    fn next_match(&mut self, cx: &mut Context<Self>) {
        let Some(find) = self.find.as_mut() else {
            return;
        };
        if find.matches.is_empty() {
            return;
        }
        find.current = (find.current + 1) % find.matches.len();
        let range = find.matches[find.current].clone();
        self.buffer
            .set_selection(Selection::new(range.start, range.end));
        self.reveal_caret();
        cx.notify();
    }

    fn toggle_case(&mut self, cx: &mut Context<Self>) {
        if let Some(find) = self.find.as_mut() {
            find.case_sensitive = !find.case_sensitive;
        }
        self.refresh_find(true);
        cx.notify();
    }

    // ---- rendering -----------------------------------------------------------

    fn highlights_for(&mut self, lines: Range<usize>) {
        if self.highlights_version != self.buffer.version() {
            self.highlights.clear();
            self.highlights_version = self.buffer.version();
        }
        if lines
            .clone()
            .all(|line| self.highlights.contains_key(&line))
        {
            return;
        }
        let Some(syntax) = self.syntax.as_mut() else {
            return;
        };
        let spans = syntax.highlight_lines(self.buffer.rope(), lines.clone());
        for (line, spans) in lines.zip(spans) {
            self.highlights.insert(line, spans);
        }
    }

    fn render_lines(
        &mut self,
        lines: Range<usize>,
        focused: bool,
        t: &Theme,
    ) -> Vec<gpui::AnyElement> {
        self.highlights_for(lines.clone());
        let selection = self.buffer.selection().range();
        let cursor = self.buffer.cursor();
        let focused_line = self.buffer.line_of(cursor);
        let matches = self
            .find
            .as_ref()
            .map(|f| f.matches.clone())
            .unwrap_or_default();
        let current_match = self
            .find
            .as_ref()
            .and_then(|f| f.matches.get(f.current).cloned());
        let font = mono_font(t);
        let mut rows = Vec::with_capacity(lines.len());
        for line in lines {
            let range = self.buffer.line_range(line);
            let display =
                DisplayLine::new(&self.buffer.slice(range.clone()), self.buffer.tab_width());
            let local = |r: Range<usize>| -> Option<Range<usize>> {
                let start = r.start.max(range.start);
                let end = r.end.min(range.end);
                (start < end)
                    .then(|| display.range_to_display(start - range.start..end - range.start))
            };
            let syntax: Vec<(Range<usize>, HighlightStyle)> = self
                .highlights
                .get(&line)
                .map(|spans| {
                    spans
                        .iter()
                        .map(|span| {
                            (
                                display.range_to_display(span.range.clone()),
                                HighlightStyle {
                                    color: Some(class_color(span.class, t)),
                                    ..Default::default()
                                },
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            let found: Vec<(Range<usize>, HighlightStyle)> = matches
                .iter()
                .filter_map(|m| local(m.clone()))
                .map(|r| {
                    (
                        r,
                        HighlightStyle {
                            background_color: Some(t.warning.opacity(0.25)),
                            ..Default::default()
                        },
                    )
                })
                .collect();
            let current: Vec<(Range<usize>, HighlightStyle)> = current_match
                .clone()
                .and_then(local)
                .map(|r| {
                    vec![(
                        r,
                        HighlightStyle {
                            background_color: Some(t.warning.opacity(0.55)),
                            ..Default::default()
                        },
                    )]
                })
                .unwrap_or_default();
            // Selection paints over everything, keeping syntax colors.
            let mut runs = flatten(display.text.len(), &[&syntax, &found, &current]);
            if let Some(selected) = local(selection.clone()) {
                runs = overlay_background(runs, selected, t.accent.opacity(0.35));
            }
            // A selected line break shows as a selected trailing space.
            let selects_newline =
                !selection.is_empty() && selection.start <= range.end && selection.end > range.end;
            let mut text = display.text.clone();
            if selects_newline {
                let start = text.len();
                text.push(' ');
                runs.push((
                    start..start + 1,
                    HighlightStyle {
                        background_color: Some(t.accent.opacity(0.35)),
                        ..Default::default()
                    },
                ));
            }
            let caret_column = (line == focused_line).then(|| display.column(cursor - range.start));
            let line_height = self.line_height;
            let advance = self.advance;
            let gutter = self.gutter_width();
            rows.push(
                div()
                    .id(("line", line))
                    .h(line_height)
                    .flex()
                    .flex_row()
                    .when(line == focused_line, |d| d.bg(t.raised_hover.opacity(0.4)))
                    .child(
                        div()
                            .w(gutter)
                            .flex_none()
                            .pr(px(12.))
                            .flex()
                            .justify_end()
                            .text_color(if line == focused_line {
                                t.text_secondary
                            } else {
                                t.text_tertiary
                            })
                            .child(SharedString::from((line + 1).to_string())),
                    )
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .whitespace_nowrap()
                            .font(font.clone())
                            .child(StyledText::new(text).with_highlights(runs))
                            .when_some(caret_column.filter(|_| focused), |d, column| {
                                d.child(
                                    div()
                                        .absolute()
                                        .top(px(1.))
                                        .left(advance * column as f32)
                                        .w(px(2.))
                                        .h(line_height - px(2.))
                                        .bg(t.accent),
                                )
                            }),
                    )
                    .into_any_element(),
            );
        }
        rows
    }

    fn gutter_width(&self) -> Pixels {
        let digits = self.buffer.line_count().to_string().len().max(3);
        self.advance * (digits as f32 + 3.)
    }

    fn status_line(&self, t: &Theme) -> impl IntoElement {
        let position = self.buffer.position(self.buffer.cursor());
        let mut parts = vec![
            self.language
                .map_or("Plain text", Language::name)
                .to_string(),
            format!("Ln {}, Col {}", position.line + 1, position.column + 1),
            self.buffer.line_ending().label().to_string(),
            self.buffer.indent().label(),
        ];
        if let Some(reason) = self.buffer.read_only() {
            parts.push(reason.label().to_string());
        } else if self.buffer.is_dirty() {
            parts.push("Modified".to_string());
        }
        div()
            .h(px(24.))
            .px(px(10.))
            .flex()
            .items_center()
            .gap(px(14.))
            .bg(t.chrome_bg)
            .border_t_1()
            .border_color(t.border)
            .text_size(t.text_size_sm)
            .text_color(t.text_secondary)
            .children(parts.into_iter().map(SharedString::from))
    }

    fn find_bar(&self, t: &Theme, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let find = self.find.as_ref()?;
        let count = if find.query.is_empty() {
            String::new()
        } else if find.matches.is_empty() {
            "No results".to_string()
        } else {
            format!("{} of {}", find.current + 1, find.matches.len())
        };
        Some(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(10.))
                .py(px(4.))
                .bg(t.chrome_bg)
                .border_b_1()
                .border_color(t.border)
                .child(
                    div()
                        .w(px(320.))
                        .overflow_hidden()
                        .child(find.input.clone()),
                )
                .child(
                    div()
                        .id("find-case")
                        .px(px(6.))
                        .rounded(t.radius_sm)
                        .cursor_pointer()
                        .when(find.case_sensitive, |d| d.bg(t.control_bg))
                        .text_color(t.text_secondary)
                        .child("Aa")
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_case(cx))),
                )
                .child(
                    div()
                        .text_size(t.text_size_sm)
                        .text_color(t.text_tertiary)
                        .child(SharedString::from(count)),
                ),
        )
    }
}

/// Add a background over `range` without losing the runs' colors.
fn overlay_background(
    runs: Vec<(Range<usize>, HighlightStyle)>,
    range: Range<usize>,
    background: Hsla,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let mut out = Vec::with_capacity(runs.len() + 2);
    let mut covered = range.start;
    for (run, style) in runs {
        if run.end <= range.start || run.start >= range.end {
            out.push((run, style));
            continue;
        }
        if run.start < range.start {
            out.push((run.start..range.start, style));
        }
        let start = run.start.max(range.start);
        if covered < start {
            out.push((
                covered..start,
                HighlightStyle {
                    background_color: Some(background),
                    ..Default::default()
                },
            ));
        }
        let end = run.end.min(range.end);
        out.push((
            start..end,
            HighlightStyle {
                background_color: Some(background),
                ..style
            },
        ));
        covered = end;
        if run.end > range.end {
            out.push((range.end..run.end, style));
        }
    }
    if covered < range.end {
        out.push((
            covered..range.end,
            HighlightStyle {
                background_color: Some(background),
                ..Default::default()
            },
        ));
    }
    out.sort_by_key(|(r, _)| r.start);
    out
}

impl Render for EditorView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme().clone();
        let font = mono_font(&t);
        let font_size = t.mono_size;
        let text_system = window.text_system();
        let font_id = text_system.resolve_font(&font);
        if let Ok(advance) = text_system.advance(font_id, font_size, 'm') {
            self.advance = advance.width;
        }
        self.line_height = (font_size * 1.5).round();
        let focused = self.focus.contains_focused(window, cx);
        let count = self.buffer.line_count();
        let entity = cx.entity();
        let gutter = self.gutter_width();
        let theme = t.clone();
        let list = uniform_list("editor-lines", count, move |range, _window, cx| {
            entity.update(cx, |this, _| this.render_lines(range, focused, &theme))
        })
        .track_scroll(self.scroll.clone())
        .size_full();
        let input_entity = cx.entity();
        let focus = self.focus.clone();
        let line_height = self.line_height;
        let scroll = self.scroll.clone();
        div()
            .key_context("Editor")
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .bg(t.surface_bg)
            .text_color(t.text_primary)
            .text_size(font_size)
            .line_height(self.line_height)
            .font(font)
            .on_key_down(cx.listener(Self::on_key))
            .children(self.find_bar(&t, cx))
            .child(
                div()
                    .id("editor-body")
                    .relative()
                    .flex_1()
                    .min_h(px(0.))
                    .cursor_text()
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
                    .on_mouse_move(cx.listener(Self::mouse_move))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _, _, _| this.selecting = false),
                    )
                    .on_scroll_wheel(cx.listener(|_, _: &ScrollWheelEvent, _, cx| cx.notify()))
                    .child(list)
                    .child(
                        canvas(
                            move |bounds, _, _| bounds,
                            move |bounds, _, window, cx| {
                                // Text area: right of the gutter, shifted by
                                // the list's scroll offset.
                                let offset = scroll.0.borrow().base_handle.offset();
                                let text_bounds = Bounds::new(
                                    point(bounds.left() + gutter, bounds.top() + offset.y),
                                    size(bounds.size.width - gutter, line_height * count as f32),
                                );
                                input_entity.update(cx, |this, _| {
                                    this.text_bounds = Some(text_bounds);
                                });
                                window.handle_input(
                                    &focus,
                                    ElementInputHandler::new(bounds, input_entity.clone()),
                                    cx,
                                );
                            },
                        )
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full(),
                    ),
            )
            .child(self.status_line(&t))
    }
}

impl EntityInputHandler for EditorView {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(range_utf16);
        actual_range.replace(self.range_to_utf16(range.clone()));
        Some(self.buffer.slice(range))
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let selection = self.buffer.selection();
        Some(UTF16Selection {
            range: self.range_to_utf16(selection.range()),
            reversed: selection.reversed(),
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.clone().map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .map(|r| self.range_from_utf16(r))
            .or(self.marked.take());
        self.marked = None;
        // Control characters arrive as key events, not text.
        if text.chars().any(|c| c.is_control() && c != '\t') {
            return;
        }
        self.edit(cx, |buffer| match range {
            Some(range) => buffer.replace_range(range, text),
            None => buffer.insert(text),
        });
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        _new_selected_range: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .map(|r| self.range_from_utf16(r))
            .or(self.marked.clone())
            .unwrap_or_else(|| self.buffer.selection().range());
        let start = range.start;
        if self.buffer.replace_range(range, text) {
            self.marked = (!text.is_empty()).then(|| start..start + text.len());
            self.after_edit(cx);
        }
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let text_bounds = self.text_bounds?;
        let offset = self.range_from_utf16(range_utf16).start;
        let line = self.buffer.line_of(offset);
        let range = self.buffer.line_range(line);
        let display = DisplayLine::new(&self.buffer.slice(range.clone()), self.buffer.tab_width());
        let column = display.column(offset - range.start);
        Some(Bounds::new(
            point(
                text_bounds.left() + self.advance * column as f32,
                text_bounds.top() + self.line_height * line as f32,
            ),
            size(self.advance, self.line_height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        let offset = self.offset_at(point);
        Some(self.range_to_utf16(offset..offset).start)
    }
}

impl EditorView {
    fn range_to_utf16(&self, range: Range<usize>) -> Range<usize> {
        let rope = self.buffer.rope();
        let to =
            |offset: usize| rope.char_to_utf16_cu(rope.byte_to_char(offset.min(rope.len_bytes())));
        to(range.start)..to(range.end)
    }

    fn range_from_utf16(&self, range: Range<usize>) -> Range<usize> {
        let rope = self.buffer.rope();
        let from =
            |unit: usize| rope.char_to_byte(rope.utf16_cu_to_char(unit.min(rope.len_utf16_cu())));
        from(range.start)..from(range.end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn style(color: u32) -> HighlightStyle {
        HighlightStyle {
            color: Some(gpui::rgb(color).into()),
            ..Default::default()
        }
    }

    #[test]
    fn selection_background_keeps_syntax_colors_and_fills_gaps() {
        let bg: Hsla = gpui::rgb(0x0000ff).into();
        let runs = vec![(0..3, style(1)), (5..8, style(2))];
        let out = overlay_background(runs, 2..7, bg);
        let summary: Vec<(Range<usize>, Option<Hsla>, bool)> = out
            .iter()
            .map(|(r, s)| (r.clone(), s.color, s.background_color == Some(bg)))
            .collect();
        assert_eq!(
            summary,
            vec![
                (0..2, style(1).color, false),
                (2..3, style(1).color, true),
                (3..5, None, true),
                (5..7, style(2).color, true),
                (7..8, style(2).color, false),
            ]
        );
        // Selection over unstyled text only.
        let plain = overlay_background(Vec::new(), 1..4, bg);
        assert_eq!(plain.len(), 1);
        assert_eq!(plain[0].0, 1..4);
    }
}
