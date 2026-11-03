//! Predictive echo: a local overlay that disappears when the daemon's real
//! echo arrives or disagrees.
//!
//! Prediction is presentation only. It never changes canonical state, never
//! travels to the server as program input, and is dropped on any sign that
//! guessing would be wrong: full-screen programs, a terminal whose echo is
//! off (password prompts), a stale mirror, or an unexpected cell where the
//! prediction should have appeared.
use std::time::Duration;

use fl_term::{Cell, PromptState, RenderFrame, TermMode};
use unicode_width::UnicodeWidthChar;

/// How aggressively to predict.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum EchoPolicy {
    /// Never predict.
    Off,
    /// Predict only on a slow link at a confirmed shell prompt.
    #[default]
    Auto,
    /// Always predict, except where prediction is unsafe.
    On,
}

/// A link slower than this is where Auto starts predicting.
pub const AUTO_RTT: Duration = Duration::from_millis(150);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PredictionOutcome {
    /// Nothing was pending.
    Idle,
    /// The daemon's echo matched these characters; they are now real.
    Committed(usize),
    /// The daemon's output contradicted the prediction; it was dropped.
    RolledBack,
}

/// Conditions outside the terminal's own state that decide prediction.
#[derive(Debug, Clone, Copy)]
pub struct PredictionContext {
    pub policy: EchoPolicy,
    /// Current round-trip time, when the link reports one.
    pub rtt: Option<Duration>,
    /// The child accepted keyboard input (canonical `ECHO` is on).
    pub echo: bool,
    /// The published frame is a cached copy that may lag the daemon.
    pub stale: bool,
    pub exited: bool,
}

impl PredictionContext {
    /// Whether prediction may begin now. Both policies refuse full-screen
    /// programs, an exited child, a stale mirror, and a terminal with echo
    /// disabled (a password prompt prints nothing for typed keys).
    pub fn allows(&self, frame: &RenderFrame) -> bool {
        if self.stale || self.exited || !self.echo || frame.modes.contains(TermMode::ALT_SCREEN) {
            return false;
        }
        match self.policy {
            EchoPolicy::Off => false,
            EchoPolicy::On => true,
            EchoPolicy::Auto => {
                self.rtt.is_some_and(|rtt| rtt >= AUTO_RTT)
                    && matches!(
                        frame.prompt,
                        PromptState::Prompt | PromptState::Input | PromptState::Finished { .. }
                    )
            }
        }
    }
}

/// The live prediction: text not yet echoed by the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prediction {
    /// Grid row and first column, in the published frame's coordinates.
    pub row: i32,
    pub column: u16,
    /// Only single-width characters are predicted.
    pub text: String,
}

#[derive(Debug, Default)]
pub struct Predictor {
    pending: Option<Prediction>,
}

impl Predictor {
    pub fn pending(&self) -> Option<&Prediction> {
        self.pending.as_ref()
    }

    /// Discard the overlay (new canonical frame, focus change, shutdown).
    pub fn clear(&mut self) {
        self.pending = None;
    }

    /// Note typed bytes. Only printable, single-width text starts a prediction;
    /// any control byte (Enter, Ctrl+C, Escape, paste of a control sequence)
    /// clears it, because the daemon's echo of control input is not linear.
    pub fn typed(&mut self, frame: &RenderFrame, context: PredictionContext, bytes: &[u8]) {
        if bytes
            .iter()
            .any(|byte| *byte < 0x20 || *byte == 0x7f || *byte == 0x1b)
        {
            self.clear();
            return;
        }
        let Ok(text) = std::str::from_utf8(bytes) else {
            self.clear();
            return;
        };
        if text.is_empty() || !context.allows(frame) {
            return;
        }
        let current = self.pending.as_ref();
        if let Some(prediction) = current
            && prediction.row != frame.cursor.row
        {
            // A scroll or cursor jump moved the insertion point; do not extend.
            self.clear();
        }
        let (row, column) = match &self.pending {
            Some(prediction) => {
                let width: u16 = prediction
                    .text
                    .chars()
                    .map(|character| character.width().unwrap_or(1) as u16)
                    .sum();
                (prediction.row, prediction.column + width)
            }
            None => (frame.cursor.row, frame.cursor.column),
        };
        let predicted: String = text
            .chars()
            .filter(|character| character.width() == Some(1))
            .collect();
        if predicted.is_empty() {
            return;
        }
        match &mut self.pending {
            Some(prediction) => {
                prediction.text.push_str(&predicted);
            }
            None => {
                self.pending = Some(Prediction {
                    row,
                    column,
                    text: predicted,
                });
            }
        }
    }

    /// Compare the pending prediction with the daemon's current frame.
    ///
    /// Matching cells commit their characters; a cell holding some other
    /// non-blank character means the guess was wrong and the overlay is
    /// dropped. Blank cells are simply "not echoed yet", so a slow echo wins.
    pub fn reconcile(&mut self, frame: &RenderFrame) -> PredictionOutcome {
        let Some(prediction) = &mut self.pending else {
            return PredictionOutcome::Idle;
        };
        if frame.modes.contains(TermMode::ALT_SCREEN) {
            self.pending = None;
            return PredictionOutcome::RolledBack;
        }
        let columns = frame.size.columns as usize;
        let mut matched = 0usize;
        for (index, character) in prediction.text.char_indices() {
            let column = prediction.column as usize + index;
            if prediction.row < 0 || (prediction.row as usize) >= frame.size.rows as usize {
                self.pending = None;
                return PredictionOutcome::RolledBack;
            }
            let Some(cell) = frame.cells.get(prediction.row as usize * columns + column) else {
                self.pending = None;
                return PredictionOutcome::RolledBack;
            };
            if cell.c == character {
                matched += character.len_utf8();
                continue;
            }
            if blank(cell) {
                break;
            }
            self.pending = None;
            return PredictionOutcome::RolledBack;
        }
        if matched == 0 {
            return PredictionOutcome::Idle;
        }
        let prediction = self.pending.as_mut().expect("checked pending");
        prediction.text.drain(..matched);
        if prediction.text.is_empty() {
            self.pending = None;
        } else {
            // Advance the anchor past the committed characters.
            prediction.column = frame.cursor.column;
            prediction.row = frame.cursor.row;
        }
        PredictionOutcome::Committed(matched)
    }
}

