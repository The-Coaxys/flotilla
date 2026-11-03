//! Pointer input that is not selection: application mouse reporting and
//! wheel scrolling.
use fl_term::TermMode;
use fl_term::input::{Modifiers, MouseButton, MouseEvent, MouseKind, encode_mouse};

/// Converts wheel and touchpad deltas into whole lines, carrying the
/// fractional remainder between events so slow precise scrolling still
/// moves. A direction change discards the opposite remainder.
#[derive(Debug, Default, Clone, Copy)]
pub struct WheelAccumulator {
    remainder: f32,
}

impl WheelAccumulator {
    /// `lines`: positive scrolls toward older output (wheel up).
    pub fn push(&mut self, lines: f32) -> i32 {
        if !lines.is_finite() || lines == 0.0 {
            return 0;
        }
        if self.remainder != 0.0 && self.remainder.signum() != lines.signum() {
            self.remainder = 0.0;
        }
        self.remainder += lines;
        let whole = self.remainder.trunc();
        self.remainder -= whole;
        whole as i32
    }

    pub fn push_pixels(&mut self, pixels: f32, line_height: f32) -> i32 {
        self.push(pixels / line_height)
    }

    /// A new touch gesture starts from zero.
    pub fn reset(&mut self) {
        self.remainder = 0.0;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WheelAction {
    None,
    /// Bytes for the application: wheel reports, or cursor keys for
    /// alternate-screen alternate scroll.
    Input(Vec<u8>),
    /// Move the local viewport by this many lines (positive = older).
    Scroll(i32),
}

/// Most wheel steps reported for one event; a flung touchpad must not
/// flood a remote application.
const MAX_WHEEL_REPORTS: u32 = 32;

pub fn wheel_action(
    lines: i32,
    column: u16,
    row: u16,
    modifiers: Modifiers,
    modes: TermMode,
) -> WheelAction {
    if lines == 0 {
        return WheelAction::None;
    }
    let reporting = modes.intersects(TermMode::MOUSE_MODE);
    let alternate_scroll = modes.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL);
    // Shift bypasses the application and scrolls locally.
    if modifiers.shift || !(reporting || alternate_scroll) {
        return if modes.contains(TermMode::ALT_SCREEN) {
            // The alternate screen has no scrollback.
            WheelAction::None
        } else {
            WheelAction::Scroll(lines)
        };
    }
    let event = MouseEvent {
        button: if lines > 0 {
            MouseButton::WheelUp
        } else {
            MouseButton::WheelDown
        },
        kind: MouseKind::Press,
        column,
        row,
        modifiers,
    };
    let Some(step) = encode_mouse(&event, modes) else {
        return WheelAction::None;
    };
    let count = lines.unsigned_abs().min(MAX_WHEEL_REPORTS) as usize;
    WheelAction::Input(step.repeat(count))
}

/// Tracks the held button and last reported cell so motion is reported once
/// per cell and a release is reported even when it happens outside the pane.
#[derive(Debug, Default, Clone, Copy)]
pub struct MouseReporter {
    held: Option<MouseButton>,
    last_cell: Option<(u16, u16)>,
}

impl MouseReporter {
    pub fn held(&self) -> Option<MouseButton> {
        self.held
    }

    pub fn press(
        &mut self,
        button: MouseButton,
        column: u16,
        row: u16,
        modifiers: Modifiers,
        modes: TermMode,
    ) -> Option<Vec<u8>> {
        self.held = Some(button);
        self.last_cell = Some((column, row));
        encode_mouse(
            &MouseEvent {
                button,
                kind: MouseKind::Press,
                column,
                row,
                modifiers,
            },
            modes,
        )
    }

    pub fn motion(
        &mut self,
        column: u16,
        row: u16,
        modifiers: Modifiers,
        modes: TermMode,
    ) -> Option<Vec<u8>> {
        if self.last_cell == Some((column, row)) {
            return None;
        }
        let bytes = encode_mouse(
            &MouseEvent {
                button: self.held.unwrap_or(MouseButton::None),
                kind: MouseKind::Move,
                column,
                row,
                modifiers,
            },
            modes,
        )?;
        self.last_cell = Some((column, row));
        Some(bytes)
    }

