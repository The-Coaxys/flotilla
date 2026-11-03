//! Keyboard routing for a focused terminal (spec §8.3).
//!
//! Two rules decide every keystroke:
//! - **The terminal comes first.** Plain `Ctrl+<letter>` always reaches the
//!   terminal, even when an application binding exists for it.
//! - **Application shortcuts are never consumed.** The spec's default app
//!   chords pass through untouched so the shell's actions (and Windows'
//!   `Alt+F4`) keep working while a terminal has focus.
//!
//! Everything else is encoded by [`fl_term::input::encode_key`] with the
//! terminal's negotiated modes; keys it leaves to the text path (plain
//! printable characters, dead keys, IME) are not consumed here.
//!
//! The input is a platform-neutral copy of a GPUI keystroke so the table can
//! be tested without a window. GPUI's Windows backend reports a shifted
//! punctuation key as its shifted character with Shift cleared (`Shift+=`
//! arrives as `+`), so chords are matched in both spellings.
use fl_term::TermMode;
use fl_term::input::{Key, KeyEvent, KeyState, Modifiers};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StrokeModifiers {
    pub control: bool,
    pub alt: bool,
    pub shift: bool,
    /// Windows/Super/Command.
    pub platform: bool,
}

/// One key event as delivered by GPUI: `key` is the layout's key name
/// (`"a"`, `"enter"`, `"f5"`, `"+"`), `key_char` the text it would type.
#[derive(Debug, Clone, Copy)]
pub struct Stroke<'a> {
    pub key: &'a str,
    pub key_char: Option<&'a str>,
    pub modifiers: StrokeModifiers,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// An application or system shortcut: leave the event unconsumed.
    App,
    /// Ctrl+Shift+C.
    Copy,
    /// Ctrl+Shift+V.
    Paste,
    /// Shift+PageUp/PageDown on the primary screen: scroll the local
    /// viewport by this many pages (positive moves into scrollback).
    ScrollPage(i32),
    /// Encode for the terminal. `encode_key` returning `None` leaves the
    /// event to the native text/IME path.
    Terminal(KeyEvent),
    /// A key the terminal protocol cannot represent (media keys, bare
    /// modifiers): leave it unconsumed.
    Unsupported,
}

/// Plain `Ctrl+<letter>`: owned by the terminal ahead of any application
/// binding. AltGr never reaches here as Ctrl (GPUI strips it on Windows).
pub fn is_terminal_owned(stroke: &Stroke) -> bool {
    let m = stroke.modifiers;
    m.control && !m.alt && !m.shift && !m.platform && letter(stroke.key).is_some()
}

pub fn route(stroke: &Stroke, state: KeyState, modes: TermMode) -> Route {
    if is_app_shortcut(stroke) {
        return Route::App;
    }
    let m = stroke.modifiers;
    if m.control && m.shift && !m.alt && !m.platform {
        match letter(stroke.key) {
            Some('c') => return Route::Copy,
            Some('v') => return Route::Paste,
            _ => {}
        }
    }
    if m.shift && !m.control && !m.alt && !m.platform && !modes.contains(TermMode::ALT_SCREEN) {
        match stroke.key {
            "pageup" => return Route::ScrollPage(1),
            "pagedown" => return Route::ScrollPage(-1),
            _ => {}
        }
    }
    match key_event(stroke, state) {
        Some(event) => Route::Terminal(event),
        None => Route::Unsupported,
    }
}

/// The spec §8.3 defaults that must keep working while a terminal is focused.
fn is_app_shortcut(stroke: &Stroke) -> bool {
    let m = stroke.modifiers;
    if m.platform {
        // Windows-key chords belong to the OS shell.
        return true;
    }
    let key = stroke.key;
    // Next / previous tab.
    if m.control && !m.alt && key == "tab" {
        return true;
    }
    // Window close on Windows.
    if m.alt && !m.control && !m.shift && key == "f4" {
        return true;
    }
    let plus = key == "+" || (m.shift && key == "=");
    let minus = key == "_" || (m.shift && key == "-");
    // Pane splits: Alt+Shift+= / Alt+Shift+-.
    if m.alt && !m.control && (plus || minus) {
        return true;
    }
    // View splits: Ctrl+Alt+Shift+= / Ctrl+Alt+Shift+-.
    if m.alt && m.control && (plus || minus) {
        return true;
    }
    match (m.control, m.alt, m.shift, letter(key)) {
        // New worktree, new terminal, right/left panel, Git action, inbox,
        // palette, files, find, PiP, run target.
        (
            true,
            false,
            true,
            Some('n' | 't' | 'e' | 'b' | 'g' | 'i' | 'k' | 'o' | 'f' | 'p' | 'r'),
        ) => true,
        // Tool picker; performance HUD.
        (true, true, true, Some('t' | 'p')) => true,
        // Toggle both panels; alternate Git action.
        (true, true, false, Some('b' | 'g')) => true,
        _ => false,
    }
}

