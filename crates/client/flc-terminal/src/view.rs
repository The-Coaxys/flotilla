//! The GPUI terminal view and its custom element.
//!
//! Rendering follows Zed's terminal_view: only visible rows are laid out,
//! same-style cells are batched into one shaped run, and backgrounds are
//! merged into as few quads as possible. Rows whose cells, palette, font and
//! cursor glyph are unchanged reuse their shaped runs from the previous
//! frame, so steady output reshapes only the rows it touched.
use std::mem;
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use fl_term::input::{
    KeyState, Modifiers as TermModifiers, MouseButton as TermButton, encode_key, encode_paste,
};
use fl_term::palette::cell_colors;
use fl_term::{Cell, CursorShape, Flags, NamedColor, RenderFrame, Size, TermMode, TextPoint};
use flc_ui::theme::ActiveTheme;
use gpui::{
    App, Bounds, ClipboardItem, ContentMask, Context, CursorStyle, DispatchPhase, Element,
    ElementId, ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle,
    Focusable, Font, FontFallbacks, FontFeatures, FontStyle, FontWeight, GlobalElementId, Hitbox,
    HitboxBehavior, Hsla, InspectorElementId, IntoElement, KeyDownEvent, KeyUpEvent, Keystroke,
    LayoutId, ModifiersChangedEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    PaintQuad, Pixels, Point, Render, ScrollDelta, ScrollWheelEvent, ShapedLine, SharedString,
    StrikethroughStyle, Style, Subscription, Task, TextRun, TouchPhase, UTF16Selection,
    UnderlineStyle, Window, div, fill, outline, point, prelude::*, px, relative, size,
};
use unicode_width::UnicodeWidthStr;

use crate::grid::{CellHit, CellMetrics, ResizeDebounce, ResizeStep, cell_at, grid_size};
use crate::keys::{self, Route, Stroke, StrokeModifiers};
use crate::links::{Link, link_at};
use crate::mouse::{MouseReporter, WheelAccumulator, WheelAction, wheel_action};
use crate::predict::{EchoPolicy, PredictionContext, Predictor};
use crate::runs::{
    BgQuad, CursorGlyph, RowLayout, RunKind, Underline, default_colors, layout_row,
    merge_backgrounds,
};
use crate::search::{self, Direction, SearchMatch, SearchOptions};
use crate::selection::{
    Anchor, FrameGrid, Selection, SelectionKind, SelectionRange, content_shift,
};
use crate::source::{Exited, Publication, TerminalSource};

/// Space between the pane edge and the cell grid, in logical pixels.
const PADDING_X: f32 = 8.0;
const PADDING_Y: f32 = 6.0;
/// The Windows default caret blink interval.
const BLINK_INTERVAL: Duration = Duration::from_millis(530);
/// Quiet period before a laid-out size change is requested.
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(60);

/// Per-surface terminal presentation settings (spec §8.2: font and size are
/// set per surface).
#[derive(Debug, Clone, PartialEq)]
pub struct TerminalViewOptions {
    /// Predictive-echo policy; `Auto` needs a slow link and a shell prompt.
    pub echo_policy: EchoPolicy,
    /// `None`: the theme's terminal font (Cascadia Mono).
    pub font_family: Option<SharedString>,
    /// `None`: the theme's monospace size.
    pub font_size: Option<Pixels>,
    /// Multiplies the font's natural line box.
    pub line_height: f32,
    pub cursor_blink: bool,
}

impl Default for TerminalViewOptions {
    fn default() -> Self {
        Self {
            echo_policy: EchoPolicy::default(),
            font_family: None,
            font_size: None,
            line_height: 1.0,
            cursor_blink: true,
        }
    }
}

/// Events for the hosting shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalEvent {
    /// Ctrl+click on an OSC 8 hyperlink, URL or path. The shell chooses the
    /// action (open in Flotilla, copy path, browser tab, OS browser);
    /// relative paths resolve against `cwd` (OSC 7).
    LinkActivated { link: Link, cwd: Option<String> },
}

/// Scrollback search progress for a find bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchStatus {
    /// Index of the highlighted hit, top to bottom.
    pub current: Option<usize>,
    pub count: usize,
}

struct SearchState {
    query: String,
    options: SearchOptions,
    matches: Vec<SearchMatch>,
    current: Option<usize>,
    /// Source generation the hits were computed at.
    generation: u64,
}

/// The cell grid's placement during the last prepaint.
#[derive(Debug, Clone, Copy)]
struct Geometry {
    origin: Point<Pixels>,
    metrics: CellMetrics,
    /// The frame's (canonical) size; may differ from the pane while another
    /// client owns resizing, in which case the grid is clipped/letterboxed.
    size: Size,
    visible_rows: u16,
    /// The whole pane, for hover tests outside mouse events.
    pane: Bounds<Pixels>,
}

impl Geometry {
    fn hit(&self, position: Point<Pixels>) -> CellHit {
        cell_at(
            f32::from(position.x - self.origin.x),
            f32::from(position.y - self.origin.y),
            self.metrics,
            self.size,
        )
    }

    fn cells(&self, row: u16, column: u16, columns: u16) -> Bounds<Pixels> {
        Bounds::new(
            point(
                self.origin.x + px(column as f32 * self.metrics.cell_width),
                self.origin.y + px(row as f32 * self.metrics.line_height),
            ),
            size(
                px(columns as f32 * self.metrics.cell_width),
                px(self.metrics.line_height),
            ),
        )
    }
}

#[derive(Clone, PartialEq)]
struct FontKey {
    family: SharedString,
    size: Pixels,
    line_height: f32,
    scale: f32,
}

#[derive(Clone)]
struct Fonts {
    key: FontKey,
    regular: Font,
    metrics: CellMetrics,
}

impl Fonts {
    fn variant(&self, bold: bool, italic: bool) -> Font {
        let mut font = self.regular.clone();
        if bold {
            font.weight = FontWeight::BOLD;
        }
        if italic {
            font.style = FontStyle::Italic;
        }
        font
    }
}

