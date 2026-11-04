// Flotilla patch: future-continuation and resource-boundary regressions.
use std::sync::{Arc, Mutex};

use super::*;
use crate::event::VoidListener;
use crate::term::test::TermSize;
use crate::vte::ansi::Processor;

fn terminal() -> Term<VoidListener> {
    let config = Config { kitty_keyboard: true, ..Config::default() };
    Term::new(config, &TermSize::new(9, 4), VoidListener)
}

fn canonical(term: &Term<VoidListener>) -> Vec<u8> {
    serde_json::to_vec(&term.snapshot()).unwrap()
}

#[test]
fn checkpoint_every_byte_preserves_hidden_state_and_future() {
    // Includes primary/alternate saved cursors, delayed wrap, charset, both keyboard
    // stacks, palette, titles, REP, combining/fullwidth, OSC metadata and sync buffering.
    let bytes = concat!(
        "123456789\x1b7\x1b(0\x0eq\x0f",
        "\x1b[2;4r\x1b[3g\x1b[2;3H\x1bH",
        "\x1b]0;first\x07\x1b[22;0t\x1b]0;second\x1b\\",
        "\x1b[>1u\x1b]4;2;#123456\x07",
        "\x1b]8;;https://example.test/one\x1b\\a\x1b7",
        "\x1b[?1049h\x1b[>3u\x1b[31m\x1b7界e\u{301}",
        "\x1b]7;file://host/tmp\x07\x1b]133;A\x07",
        "\x1b[?2026hbuffered\x1b[?2026l",
        "\x1b8\x1b[?1049l\x1b8\x1b[2b",
        "\x1b[<u\x1b[23;0t\x1b]8;;\x07",
        "\x1b]133;B\x07\x1b]133;C\x07\x1b]133;D;17\x07"
    ).as_bytes();
    let suffix = b"\x1b[?1049h\x1b8\x1b[<uZ\x1b[?1049l\x1b8\x1b[23;0tQ\x1b[2b";
    let mut baseline = terminal();
    let mut processor: Processor = Processor::new();
    processor.advance(&mut baseline, bytes);
    processor.advance(&mut baseline, suffix);
    baseline.resize(TermSize::new(6, 6));
    processor.advance(&mut baseline, b"\r\nnext\x1b[6n");
    let expected = canonical(&baseline);
    let expected_processor = serde_json::to_vec(&processor.snapshot()).unwrap();

    for cut in 0..=bytes.len() {
        let mut term = terminal();
        let mut processor: Processor = Processor::new();
        processor.advance(&mut term, &bytes[..cut]);
        let snapshot = serde_json::from_slice(&canonical(&term)).unwrap();
        let mut restored = Term::from_snapshot(snapshot, VoidListener).unwrap();
        let processor_snapshot = serde_json::from_slice(
            &serde_json::to_vec(&processor.snapshot()).unwrap(),
        ).unwrap();
        let mut processor: Processor = Processor::from_snapshot(processor_snapshot).unwrap();
        processor.advance(&mut restored, &bytes[cut..]);
        processor.advance(&mut restored, suffix);
        restored.resize(TermSize::new(6, 6));
        processor.advance(&mut restored, b"\r\nnext\x1b[6n");
        assert_eq!(canonical(&restored), expected, "terminal cut {cut}");
        assert_eq!(serde_json::to_vec(&processor.snapshot()).unwrap(), expected_processor,
            "processor cut {cut}");
    }
}

#[test]
fn both_grid_cursor_records_are_explicit_not_grid_equality() {
    let mut term = terminal();
    term.grid.cursor.template.fg = Color::Indexed(17);
    term.grid.cursor.charsets[CharsetIndex::G2] = StandardCharset::SpecialCharacterAndLineDrawing;
    term.grid.cursor.input_needs_wrap = true;
    term.grid.saved_cursor = term.grid.cursor.clone();
    term.inactive_grid.cursor.template.bg = Color::Indexed(19);
    term.inactive_grid.saved_cursor.template.c = 'x';
    let expected = canonical(&term);
    let snapshot = serde_json::from_slice(&expected).unwrap();
    let restored = Term::from_snapshot(snapshot, VoidListener).unwrap();
    assert_eq!(restored.grid.cursor, term.grid.cursor);
    assert_eq!(restored.grid.saved_cursor, term.grid.saved_cursor);
    assert_eq!(restored.inactive_grid.cursor, term.inactive_grid.cursor);
    assert_eq!(restored.inactive_grid.saved_cursor, term.inactive_grid.saved_cursor);
    assert_eq!(canonical(&restored), expected);
}

