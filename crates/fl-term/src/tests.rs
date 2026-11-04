use super::*;

fn engine() -> Engine {
    Engine::new(Size::new(20, 5), EffectMode::Authoritative).unwrap()
}

fn replies(effects: &[Effect]) -> Vec<&[u8]> {
    effects
        .iter()
        .filter_map(|e| match &e.kind {
            EffectKind::Reply(bytes) => Some(bytes.as_slice()),
            _ => None,
        })
        .collect()
}

#[test]
fn every_byte_cut_preserves_future_state_and_effects() {
    let fixtures: &[&[u8]] = &[
        b"plain\r\nsecond\tline\x08!",
        "a\u{301}中🙂\r\nwide at right edge 界".as_bytes(),
        b"\x1b[31;1mred\x1b7\x1b[2;3Hnew\x1b8!\x1b[6n",
        b"\x1b(0lqk\x1b7\x1b(Babc\x1b8m\x1b(B",
        b"primary\x1b[?1049halt\x1b7\x1b[Hreplace\x1b8!\x1b[?1049lEND",
        b"\x1b]0;one\x07\x1b[22;0t\x1b]0;two\x1b\\\x1b[23;0t",
        b"\x1b]8;;https://example.com/a\x1b\\link\x1b]8;;\x1b\\ next \x1b]8;;https://example.com/b\x07B",
        b"\x1b]7;file://server/srv/project\x07\x1b]133;A\x07$ \x1b]133;B\x07ls\x1b]133;C\x07done\x1b]133;D;2\x07",
        b"\x1b]10;#112233\x07\x1b]10;?\x07\x1b]10;#445566\x07\x1b]10;?\x07",
        b"a\x1b[5b\x1b[2;4r\x1b[?6h\x1b[3;2H!\x1b[?6l\x1b[0m",
        b"\x1b[>15u\x1b[>7u\x1b[<u\x1b[?1049h\x1b[>31u\x1b[?1049l\x1b[<u",
        b"\x1b[?2026hbuffered\x1b[6n\x1b[?2026lvisible\x07",
        b"\x1b]52;c;?\x1b\\\x1b]52;c;aGVsbG8=\x07",
        b"\xf0\x9f\x92\xa9\xe2\x82\xac\xff\xc3(\x1bPignored\x1b\\DONE",
        b"01234567890123456789WRAPPED\r\n1\r\n2\r\n3\r\n4\r\n5\r\n6",
    ];
    for (fixture_index, bytes) in fixtures.iter().enumerate() {
        let mut uninterrupted = engine();
        let mut expected_effects = uninterrupted.feed(0, bytes).unwrap();
        expected_effects.extend(uninterrupted.flush_sync().unwrap());
        let expected_state = uninterrupted.checkpoint().unwrap();
        for cut in 0..=bytes.len() {
            let mut prefix = engine();
            let mut actual_effects = prefix.feed(0, &bytes[..cut]).unwrap();
            let checkpoint = prefix.checkpoint().unwrap();
            let mut restored = Engine::restore(&checkpoint, EffectMode::Authoritative).unwrap();
            actual_effects.extend(restored.feed(cut as u64, &bytes[cut..]).unwrap());
            actual_effects.extend(restored.flush_sync().unwrap());
            assert_eq!(
                restored.checkpoint().unwrap(),
                expected_state,
                "fixture {fixture_index}, cut {cut}"
            );
            assert_eq!(
                actual_effects, expected_effects,
                "effects fixture {fixture_index}, cut {cut}"
            );
        }
    }
}

#[test]
fn resize_after_restoring_inactive_screen_retains_saved_cursor_and_wrap() {
    let mut original = engine();
    original
        .feed(0, b"abcdefghijklmnopqrstuvwxyz\x1b7\x1b[?1049hALT\x1b7")
        .unwrap();
    let checkpoint = original.checkpoint().unwrap();
    let mut restored = Engine::restore(&checkpoint, EffectMode::Authoritative).unwrap();
    let size = Size {
        columns: 12,
        rows: 7,
        cell_width: 8,
        cell_height: 16,
    };
    assert_eq!(
        original.resize(size).unwrap(),
        restored.resize(size).unwrap()
    );
    let suffix = b"\x1b8!\x1b[?1049l\x1b8?\x1b[6n";
    let offset = original.offset();
    assert_eq!(
        original.feed(offset, suffix).unwrap(),
        restored.feed(offset, suffix).unwrap()
    );
    assert_eq!(
        original.checkpoint().unwrap(),
        restored.checkpoint().unwrap()
    );
}

#[test]
fn mirrors_converge_without_emitting_external_effects() {
    let bytes = b"\x1b]0;title\x07\x1b[6n\x07\x1b]52;c;aGVsbG8=\x07\x1b]52;c;?\x07";
    let mut authority = engine();
    let mut mirror = Engine::new(Size::new(20, 5), EffectMode::Mirror).unwrap();
    let authoritative_effects = authority.feed(0, bytes).unwrap();
    let mirrored_effects = mirror.feed(0, bytes).unwrap();
    assert!(
        authoritative_effects
            .iter()
            .any(|e| matches!(e.kind, EffectKind::Reply(_)))
    );
    assert!(
        authoritative_effects
            .iter()
            .any(|e| matches!(e.kind, EffectKind::ClipboardRead { .. }))
    );
    assert!(mirrored_effects.iter().all(|e| !e.kind.external()));
    assert_eq!(
        authority.checkpoint().unwrap(),
        mirror.checkpoint().unwrap()
    );
}