struct ShapedRun {
    column: u16,
    line: ShapedLine,
}

#[derive(Default)]
struct CachedRow {
    cells: Vec<Cell>,
    cursor: Option<CursorGlyph>,
    layout: RowLayout,
    shaped: Vec<ShapedRun>,
    valid: bool,
}

/// Retained per-row shaping plus reusable paint buffers. Moved into the
/// element between prepaint and paint, then returned, so nothing is cloned.
#[derive(Default)]
struct RenderCache {
    font: Option<FontKey>,
    colors: Vec<u32>,
    rows: Vec<CachedRow>,
    merged: Vec<BgQuad>,
    merge_scratch: Vec<usize>,
    backgrounds: Vec<PaintQuad>,
    highlights: Vec<PaintQuad>,
    overlays: Vec<PaintQuad>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CursorPaint {
    Block,
    Underline,
    Beam,
    Hollow,
}

struct Prepared {
    geometry: Geometry,
    hitbox: Hitbox,
    background: Hsla,
    cursor: Option<(Bounds<Pixels>, CursorPaint, Hsla)>,
    preedit: Option<(Bounds<Pixels>, ShapedLine, Hsla)>,
    cursor_style: CursorStyle,
    cache: RenderCache,
}

/// A terminal surface bound to one [`TerminalSource`].
pub struct TerminalView {
    source: Arc<dyn TerminalSource>,
    focus_handle: FocusHandle,
    options: TerminalViewOptions,
    frame: Arc<RenderFrame>,
    generation: u64,
    stale: bool,
    exited: Option<Exited>,
    fonts: Option<Fonts>,
    geometry: Option<Geometry>,
    cache: RenderCache,
    selection: Option<Selection>,
    selecting: bool,
    hovered_link: Option<Link>,
    search: Option<SearchState>,
    mouse: MouseReporter,
    wheel: WheelAccumulator,
    predict: Predictor,
    resize: ResizeDebounce,
    resize_check: Option<Task<()>>,
    /// IME composition (preedit); never sent until committed.
    marked: String,
    marked_selection: Option<Range<usize>>,
    /// Keys currently down, for repeat detection where GPUI omits it.
    held: Vec<String>,
    blink_on: bool,
    blink_hold: bool,
    blink_task: Option<Task<()>>,
    _publications: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<TerminalEvent> for TerminalView {}

impl Focusable for TerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

fn hsla(rgb: u32) -> Hsla {
    gpui::rgb(rgb).into()
}

fn stroke_of(keystroke: &Keystroke) -> Stroke<'_> {
    Stroke {
        key: &keystroke.key,
        key_char: keystroke.key_char.as_deref(),
        modifiers: StrokeModifiers {
            control: keystroke.modifiers.control,
            alt: keystroke.modifiers.alt,
            shift: keystroke.modifiers.shift,
            platform: keystroke.modifiers.platform,
        },
    }
}

fn term_modifiers(modifiers: gpui::Modifiers) -> TermModifiers {
    TermModifiers {
        shift: modifiers.shift,
        alt: modifiers.alt,
        control: modifiers.control,
        super_key: modifiers.platform,
        alt_graph: false,
    }
}

fn term_button(button: MouseButton) -> Option<TermButton> {
    match button {
        MouseButton::Left => Some(TermButton::Left),
        MouseButton::Middle => Some(TermButton::Middle),
        MouseButton::Right => Some(TermButton::Right),
        MouseButton::Navigate(_) => None,
    }
}

fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

fn utf16_to_byte(text: &str, utf16: usize) -> usize {
    let mut units = 0;
    for (index, c) in text.char_indices() {
        if units >= utf16 {
            return index;
        }
        units += c.len_utf16();
    }
    text.len()
}

impl TerminalView {
    pub fn new(
        source: Arc<dyn TerminalSource>,
        options: TerminalViewOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let (generation, frame) = source.frame();
        let publications = {
            let source = source.clone();
            cx.spawn(async move |this, cx| {
                let mut seen = generation;
                loop {
                    Publication::new(source.clone(), seen).await;
                    match this.update(cx, |view, cx| view.refresh_frame(cx)) {
                        Ok(next) => seen = next,
                        Err(_) => break,
                    }
                }
            })
        };
        let weak = cx.weak_entity();
        let subscriptions = vec![
            // Plain Ctrl+letter belongs to the terminal before any app
            // binding is matched (interceptors run ahead of the keymap).
            cx.intercept_keystrokes(move |event, window, cx| {
                let stroke = stroke_of(&event.keystroke);
                if !keys::is_terminal_owned(&stroke) {
                    return;
                }
                weak.update(cx, |view, cx| {
                    if !view.focus_handle.is_focused(window) || !view.marked.is_empty() {
                        return;
                    }
                    let state = view.press(&event.keystroke.key, false);
                    if view.handle_stroke(stroke, state, cx) {
                        cx.stop_propagation();
                    }
                })
                .ok();
            }),
            cx.on_focus(&focus_handle, window, |view, _, cx| {
                view.report_focus(true);
                view.start_blink(cx);
                cx.notify();
            }),
            cx.on_blur(&focus_handle, window, |view, _, cx| {
                view.report_focus(false);
                view.held.clear();
                view.blink_task = None;
                view.hovered_link = None;
                cx.notify();
            }),
        ];
        Self {
            stale: source.stale(),
            exited: source.exited(),
            source,
            focus_handle,
            options,
            frame,
            generation,
            fonts: None,
            geometry: None,
            cache: RenderCache::default(),
            selection: None,
            selecting: false,
            hovered_link: None,
            search: None,
            mouse: MouseReporter::default(),
            wheel: WheelAccumulator::default(),
            predict: Predictor::default(),
            resize: ResizeDebounce::new(RESIZE_DEBOUNCE),
            resize_check: None,
            marked: String::new(),
            marked_selection: None,
            held: Vec::new(),
            blink_on: true,
            blink_hold: false,
            blink_task: None,
            _publications: publications,
            _subscriptions: subscriptions,
        }
    }

