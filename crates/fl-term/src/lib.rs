//! The same exact terminal engine runs on the VPS and in each native client.
//! Checkpoints include the incremental parser, not just visible screen contents.
mod codec;
mod effects;
pub mod input;
pub mod palette;

use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::{Config, Osc52, Term, TermSnapshot};
use serde::{Deserialize, Serialize};
use vte::ansi::{Processor, ProcessorSnapshot};

pub use alacritty_terminal::term::cell::{Cell, Flags};
pub use alacritty_terminal::term::{PromptState, TermMode};
pub use effects::{ClipboardTarget, Effect, EffectKind};
pub use fl_model::Size;
pub use vte::ansi::{Color, CursorShape};

pub const ENGINE_ID: &str = "flotilla-term/1;alacritty/0.26.0;vte/0.15.0";
const MAGIC: &[u8; 8] = b"FLTERM01";
pub const MAX_OUTPUT_CHUNK: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("invalid terminal size or pixel dimensions")]
    InvalidSize,
    #[error("terminal palette must contain 269 RGB colors")]
    InvalidPalette,
    #[error("output gap: expected byte {expected}, received {got}")]
    OutputGap { expected: u64, got: u64 },
    #[error("output chunk exceeds {MAX_OUTPUT_CHUNK} bytes: {0}")]
    OutputTooLarge(usize),
    #[error("terminal output or effect sequence overflow")]
    SequenceOverflow,
    #[error("invalid terminal checkpoint: {0}")]
    Checkpoint(String),
    #[error("invalid terminal-generated effect: {0}")]
    InvalidEffect(String),
}

impl From<fl_model::InvalidSize> for EngineError {
    fn from(_: fl_model::InvalidSize) -> Self {
        Self::InvalidSize
    }
}

struct Geometry(Size);