fn letter(key: &str) -> Option<char> {
    let mut chars = key.chars();
    let c = chars.next()?;
    (chars.next().is_none() && c.is_ascii_alphabetic()).then(|| c.to_ascii_lowercase())
}

/// Translate a GPUI key name into the encoder's key model.
pub fn key_event(stroke: &Stroke, state: KeyState) -> Option<KeyEvent> {
    let m = stroke.modifiers;
    let modifiers = Modifiers {
        shift: m.shift,
        alt: m.alt,
        control: m.control,
        super_key: m.platform,
        // GPUI reports AltGr as no Ctrl/Alt at all, so it never reaches the
        // encoder as a shortcut chord.
        alt_graph: false,
    };
    let key = match stroke.key {
        "enter" => Key::Enter,
        "tab" => Key::Tab,
        "backspace" => Key::Backspace,
        "escape" => Key::Escape,
        "up" => Key::Up,
        "down" => Key::Down,
        "left" => Key::Left,
        "right" => Key::Right,
        "home" => Key::Home,
        "end" => Key::End,
        "insert" => Key::Insert,
        "delete" => Key::Delete,
        "pageup" => Key::PageUp,
        "pagedown" => Key::PageDown,
        "space" => Key::Character(' '),
        name => {
            if let Some(number) = name.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
                Key::Function(number)
            } else {
                let mut chars = name.chars();
                let c = chars.next()?;
                if chars.next().is_some() {
                    return None;
                }
                Key::Character(if c.is_ascii_uppercase() {
                    c.to_ascii_lowercase()
                } else {
                    c
                })
            }
        }
    };
    let text = match (key, stroke.key_char) {
        (Key::Character(' '), None) => Some(" ".to_owned()),
        (_, Some(text)) if !text.is_empty() && !text.chars().any(char::is_control) => {
            Some(text.to_owned())
        }
        _ => None,
    };
    let shifted_key = match (key, m.shift, stroke.key_char) {
        (Key::Character(_), true, Some(text)) => {
            let mut chars = text.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) if !c.is_control() => Some(c),
                _ => None,
            }
        }
        _ => None,
    };
    Some(KeyEvent {
        key,
        modifiers,
        state,
        text,
        shifted_key,
        base_layout_key: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_term::input::encode_key;
    use fl_term::{EffectMode, Engine, Size};

    fn stroke<'a>(key: &'a str, mods: &str) -> Stroke<'a> {
        Stroke {
            key,
            key_char: None,
            modifiers: StrokeModifiers {
                control: mods.contains('c'),
                alt: mods.contains('a'),
                shift: mods.contains('s'),
                platform: mods.contains('w'),
            },
        }
    }

    fn modes_after(bytes: &[u8]) -> TermMode {
        let mut engine = Engine::new(Size::new(80, 24), EffectMode::Mirror).unwrap();
        engine.feed(0, bytes).unwrap();
        engine.modes()
    }

    fn encoded(stroke: &Stroke, modes: TermMode) -> Option<Vec<u8>> {
        match route(stroke, KeyState::Press, modes) {
            Route::Terminal(event) => encode_key(&event, modes),
            other => panic!("{stroke:?} routed to {other:?}"),
        }
    }

    #[test]
    fn plain_ctrl_letters_reach_the_terminal_as_c0_controls() {
        let modes = modes_after(b"");
        for (key, byte) in [
            ("a", 1u8),
            ("c", 3),
            ("d", 4),
            ("l", 12),
            ("v", 22),
            ("z", 26),
        ] {
            let s = stroke(key, "c");
            assert!(is_terminal_owned(&s), "ctrl-{key}");
            assert_eq!(encoded(&s, modes), Some(vec![byte]), "ctrl-{key}");
        }
        // Even a letter whose Ctrl+Shift form is an app action.
        assert!(is_terminal_owned(&stroke("t", "c")));
        assert_eq!(encoded(&stroke("t", "c"), modes), Some(vec![20]));
    }

    #[test]
    fn kitty_disambiguation_encodes_ctrl_letters_as_csi_u() {
        // CSI > 1 u pushes "disambiguate escape codes".
        let modes = modes_after(b"\x1b[>1u");
        assert!(modes.contains(TermMode::DISAMBIGUATE_ESC_CODES));
        assert_eq!(
            encoded(&stroke("c", "c"), modes),
            Some(b"\x1b[99;5u".to_vec())
        );
    }

    #[test]
    fn copy_paste_use_ctrl_shift_and_are_not_terminal_owned() {
        let modes = modes_after(b"");
        assert_eq!(
            route(&stroke("c", "cs"), KeyState::Press, modes),
            Route::Copy
        );
        assert_eq!(
            route(&stroke("v", "cs"), KeyState::Press, modes),
            Route::Paste
        );
        assert!(!is_terminal_owned(&stroke("c", "cs")));
    }

    #[test]
    fn spec_app_shortcuts_pass_through_in_both_shift_spellings() {
        let modes = modes_after(b"");
        for (key, mods) in [
            ("t", "cs"),  // new terminal
            ("t", "cas"), // tool picker
            ("n", "cs"),  // new worktree
            ("f", "cs"),  // find in scrollback
            ("=", "as"),  // split right (US layout, other platforms)
            ("+", "a"),   // split right (Windows normalizes Shift+=)
            ("-", "as"),  // split down
            ("_", "a"),   // split down (Windows)
            ("=", "cas"), // view split right
            ("+", "ca"),  // view split right (Windows)
            ("-", "cas"), // view split down
            ("_", "ca"),  // view split down (Windows)
            ("tab", "c"), // next tab
            ("tab", "cs"),
            ("f4", "a"), // Windows close
            ("b", "ca"),
            ("p", "cas"),
        ] {
            assert_eq!(
                route(&stroke(key, mods), KeyState::Press, modes),
                Route::App,
                "{mods}-{key}"
            );
            assert!(!is_terminal_owned(&stroke(key, mods)));
        }
    }

    #[test]
    fn non_shortcut_modified_keys_are_encoded_for_the_terminal() {
        let modes = modes_after(b"");
        // Alt+= is not a split chord (no Shift): ESC-prefixed.
        assert_eq!(encoded(&stroke("=", "a"), modes), Some(b"\x1b=".to_vec()));
        // Alt+letter.
        assert_eq!(encoded(&stroke("x", "a"), modes), Some(b"\x1bx".to_vec()));
        // Ctrl+Shift+A has no app meaning.
        assert!(matches!(
            route(&stroke("a", "cs"), KeyState::Press, modes),
            Route::Terminal(_)
        ));
        assert_eq!(encoded(&stroke("enter", ""), modes), Some(b"\r".to_vec()));
        assert_eq!(
            encoded(&stroke("f5", ""), modes),
            Some(b"\x1b[15~".to_vec())
        );
    }

    #[test]
    fn printable_text_is_left_to_the_text_path_but_report_all_takes_it() {
        let modes = modes_after(b"");
        let shifted = Stroke {
            key: "a",
            key_char: Some("A"),
            modifiers: StrokeModifiers {
                shift: true,
                ..Default::default()
            },
        };
        assert_eq!(encoded(&shifted, modes), None);
        // Kitty "report all keys as escape codes" (flags 1|8).
        let report_all = modes_after(b"\x1b[>9u");
        assert!(report_all.contains(TermMode::REPORT_ALL_KEYS_AS_ESC));
        let bytes = encoded(&shifted, report_all).unwrap();
        assert!(
            bytes.starts_with(b"\x1b[97"),
            "{:?}",
            String::from_utf8_lossy(&bytes)
        );
    }

    #[test]
    fn application_cursor_mode_changes_arrow_encoding() {
        assert_eq!(
            encoded(&stroke("up", ""), modes_after(b"")),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            encoded(&stroke("up", ""), modes_after(b"\x1b[?1h")),
            Some(b"\x1bOA".to_vec())
        );
    }

    #[test]
    fn shift_page_scrolls_locally_only_on_the_primary_screen() {
        let primary = modes_after(b"");
        assert_eq!(
            route(&stroke("pageup", "s"), KeyState::Press, primary),
            Route::ScrollPage(1)
        );
        let alternate = modes_after(b"\x1b[?1049h");
        assert!(matches!(
            route(&stroke("pageup", "s"), KeyState::Press, alternate),
            Route::Terminal(_)
        ));
    }

    #[test]
    fn releases_are_reported_only_when_the_application_asks() {
        let s = stroke("a", "c");
        let Route::Terminal(release) = route(&s, KeyState::Release, modes_after(b"")) else {
            panic!()
        };
        assert_eq!(encode_key(&release, modes_after(b"")), None);
        // Kitty flags 1|2 report event types.
        let modes = modes_after(b"\x1b[>3u");
        assert_eq!(encode_key(&release, modes), Some(b"\x1b[97;5:3u".to_vec()));
    }

    #[test]
    fn unrepresentable_keys_are_not_consumed() {
        let modes = modes_after(b"");
        assert_eq!(
            route(&stroke("menu", ""), KeyState::Press, modes),
            Route::Unsupported
        );
        assert_eq!(
            route(&stroke("a", "w"), KeyState::Press, modes),
            Route::App,
            "Windows-key chords belong to the OS"
        );
    }
}