    pub fn source(&self) -> &Arc<dyn TerminalSource> {
        &self.source
    }

    pub fn options(&self) -> &TerminalViewOptions {
        &self.options
    }

    /// Font family/size/line height take effect on the next frame; a grid
    /// size change is requested (debounced) like any other layout change.
    pub fn set_options(&mut self, options: TerminalViewOptions, cx: &mut Context<Self>) {
        if self.options != options {
            if !options.cursor_blink {
                self.blink_task = None;
                self.blink_on = true;
            }
            self.options = options;
            self.fonts = None;
            cx.notify();
        }
    }

    /// OSC 0/2 window title of the latest frame.
    pub fn title(&self) -> &str {
        &self.frame.title
    }

    /// OSC 7 working directory of the latest frame.
    pub fn cwd(&self) -> Option<&str> {
        self.frame.cwd.as_deref()
    }

    pub fn has_selection(&self) -> bool {
        self.selection_range().is_some()
    }

    /// The selected text, including scrollback outside the viewport.
    pub fn selection_text(&self) -> Option<String> {
        let range = self.selection_range()?;
        Some(self.source.text_between(range.start, range.end)).filter(|t| !t.is_empty())
    }

    /// Copy the selection to the clipboard; `false` when nothing is selected.
    pub fn copy(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(text) = self.selection_text() else {
            return false;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        true
    }

    /// Paste clipboard text: multiline Unicode preserved (CRLF → LF), wrapped
    /// in exactly one bracketed-paste pair when the application enabled it.
    pub fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        self.paste_text(&text, cx);
    }

    /// Paste `text` as if from the clipboard (drag-and-drop, menus).
    pub fn paste_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if text.is_empty() {
            return;
        }
        let bytes = encode_paste(text, self.source.modes());
        self.send_typed(bytes, cx);
    }

    /// Search all retained scrollback and the screen; highlights every hit
    /// and reveals the newest one.
    pub fn search(
        &mut self,
        query: &str,
        options: SearchOptions,
        cx: &mut Context<Self>,
    ) -> SearchStatus {
        let mut state = SearchState {
            query: query.to_owned(),
            options,
            matches: Vec::new(),
            current: None,
            generation: self.generation,
        };
        self.run_search(&mut state);
        state.current = search::step(state.matches.len(), None, Direction::Older);
        let status = SearchStatus {
            current: state.current,
            count: state.matches.len(),
        };
        self.search = Some(state);
        self.reveal_current_match();
        cx.notify();
        status
    }

    /// Move to the next hit toward older (up) or newer (down) output.
    pub fn search_step(
        &mut self,
        direction: Direction,
        cx: &mut Context<Self>,
    ) -> Option<SearchStatus> {
        let mut state = self.search.take()?;
        if state.generation != self.generation {
            // Output arrived: recompute, then continue from the old position.
            let from = state
                .current
                .and_then(|i| state.matches.get(i))
                .map(|m| m.start);
            self.run_search(&mut state);
            state.current = match from {
                Some(from) => search::step_from(&state.matches, from, direction),
                None => search::step(state.matches.len(), None, direction),
            };
        } else {
            state.current = search::step(state.matches.len(), state.current, direction);
        }
        let status = SearchStatus {
            current: state.current,
            count: state.matches.len(),
        };
        self.search = Some(state);
        self.reveal_current_match();
        cx.notify();
        Some(status)
    }

    pub fn search_status(&self) -> Option<SearchStatus> {
        self.search.as_ref().map(|s| SearchStatus {
            current: s.current,
            count: s.matches.len(),
        })
    }

    pub fn clear_search(&mut self, cx: &mut Context<Self>) {
        if self.search.take().is_some() {
            cx.notify();
        }
    }

    fn run_search(&self, state: &mut SearchState) {
        let frame = &self.frame;
        search::search(
            &state.query,
            state.options,
            -(frame.history_lines as i32),
            frame.size.rows as i32 - 1,
            |start, count| self.source.history_page(start, count),
            &mut state.matches,
        );
        state.generation = self.generation;
    }

    fn reveal_current_match(&self) {
        let Some(m) = self
            .search
            .as_ref()
            .and_then(|s| s.current.and_then(|i| s.matches.get(i)))
        else {
            return;
        };
        let delta = search::reveal(
            m.start.line,
            self.frame.display_offset,
            self.frame.size.rows,
        );
        if delta != 0 {
            self.source.scroll_display(delta);
        }
    }

    fn selection_range(&self) -> Option<SelectionRange> {
        self.selection?.range(&FrameGrid(&self.frame))
    }

    /// Adopt the latest published frame; returns its generation.
    fn refresh_frame(&mut self, cx: &mut Context<Self>) -> u64 {
        let (generation, frame) = self.source.frame();
        self.stale = self.source.stale();
        self.exited = self.source.exited();
        if generation == self.generation {
            return generation;
        }
        let shift = content_shift(&self.frame, &frame);
        self.frame = frame;
        self.generation = generation;
        match shift {
            Some(0) => {}
            Some(lines) => {
                if let Some(selection) = self.selection.as_mut() {
                    selection.scroll_content(lines);
                }
                if let Some(state) = self.search.as_mut() {
                    for m in &mut state.matches {
                        m.start.line -= lines;
                        m.end.line -= lines;
                    }
                }
                self.hovered_link = None;
            }
            None => {
                self.selection = None;
                self.selecting = false;
                self.hovered_link = None;
                if let Some(state) = self.search.as_mut() {
                    state.matches.clear();
                    state.current = None;
                }
            }
        }
        let context = self.prediction_context();
        if context.allows(&self.frame) {
            self.predict.reconcile(&self.frame);
        } else {
            self.predict.clear();
        }
        cx.notify();
        generation
    }