#[test]
fn queries_observe_palette_at_query_time_not_end_of_chunk() {
    let mut terminal = engine();
    let effects = terminal
        .feed(
            0,
            b"\x1b]10;#112233\x07\x1b]10;?\x07\x1b]10;#445566\x07\x1b]10;?\x1b\\",
        )
        .unwrap();
    assert_eq!(
        replies(&effects),
        [
            b"\x1b]10;rgb:1111/2222/3333\x07".as_slice(),
            b"\x1b]10;rgb:4444/5555/6666\x1b\\".as_slice()
        ]
    );
}

#[test]
fn clipboard_read_is_data_until_consent_produces_a_reply() {
    let mut terminal = engine();
    let effects = terminal.feed(0, b"\x1b]52;c;?\x1b\\").unwrap();
    assert_eq!(replies(&effects), Vec::<&[u8]>::new());
    let request = effects
        .iter()
        .find(|e| matches!(e.kind, EffectKind::ClipboardRead { .. }))
        .unwrap();
    assert_eq!(
        request.kind.clipboard_reply("hello\n").unwrap(),
        b"\x1b]52;c;aGVsbG8K\x1b\\"
    );
}

#[test]
fn resume_overlap_is_not_repeated_and_a_gap_cannot_mutate_the_engine() {
    let mut terminal = engine();
    terminal.feed(0, b"abc").unwrap();
    terminal.feed(1, b"bcde").unwrap();
    let before = terminal.checkpoint().unwrap();
    terminal.feed(0, b"abcde").unwrap();
    assert_eq!(terminal.offset(), 5);
    assert_eq!(
        terminal.text_between(
            TextPoint { line: 0, column: 0 },
            TextPoint { line: 0, column: 4 }
        ),
        "abcde"
    );
    assert!(matches!(
        terminal.feed(6, b"!"),
        Err(EngineError::OutputGap {
            expected: 5,
            got: 6
        })
    ));
    assert_eq!(terminal.checkpoint().unwrap(), before);
}

#[test]
fn visible_wide_cells_combining_marks_and_soft_wrap_copy_agree() {
    let mut terminal = Engine::new(Size::new(4, 2), EffectMode::Authoritative).unwrap();
    terminal.feed(0, "a\u{301}b界c".as_bytes()).unwrap();
    let mut frame = RenderFrame::default();
    terminal.render_into(&mut frame);
    assert_eq!(frame.cells[0].zerowidth(), Some(['\u{301}'].as_slice()));
    assert_eq!(frame.cells[2].c, '界');
    assert!(frame.cells[3].flags.contains(Flags::WIDE_CHAR_SPACER));
    assert_eq!(frame.cells[4].c, 'c');
    assert_eq!(
        terminal.text_between(
            TextPoint { line: 0, column: 0 },
            TextPoint { line: 1, column: 0 }
        ),
        "a\u{301}b界c"
    );
}

#[test]
fn selection_endpoints_are_normalized_after_stale_rows_are_clamped() {
    let mut terminal = Engine::new(Size::new(8, 2), EffectMode::Authoritative).unwrap();
    terminal.feed(0, b"top\r\nbottom").unwrap();
    assert_eq!(
        terminal.text_between(
            TextPoint {
                line: 10,
                column: 7
            },
            TextPoint {
                line: 11,
                column: 0
            }
        ),
        "bottom"
    );
}

#[test]
fn invalid_geometry_pixels_and_checkpoint_identity_fail_before_use() {
    assert!(matches!(
        Engine::new(Size::new(1, 5), EffectMode::Mirror),
        Err(EngineError::InvalidSize)
    ));
    assert!(
        Size {
            columns: 1024,
            rows: 5,
            cell_width: 64,
            cell_height: 16
        }
        .validate()
        .is_err()
    );
    let mut checkpoint = engine().checkpoint().unwrap();
    checkpoint[7] = b'9';
    assert!(matches!(
        Engine::restore(&checkpoint, EffectMode::Mirror),
        Err(EngineError::Checkpoint(_))
    ));
}

#[test]
fn display_scrolling_does_not_change_the_canonical_checkpoint() {
    let mut terminal = engine();
    terminal
        .feed(0, b"0\r\n1\r\n2\r\n3\r\n4\r\n5\r\n6")
        .unwrap();
    let before = terminal.checkpoint().unwrap();
    terminal.scroll(2);
    let mut frame = RenderFrame::default();
    terminal.render_into(&mut frame);
    assert_eq!(frame.display_offset, 2);
    assert_eq!(frame.cells[0].c, '0');
    assert_eq!(terminal.checkpoint().unwrap(), before);
}

#[test]
fn application_title_returns_after_checkpointed_temporary_title() {
    let mut terminal = engine();
    terminal
        .feed(0, b"\x1b]0;remote shell\x07\x1b[22;0t\x1b]0;editor\x07")
        .unwrap();
    let mut restored =
        Engine::restore(&terminal.checkpoint().unwrap(), EffectMode::Authoritative).unwrap();
    assert_eq!(restored.title(), Some("editor"));
    let effects = restored.feed(restored.offset(), b"\x1b[23;0t").unwrap();
    assert_eq!(restored.title(), Some("remote shell"));
    assert!(
        effects
            .iter()
            .any(|e| e.kind == EffectKind::Title(Some("remote shell".into())))
    );
}
