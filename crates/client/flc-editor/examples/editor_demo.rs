//! Local visual check: `cargo run -p flc-editor --features linux-x11 --example
//! editor_demo -- <file> [<old> <new>]` opens the editor, or a diff of two files.
use fl_model::workspace::{FileContent, FileDiff};
use flc_editor::diff::Layout;
use flc_editor::diff_view::DiffView;
use flc_editor::editor::EditorView;
use gpui::prelude::*;
use gpui::{App, Application, Bounds, Focusable, WindowBounds, WindowOptions, px, size};

fn content(path: &str) -> FileContent {
    let bytes = std::fs::read(path).expect("readable file");
    FileContent {
        path: path.into(),
        size: bytes.len() as u64,
        binary: bytes.iter().take(8192).any(|b| *b == 0),
        bytes,
        sha256: [0; 32],
        truncated: false,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    Application::new().run(move |cx: &mut App| {
        flc_ui::theme::init(cx);
        flc_ui::input::bind_keys(cx);
        let bounds = Bounds::centered(None, size(px(1100.), px(720.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        };
        match args.as_slice() {
            [file] => {
                let file = content(file);
                cx.open_window(options, |window, cx| {
                    let view = cx.new(|cx| EditorView::from_content(&file, cx));
                    view.read(cx).focus_handle(cx).focus(window);
                    view
                })
                .unwrap();
            }
            [old, new, rest @ ..] => {
                let diff = FileDiff {
                    worktree: fl_model::WorktreeId::new(),
                    path: new.clone(),
                    old: Some(content(old)),
                    new: Some(content(new)),
                };
                let layout = if rest.first().map(String::as_str) == Some("split") {
                    Layout::SideBySide
                } else {
                    Layout::Unified
                };
                cx.open_window(options, |_, cx| {
                    cx.new(|cx| DiffView::new(&diff, layout, cx))
                })
                .unwrap();
            }
            _ => panic!("usage: editor_demo <file> | <old> <new> [split]"),
        }
        cx.activate(true);
    });
}