    fn prediction_context(&self) -> PredictionContext {
        PredictionContext {
            policy: self.options.echo_policy,
            rtt: self.source.rtt(),
            echo: self.source.echo() == Some(true),
            stale: self.stale,
            exited: self.exited.is_some(),
        }
    }

    /// The unconfirmed overlay, if any.
    pub(crate) fn prediction(&self) -> Option<&crate::predict::Prediction> {
        self.predict.pending()
    }

    fn report_focus(&self, focused: bool) {
        if self.source.modes().contains(TermMode::FOCUS_IN_OUT) {
            self.source.send_input(if focused {
                b"\x1b[I".to_vec()
            } else {
                b"\x1b[O".to_vec()
            });
        }
    }

    fn start_blink(&mut self, cx: &mut Context<Self>) {
        self.blink_on = true;
        if !self.options.cursor_blink {
            return;
        }
        self.blink_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(BLINK_INTERVAL).await;
                let alive = this.update(cx, |view, cx| {
                    if mem::take(&mut view.blink_hold) {
                        view.blink_on = true;
                    } else {
                        view.blink_on = !view.blink_on;
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        }));
    }

    /// Keep the cursor solid while typing.
    fn hold_blink(&mut self) {
        self.blink_on = true;
        self.blink_hold = true;
    }

    fn press(&mut self, key: &str, is_held: bool) -> KeyState {
        if is_held || self.held.iter().any(|k| k == key) {
            KeyState::Repeat
        } else {
            self.held.push(key.to_owned());
            KeyState::Press
        }
    }

    /// Route one keystroke; `true` when it was consumed.
    fn handle_stroke(&mut self, stroke: Stroke, state: KeyState, cx: &mut Context<Self>) -> bool {
        let modes = self.source.modes();
        let release = state == KeyState::Release;
        match keys::route(&stroke, state, modes) {
            Route::App | Route::Unsupported => false,
            Route::Copy if !release => {
                self.copy(cx);
                true
            }
            Route::Paste if !release => {
                self.paste(cx);
                true
            }
            Route::ScrollPage(pages) if !release => {
                let rows = self.frame.size.rows.saturating_sub(1).max(1) as i32;
                self.source.scroll_display(pages * rows);
                true
            }
            Route::Copy | Route::Paste | Route::ScrollPage(_) => false,
            Route::Terminal(event) => match encode_key(&event, modes) {
                Some(bytes) => {
                    self.send_typed(bytes, cx);
                    true
                }
                None => false,
            },
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let state = self.press(&event.keystroke.key, event.is_held);
        if self.handle_stroke(stroke_of(&event.keystroke), state, cx) {
            cx.stop_propagation();
        }
    }

    fn on_key_up(&mut self, event: &KeyUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.held.retain(|k| *k != event.keystroke.key);
        if self.handle_stroke(stroke_of(&event.keystroke), KeyState::Release, cx) {
            cx.stop_propagation();
        }
    }

    fn on_modifiers_changed(
        &mut self,
        event: &ModifiersChangedEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let hit = event
            .modifiers
            .control
            .then(|| {
                let position = window.mouse_position();
                self.geometry
                    .filter(|g| g.pane.contains(&position))
                    .map(|g| g.hit(position))
            })
            .flatten();
        self.set_hovered_link(hit, cx);
    }

    /// Keyboard, paste or IME input: jumps back to the live screen, clears
    /// the selection and reasserts this pane's size (the daemon grants
    /// resize ownership to the latest input owner).
    fn send_typed(&mut self, bytes: Vec<u8>, cx: &mut Context<Self>) {
        if self.frame.display_offset > 0 {
            self.source
                .scroll_display(-(self.frame.display_offset as i32));
        }
        if let Some(requested) = self.resize.requested()
            && (requested.columns, requested.rows)
                != (self.frame.size.columns, self.frame.size.rows)
        {
            self.source.resize(requested);
        }
        let context = self.prediction_context();
        self.predict.typed(&self.frame, context, &bytes);
        self.source.send_input(bytes);
        self.selection = None;
        self.hold_blink();
        cx.notify();
    }

    fn observe_grid(&mut self, size: Size, cx: &mut Context<Self>) {
        match self.resize.observe(size, Instant::now()) {
            ResizeStep::Idle => {}
            ResizeStep::Send(size) => self.source.resize(size),
            ResizeStep::CheckAt(deadline) => {
                if self.resize_check.is_some() {
                    return;
                }
                self.resize_check = Some(cx.spawn(async move |this, cx| {
                    let mut deadline = deadline;
                    loop {
                        let wait = deadline.saturating_duration_since(Instant::now());
                        cx.background_executor().timer(wait).await;
                        let due = this.update(cx, |view, _| {
                            let due = view.resize.due(Instant::now());
                            match due {
                                Ok(Some(size)) => view.source.resize(size),
                                Ok(None) => {}
                                Err(_) => return due,
                            }
                            view.resize_check = None;
                            due
                        });
                        match due {
                            Ok(Err(next)) => deadline = next,
                            _ => break,
                        }
                    }
                }));
            }
        }
    }

    fn reporting(&self, modes: TermMode, modifiers: gpui::Modifiers) -> bool {
        modes.intersects(TermMode::MOUSE_MODE) && !modifiers.shift
    }

    fn anchor(&self, hit: CellHit) -> Anchor {
        Anchor {
            point: TextPoint {
                line: FrameGrid(&self.frame).line_of_row(hit.row),
                column: hit.column,
            },
            side: hit.side,
        }
    }

    fn link_at_hit(&self, hit: CellHit) -> Option<Link> {
        let grid = FrameGrid(&self.frame);
        link_at(&grid, self.anchor(hit).point)
    }

    fn set_hovered_link(&mut self, hit: Option<CellHit>, cx: &mut Context<Self>) {
        let link = hit
            .filter(|h| h.overflow == 0)
            .and_then(|h| self.link_at_hit(h));
        if link != self.hovered_link {
            self.hovered_link = link;
            cx.notify();
        }
    }

    fn mouse_down(
        &mut self,
        event: &MouseDownEvent,
        geometry: Geometry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle);
        let hit = geometry.hit(event.position);
        let modes = self.source.modes();
        if event.button == MouseButton::Left
            && event.modifiers.control
            && let Some(link) = self.link_at_hit(hit)
        {
            cx.emit(TerminalEvent::LinkActivated {
                link,
                cwd: self.frame.cwd.clone(),
            });
            cx.stop_propagation();
            return;
        }
        if self.reporting(modes, event.modifiers) {
            if let Some(button) = term_button(event.button) {
                if let Some(bytes) = self.mouse.press(
                    button,
                    hit.column,
                    hit.row,
                    term_modifiers(event.modifiers),
                    modes,
                ) {
                    self.source.send_input(bytes);
                }
                cx.stop_propagation();
            }
            return;
        }
        if event.button != MouseButton::Left {
            return;
        }
        let anchor = self.anchor(hit);
        match self.selection.as_mut() {
            Some(selection) if event.modifiers.shift && event.click_count == 1 => {
                selection.update(anchor);
            }
            _ => {
                let kind = match event.click_count {
                    0 | 1 => SelectionKind::Simple,
                    2 => SelectionKind::Word,
                    _ => SelectionKind::Line,
                };
                self.selection = Some(Selection::new(kind, anchor));
            }
        }
        self.selecting = true;
        cx.stop_propagation();
        cx.notify();
    }

