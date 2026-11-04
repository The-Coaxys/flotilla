//! Platform-neutral terminal input, using the terminal's negotiated modes.
//!
//! Keyboard encodings follow the [kitty keyboard protocol](https://sw.kovidgoyal.net/kitty/keyboard-protocol/)
//! and the DEC/xterm application cursor/keypad conventions. `Some(bytes)` consumes
//! a key event: the native adapter must not also forward its printable text
//! commit. `None` leaves printable text/IME commits to the native text path, or
//! suppresses an unrequested release/unsupported key. IME preedit is never input
//! to this encoder. Native adapters must distinguish AltGr from Ctrl+Alt.
//!
//! The API carries the four common shortcut modifiers, not lock state,
//! Hyper/Meta, physical modifier-key identities, or arbitrary IME text-only
//! events. Those cannot be reconstructed here. Keypad digit versus navigation
//! identity must be resolved by the native adapter. If its Num Lock policy
//! overrides application-keypad mode, it submits `Character`/normal navigation
//! identity instead of a keypad identity for that event.

use alacritty_terminal::term::TermMode;

#[derive(Default, Copy, Clone, Debug, Eq, PartialEq)]
pub struct Modifiers {
    pub shift: bool,
    pub alt: bool,
    pub control: bool,
    pub super_key: bool,
    pub alt_graph: bool,
}

impl Modifiers {
    fn effective(self) -> Self {
        if self.alt_graph {
            // Windows represents AltGr as a synthetic Ctrl+Alt chord. Neither
            // modifier is a terminal shortcut modifier in this case.
            Self {
                alt: false,
                control: false,
                ..self
            }
        } else {
            self
        }
    }