    /// Releases the held button, if it is `button`.
    pub fn release(
        &mut self,
        button: MouseButton,
        column: u16,
        row: u16,
        modifiers: Modifiers,
        modes: TermMode,
    ) -> Option<Vec<u8>> {
        if self.held != Some(button) {
            return None;
        }
        self.held = None;
        self.last_cell = Some((column, row));
        encode_mouse(
            &MouseEvent {
                button,
                kind: MouseKind::Release,
                column,
                row,
                modifiers,
            },
            modes,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_term::{EffectMode, Engine, Size};

    fn modes_after(bytes: &[u8]) -> TermMode {
        let mut engine = Engine::new(Size::new(80, 24), EffectMode::Mirror).unwrap();
        engine.feed(0, bytes).unwrap();
        engine.modes()
    }

    #[test]
    fn fractional_wheel_deltas_accumulate_into_whole_lines() {
        let mut wheel = WheelAccumulator::default();
        // A precise touchpad delivering a third of a line per event.
        assert_eq!(wheel.push_pixels(16.0 / 3.0, 16.0), 0);
        assert_eq!(wheel.push_pixels(16.0 / 3.0, 16.0), 0);
        assert_eq!(wheel.push_pixels(16.0 / 3.0 + 0.01, 16.0), 1);
        assert_eq!(wheel.push(2.5), 2);
        assert_eq!(wheel.push(0.6), 1);
        // Reversing discards the leftover upward fraction.
        assert_eq!(wheel.push(-0.7), 0);
        assert_eq!(wheel.push(-0.4), -1);
        wheel.reset();
        assert_eq!(wheel.push(0.9), 0);
        assert_eq!(wheel.push(f32::NAN), 0);
    }

    #[test]
    fn wheel_scrolls_scrollback_on_the_primary_screen() {
        let modes = modes_after(b"");
        assert_eq!(
            wheel_action(3, 0, 0, Modifiers::default(), modes),
            WheelAction::Scroll(3)
        );
    }

    #[test]
    fn alternate_screen_wheel_becomes_cursor_keys() {
        // less/vim: alternate screen with alternate scroll (enabled by default).
        let modes = modes_after(b"\x1b[?1049h");
        assert!(modes.contains(TermMode::ALTERNATE_SCROLL));
        assert_eq!(
            wheel_action(2, 0, 0, Modifiers::default(), modes),
            WheelAction::Input(b"\x1b[A\x1b[A".to_vec())
        );
        let app_cursor = modes_after(b"\x1b[?1049h\x1b[?1h");
        assert_eq!(
            wheel_action(-1, 0, 0, Modifiers::default(), app_cursor),
            WheelAction::Input(b"\x1bOB".to_vec())
        );
        // Without alternate scroll the alternate screen has nothing to scroll.
        let plain_alt = modes_after(b"\x1b[?1049h\x1b[?1007l");
        assert_eq!(
            wheel_action(2, 0, 0, Modifiers::default(), plain_alt),
            WheelAction::None
        );
    }

    #[test]
    fn mouse_reporting_takes_the_wheel_unless_shift_is_held() {
        let modes = modes_after(b"\x1b[?1000h\x1b[?1006h");
        assert_eq!(
            wheel_action(2, 4, 1, Modifiers::default(), modes),
            WheelAction::Input(b"\x1b[<64;5;2M\x1b[<64;5;2M".to_vec())
        );
        let shift = Modifiers {
            shift: true,
            ..Default::default()
        };
        assert_eq!(
            wheel_action(-1, 4, 1, shift, modes),
            WheelAction::Scroll(-1)
        );
        assert_eq!(
            wheel_action(-100, 0, 0, Modifiers::default(), modes),
            WheelAction::Input(b"\x1b[<65;1;1M".repeat(32))
        );
    }

    #[test]
    fn drag_reports_once_per_cell_and_release_outside_still_reports() {
        // Button-event tracking with SGR coordinates.
        let modes = modes_after(b"\x1b[?1002h\x1b[?1006h");
        let none = Modifiers::default();
        let mut reporter = MouseReporter::default();
        assert_eq!(
            reporter.press(MouseButton::Left, 2, 3, none, modes),
            Some(b"\x1b[<0;3;4M".to_vec())
        );
        assert_eq!(reporter.motion(2, 3, none, modes), None, "same cell");
        assert_eq!(
            reporter.motion(5, 3, none, modes),
            Some(b"\x1b[<32;6;4M".to_vec())
        );
        assert_eq!(reporter.motion(5, 3, none, modes), None);
        // Released after leaving the pane: clamped edge coordinates.
        assert_eq!(
            reporter.release(MouseButton::Left, 79, 23, none, modes),
            Some(b"\x1b[<0;80;24m".to_vec())
        );
        assert_eq!(reporter.held(), None);
        assert_eq!(reporter.release(MouseButton::Left, 0, 0, none, modes), None);
        // Click-only tracking ignores motion without a held button.
        let click_only = modes_after(b"\x1b[?1000h");
        assert_eq!(reporter.motion(1, 1, none, click_only), None);
    }
}