#[test]
fn keyboard_overflow_evicts_keyboard_not_title_stack() {
    let mut term = terminal();
    term.set_title(Some("saved".into()));
    term.push_title();
    for _ in 0..KEYBOARD_MODE_STACK_MAX_DEPTH + 2 {
        term.push_keyboard_mode(KeyboardModes::DISAMBIGUATE_ESC_CODES);
    }
    assert_eq!(term.keyboard_mode_stack.len(), KEYBOARD_MODE_STACK_MAX_DEPTH);
    assert_eq!(term.title_stack, vec![Some("saved".into())]);
    term.pop_keyboard_modes(KEYBOARD_MODE_STACK_MAX_DEPTH as u16);
    assert!(!term.mode.contains(TermMode::KITTY_KEYBOARD_PROTOCOL));
    term.pop_title();
    assert_eq!(term.title.as_deref(), Some("saved"));
    // The original defect also panicked when the unrelated title stack was empty.
    for _ in 0..KEYBOARD_MODE_STACK_MAX_DEPTH + 1 {
        term.push_keyboard_mode(KeyboardModes::NO_MODE);
    }
}

#[test]
fn hyperlinks_are_terminal_local_and_resume_the_counter() {
    fn link(term: &mut Term<VoidListener>, id: Option<&str>) -> cell::Hyperlink {
        term.set_hyperlink(Some(Hyperlink {
            id: id.map(str::to_owned), uri: "https://example.test".into(),
        }));
        term.grid.cursor.template.hyperlink().unwrap()
    }
    let mut first = terminal();
    let initial = link(&mut first, None);
    let mut unrelated = terminal();
    assert_eq!(link(&mut unrelated, None), initial);
    let mut restored = Term::from_snapshot(first.snapshot(), VoidListener).unwrap();
    assert_eq!(link(&mut first, None), link(&mut restored, None));
    assert_eq!(link(&mut first, Some("explicit")).id(), "explicit");
    first.hyperlink_bytes = MAX_HYPERLINK_BYTES;
    first.set_hyperlink(Some(Hyperlink { id: None, uri: "more".into() }));
    assert!(first.grid.cursor.template.hyperlink().is_none());
    restored.next_hyperlink_id = u64::MAX;
    restored.set_hyperlink(Some(Hyperlink { id: None, uri: "more".into() }));
    assert!(restored.grid.cursor.template.hyperlink().is_none());
    assert_eq!(restored.next_hyperlink_id, u64::MAX);
    restored.reset_state();
    assert_eq!(restored.next_hyperlink_id, 0);
    assert_eq!(restored.hyperlink_bytes, 0);
    assert_eq!(link(&mut restored, None), initial);
}

#[test]
fn checkpoint_interns_hyperlinks_and_rejects_bad_references() {
    let mut term = terminal();
    term.set_hyperlink(Some(Hyperlink { id: None, uri: "x".repeat(MAX_URI_BYTES) }));
    for _ in 0..30 {
        term.input('x');
    }
    let snapshot = term.snapshot();
    assert_eq!(snapshot.hyperlinks.len(), 1);
    assert!(serde_json::to_vec(&snapshot).unwrap().len() < 20_000);
    let mut invalid = snapshot.clone();
    invalid.grid.cursor.template.hyperlink = Some(1);
    assert!(Term::from_snapshot(invalid, VoidListener).is_err());
    let mut invalid = snapshot.clone();
    invalid.hyperlink_bytes = 0;
    assert!(Term::from_snapshot(invalid, VoidListener).is_err());
    let restored = Term::from_snapshot(snapshot, VoidListener).unwrap();
    assert_eq!(canonical(&restored), canonical(&term));
}