    fn bits(self) -> u8 {
        let modifiers = self.effective();
        u8::from(modifiers.shift)
            | (u8::from(modifiers.alt) << 1)
            | (u8::from(modifiers.control) << 2)
            | (u8::from(modifiers.super_key) << 3)
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Key {
    /// Unshifted logical key in the active layout, not its physical US key.
    Character(char),
    Enter,
    Tab,
    Backspace,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    Insert,
    Delete,
    PageUp,
    PageDown,
    /// F1 through F35. Other numbers are unsupported, not approximated.
    Function(u8),
    /// Numeric keypad identity, with a value in 0..=9.
    KeypadDigit(u8),
    KeypadDecimal,
    KeypadDivide,
    KeypadMultiply,
    KeypadSubtract,
    KeypadAdd,
    KeypadEnter,
    KeypadEqual,
    KeypadSeparator,
    KeypadLeft,
    KeypadRight,
    KeypadUp,
    KeypadDown,
    KeypadPageUp,
    KeypadPageDown,
    KeypadHome,
    KeypadEnd,
    KeypadInsert,
    KeypadDelete,
    KeypadBegin,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum KeyState {
    Press,
    Repeat,
    Release,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyEvent {
    pub key: Key,
    pub modifiers: Modifiers,
    pub state: KeyState,
    /// Text produced by this key, if known; not IME preedit or a later commit.
    pub text: Option<String>,
    /// The shifted logical key. Reported only while Shift is active.
    pub shifted_key: Option<char>,
    /// The corresponding unshifted PC-101 key, if the platform knows it.
    pub base_layout_key: Option<char>,
}

/// Encode a key without querying or mutating terminal state.
///
/// Plain printable keys return `None` unless report-all is negotiated, even if
/// alternate-key/event-type reporting is enabled: those flags do not take over
/// the text path. Enter, Tab and Backspace retain their unmodified legacy
/// encoding until report-all is enabled, and do not report releases before it.
/// Associated text is included only with report-all, never on release, and
/// never includes C0/DEL/C1 controls. Invalid F/keypad numbers return `None`.
pub fn encode_key(event: &KeyEvent, mode: TermMode) -> Option<Vec<u8>> {
    let report_all = mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC);
    let report_events = mode.contains(TermMode::REPORT_EVENT_TYPES);
    if event.state == KeyState::Release && !report_events {
        return None;
    }

    let modifiers = event.modifiers.effective();
    let bits = modifiers.bits();
    let enhanced = mode.intersects(
        TermMode::DISAMBIGUATE_ESC_CODES
            | TermMode::REPORT_EVENT_TYPES
            | TermMode::REPORT_ALL_KEYS_AS_ESC,
    );
    let distinguish_keypad =
        mode.intersects(TermMode::DISAMBIGUATE_ESC_CODES | TermMode::REPORT_ALL_KEYS_AS_ESC);

    let mut key = event.key;
    if let Some((normal, application)) = keypad_key(key) {
        if distinguish_keypad {
            let (number, suffix) = kitty_key(key)?;
            return Some(encode_csi(event, mode, number, suffix, false));
        }
        if mode.contains(TermMode::APP_KEYPAD) && !enhanced {
            return Some(encode_application_keypad(application, bits));
        }
        key = normal;
    }

    if let Key::Character(character) = key {
        if character.is_control() {
            return None;
        }
        if !report_all && !modifiers.alt && !modifiers.control && !modifiers.super_key {
            // A native text commit preserves composition, dead keys, combining
            // sequences and non-BMP characters without duplicate UTF-8 bytes.
            return None;
        }
        if !enhanced && let Some(bytes) = legacy_character(event, character, bits) {
            return Some(bytes);
        }
        return Some(encode_csi(event, mode, character as u32, b'u', true));
    }

    if !report_all
        && matches!(key, Key::Enter | Key::Tab | Key::Backspace)
        && (bits == 0 || (!enhanced && !modifiers.super_key))
    {
        if event.state == KeyState::Release {
            return None;
        }
        let mut bytes = Vec::with_capacity(5);
        if modifiers.alt {
            bytes.push(0x1b);
        }
        match key {
            Key::Enter => {
                bytes.push(b'\r');
                if mode.contains(TermMode::LINE_FEED_NEW_LINE) {
                    bytes.push(b'\n');
                }
            }
            Key::Tab if modifiers.shift => bytes.extend_from_slice(b"\x1b[Z"),
            Key::Tab => bytes.push(b'\t'),
            Key::Backspace => bytes.push(if modifiers.control { 8 } else { 127 }),
            _ => unreachable!(),
        }
        return Some(bytes);
    }

    if !enhanced && !modifiers.super_key && key == Key::Escape {
        return Some(if modifiers.alt {
            b"\x1b\x1b".to_vec()
        } else {
            vec![0x1b]
        });
    }

    let (number, suffix) = kitty_key(key)?;
    if !enhanced && bits == 0 {
        let application = match key {
            Key::Up => Some(b'A'),
            Key::Down => Some(b'B'),
            Key::Right => Some(b'C'),
            Key::Left => Some(b'D'),
            Key::Home => Some(b'H'),
            Key::End => Some(b'F'),
            // KP_BEGIN is not a digit; it is the Num-Lock-off center key.
            Key::KeypadBegin => Some(b'E'),
            _ => None,
        };
        if let Some(suffix) = application.filter(|_| mode.contains(TermMode::APP_CURSOR)) {
            return Some(vec![0x1b, b'O', suffix]);
        }
        if let Key::Function(function @ 1..=4) = key {
            return Some(vec![0x1b, b'O', b'P' + function - 1]);
        }
    }
    Some(encode_csi(event, mode, number, suffix, false))
}

// Native Num-Lock resolution selects a digit or navigation identity. Invalid
// digits fall through to kitty_key, which rejects them instead of inventing a
// sequence. KP_BEGIN retains its normal identity for the canonical CSI E form.
fn keypad_key(key: Key) -> Option<(Key, u8)> {
    let value = match key {
        Key::KeypadDigit(digit @ 0..=9) => (Key::Character(char::from(b'0' + digit)), b'p' + digit),
        Key::KeypadDigit(_) => return None,
        Key::KeypadDecimal => (Key::Character('.'), b'n'),
        Key::KeypadDivide => (Key::Character('/'), b'o'),
        Key::KeypadMultiply => (Key::Character('*'), b'j'),
        Key::KeypadSubtract => (Key::Character('-'), b'm'),
        Key::KeypadAdd => (Key::Character('+'), b'k'),
        Key::KeypadEnter => (Key::Enter, b'M'),
        Key::KeypadEqual => (Key::Character('='), b'X'),
        Key::KeypadSeparator => (Key::Character(','), b'l'),
        Key::KeypadLeft => (Key::Left, b't'),
        Key::KeypadRight => (Key::Right, b'v'),
        Key::KeypadUp => (Key::Up, b'x'),
        Key::KeypadDown => (Key::Down, b'r'),
        Key::KeypadPageUp => (Key::PageUp, b'y'),
        Key::KeypadPageDown => (Key::PageDown, b's'),
        Key::KeypadHome => (Key::Home, b'w'),
        Key::KeypadEnd => (Key::End, b'q'),
        Key::KeypadInsert => (Key::Insert, b'p'),
        Key::KeypadDelete => (Key::Delete, b'n'),
        Key::KeypadBegin => (Key::KeypadBegin, b'u'),
        _ => return None,
    };
    Some(value)
}

fn encode_application_keypad(suffix: u8, bits: u8) -> Vec<u8> {
    if bits == 0 {
        vec![0x1b, b'O', suffix]
    } else {
        let mut bytes = Vec::with_capacity(8);
        bytes.extend_from_slice(b"\x1bO");
        push_decimal(&mut bytes, u32::from(bits) + 1);
        bytes.push(suffix);
        bytes
    }
}

fn legacy_character(event: &KeyEvent, character: char, mut bits: u8) -> Option<Vec<u8>> {
    let mut character = character;
    // In a non-Latin layout, the native physical/base key can preserve legacy
    // Ctrl-C and similar application shortcuts. Do not substitute it in kitty.
    if !character.is_ascii() && bits & 1 == 0 && bits & 8 == 0 {
        character = event.base_layout_key.filter(char::is_ascii)?;
    }
    if !character.is_ascii() {
        return None;
    }
    if character.is_ascii_uppercase() && bits & 4 != 0 {
        character = character.to_ascii_lowercase();
    }
    if bits & 1 != 0 {
        let shifted = event.shifted_key.or_else(|| {
            let text = event.text.as_deref()?;
            let mut characters = text.chars();
            let first = characters.next()?;
            characters.next().is_none().then_some(first)
        });
        if let Some(shifted) = shifted.filter(|shifted| {
            shifted.is_ascii()
                && *shifted != character
                && (bits & 4 == 0 || !character.is_ascii_lowercase())
        }) {
            character = shifted;
            bits &= !1;
        }
    }
    let byte = character as u8;
    let byte = match bits {
        2 => byte,
        4 | 6 => control_byte(byte),
        5 if byte == b' ' => 0,
        3 if byte == b' ' => b' ',
        _ => return None,
    };
    Some(if bits & 2 != 0 {
        vec![0x1b, byte]
    } else {
        vec![byte]
    })
}

fn control_byte(byte: u8) -> u8 {
    match byte {
        b'a'..=b'z' | b'A'..=b'Z' => byte & 0x1f,
        b' ' | b'@' | b'2' => 0,
        b'[' | b'3' => 27,
        b'\\' | b'4' => 28,
        b']' | b'5' => 29,
        b'^' | b'~' | b'6' => 30,
        b'_' | b'/' | b'7' => 31,
        b'?' | b'8' => 127,
        _ => byte,
    }
}

fn kitty_key(key: Key) -> Option<(u32, u8)> {
    let value = match key {
        Key::Character(character) => (character as u32, b'u'),
        Key::Escape => (27, b'u'),
        Key::Enter => (13, b'u'),
        Key::Tab => (9, b'u'),
        Key::Backspace => (127, b'u'),
        Key::Insert => (2, b'~'),
        Key::Delete => (3, b'~'),
        Key::PageUp => (5, b'~'),
        Key::PageDown => (6, b'~'),
        Key::Up => (1, b'A'),
        Key::Down => (1, b'B'),
        Key::Right => (1, b'C'),
        Key::Left => (1, b'D'),
        Key::Home => (1, b'H'),
        Key::End => (1, b'F'),
        Key::Function(1) => (1, b'P'),
        Key::Function(2) => (1, b'Q'),
        // CSI R collides with the cursor-position report; current kitty uses ~.
        Key::Function(3) => (13, b'~'),
        Key::Function(4) => (1, b'S'),
        Key::Function(5) => (15, b'~'),
        Key::Function(function @ 6..=10) => (u32::from(function) + 11, b'~'),
        Key::Function(function @ 11..=12) => (u32::from(function) + 12, b'~'),
        Key::Function(function @ 13..=35) => (u32::from(function) + 57363, b'u'),
        Key::Function(_) => return None,
        Key::KeypadDigit(digit @ 0..=9) => (57399 + u32::from(digit), b'u'),
        Key::KeypadDigit(_) => return None,
        Key::KeypadDecimal => (57409, b'u'),
        Key::KeypadDivide => (57410, b'u'),
        Key::KeypadMultiply => (57411, b'u'),
        Key::KeypadSubtract => (57412, b'u'),
        Key::KeypadAdd => (57413, b'u'),
        Key::KeypadEnter => (57414, b'u'),
        Key::KeypadEqual => (57415, b'u'),
        Key::KeypadSeparator => (57416, b'u'),
        Key::KeypadLeft => (57417, b'u'),
        Key::KeypadRight => (57418, b'u'),
        Key::KeypadUp => (57419, b'u'),
        Key::KeypadDown => (57420, b'u'),
        Key::KeypadPageUp => (57421, b'u'),
        Key::KeypadPageDown => (57422, b'u'),
        Key::KeypadHome => (57423, b'u'),
        Key::KeypadEnd => (57424, b'u'),
        Key::KeypadInsert => (57425, b'u'),
        Key::KeypadDelete => (57426, b'u'),
        Key::KeypadBegin => (1, b'E'),
    };
    Some(value)
}

fn encode_csi(
    event: &KeyEvent,
    mode: TermMode,
    number: u32,
    suffix: u8,
    alternates: bool,
) -> Vec<u8> {
    let bits = event.modifiers.bits();
    let report_event =
        mode.contains(TermMode::REPORT_EVENT_TYPES) && event.state != KeyState::Press;
    let shifted = event.shifted_key.filter(|_| event.modifiers.shift);
    let base = event.base_layout_key;
    let report_alternates = alternates
        && mode.contains(TermMode::REPORT_ALTERNATE_KEYS)
        && (shifted.is_some() || base.is_some());
    let text = event.text.as_deref().filter(|text| {
        mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC | TermMode::REPORT_ASSOCIATED_TEXT)
            && event.state != KeyState::Release
            && !text.is_empty()
            && !text.chars().any(char::is_control)
    });
    // Four output bytes per UTF-8 input byte bounds decimal codepoints plus
    // separators. The header bound includes both seven-digit alternate keys.
    let text_capacity = text.map_or(0, |text| text.len().saturating_mul(4));
    let mut bytes = Vec::with_capacity(40usize.saturating_add(text_capacity));
    bytes.extend_from_slice(b"\x1b[");
    if number != 1
        || suffix == b'u'
        || bits != 0
        || report_event
        || report_alternates
        || text.is_some()
    {
        push_decimal(&mut bytes, number);
    }
    if report_alternates {
        bytes.push(b':');
        if let Some(shifted) = shifted {
            push_decimal(&mut bytes, shifted as u32);
        }
        if let Some(base) = base {
            bytes.push(b':');
            push_decimal(&mut bytes, base as u32);
        }
    }
    if bits != 0 || report_event || text.is_some() {
        bytes.push(b';');
        if bits != 0 || report_event {
            push_decimal(&mut bytes, u32::from(bits) + 1);
        }
        if report_event {
            bytes.push(b':');
            bytes.push(match event.state {
                KeyState::Press => b'1',
                KeyState::Repeat => b'2',
                KeyState::Release => b'3',
            });
        }
    }
    if let Some(text) = text {
        bytes.push(b';');
        for (index, character) in text.chars().enumerate() {
            if index != 0 {
                bytes.push(b':');
            }
            push_decimal(&mut bytes, character as u32);
        }
    }
    bytes.push(suffix);
    bytes
}

fn push_decimal(bytes: &mut Vec<u8>, mut number: u32) {
    let mut digits = [0; 10];
    let mut start = digits.len();
    loop {
        start -= 1;
        digits[start] = b'0' + (number % 10) as u8;
        number /= 10;
        if number == 0 {
            break;
        }
    }
    bytes.extend_from_slice(&digits[start..]);
}

/// Preserve multiline Unicode, normalizing Windows CRLF to one LF. Standalone
/// CR and LF are otherwise unchanged in both modes. Add exactly one surrounding
/// bracketed-paste pair when negotiated; `text` is raw clipboard content, not a
/// previously encoded paste. Clipboard/control-character policy belongs to the
/// native paste action, not a second keyboard/IME encoding pass.
pub fn encode_paste(text: &str, mode: TermMode) -> Vec<u8> {
    let bracketed = mode.contains(TermMode::BRACKETED_PASTE);
    let mut bytes = Vec::with_capacity(text.len().saturating_add(if bracketed { 12 } else { 0 }));
    if bracketed {
        bytes.extend_from_slice(b"\x1b[200~");
    }
    let mut source = text.bytes().peekable();
    while let Some(byte) = source.next() {
        if byte == b'\r' && source.peek() == Some(&b'\n') {
            continue;
        }
        bytes.push(byte);
    }
    if bracketed {
        bytes.extend_from_slice(b"\x1b[201~");
    }
    bytes
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    /// No held button, for all-motion reports (not an unidentified release).
    None,
    WheelUp,
    WheelDown,
    WheelLeft,
    WheelRight,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MouseKind {
    Press,
    Release,
    Move,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct MouseEvent {
    /// The changed button for a press/release, or held button for motion.
    pub button: MouseButton,
    pub kind: MouseKind,
    /// Zero-based cell coordinates. Never silently clamped or wrapped.
    pub column: u16,
    pub row: u16,
    pub modifiers: Modifiers,
}

/// Encode DEC 1000/1002/1003 tracking with SGR, UTF-8, or legacy coordinates.
/// SGR takes precedence over UTF-8. Unrepresentable coordinates return `None`
/// (legacy: one-based 223; UTF-8: 2015); SGR supports the entire input range.
/// Wheel releases/motion and unidentified releases are not protocol events.
/// The caller tracks held buttons and suppresses same-cell duplicate motion.
/// Without reporting, alternate-screen alternate-scroll maps vertical wheel
/// presses to cursor keys; otherwise scrolling remains a local UI action.
pub fn encode_mouse(event: &MouseEvent, mode: TermMode) -> Option<Vec<u8>> {
    let wheel = matches!(
        event.button,
        MouseButton::WheelUp
            | MouseButton::WheelDown
            | MouseButton::WheelLeft
            | MouseButton::WheelRight
    );
    if !mode.intersects(TermMode::MOUSE_MODE) {
        if event.kind != MouseKind::Press
            || !mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL)
        {
            return None;
        }
        let key = match event.button {
            MouseButton::WheelUp => Key::Up,
            MouseButton::WheelDown => Key::Down,
            _ => return None,
        };
        return encode_key(
            &KeyEvent {
                key,
                modifiers: event.modifiers,
                state: KeyState::Press,
                text: None,
                shifted_key: None,
                base_layout_key: None,
            },
            mode,
        );
    }
    if wheel && event.kind != MouseKind::Press {
        return None;
    }
    if event.kind == MouseKind::Move {
        let all_motion = mode.contains(TermMode::MOUSE_MOTION);
        let drag = mode.contains(TermMode::MOUSE_DRAG) && event.button != MouseButton::None;
        if !all_motion && !drag {
            return None;
        }
    } else if event.button == MouseButton::None {
        return None;
    }

    let mut button = match event.button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
        MouseButton::None => 3,
        MouseButton::WheelUp => 64,
        MouseButton::WheelDown => 65,
        MouseButton::WheelLeft => 66,
        MouseButton::WheelRight => 67,
    };
    let sgr = mode.contains(TermMode::SGR_MOUSE);
    if event.kind == MouseKind::Release && !sgr {
        button = 3;
    }
    if event.kind == MouseKind::Move {
        button += 32;
    }
    let modifiers = event.modifiers.effective();
    button |= u8::from(modifiers.shift) << 2;
    button |= u8::from(modifiers.alt) << 3;
    button |= u8::from(modifiers.control) << 4;
    let column = u32::from(event.column) + 1;
    let row = u32::from(event.row) + 1;
    if sgr {
        let mut bytes = Vec::with_capacity(24);
        bytes.extend_from_slice(b"\x1b[<");
        push_decimal(&mut bytes, u32::from(button));
        bytes.push(b';');
        push_decimal(&mut bytes, column);
        bytes.push(b';');
        push_decimal(&mut bytes, row);
        bytes.push(if event.kind == MouseKind::Release {
            b'm'
        } else {
            b'M'
        });
        return Some(bytes);
    }
    let utf8 = mode.contains(TermMode::UTF8_MOUSE);
    let maximum = if utf8 { 2015 } else { 223 };
    if column > maximum || row > maximum {
        return None;
    }
    let mut bytes = Vec::with_capacity(9);
    bytes.extend_from_slice(b"\x1b[M");
    if utf8 {
        for value in [u32::from(button) + 32, column + 32, row + 32] {
            let mut buffer = [0; 4];
            bytes.extend_from_slice(char::from_u32(value)?.encode_utf8(&mut buffer).as_bytes());
        }
    } else {
        bytes.extend_from_slice(&[button + 32, (column + 32) as u8, (row + 32) as u8]);
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::term::{Config, Term, test::TermSize};
    use vte::ansi::Processor;

    fn key(key: Key) -> KeyEvent {
        KeyEvent {
            key,
            modifiers: Modifiers::default(),
            state: KeyState::Press,
            text: None,
            shifted_key: None,
            base_layout_key: None,
        }
    }

    fn terminal() -> (Term<VoidListener>, Processor) {
        let config = Config {
            kitty_keyboard: true,
            ..Config::default()
        };
        (
            Term::new(config, &TermSize::new(80, 24), VoidListener),
            Processor::default(),
        )
    }

    fn assert_key(event: &KeyEvent, mode: TermMode, expected: &[u8]) {
        assert_eq!(encode_key(event, mode).as_deref(), Some(expected));
    }

    #[test]
    fn ctrl_chords_are_bytes_without_a_windows_character_message() {
        for (character, byte) in [
            ('a', 1),
            ('c', 3),
            ('d', 4),
            ('l', 12),
            ('v', 22),
            ('z', 26),
            (' ', 0),
            ('2', 0),
            ('3', 27),
            ('4', 28),
            ('5', 29),
            ('6', 30),
            ('7', 31),
            ('8', 127),
            ('/', 31),
            ('?', 127),
            ('~', 30),
        ] {
            let mut event = key(Key::Character(character));
            event.modifiers.control = true;
            assert_key(&event, TermMode::empty(), &[byte]);
            event.modifiers.alt = true;
            assert_key(&event, TermMode::empty(), &[0x1b, byte]);
        }
        let mut event = key(Key::Character('с'));
        event.modifiers.control = true;
        event.base_layout_key = Some('c');
        assert_key(&event, TermMode::empty(), b"\x03");
        assert_key(&event, TermMode::DISAMBIGUATE_ESC_CODES, b"\x1b[1089;5u");
    }

    #[test]
    fn text_path_is_not_duplicated_or_mistaken_for_alt_shortcuts() {
        let mut event = key(Key::Character('e'));
        event.text = Some("e\u{301}😀".into());
        assert_eq!(encode_key(&event, TermMode::empty()), None);
        assert_eq!(
            encode_key(
                &event,
                TermMode::REPORT_EVENT_TYPES | TermMode::REPORT_ALTERNATE_KEYS
            ),
            None
        );
        event.key = Key::Character('€');
        event.modifiers = Modifiers {
            alt: true,
            control: true,
            alt_graph: true,
            ..Modifiers::default()
        };
        assert_eq!(encode_key(&event, TermMode::DISAMBIGUATE_ESC_CODES), None);
        assert_key(&event, TermMode::REPORT_ALL_KEYS_AS_ESC, b"\x1b[8364u");
        event.modifiers.alt_graph = false;
        event.key = Key::Character('e');
        assert_key(&event, TermMode::empty(), b"\x1b\x05");
        event.modifiers.control = false;
        assert_key(&event, TermMode::empty(), b"\x1be");
    }

    #[test]
    fn keypad_identities_cover_the_protocol_table_and_application_mode() {
        for (keycode, application, kitty) in [
            (
                Key::KeypadDigit(0),
                b"\x1bOp".as_slice(),
                b"\x1b[57399u".as_slice(),
            ),
            (
                Key::KeypadDigit(9),
                b"\x1bOy".as_slice(),
                b"\x1b[57408u".as_slice(),
            ),
            (
                Key::KeypadDecimal,
                b"\x1bOn".as_slice(),
                b"\x1b[57409u".as_slice(),
            ),
            (
                Key::KeypadDivide,
                b"\x1bOo".as_slice(),
                b"\x1b[57410u".as_slice(),
            ),
            (
                Key::KeypadMultiply,
                b"\x1bOj".as_slice(),
                b"\x1b[57411u".as_slice(),
            ),
            (
                Key::KeypadSubtract,
                b"\x1bOm".as_slice(),
                b"\x1b[57412u".as_slice(),
            ),
            (
                Key::KeypadAdd,
                b"\x1bOk".as_slice(),
                b"\x1b[57413u".as_slice(),
            ),
            (
                Key::KeypadEnter,
                b"\x1bOM".as_slice(),
                b"\x1b[57414u".as_slice(),
            ),
            (
                Key::KeypadEqual,
                b"\x1bOX".as_slice(),
                b"\x1b[57415u".as_slice(),
            ),
            (
                Key::KeypadSeparator,
                b"\x1bOl".as_slice(),
                b"\x1b[57416u".as_slice(),
            ),
            (
                Key::KeypadLeft,
                b"\x1bOt".as_slice(),
                b"\x1b[57417u".as_slice(),
            ),
            (
                Key::KeypadRight,
                b"\x1bOv".as_slice(),
                b"\x1b[57418u".as_slice(),
            ),
            (
                Key::KeypadUp,
                b"\x1bOx".as_slice(),
                b"\x1b[57419u".as_slice(),
            ),
            (
                Key::KeypadDown,
                b"\x1bOr".as_slice(),
                b"\x1b[57420u".as_slice(),
            ),
            (
                Key::KeypadPageUp,
                b"\x1bOy".as_slice(),
                b"\x1b[57421u".as_slice(),
            ),
            (
                Key::KeypadPageDown,
                b"\x1bOs".as_slice(),
                b"\x1b[57422u".as_slice(),
            ),
            (
                Key::KeypadHome,
                b"\x1bOw".as_slice(),
                b"\x1b[57423u".as_slice(),
            ),
            (
                Key::KeypadEnd,
                b"\x1bOq".as_slice(),
                b"\x1b[57424u".as_slice(),
            ),
            (
                Key::KeypadInsert,
                b"\x1bOp".as_slice(),
                b"\x1b[57425u".as_slice(),
            ),
            (
                Key::KeypadDelete,
                b"\x1bOn".as_slice(),
                b"\x1b[57426u".as_slice(),
            ),
            (Key::KeypadBegin, b"\x1bOu".as_slice(), b"\x1b[E".as_slice()),
        ] {
            assert_key(&key(keycode), TermMode::APP_KEYPAD, application);
            assert_key(
                &key(keycode),
                TermMode::APP_KEYPAD | TermMode::DISAMBIGUATE_ESC_CODES,
                kitty,
            );
        }
        let mut event = key(Key::KeypadDigit(1));
        event.modifiers.shift = true;
        assert_key(&event, TermMode::APP_KEYPAD, b"\x1bO2q");
        assert_eq!(
            encode_key(&key(Key::KeypadDigit(10)), TermMode::APP_KEYPAD),
            None
        );
        assert_eq!(
            encode_key(
                &key(Key::KeypadDigit(255)),
                TermMode::REPORT_ALL_KEYS_AS_ESC
            ),
            None
        );
    }

    #[test]
    fn modified_function_and_editing_keys_keep_their_canonical_trailers() {
        for (keycode, expected) in [
            (Key::Up, b"\x1b[1;5A".as_slice()),
            (Key::Down, b"\x1b[1;5B".as_slice()),
            (Key::Right, b"\x1b[1;5C".as_slice()),
            (Key::Left, b"\x1b[1;5D".as_slice()),
            (Key::Home, b"\x1b[1;5H".as_slice()),
            (Key::End, b"\x1b[1;5F".as_slice()),
            (Key::Insert, b"\x1b[2;5~".as_slice()),
            (Key::Delete, b"\x1b[3;5~".as_slice()),
            (Key::PageUp, b"\x1b[5;5~".as_slice()),
            (Key::PageDown, b"\x1b[6;5~".as_slice()),
            (Key::Function(1), b"\x1b[1;5P".as_slice()),
            (Key::Function(2), b"\x1b[1;5Q".as_slice()),
            (Key::Function(3), b"\x1b[13;5~".as_slice()),
            (Key::Function(4), b"\x1b[1;5S".as_slice()),
            (Key::Function(5), b"\x1b[15;5~".as_slice()),
            (Key::Function(6), b"\x1b[17;5~".as_slice()),
            (Key::Function(7), b"\x1b[18;5~".as_slice()),
            (Key::Function(8), b"\x1b[19;5~".as_slice()),
            (Key::Function(9), b"\x1b[20;5~".as_slice()),
            (Key::Function(10), b"\x1b[21;5~".as_slice()),
            (Key::Function(11), b"\x1b[23;5~".as_slice()),
            (Key::Function(12), b"\x1b[24;5~".as_slice()),
        ] {
            let mut event = key(keycode);
            event.modifiers.control = true;
            assert_key(&event, TermMode::APP_CURSOR, expected);
        }
        let mut event = key(Key::Character('a'));
        event.modifiers = Modifiers {
            alt: true,
            shift: true,
            ..Modifiers::default()
        };
        event.shifted_key = Some('A');
        assert_key(&event, TermMode::empty(), b"\x1bA");
        event = key(Key::Enter);
        assert_key(&event, TermMode::LINE_FEED_NEW_LINE, b"\r\n");
        event.key = Key::Backspace;
        assert_key(&event, TermMode::empty(), b"\x7f");
        event.modifiers.control = true;
        assert_key(&event, TermMode::empty(), b"\x08");
    }

    #[test]
    fn parsed_cursor_and_keypad_modes_change_wire_bytes_and_reset() {
        let (mut term, mut parser) = terminal();
        assert_key(&key(Key::Up), *term.mode(), b"\x1b[A");
        assert_eq!(encode_key(&key(Key::KeypadDigit(1)), *term.mode()), None);
        parser.advance(&mut term, b"\x1b[?1h\x1b=");
        assert_key(&key(Key::Up), *term.mode(), b"\x1bOA");
        assert_key(&key(Key::KeypadDigit(1)), *term.mode(), b"\x1bOq");
        assert_key(&key(Key::KeypadDecimal), *term.mode(), b"\x1bOn");
        assert_key(&key(Key::KeypadEnter), *term.mode(), b"\x1bOM");
        let mut event = key(Key::Left);
        event.modifiers = Modifiers {
            shift: true,
            control: true,
            ..Modifiers::default()
        };
        assert_key(&event, *term.mode(), b"\x1b[1;6D");
        parser.advance(&mut term, b"\x1b[?1l\x1b>");
        assert_key(&key(Key::Up), *term.mode(), b"\x1b[A");
        assert_eq!(encode_key(&key(Key::KeypadDigit(1)), *term.mode()), None);
    }

    #[test]
    fn functional_keys_use_real_numbers_instead_of_a_csi_u_approximation() {
        let mut tab = key(Key::Tab);
        tab.modifiers.shift = true;
        assert_key(&tab, TermMode::empty(), b"\x1b[Z");
        tab.modifiers.alt = true;
        assert_key(&tab, TermMode::empty(), b"\x1b\x1b[Z");
        tab.modifiers.alt = false;
        tab.modifiers.control = true;
        assert_key(&tab, TermMode::DISAMBIGUATE_ESC_CODES, b"\x1b[9;6u");
        assert_key(&key(Key::Function(3)), TermMode::empty(), b"\x1bOR");
        assert_key(
            &key(Key::Function(3)),
            TermMode::DISAMBIGUATE_ESC_CODES,
            b"\x1b[13~",
        );
        assert_key(&key(Key::Function(13)), TermMode::empty(), b"\x1b[57376u");
        assert_key(&key(Key::Function(35)), TermMode::empty(), b"\x1b[57398u");
        assert_eq!(
            encode_key(&key(Key::Function(0)), TermMode::REPORT_ALL_KEYS_AS_ESC),
            None
        );
        assert_eq!(encode_key(&key(Key::Function(36)), TermMode::empty()), None);
        let mut event = key(Key::Function(5));
        event.modifiers.super_key = true;
        assert_key(&event, TermMode::empty(), b"\x1b[15;9~");
        assert_key(
            &key(Key::KeypadLeft),
            TermMode::DISAMBIGUATE_ESC_CODES,
            b"\x1b[57417u",
        );
        assert_key(
            &key(Key::KeypadBegin),
            TermMode::DISAMBIGUATE_ESC_CODES,
            b"\x1b[E",
        );
    }

    #[test]
    fn kitty_push_pop_changes_ctrl_escape_and_unmodified_text_reporting() {
        let (mut term, mut parser) = terminal();
        let mut event = key(Key::Character('c'));
        event.modifiers.control = true;
        assert_key(&event, *term.mode(), b"\x03");
        parser.advance(&mut term, b"\x1b[>1u");
        assert_key(&event, *term.mode(), b"\x1b[99;5u");
        assert_key(&key(Key::Escape), *term.mode(), b"\x1b[27u");
        assert_key(&key(Key::Enter), *term.mode(), b"\r");
        assert_eq!(encode_key(&key(Key::Character('猫')), *term.mode()), None);
        parser.advance(&mut term, b"\x1b[>8u");
        assert_key(&key(Key::Character('猫')), *term.mode(), b"\x1b[29483u");
        assert_key(&key(Key::Enter), *term.mode(), b"\x1b[13u");
        parser.advance(&mut term, b"\x1b[<u");
        assert_key(&event, *term.mode(), b"\x1b[99;5u");
        parser.advance(&mut term, b"\x1b[<u");
        assert_key(&event, *term.mode(), b"\x03");
    }

    #[test]
    fn kitty_event_types_preserve_repeat_release_and_reset_shell_keys() {
        let mode = TermMode::DISAMBIGUATE_ESC_CODES | TermMode::REPORT_EVENT_TYPES;
        let mut event = key(Key::Up);
        assert_key(&event, mode, b"\x1b[A");
        event.state = KeyState::Repeat;
        assert_key(&event, mode, b"\x1b[1;1:2A");
        event.state = KeyState::Release;
        assert_key(&event, mode, b"\x1b[1;1:3A");
        assert_eq!(encode_key(&event, TermMode::DISAMBIGUATE_ESC_CODES), None);
        for keycode in [Key::Enter, Key::Tab, Key::Backspace] {
            event.key = keycode;
            assert_eq!(encode_key(&event, mode), None);
        }
        event.key = Key::Enter;
        assert_key(
            &event,
            mode | TermMode::REPORT_ALL_KEYS_AS_ESC,
            b"\x1b[13;1:3u",
        );
        event = key(Key::Character('a'));
        event.state = KeyState::Repeat;
        assert_eq!(encode_key(&event, mode), None);
        assert_key(
            &event,
            mode | TermMode::REPORT_ALL_KEYS_AS_ESC,
            b"\x1b[97;1:2u",
        );
    }

    #[test]
    fn kitty_alternates_and_unicode_text_are_progressive_not_independent_modes() {
        let mut event = key(Key::Character('ф'));
        event.modifiers.shift = true;
        event.shifted_key = Some('Ф');
        event.base_layout_key = Some('a');
        event.text = Some("Ф\u{301}😀".into());
        let alternates = TermMode::REPORT_ALTERNATE_KEYS;
        assert_eq!(
            encode_key(&event, alternates | TermMode::REPORT_ASSOCIATED_TEXT),
            None
        );
        let mode = alternates | TermMode::REPORT_ALL_KEYS_AS_ESC | TermMode::REPORT_ASSOCIATED_TEXT;
        assert_key(&event, mode, b"\x1b[1092:1060:97;2;1060:769:128512u");
        event.modifiers.shift = false;
        assert_key(&event, mode, b"\x1b[1092::97;;1060:769:128512u");
        event.state = KeyState::Release;
        assert_key(
            &event,
            mode | TermMode::REPORT_EVENT_TYPES,
            b"\x1b[1092::97;1:3u",
        );
        event.state = KeyState::Press;
        event.text = Some("a\u{85}b".into());
        assert_key(&event, mode, b"\x1b[1092::97u");
        event = key(Key::Enter);
        event.text = Some("\r".into());
        assert_key(&event, mode, b"\x1b[13u");
    }

    fn mouse(button: MouseButton, kind: MouseKind) -> MouseEvent {
        MouseEvent {
            button,
            kind,
            column: 0,
            row: 0,
            modifiers: Modifiers::default(),
        }
    }

    #[test]
    fn parsed_mouse_modes_distinguish_click_drag_and_all_motion() {
        let (mut term, mut parser) = terminal();
        let mut event = mouse(MouseButton::Left, MouseKind::Press);
        assert_eq!(encode_mouse(&event, *term.mode()), None);
        parser.advance(&mut term, b"\x1b[?1000h\x1b[?1006h");
        assert_eq!(
            encode_mouse(&event, *term.mode()),
            Some(b"\x1b[<0;1;1M".to_vec())
        );
        event.kind = MouseKind::Move;
        assert_eq!(encode_mouse(&event, *term.mode()), None);
        parser.advance(&mut term, b"\x1b[?1002h");
        assert_eq!(
            encode_mouse(&event, *term.mode()),
            Some(b"\x1b[<32;1;1M".to_vec())
        );
        event.button = MouseButton::None;
        assert_eq!(encode_mouse(&event, *term.mode()), None);
        parser.advance(&mut term, b"\x1b[?1003h");
        assert_eq!(
            encode_mouse(&event, *term.mode()),
            Some(b"\x1b[<35;1;1M".to_vec())
        );
        event.button = MouseButton::Right;
        event.kind = MouseKind::Release;
        event.modifiers = Modifiers {
            shift: true,
            alt: true,
            control: true,
            ..Modifiers::default()
        };
        assert_eq!(
            encode_mouse(&event, *term.mode()),
            Some(b"\x1b[<30;1;1m".to_vec())
        );
        parser.advance(&mut term, b"\x1b[?1006l");
        assert_eq!(
            encode_mouse(&event, *term.mode()),
            Some(b"\x1b[M?!!".to_vec())
        );
    }

    #[test]
    fn mouse_coordinate_boundaries_never_wrap_to_another_cell() {
        let mut event = mouse(MouseButton::Left, MouseKind::Press);
        let mode = TermMode::MOUSE_REPORT_CLICK;
        event.column = 222;
        assert_eq!(
            encode_mouse(&event, mode),
            Some(vec![0x1b, b'[', b'M', 32, 255, 33])
        );
        event.column = 223;
        assert_eq!(encode_mouse(&event, mode), None);
        event.column = 94;
        event.row = 95;
        assert_eq!(
            encode_mouse(&event, mode | TermMode::UTF8_MOUSE),
            Some(b"\x1b[M \x7f\xc2\x80".to_vec())
        );
        event.column = 2014;
        assert_eq!(
            encode_mouse(&event, mode | TermMode::UTF8_MOUSE),
            Some(b"\x1b[M \xdf\xbf\xc2\x80".to_vec())
        );
        event.column = 2015;
        assert_eq!(encode_mouse(&event, mode | TermMode::UTF8_MOUSE), None);
        event.column = u16::MAX;
        event.row = u16::MAX;
        assert_eq!(
            encode_mouse(&event, mode | TermMode::UTF8_MOUSE | TermMode::SGR_MOUSE),
            Some(b"\x1b[<0;65536;65536M".to_vec())
        );
    }

    #[test]
    fn wheel_buttons_have_no_release_and_alternate_scroll_is_mode_scoped() {
        let mode = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        for (button, expected) in [
            (MouseButton::WheelUp, b"\x1b[<64;1;1M".as_slice()),
            (MouseButton::WheelDown, b"\x1b[<65;1;1M".as_slice()),
            (MouseButton::WheelLeft, b"\x1b[<66;1;1M".as_slice()),
            (MouseButton::WheelRight, b"\x1b[<67;1;1M".as_slice()),
        ] {
            let mut event = mouse(button, MouseKind::Press);
            assert_eq!(encode_mouse(&event, mode).as_deref(), Some(expected));
            event.kind = MouseKind::Release;
            assert_eq!(encode_mouse(&event, mode), None);
            event.kind = MouseKind::Move;
            assert_eq!(encode_mouse(&event, mode | TermMode::MOUSE_MOTION), None);
        }
        let event = mouse(MouseButton::WheelUp, MouseKind::Press);
        assert_eq!(encode_mouse(&event, TermMode::ALTERNATE_SCROLL), None);
        let alternate = TermMode::ALTERNATE_SCROLL | TermMode::ALT_SCREEN;
        assert_eq!(encode_mouse(&event, alternate), Some(b"\x1b[A".to_vec()));
        assert_eq!(
            encode_mouse(&event, alternate | TermMode::APP_CURSOR),
            Some(b"\x1bOA".to_vec())
        );
    }

    #[test]
    fn parsed_bracketed_paste_preserves_unicode_and_multiline_without_double_crlf() {
        let (mut term, mut parser) = terminal();
        let text = "a\r\n猫\n😀\re\u{301}";
        assert_eq!(
            encode_paste(text, *term.mode()),
            "a\n猫\n😀\re\u{301}".as_bytes()
        );
        parser.advance(&mut term, b"\x1b[?2004h");
        assert_eq!(
            encode_paste(text, *term.mode()),
            "\x1b[200~a\n猫\n😀\re\u{301}\x1b[201~".as_bytes()
        );
        assert_eq!(encode_paste("", *term.mode()), b"\x1b[200~\x1b[201~");
        parser.advance(&mut term, b"\x1b[?2004l");
        assert_eq!(encode_paste("猫\n", *term.mode()), "猫\n".as_bytes());
    }
}