    fn mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        geometry: Geometry,
        hovered: bool,
        cx: &mut Context<Self>,
    ) {
        let hit = geometry.hit(event.position);
        let idle = self.mouse.held().is_none() && !self.selecting;
        self.set_hovered_link(
            (idle && hovered && event.modifiers.control).then_some(hit),
            cx,
        );
        let modes = self.source.modes();
        let reporting = modes.intersects(TermMode::MOUSE_MODE);
        if self.mouse.held().is_some() || (reporting && hovered && !event.modifiers.shift) {
            if let Some(bytes) =
                self.mouse
                    .motion(hit.column, hit.row, term_modifiers(event.modifiers), modes)
            {
                self.source.send_input(bytes);
            }
            return;
        }
        if self.selecting && event.pressed_button == Some(MouseButton::Left) {
            if hit.overflow != 0 {
                // Dragging past the top or bottom edge scrolls the viewport.
                self.source.scroll_display(-hit.overflow.signum());
            }
            let anchor = self.anchor(hit);
            if let Some(selection) = self.selection.as_mut() {
                selection.update(anchor);
            }
            cx.notify();
        }
    }

    fn mouse_up(&mut self, event: &MouseUpEvent, geometry: Geometry, cx: &mut Context<Self>) {
        // Window-level: also fires when a drag is released outside the pane.
        let hit = geometry.hit(event.position);
        if let Some(button) = term_button(event.button)
            && self.mouse.held() == Some(button)
        {
            if let Some(bytes) = self.mouse.release(
                button,
                hit.column,
                hit.row,
                term_modifiers(event.modifiers),
                self.source.modes(),
            ) {
                self.source.send_input(bytes);
            }
            cx.stop_propagation();
            return;
        }
        if self.selecting && event.button == MouseButton::Left {
            self.selecting = false;
            if self.selection_range().is_none() {
                self.selection = None;
            }
            cx.notify();
        }
    }

    fn scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        geometry: Geometry,
        cx: &mut Context<Self>,
    ) {
        if matches!(event.touch_phase, TouchPhase::Started) {
            self.wheel.reset();
        }
        let lines = match event.delta {
            ScrollDelta::Pixels(delta) => self
                .wheel
                .push_pixels(f32::from(delta.y), geometry.metrics.line_height),
            ScrollDelta::Lines(delta) => self.wheel.push(delta.y),
        };
        let hit = geometry.hit(event.position);
        match wheel_action(
            lines,
            hit.column,
            hit.row,
            term_modifiers(event.modifiers),
            self.source.modes(),
        ) {
            WheelAction::None => {}
            WheelAction::Input(bytes) => self.source.send_input(bytes),
            WheelAction::Scroll(lines) => self.source.scroll_display(lines),
        }
        cx.stop_propagation();
    }

    fn fonts(&mut self, window: &mut Window, cx: &App) -> Fonts {
        let theme = cx.theme();
        let key = FontKey {
            family: self
                .options
                .font_family
                .clone()
                .unwrap_or_else(|| theme.font_terminal.clone()),
            size: self.options.font_size.unwrap_or(theme.mono_size),
            line_height: self.options.line_height,
            scale: window.scale_factor(),
        };
        if let Some(fonts) = self.fonts.as_ref().filter(|f| f.key == key) {
            return fonts.clone();
        }
        // A family that is not installed would silently resolve to a
        // proportional system face (glyphs drift inside their cells, as on
        // Windows 10 or Linux without Cascadia): use the bundled monospace
        // face instead.
        let installed = window.text_system().all_font_names();
        let family = if installed
            .iter()
            .any(|name| name.as_str() == key.family.as_ref())
        {
            key.family.clone()
        } else {
            theme.font_mono.clone()
        };
        let regular = Font {
            family,
            // One glyph per cell: no contextual alternates or ligatures.
            features: FontFeatures(Arc::new(vec![("calt".into(), 0), ("liga".into(), 0)])),
            fallbacks: Some(FontFallbacks::from_fonts(vec![theme.font_mono.to_string()])),
            weight: FontWeight::NORMAL,
            style: FontStyle::Normal,
        };
        let text = window.text_system();
        let id = text.resolve_font(&regular);
        let advance = text
            .advance(id, key.size, 'm')
            .map(|advance| f32::from(advance.width))
            .unwrap_or(f32::from(key.size) * 0.6);
        let metrics = CellMetrics::from_font(
            advance,
            f32::from(text.ascent(id, key.size)),
            f32::from(text.descent(id, key.size)),
            key.line_height,
            key.scale,
        );
        let fonts = Fonts {
            key,
            regular,
            metrics,
        };
        self.fonts = Some(fonts.clone());
        fonts
    }

    fn prepare(
        &mut self,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Prepared {
        let fonts = self.fonts(window, cx);
        let metrics = fonts.metrics;
        let (accent, warning, surface, raised, text_primary) = {
            let theme = cx.theme();
            (
                theme.accent,
                theme.warning,
                theme.surface_bg,
                theme.surface_raised,
                theme.text_primary,
            )
        };
        let origin = bounds.origin + point(px(PADDING_X), px(PADDING_Y));
        let available = (
            f32::from(bounds.size.width) - 2.0 * PADDING_X,
            f32::from(bounds.size.height) - 2.0 * PADDING_Y,
        );
        // A collapsed pane (mid-layout, minimized) must not reflow the
        // remote shell to a sliver.
        if available.0 >= 2.0 * metrics.cell_width && available.1 >= metrics.line_height {
            self.observe_grid(
                grid_size(available.0, available.1, metrics, window.scale_factor()),
                cx,
            );
        }

        let frame = self.frame.clone();
        let columns = frame.size.columns;
        let mask = window.content_mask().bounds.intersect(&bounds);
        let bottom = f32::from(mask.bottom() - origin.y);
        let visible_rows = ((bottom / metrics.line_height).ceil().max(0.0) as u16)
            .min(frame.size.rows)
            .min((frame.cells.len() / columns.max(1) as usize) as u16);
        let geometry = Geometry {
            origin,
            metrics,
            size: frame.size,
            visible_rows,
            pane: bounds,
        };
        self.geometry = Some(geometry);
        let focused = self.focus_handle.is_focused(window);
        if focused && self.options.cursor_blink && self.blink_task.is_none() {
            self.start_blink(cx);
        }
        let colors = &frame.colors;
        let (_, default_bg) = default_colors(colors);

        // Cursor shape and the glyph drawn inside a block cursor.
        let composing = !self.marked.is_empty();
        let cursor_row = u16::try_from(frame.cursor.row)
            .ok()
            .filter(|row| *row < visible_rows);
        let mut cursor = None;
        let mut cursor_glyph = None;
        if let Some(row) = cursor_row
            && frame.cursor.shape != CursorShape::Hidden
            && frame.cursor.column < columns
            && !composing
            && (!focused || self.blink_on)
        {
            let column = frame.cursor.column;
            let cell = &frame.cells[row as usize * columns as usize + column as usize];
            let width = if cell.flags.contains(Flags::WIDE_CHAR) {
                2
            } else {
                1
            };
            let paint = match (focused, frame.cursor.shape) {
                (false, _) | (_, CursorShape::HollowBlock) => CursorPaint::Hollow,
                (_, CursorShape::Underline) => CursorPaint::Underline,
                (_, CursorShape::Beam) => CursorPaint::Beam,
                _ => CursorPaint::Block,
            };
            if paint == CursorPaint::Block {
                cursor_glyph = Some((
                    row,
                    CursorGlyph {
                        column,
                        fg: cell_colors(cell, colors).1,
                    },
                ));
            }
            cursor = Some((
                geometry.cells(row, column, width),
                paint,
                hsla(colors[NamedColor::Cursor as usize]),
            ));
        }

        let mut cache = mem::take(&mut self.cache);
        cache.prepare_rows(&frame, visible_rows, cursor_glyph, &fonts, window);

        // Backgrounds: merged non-default spans.
        merge_backgrounds(
            cache.rows[..visible_rows as usize]
                .iter()
                .enumerate()
                .map(|(row, cached)| (row as u16, cached.layout.backgrounds.as_slice())),
            &mut cache.merged,
            &mut cache.merge_scratch,
        );
        cache.backgrounds.clear();
        for quad in &cache.merged {
            let rows = quad.rows.end - quad.rows.start;
            let mut area = geometry.cells(
                quad.rows.start,
                quad.columns.start,
                quad.columns.end - quad.columns.start,
            );
            area.size.height = px(rows as f32 * metrics.line_height);
            cache.backgrounds.push(fill(area, hsla(quad.color)));
        }

        // Selection and search highlights sit between backgrounds and text.
        cache.highlights.clear();
        let grid = FrameGrid(&frame);
        let top_line = grid.line_of_row(0);
        let bottom_line = grid.line_of_row(visible_rows.saturating_sub(1));
        let highlight = |range: SelectionRange, color: Hsla, out: &mut Vec<PaintQuad>| {
            for line in range.start.line.max(top_line)..=range.end.line.min(bottom_line) {
                if let (Some(row), Some((first, last))) =
                    (grid.row_of_line(line), range.columns_on(line, columns))
                {
                    out.push(fill(geometry.cells(row, first, last - first + 1), color));
                }
            }
        };
        if let Some(state) = self.search.as_ref() {
            let first = state.matches.partition_point(|m| m.end.line < top_line);
            for (index, m) in state.matches[first..].iter().enumerate() {
                if m.start.line > bottom_line {
                    break;
                }
                let current = state.current == Some(first + index);
                let range = SelectionRange {
                    start: m.start,
                    end: m.end,
                };
                highlight(
                    range,
                    warning.opacity(if current { 0.7 } else { 0.3 }),
                    &mut cache.highlights,
                );
            }
        }
        if let Some(range) = self.selection_range() {
            highlight(range, accent.opacity(0.35), &mut cache.highlights);
        }

        // Overlays above text: hovered link underline, stale dimming.
        cache.overlays.clear();
        if let Some(link) = self.hovered_link.as_ref() {
            let range = SelectionRange {
                start: link.start,
                end: link.end,
            };
            let mut underlines = Vec::new();
            highlight(range, accent, &mut underlines);
            for mut quad in underlines {
                let thickness = px(1.0);
                quad.bounds.origin.y += quad.bounds.size.height - thickness;
                quad.bounds.size.height = thickness;
                cache.overlays.push(quad);
            }
        }
        if self.stale {
            cache.overlays.push(fill(bounds, surface.opacity(0.4)));
        }

        // IME preedit at the cursor.
        let preedit = (composing && cursor_row.is_some()).then(|| {
            let cells = self.marked.width().max(1) as u16;
            let area = geometry.cells(cursor_row.unwrap_or(0), frame.cursor.column, cells);
            let text: SharedString = self.marked.clone().into();
            let run = TextRun {
                len: text.len(),
                font: fonts.regular.clone(),
                color: text_primary,
                background_color: None,
                underline: Some(UnderlineStyle {
                    thickness: px(1.0),
                    color: Some(text_primary),
                    wavy: false,
                }),
                strikethrough: None,
            };
            let line = window
                .text_system()
                .shape_line(text, fonts.key.size, &[run], None);
            (area, line, raised)
        });

        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        let cursor_style = if self.hovered_link.is_some() {
            CursorStyle::PointingHand
        } else if frame.modes.intersects(TermMode::MOUSE_MODE) {
            CursorStyle::Arrow
        } else {
            CursorStyle::IBeam
        };
        Prepared {
            geometry,
            hitbox,
            background: hsla(default_bg),
            cursor,
            preedit,
            cursor_style,
            cache,
        }
    }

    fn render_status(&self, cx: &Context<Self>) -> Option<impl IntoElement> {
        let message: SharedString = match (self.exited, self.stale) {
            (Some(Exited { code: Some(code) }), _) => {
                format!("Process exited with code {code}").into()
            }
            (Some(Exited { code: None }), _) => "Process exited".into(),
            (None, true) => "Offline · showing cached output".into(),
            (None, false) => return None,
        };
        let theme = cx.theme();
        Some(
            div()
                .absolute()
                .top(px(PADDING_Y))
                .right(px(PADDING_X))
                .px(px(8.))
                .py(px(3.))
                .rounded(theme.radius_sm)
                .bg(theme.surface_raised)
                .border_1()
                .border_color(theme.border)
                .font_family(theme.font_ui.clone())
                .text_size(theme.text_size_sm)
                .text_color(theme.text_secondary)
                .child(message),
        )
    }
}

