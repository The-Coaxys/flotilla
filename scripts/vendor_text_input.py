#!/usr/bin/env python3
"""Vendor gpui 0.2.2 examples/input.rs (Apache-2.0) as flc_ui::input with Flotilla theming.

Usage: python3 scripts/vendor_text_input.py <path-to-gpui-0.2.2>/examples/input.rs crates/client/flc-ui/src/input.rs
Every substitution must match exactly once, or the script aborts.
The generated import list is pruned by hand afterwards: the upstream demo imports that
become unused (Application, WindowBounds, WindowOptions, black, hsla, opaque_grey, rgb,
rgba, white, yellow, Keystroke, MouseUpEvent, size, ...) are removed to satisfy rustc and clippy.
"""
import sys

src_path, dst_path = sys.argv[1], sys.argv[2]
lines = open(src_path, encoding="utf-8").read().split("\n")
assert lines[602] == "impl Focusable for TextInput {", repr(lines[602])
assert lines[606] == "}", repr(lines[606])
text = "\n".join(lines[:607]) + "\n"


def sub(old, new):
    global text
    n = text.count(old)
    if n != 1:
        sys.exit(f"expected exactly one match, found {n}: {old!r}")
    text = text.replace(old, new)


sub("        Copy,\n        Quit,\n    ]", "        Copy,\n    ]")
sub("\nstruct TextInput {", "\npub struct TextInput {")
sub(
    "        let input = self.input.read(cx);\n        let content = input.content.clone();",
    "        let (placeholder_color, accent) = {\n"
    "            let t = cx.theme();\n"
    "            (t.text_tertiary, t.accent)\n"
    "        };\n"
    "        let input = self.input.read(cx);\n        let content = input.content.clone();",
)
sub("(input.placeholder.clone(), hsla(0., 0., 0., 0.2))", "(input.placeholder.clone(), placeholder_color)")
sub("                    gpui::blue(),\n", "                    accent,\n")
sub("                    rgba(0x3311ff30),\n", "                    accent.opacity(0.3),\n")
sub(
    "impl Render for TextInput {\n    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {\n        div()",
    "impl Render for TextInput {\n    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {\n"
    "        let theme = cx.theme().clone();\n"
    "        let focused = self.focus_handle.is_focused(window);\n"
    "        div()",
)
sub(
    "            .bg(rgb(0xeeeeee))\n            .line_height(px(30.))\n            .text_size(px(24.))",
    "            .w_full()\n            .line_height(px(20.))\n            .text_size(theme.text_size)\n"
    "            .text_color(theme.text_primary)\n            .font_family(theme.font_ui.clone())",
)
sub(
    "                    .h(px(30. + 4. * 2.))\n                    .w_full()\n                    .p(px(4.))\n                    .bg(white())",
    "                    .h(px(32.))\n                    .w_full()\n                    .px(px(10.))\n                    .py(px(6.))\n"
    "                    .bg(theme.control_bg)\n                    .rounded(theme.radius_sm)\n                    .border_1()\n"
    "                    .border_color(if focused { theme.accent } else { theme.border })",
)

header = (
    "//! Single-line text input.\n"
    "//!\n"
    "//! Vendored from gpui 0.2.2 `examples/input.rs` (Apache-2.0, Copyright Zed Industries)\n"
    "//! by `scripts/vendor_text_input.py`, with Flotilla theming and a public API.\n"
    "//! See crates/client/flc-ui/NOTICE.\n"
    "#![allow(clippy::all)]\n"
    "use crate::theme::ActiveTheme;\n"
)

footer = '''
impl TextInput {
    pub fn new(cx: &mut Context<Self>, placeholder: impl Into<SharedString>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content: "".into(),
            placeholder: placeholder.into(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            is_selecting: false,
        }
    }

    pub fn text(&self) -> &str {
        &self.content
    }

    pub fn set_text(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.content = text.into();
        let end = self.content.len();
        self.selected_range = end..end;
        self.selection_reversed = false;
        self.marked_range = None;
        cx.notify();
    }
}

/// Windows-style editing keys, active while a `TextInput` has focus.
pub fn bind_keys(cx: &mut App) {
    let ctx = Some("TextInput");
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, ctx),
        KeyBinding::new("delete", Delete, ctx),
        KeyBinding::new("left", Left, ctx),
        KeyBinding::new("right", Right, ctx),
        KeyBinding::new("shift-left", SelectLeft, ctx),
        KeyBinding::new("shift-right", SelectRight, ctx),
        KeyBinding::new("home", Home, ctx),
        KeyBinding::new("end", End, ctx),
        KeyBinding::new("ctrl-a", SelectAll, ctx),
        KeyBinding::new("ctrl-c", Copy, ctx),
        KeyBinding::new("ctrl-v", Paste, ctx),
        KeyBinding::new("ctrl-x", Cut, ctx),
    ]);
}
'''

open(dst_path, "w", encoding="utf-8").write(header + text + footer)
print(f"wrote {dst_path}")