impl Dimensions for Geometry {
    fn total_lines(&self) -> usize {
        self.0.rows as usize
    }
    fn screen_lines(&self) -> usize {
        self.0.rows as usize
    }
    fn columns(&self) -> usize {
        self.0.columns as usize
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectMode {
    Authoritative,
    /// Parse identical bytes but never originate PTY, clipboard or bell effects.
    Mirror,
}

#[derive(Clone, Default)]
struct Collector(Arc<Mutex<Vec<Event>>>);

impl EventListener for Collector {
    fn send_event(&self, event: Event) {
        self.0
            .lock()
            .expect("terminal event lock poisoned")
            .push(event);
    }
}

#[derive(Serialize, Deserialize)]
struct Checkpoint {
    magic: [u8; 8],
    offset: u64,
    next_effect: u64,
    size: Size,
    palette: Vec<u32>,
    term: TermSnapshot,
    processor: ProcessorSnapshot,
}

pub struct Engine {
    term: Term<Collector>,
    processor: Processor,
    events: Collector,
    size: Size,
    palette: Vec<u32>,
    offset: u64,
    next_effect: u64,
    generation: u64,
    mode: EffectMode,
}

impl Engine {
    pub fn new(size: Size, mode: EffectMode) -> Result<Self, EngineError> {
        let size = size.validate()?;
        let events = Collector::default();
        let config = Config {
            kitty_keyboard: true,
            osc52: Osc52::CopyPaste,
            ..Config::default()
        };
        Ok(Self {
            term: Term::new(config, &Geometry(size), events.clone()),
            processor: Processor::default(),
            events,
            size,
            palette: palette::default_palette(),
            offset: 0,
            next_effect: 0,
            generation: 0,
            mode,
        })
    }

    pub fn restore(bytes: &[u8], mode: EffectMode) -> Result<Self, EngineError> {
        if bytes.len() < MAGIC.len() || &bytes[..MAGIC.len()] != MAGIC {
            return Err(EngineError::Checkpoint(
                "unknown engine identity/schema".into(),
            ));
        }
        let checkpoint: Checkpoint =
            codec::decode(bytes).map_err(|e| EngineError::Checkpoint(e.to_string()))?;
        checkpoint.size.validate()?;
        validate_palette(&checkpoint.palette)?;
        let events = Collector::default();
        let term = Term::from_snapshot(checkpoint.term, events.clone())
            .map_err(EngineError::Checkpoint)?;
        if term.columns() != checkpoint.size.columns as usize
            || term.screen_lines() != checkpoint.size.rows as usize
        {
            return Err(EngineError::Checkpoint(
                "engine size differs from grid dimensions".into(),
            ));
        }
        let processor =
            Processor::from_snapshot(checkpoint.processor).map_err(EngineError::Checkpoint)?;
        Ok(Self {
            term,
            processor,
            events,
            size: checkpoint.size,
            palette: checkpoint.palette,
            offset: checkpoint.offset,
            next_effect: checkpoint.next_effect,
            generation: 0,
            mode,
        })
    }

    pub fn checkpoint(&self) -> Result<Vec<u8>, EngineError> {
        let checkpoint = Checkpoint {
            magic: *MAGIC,
            offset: self.offset,
            next_effect: self.next_effect,
            size: self.size,
            palette: self.palette.clone(),
            term: self.term.snapshot(),
            processor: self.processor.snapshot(),
        };
        codec::encode(&checkpoint).map_err(|e| EngineError::Checkpoint(e.to_string()))
    }

    /// Bytes before offset have already been consumed, including partial escapes.
    pub fn offset(&self) -> u64 {
        self.offset
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn size(&self) -> Size {
        self.size
    }
    pub fn modes(&self) -> TermMode {
        *self.term.mode()
    }
    pub fn title(&self) -> Option<&str> {
        self.term.title()
    }
    pub fn cwd(&self) -> Option<&str> {
        self.term.cwd()
    }
    pub fn prompt_state(&self) -> PromptState {
        self.term.prompt_state()
    }
    pub fn is_syncing(&self) -> bool {
        self.processor.is_syncing()
    }

    /// The caller orders byte records and resizes. Overlapping resume prefixes
    /// are ignored; gaps fail before modifying parser state.
    pub fn feed(&mut self, offset: u64, bytes: &[u8]) -> Result<Vec<Effect>, EngineError> {
        if bytes.len() > MAX_OUTPUT_CHUNK {
            return Err(EngineError::OutputTooLarge(bytes.len()));
        }
        let end = offset
            .checked_add(bytes.len() as u64)
            .ok_or(EngineError::SequenceOverflow)?;
        if offset > self.offset {
            return Err(EngineError::OutputGap {
                expected: self.offset,
                got: offset,
            });
        }
        if end <= self.offset {
            return Ok(Vec::new());
        }
        let skip = (self.offset - offset) as usize;
        let mut effects = Vec::new();
        for chunk in bytes[skip..].chunks(16 * 1024) {
            self.processor.advance(&mut self.term, chunk);
            self.offset += chunk.len() as u64;
            self.collect_effects(&mut effects)?;
        }
        self.generation = self.generation.wrapping_add(1);
        Ok(effects)
    }

    /// Called from an authoritative ordered flush record, never an independent
    /// mirror timer. A resize also deterministically flushes before reflowing.
    pub fn flush_sync(&mut self) -> Result<Vec<Effect>, EngineError> {
        if !self.processor.is_syncing() {
            return Ok(Vec::new());
        }
        self.processor.flush_sync(&mut self.term);
        let mut effects = Vec::new();
        self.collect_effects(&mut effects)?;
        self.generation = self.generation.wrapping_add(1);
        Ok(effects)
    }

    pub fn resize(&mut self, size: Size) -> Result<Vec<Effect>, EngineError> {
        let size = size.validate()?;
        if size == self.size {
            return Ok(Vec::new());
        }
        let effects = self.flush_sync()?;
        self.term.resize(Geometry(size));
        self.size = size;
        self.generation = self.generation.wrapping_add(1);
        Ok(effects)
    }

    pub fn set_palette(&mut self, palette: Vec<u32>) -> Result<(), EngineError> {
        validate_palette(&palette)?;
        self.palette = palette;
        self.generation = self.generation.wrapping_add(1);
        Ok(())
    }

    fn collect_effects(&mut self, out: &mut Vec<Effect>) -> Result<(), EngineError> {
        let mut events = self.events.0.lock().expect("terminal event lock poisoned");
        for event in events.drain(..) {
            if let Some(kind) = effects::translate(event, self.size, &self.palette)? {
                let sequence = self.next_effect;
                self.next_effect = sequence
                    .checked_add(1)
                    .ok_or(EngineError::SequenceOverflow)?;
                if self.mode == EffectMode::Authoritative || !kind.external() {
                    out.push(Effect { sequence, kind });
                }
            }
        }
        Ok(())
    }

    pub fn scroll(&mut self, lines: i32) {
        self.term.scroll_display(Scroll::Delta(lines));
        self.generation = self.generation.wrapping_add(1);
    }

    pub fn set_focused(&mut self, focused: bool) {
        self.term.is_focused = focused;
    }

    /// Reuses all cell/palette/string storage between updates. Shaping and GPU
    /// resources remain in flc-terminal; only visible cells are copied here.
    pub fn render_into(&self, frame: &mut RenderFrame) {
        let columns = self.term.columns();
        let rows = self.term.screen_lines();
        let offset = self.term.grid().display_offset();
        frame.cells.resize_with(columns * rows, Cell::default);
        for row in 0..rows {
            for column in 0..columns {
                let point = Point::new(Line(row as i32 - offset as i32), Column(column));
                frame.cells[row * columns + column].clone_from(&self.term.grid()[point]);
            }
        }
        frame.colors.clone_from(&self.palette);
        for (index, color) in frame.colors.iter_mut().enumerate() {
            if let Some(value) = self.term.colors()[index] {
                *color = palette::pack(value);
            }
        }
        let cursor = self.term.renderable_content().cursor;
        frame.cursor = RenderCursor {
            row: cursor.point.line.0 + offset as i32,
            column: cursor.point.column.0 as u16,
            shape: cursor.shape,
        };
        frame.size = self.size;
        frame.modes = *self.term.mode();
        frame.display_offset = offset as u32;
        frame.history_lines = self.term.history_size() as u32;
        frame.generation = self.generation;
        frame.title.clear();
        if let Some(title) = self.term.title() {
            frame.title.push_str(title);
        }
        update_optional_string(&mut frame.cwd, self.term.cwd());
        frame.prompt = self.term.prompt_state();
    }

    pub fn text_between(&self, start: TextPoint, end: TextPoint) -> String {
        let clamp = |p: TextPoint| {
            Point::new(
                Line(
                    p.line
                        .clamp(self.term.topmost_line().0, self.term.bottommost_line().0),
                ),
                Column((p.column as usize).min(self.term.columns() - 1)),
            )
        };
        let (mut a, mut b) = (clamp(start), clamp(end));
        if (a.line.0, a.column.0) > (b.line.0, b.column.0) {
            std::mem::swap(&mut a, &mut b);
        }
        self.term.bounds_to_string(a, b)
    }

    pub fn history_page(&self, start: i32, count: u16) -> Vec<HistoryLine> {
        let start = start.clamp(self.term.topmost_line().0, self.term.bottommost_line().0);
        let count = (count as i32)
            .min(1000)
            .min(self.term.bottommost_line().0 - start + 1);
        let last = self.term.columns() as u16 - 1;
        (start..start + count)
            .map(|line| HistoryLine {
                line,
                text: self.text_between(
                    TextPoint { line, column: 0 },
                    TextPoint { line, column: last },
                ),
                wrapped: self.term.grid()[Point::new(Line(line), Column(last as usize))]
                    .flags
                    .contains(Flags::WRAPLINE),
            })
            .collect()
    }
}

fn validate_palette(palette: &[u32]) -> Result<(), EngineError> {
    if palette.len() != palette::COLOR_COUNT || palette.iter().any(|c| *c > 0xffffff) {
        return Err(EngineError::InvalidPalette);
    }
    Ok(())
}

fn update_optional_string(target: &mut Option<String>, source: Option<&str>) {
    match source {
        Some(source) => {
            let target = target.get_or_insert_with(String::new);
            if target != source {
                target.clear();
                target.push_str(source);
            }
        }
        None => *target = None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextPoint {
    /// Grid-relative row: scrollback is negative, visible rows start at zero.
    pub line: i32,
    pub column: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryLine {
    pub line: i32,
    pub text: String,
    pub wrapped: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderCursor {
    pub row: i32,
    pub column: u16,
    pub shape: CursorShape,
}

#[derive(Clone)]
pub struct RenderFrame {
    pub size: Size,
    pub cells: Vec<Cell>,
    pub colors: Vec<u32>,
    pub cursor: RenderCursor,
    pub modes: TermMode,
    pub display_offset: u32,
    pub history_lines: u32,
    pub generation: u64,
    pub title: String,
    pub cwd: Option<String>,
    pub prompt: PromptState,
}

impl Default for RenderFrame {
    fn default() -> Self {
        Self {
            size: Size::new(2, 1),
            cells: Vec::new(),
            colors: palette::default_palette(),
            cursor: RenderCursor {
                row: 0,
                column: 0,
                shape: CursorShape::Hidden,
            },
            modes: TermMode::empty(),
            display_offset: 0,
            history_lines: 0,
            generation: 0,
            title: String::new(),
            cwd: None,
            prompt: PromptState::Unknown,
        }
    }
}

#[cfg(test)]
mod tests;