impl RenderCache {
    fn prepare_rows(
        &mut self,
        frame: &RenderFrame,
        visible_rows: u16,
        cursor: Option<(u16, CursorGlyph)>,
        fonts: &Fonts,
        window: &mut Window,
    ) {
        let columns = frame.size.columns as usize;
        if self.font.as_ref() != Some(&fonts.key) || self.colors != frame.colors {
            self.font = Some(fonts.key.clone());
            self.colors.clone_from(&frame.colors);
            for row in &mut self.rows {
                row.valid = false;
            }
        }
        if self.rows.len() < visible_rows as usize {
            self.rows
                .resize_with(visible_rows as usize, CachedRow::default);
        }
        for (index, cached) in self.rows[..visible_rows as usize].iter_mut().enumerate() {
            let cells = &frame.cells[index * columns..(index + 1) * columns];
            let glyph = cursor
                .filter(|(row, _)| *row as usize == index)
                .map(|(_, g)| g);
            if cached.valid && cached.cursor == glyph && cached.cells.as_slice() == cells {
                continue;
            }
            cached.cells.clear();
            cached.cells.extend_from_slice(cells);
            cached.cursor = glyph;
            layout_row(cells, &frame.colors, glyph, &mut cached.layout);
            cached.shaped.clear();
            for run in &cached.layout.runs {
                let style = run.style;
                let color = hsla(style.fg);
                let underline = match style.underline {
                    Underline::None => None,
                    kind => Some(UnderlineStyle {
                        thickness: px(if kind == Underline::Double { 2.0 } else { 1.0 }),
                        color: Some(hsla(style.underline_color)),
                        wavy: kind == Underline::Curly,
                    }),
                };
                let text: SharedString = cached.layout.run_text(run).to_owned().into();
                let text_run = TextRun {
                    len: text.len(),
                    font: fonts.variant(style.bold, style.italic),
                    color,
                    background_color: None,
                    underline,
                    strikethrough: style.strikethrough.then_some(StrikethroughStyle {
                        thickness: px(1.0),
                        color: Some(color),
                    }),
                };
                let force_width =
                    (run.kind == RunKind::Grid).then_some(px(fonts.metrics.cell_width));
                let line =
                    window
                        .text_system()
                        .shape_line(text, fonts.key.size, &[text_run], force_width);
                cached.shaped.push(ShapedRun {
                    column: run.column,
                    line,
                });
            }
            cached.valid = true;
        }
    }
}

impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let total = utf16_len(&self.marked);
        let range = range.start.min(total)..range.end.min(total);
        *adjusted = Some(range.clone());
        let start = utf16_to_byte(&self.marked, range.start);
        let end = utf16_to_byte(&self.marked, range.end);
        Some(self.marked[start..end].to_owned())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let end = utf16_len(&self.marked);
        Some(UTF16Selection {
            range: self.marked_selection.clone().unwrap_or(end..end),
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        (!self.marked.is_empty()).then(|| 0..utf16_len(&self.marked))
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.marked.clear();
        self.marked_selection = None;
        cx.notify();
    }

    /// A commit: typed text, a dead-key result or an IME composition.
    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked.clear();
        self.marked_selection = None;
        if text.is_empty() {
            cx.notify();
            return;
        }
        self.send_typed(text.as_bytes().to_vec(), cx);
    }

    /// Preedit only: shown at the cursor, never sent.
    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked.clear();
        self.marked.push_str(text);
        self.marked_selection = selected;
        self.hold_blink();
        cx.notify();
    }

    /// Candidate windows anchor to the composed text at the cursor.
    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let geometry = self.geometry?;
        let row = u16::try_from(self.frame.cursor.row)
            .ok()?
            .min(geometry.size.rows.saturating_sub(1));
        let start = utf16_to_byte(&self.marked, range.start);
        let end = utf16_to_byte(&self.marked, range.end).max(start);
        let before = self.marked[..start].width() as u16;
        let cells = (self.marked[start..end].width() as u16).max(1);
        Some(geometry.cells(row, self.frame.cursor.column + before, cells))
    }

    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