fn blank(cell: &Cell) -> bool {
    // A cell that still holds nothing is "the echo has not arrived yet".
    matches!(cell.c, ' ' | '\0')
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_term::{EffectMode, Engine, Size};

    fn frame(engine: &Engine) -> RenderFrame {
        let mut frame = RenderFrame::default();
        engine.render_into(&mut frame);
        frame
    }

    /// A confirmed shell prompt: OSC 133 marks it, as a real shell does.
    fn engine() -> Engine {
        let mut engine = Engine::new(Size::new(20, 4), EffectMode::Mirror).unwrap();
        engine.feed(0, b"\x1b]133;A\x07$ ").unwrap();
        engine
    }

    fn context(policy: EchoPolicy, rtt: Duration) -> PredictionContext {
        PredictionContext {
            policy,
            rtt: Some(rtt),
            echo: true,
            stale: false,
            exited: false,
        }
    }

    #[test]
    fn matched_echo_commits_and_advances_the_anchor() {
        let mut engine = engine();
        let mut predictor = Predictor::default();
        predictor.typed(
            &frame(&engine),
            context(EchoPolicy::On, Duration::ZERO),
            b"ls",
        );
        let prediction = predictor.pending().unwrap().clone();
        assert_eq!((prediction.row, prediction.column), (0, 2));
        assert_eq!(prediction.text, "ls");
        let offset = engine.offset();
        engine.feed(offset, b"l").unwrap();
        assert_eq!(
            predictor.reconcile(&frame(&engine)),
            PredictionOutcome::Committed(1)
        );
        assert_eq!(predictor.pending().unwrap().text, "s");
        let offset = engine.offset();
        engine.feed(offset, b"s").unwrap();
        assert_eq!(
            predictor.reconcile(&frame(&engine)),
            PredictionOutcome::Committed(1)
        );
        assert!(predictor.pending().is_none());
    }

    #[test]
    fn a_different_character_rolls_back_the_whole_overlay() {
        let mut engine = engine();
        let mut predictor = Predictor::default();
        predictor.typed(
            &frame(&engine),
            context(EchoPolicy::On, Duration::ZERO),
            b"lx",
        );
        // The shell completed the line into something else (`ls`, history, a
        // completion popup): the prediction is no longer trustworthy.
        let offset = engine.offset();
        engine.feed(offset, b"lZ").unwrap();
        assert_eq!(
            predictor.reconcile(&frame(&engine)),
            PredictionOutcome::RolledBack
        );
        assert!(predictor.pending().is_none());
    }

    #[test]
    fn control_input_and_full_screen_programs_never_predict() {
        let mut engine = engine();
        let mut predictor = Predictor::default();
        predictor.typed(
            &frame(&engine),
            context(EchoPolicy::On, Duration::ZERO),
            b"a",
        );
        predictor.typed(
            &frame(&engine),
            context(EchoPolicy::On, Duration::ZERO),
            b"\r",
        );
        assert!(predictor.pending().is_none());
        // Alternate screen (vim, less): nothing is guessed.
        engine.feed(engine.offset(), b"\x1b[?1049h").unwrap();
        let alternate = frame(&engine);
        assert!(alternate.modes.contains(TermMode::ALT_SCREEN));
        predictor.typed(&alternate, context(EchoPolicy::On, Duration::ZERO), b"i");
        assert!(predictor.pending().is_none());
    }

    #[test]
    fn auto_requires_a_slow_link_and_a_confirmed_prompt() {
        let engine = engine();
        let mut predictor = Predictor::default();
        predictor.typed(
            &frame(&engine),
            context(EchoPolicy::Auto, Duration::from_millis(149)),
            b"x",
        );
        assert!(predictor.pending().is_none());
        predictor.typed(
            &frame(&engine),
            context(EchoPolicy::Auto, Duration::from_millis(200)),
            b"x",
        );
        assert_eq!(predictor.pending().unwrap().text, "x");
    }

    #[test]
    fn disabled_echo_and_stale_mirrors_are_never_predicted() {
        let engine = engine();
        let mut predictor = Predictor::default();
        let mut context = context(EchoPolicy::On, Duration::ZERO);
        context.echo = false;
        predictor.typed(&frame(&engine), context, b"secret");
        assert!(predictor.pending().is_none());
        context.echo = true;
        context.stale = true;
        predictor.typed(&frame(&engine), context, b"secret");
        assert!(predictor.pending().is_none());
    }

    #[test]
    fn wide_characters_are_not_predicted_but_do_not_break_ascii() {
        let mut engine = engine();
        let mut predictor = Predictor::default();
        predictor.typed(
            &frame(&engine),
            context(EchoPolicy::On, Duration::ZERO),
            "a界b".as_bytes(),
        );
        assert_eq!(predictor.pending().unwrap().text, "ab");
        let offset = engine.offset();
        engine.feed(offset, b"ab").unwrap();
        assert_eq!(
            predictor.reconcile(&frame(&engine)),
            PredictionOutcome::Committed(2)
        );
    }
}