#[test]
fn live_limits_and_restore_validation_agree() {
    let mut term = terminal();
    for _ in 0..MAX_COMBINING_CHARS + 2 {
        term.grid.cursor.template.push_zerowidth('\u{301}');
    }
    assert_eq!(term.grid.cursor.template.zerowidth().unwrap().len(), MAX_COMBINING_CHARS);
    term.set_title(Some("ok".into()));
    term.set_title(Some("x".repeat(MAX_TEXT_BYTES + 1)));
    assert_eq!(term.title.as_deref(), Some("ok"));
    term.current_directory("file://host/work");
    term.current_directory(&"x".repeat(MAX_CWD_BYTES + 1));
    assert_eq!(term.cwd(), Some("file://host/work"));
    term.shell_integration(b'D', Some(7));
    let restored = Term::from_snapshot(term.snapshot(), VoidListener).unwrap();
    assert_eq!(restored.prompt_state(), PromptState::Finished { exit_status: Some(7) });
    let original = term.snapshot();
    let mut invalid = original.clone();
    invalid.grid.cursor.point.column = Column(invalid.grid.columns);
    assert!(Term::from_snapshot(invalid, VoidListener).is_err());
    let mut invalid = original.clone();
    invalid.inactive_grid.saved_cursor.point.line = Line(-1);
    assert!(Term::from_snapshot(invalid, VoidListener).is_err());
    let mut invalid = original.clone();
    invalid.mode |= 1 << 31;
    assert!(Term::from_snapshot(invalid, VoidListener).is_err());
    let mut invalid = original.clone();
    invalid.grid.rows[0].occ = invalid.grid.columns + 1;
    assert!(Term::from_snapshot(invalid, VoidListener).is_err());
    let mut invalid = original.clone();
    invalid.grid.rows[0][Column(0)].cell.flags = Flags::from_bits_retain(1 << 15);
    assert!(Term::from_snapshot(invalid, VoidListener).is_err());
    let mut invalid = original;
    invalid.keyboard_mode_stack = vec![255];
    assert!(Term::from_snapshot(invalid, VoidListener).is_err());
}

#[test]
fn geometry_and_configuration_limits_apply_before_allocation() {
    let config = Config {
        scrolling_history: MAX_HISTORY + 1,
        semantic_escape_chars: "x".repeat(MAX_TEXT_BYTES + 1),
        ..Config::default()
    };
    let mut term = Term::new(config, &TermSize::new(0, 0), VoidListener);
    assert_eq!(term.columns(), MIN_COLUMNS);
    assert_eq!(term.screen_lines(), MIN_SCREEN_LINES);
    assert_eq!(term.config.scrolling_history, MAX_HISTORY);
    assert_eq!(term.config.semantic_escape_chars, SEMANTIC_ESCAPE_CHARS);
    term.resize(TermSize::new(MAX_COLUMNS + 1, 1));
    assert_eq!(term.columns(), MAX_COLUMNS);
    assert_eq!(term.snapshot().grid.max_scroll_limit, history_limit(MAX_HISTORY, MAX_COLUMNS, 1));
    assert!(term.snapshot().validate().is_ok());
}

#[test]
fn canonical_snapshot_excludes_local_presentation_and_restore_is_fully_damaged() {
    let mut term = terminal();
    let mut processor: Processor = Processor::new();
    processor.advance(&mut term, b"1\r\n2\r\n3\r\n4\r\n5\r\n6");
    let original = canonical(&term);
    term.is_focused = true;
    term.mode.insert(TermMode::VI);
    term.vi_mode_cursor.point = Point::new(Line(2), Column(2));
    term.scroll_display(Scroll::Top);
    term.reset_damage();
    assert_eq!(canonical(&term), original);
    let mut restored = Term::from_snapshot(term.snapshot(), VoidListener).unwrap();
    assert!(!restored.is_focused);
    assert!(!restored.mode.contains(TermMode::VI));
    assert_eq!(restored.grid.display_offset(), 0);
    assert!(matches!(restored.damage(), TermDamage::Full));
}

#[derive(Clone, Default)]
struct PaletteEvents(Arc<Mutex<Vec<Option<Rgb>>>>);
impl EventListener for PaletteEvents {
    fn send_event(&self, event: Event) {
        if let Event::ColorRequestResolved(_, color, _) = event {
            self.0.lock().unwrap().push(color);
        }
    }
}

#[test]
fn palette_query_captures_override_before_later_mutations_in_same_chunk() {
    let listener = PaletteEvents::default();
    let mut term = Term::new(Config::default(), &TermSize::new(9, 4), listener.clone());
    let mut processor: Processor = Processor::new();
    processor.advance(&mut term, b"\x1b]4;2;#123456\x07\x1b]4;2;?\x07\x1b]4;2;#abcdef\x07");
    assert_eq!(*listener.0.lock().unwrap(), vec![Some(Rgb { r: 0x12, g: 0x34, b: 0x56 })]);
    assert_eq!(term.colors[2], Some(Rgb { r: 0xab, g: 0xcd, b: 0xef }));
}