impl Render for TerminalView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let background = hsla(default_colors(&self.frame.colors).1);
        div()
            .key_context("Terminal")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_key_up(cx.listener(Self::on_key_up))
            .on_modifiers_changed(cx.listener(Self::on_modifiers_changed))
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(background)
            .child(TerminalElement { view: cx.entity() })
            .children(self.render_status(cx))
    }
}

struct TerminalElement {
    view: Entity<TerminalView>,
}

impl IntoElement for TerminalElement {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = Option<Prepared>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Prepared> {
        Some(
            self.view
                .update(cx, |view, cx| view.prepare(bounds, window, cx)),
        )
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        prepared: &mut Option<Prepared>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(Prepared {
            geometry,
            hitbox,
            background,
            cursor,
            preedit,
            cursor_style,
            mut cache,
        }) = prepared.take()
        else {
            return;
        };
        let focus_handle = self.view.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.view.clone()),
            cx,
        );
        window.set_cursor_style(cursor_style, &hitbox);
        self.register_mouse(geometry, &hitbox, window);

        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            window.paint_quad(fill(bounds, background));
            for quad in cache.backgrounds.drain(..) {
                window.paint_quad(quad);
            }
            for quad in cache.highlights.drain(..) {
                window.paint_quad(quad);
            }
            if let Some((area, CursorPaint::Block, color)) = cursor {
                window.paint_quad(fill(area, color));
            }
            let metrics = geometry.metrics;
            let line_height = px(metrics.line_height);
            for (row, cached) in cache.rows[..geometry.visible_rows as usize]
                .iter()
                .enumerate()
            {
                let y = geometry.origin.y + px(row as f32 * metrics.line_height);
                for run in &cached.shaped {
                    let x = geometry.origin.x + px(run.column as f32 * metrics.cell_width);
                    let _ = run.line.paint(point(x, y), line_height, window, cx);
                }
            }
            // Unconfirmed predictive echo: dim, at the cursor, gone the moment
            // the daemon's real output arrives or disagrees.
            let overlay = {
                let view = self.view.read(cx);
                view.prediction().cloned().zip(view.fonts.clone())
            };
            if let Some((prediction, fonts)) = overlay {
                let row = prediction.row;
                if row >= 0 && (row as u32) < geometry.visible_rows as u32 {
                    let text: SharedString = prediction.text.clone().into();
                    let color = hsla(cache.colors[NamedColor::Foreground as usize]).opacity(0.6);
                    let run = TextRun {
                        len: text.len(),
                        font: fonts.variant(false, false),
                        color,
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    };
                    let line = window.text_system().shape_line(
                        text,
                        fonts.key.size,
                        &[run],
                        Some(px(metrics.cell_width)),
                    );
                    let x = geometry.origin.x + px(prediction.column as f32 * metrics.cell_width);
                    let y = geometry.origin.y + px(row as f32 * metrics.line_height);
                    let _ = line.paint(point(x, y), line_height, window, cx);
                }
            }
            if let Some((area, paint, color)) = cursor {
                let thickness = px(2.0);
                match paint {
                    CursorPaint::Block => {}
                    CursorPaint::Hollow => {
                        window.paint_quad(outline(area, color, gpui::BorderStyle::Solid));
                    }
                    CursorPaint::Underline => window.paint_quad(fill(
                        Bounds::new(
                            point(area.origin.x, area.bottom() - thickness),
                            size(area.size.width, thickness),
                        ),
                        color,
                    )),
                    CursorPaint::Beam => window.paint_quad(fill(
                        Bounds::new(area.origin, size(thickness, area.size.height)),
                        color,
                    )),
                }
            }
            if let Some((area, line, raised)) = preedit {
                window.paint_quad(fill(area, raised));
                let _ = line.paint(area.origin, line_height, window, cx);
                let caret = Bounds::new(
                    point(area.origin.x + line.width, area.origin.y),
                    size(px(2.0), area.size.height),
                );
                window.paint_quad(fill(caret, hsla(cache.colors[NamedColor::Cursor as usize])));
            }
            for quad in cache.overlays.drain(..) {
                window.paint_quad(quad);
            }
        });

        self.view.update(cx, |view, _| view.cache = cache);
    }
}

impl TerminalElement {
    fn register_mouse(&self, geometry: Geometry, hitbox: &Hitbox, window: &mut Window) {
        let view = self.view.clone();
        let hit = hitbox.clone();
        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble && hit.is_hovered(window) {
                view.update(cx, |view, cx| view.mouse_down(event, geometry, window, cx));
            }
        });
        let view = self.view.clone();
        let hit = hitbox.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble {
                let hovered = hit.is_hovered(window);
                view.update(cx, |view, cx| view.mouse_move(event, geometry, hovered, cx));
            }
        });
        let view = self.view.clone();
        window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
            if phase == DispatchPhase::Bubble {
                view.update(cx, |view, cx| view.mouse_up(event, geometry, cx));
            }
        });
        let view = self.view.clone();
        let hit = hitbox.clone();
        window.on_mouse_event(move |event: &ScrollWheelEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble && hit.is_hovered(window) {
                view.update(cx, |view, cx| view.scroll_wheel(event, geometry, cx));
            }
        });
    }
}
