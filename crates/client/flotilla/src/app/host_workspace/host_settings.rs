//! The settings overlay: theme editor and keymap editor (spec §8.2, §8.3).
//!
//! Every edit goes through the shared `Settings` entity, which rebuilds the
//! theme global and reinstalls the keymap immediately; `Save` is the only
//! thing that writes `settings.json` / `keymap.json`.
use flc_terminal::EchoPolicy;
use flc_ui::input::{TextInput, TextInputEvent};
use flc_ui::keymap::{self, Chord};
use flc_ui::notify::{EventClass, NotificationSettings};
use flc_ui::theme::Theme;
use flc_ui::theme_settings::{Mode, format_hex, parse_hex};
use gpui::Focusable as _;
use gpui::prelude::*;
use gpui::{
    AnyElement, Context, Div, Entity, KeyDownEvent, MouseButton, SharedString, Stateful,
    Subscription, Window, div, px,
};

use fl_model::workspace::LayoutScope;

use super::HostWorkspace;
use crate::app::host_view::{primary_button, secondary_button};

/// Canvas columns: hue left to right.
const CANVAS_COLS: usize = 18;
/// Canvas rows: value (brightness) top to bottom.
const CANVAS_ROWS: usize = 8;
/// Slider steps for 0..1 values.
const SLIDER_STEPS: usize = 20;
const SWATCHES: [u32; 12] = [
    0x3D8BFD, 0x7C5CFF, 0xB88BE5, 0xE5534B, 0xFF8A3D, 0xD9A53C, 0x5DBB63, 0x2FB7A8, 0x59B8C7,
    0x8A8F98, 0xDFE0E5, 0x1D1E21,
];
const MONO_FAMILIES: [&str; 6] = [
    "Cascadia Mono",
    "JetBrains Mono",
    "Consolas",
    "Menlo",
    "Fira Code",
    "Monaco",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tab {
    Theme,
    Keyboard,
    Notifications,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Target {
    Background,
    Accent,
    Highlight,
    /// The ANSI color at this index in the terminal palette editor.
    Ansi(usize),
}

impl Target {
    fn label(self) -> &'static str {
        match self {
            Target::Background => "Background",
            Target::Accent => "Accent",
            Target::Highlight => "Highlight",
            Target::Ansi(_) => "Terminal ANSI",
        }
    }
}

/// Which theme slider a row edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SliderKind {
    Intensity,
    Alpha,
    Accent,
    Highlight,
}

/// Which per-surface font setting a stepper edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FontField {
    Editor,
    Terminal,
    PanelTerminal,
    Chat,
}

impl FontField {
    fn delta(self) -> f32 {
        match self {
            FontField::Chat => 0.05,
            _ => 0.5,
        }
    }

    fn apply(self, fonts: &mut flc_ui::theme_settings::SurfaceFonts, delta: f32) {
        match self {
            FontField::Editor => {
                fonts.editor_size = step(fonts.editor_size, delta, 8.0, 28.0);
            }
            FontField::Terminal => {
                fonts.terminal_size = step(fonts.terminal_size, delta, 8.0, 28.0);
            }
            FontField::PanelTerminal => {
                fonts.panel_terminal_size = step(fonts.panel_terminal_size, delta, 8.0, 28.0);
            }
            FontField::Chat => {
                fonts.chat_scale = step(fonts.chat_scale, delta, 0.6, 2.0);
            }
        }
    }
}

/// Open overlay state.
pub(super) struct Ui {
    pub(super) tab: Tab,
    /// The palette under edit (Light or Dark); independent of the mode.
    pub(super) editing_dark: bool,
    pub(super) target: Target,
    /// The action waiting for a chord, if any.
    pub(super) capture: Option<&'static str>,
    pub(super) message: Option<SharedString>,
    hex: Entity<TextInput>,
    _hex: Subscription,
    _changes: Subscription,
}

/// HSL to `0xRRGGBB` with the canvas's fixed saturation.
fn hsl(hue: f32, saturation: f32, value: f32) -> u32 {
    let c = value * saturation;
    let h = (hue.rem_euclid(360.0)) / 60.0;
    let x = c * (1.0 - (h % 2.0 - 1.0).abs());
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = value - c;
    let channel = |v: f32| ((v + m).clamp(0.0, 1.0) * 255.0).round() as u32;
    (channel(r) << 16) | (channel(g) << 8) | channel(b)
}

/// One cell of the color canvas: hue across, brightness down.
pub(super) fn canvas_color(column: usize, row: usize) -> u32 {
    let hue = column as f32 / CANVAS_COLS as f32 * 360.0;
    let value = 1.0 - row as f32 / CANVAS_ROWS as f32 * 0.75;
    hsl(hue, 1.0, value)
}

/// Step a bounded value, snapping to quarter steps for sizes and hundredths
/// for sliders.
fn step(current: f32, delta: f32, min: f32, max: f32) -> f32 {
    ((current + delta).clamp(min, max) * 100.0).round() / 100.0
}

impl HostWorkspace {
    pub(crate) fn active_workspace(&self) -> Option<fl_model::WorkspaceId> {
        let LayoutScope::Worktree(id) = self.scope else {
            return None;
        };
        let worktree = self.state.worktree(id)?;
        self.state
            .tree
            .as_ref()?
            .projects
            .iter()
            .find(|project| project.id == worktree.project)
            .map(|project| project.workspace)
    }

    /// The terminal-tab font, size and explicit palette for the visible
    /// workspace.
    pub(super) fn shell_options(&self, cx: &gpui::App) -> super::ShellViewOptions {
        let client = self.settings.read(cx).client();
        let id = self.active_workspace().map(|id| id.to_string());
        let workspace = client.for_workspace(id.as_deref());
        super::ShellViewOptions {
            echo_policy: EchoPolicy::default(),
            font_family: Some(client.fonts.terminal_family.clone().into()),
            font_size: Some(px(client.fonts.terminal_size)),
            palette: workspace.terminal.palette(),
        }
    }

    /// Setup/Run/Teardown terminals: their own size, same palette override.
    pub(super) fn script_options(&self, cx: &gpui::App) -> super::TerminalViewOptions {
        let client = self.settings.read(cx).client();
        let id = self.active_workspace().map(|id| id.to_string());
        let workspace = client.for_workspace(id.as_deref());
        super::TerminalViewOptions {
            font_family: Some(client.fonts.terminal_family.clone().into()),
            font_size: Some(px(client.fonts.panel_terminal_size)),
            palette: workspace.terminal.palette(),
            ..Default::default()
        }
    }

    /// Apply the current settings to every live terminal surface.
    pub(super) fn apply_presentation(&mut self, cx: &mut Context<Self>) {
        let options = self.shell_options(cx);
        for (shell, _) in self.shells.values() {
            shell.update(cx, |shell, cx| shell.set_presentation(options.clone(), cx));
        }
        let script = self.script_options(cx);
        for view in self.script_views.values() {
            view.update(cx, |view, cx| {
                let mut next = view.options().clone();
                next.font_family = script.font_family.clone();
                next.font_size = script.font_size;
                next.palette = script.palette.clone();
                view.set_options(next, cx);
            });
        }
        cx.notify();
    }

    pub(crate) fn request_settings(&mut self, cx: &mut Context<Self>) {
        self.pending_settings = true;
        cx.notify();
    }

    pub(super) fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings_ui.is_some() {
            return;
        }
        let hex = cx.new(|cx| TextInput::new(cx, "#3D8BFD"));
        let events = cx.subscribe_in(
            &hex,
            window,
            |host, _input, event: &TextInputEvent, window, cx| match event {
                TextInputEvent::Submitted(text) => {
                    host.settings_apply_hex(text.clone(), window, cx)
                }
                TextInputEvent::Cancelled => host.close_settings(window, cx),
            },
        );
        let changes = cx.observe(&hex, |_, _, cx| cx.notify());
        hex.read(cx).focus_handle(cx).focus(window);
        self.settings_ui = Some(Ui {
            tab: Tab::Theme,
            editing_dark: true,
            target: Target::Background,
            capture: None,
            message: None,
            hex,
            _hex: events,
            _changes: changes,
        });
        cx.notify();
    }

    pub(super) fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings_ui.take().is_some() {
            self.refocus(window, cx);
        }
        cx.notify();
    }

    fn settings_target_color(&self, cx: &gpui::App) -> u32 {
        let Some(ui) = self.settings_ui.as_ref() else {
            return 0;
        };
        let settings = self.settings.read(cx);
        let workspace = settings.current();
        let palette = workspace.theme.palette(ui.editing_dark);
        match ui.target {
            Target::Background => palette.background,
            Target::Accent => palette.accent,
            Target::Highlight => palette.highlight,
            Target::Ansi(index) => workspace.terminal.ansi[index.min(15)],
        }
    }

    fn settings_apply_color(&mut self, color: u32, cx: &mut Context<Self>) {
        let Some(ui) = self.settings_ui.as_ref() else {
            return;
        };
        let target = ui.target;
        let dark = ui.editing_dark;
        let hex = ui.hex.clone();
        hex.update(cx, |input, cx| {
            input.set_text(format!("#{}", format_hex(color)), cx);
        });
        if let Some(ui) = self.settings_ui.as_mut() {
            ui.message = None;
        }
        let settings = self.settings.clone();
        settings.update(cx, |settings, cx| match target {
            Target::Background => {
                settings.edit_theme(|theme| theme.palette_mut(dark).background = color, cx)
            }
            Target::Accent => {
                settings.edit_theme(|theme| theme.palette_mut(dark).accent = color, cx)
            }
            Target::Highlight => {
                settings.edit_theme(|theme| theme.palette_mut(dark).highlight = color, cx)
            }
            Target::Ansi(index) => settings.edit_terminal(
                |terminal| {
                    terminal.override_engine = true;
                    terminal.ansi[index.min(15)] = color;
                },
                cx,
            ),
        });
    }

    fn settings_apply_hex(
        &mut self,
        text: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match parse_hex(&text) {
            Some(color) => {
                self.settings_apply_color(color, cx);
                if let Some(ui) = self.settings_ui.as_mut() {
                    ui.message = None;
                }
            }
            None => {
                if let Some(ui) = self.settings_ui.as_mut() {
                    ui.message = Some(SharedString::from(format!(
                        "“{text}” is not a color; use #RRGGBB."
                    )));
                }
            }
        }
        window.focus(&self.focus);
        cx.notify();
    }

    fn settings_hex_sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let color = self.settings_target_color(cx);
        let Some(ui) = self.settings_ui.as_ref() else {
            return;
        };
        // Never rewrite the field while the user is typing in it.
        let focused = ui.hex.read(cx).focus_handle(cx).is_focused(window);
        let text = format!("#{}", format_hex(color));
        if !focused && ui.hex.read(cx).text() != text {
            ui.hex
                .clone()
                .update(cx, |input, cx| input.set_text(text, cx));
        }
    }

    /// True while a rebind is waiting for its chord.
    pub(super) fn capturing(&self) -> bool {
        self.settings_ui
            .as_ref()
            .is_some_and(|ui| ui.capture.is_some())
    }

    /// Take the next key as the new chord. Runs from a window keystroke
    /// interceptor so even terminal-owned chords (Ctrl+P) are seen by the
    /// editor instead of being swallowed by the focused terminal.
    pub(super) fn capture_chord(
        &mut self,
        key: &str,
        control: bool,
        shift: bool,
        alt: bool,
        platform: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(ui) = self.settings_ui.as_mut() else {
            return;
        };
        if key == "escape" {
            ui.capture = None;
            ui.message = Some(SharedString::from("Rebind cancelled."));
            cx.notify();
            return;
        }
        let Some(action) = ui.capture else {
            return;
        };
        let chord = Chord::new(key, control, shift, alt, platform);
        let settings = self.settings.clone();
        let result = settings.update(cx, |settings, cx| settings.bind(action, chord, cx));
        if let Some(ui) = self.settings_ui.as_mut() {
            ui.capture = None;
            ui.message = Some(match result {
                Ok(()) => SharedString::from("Binding applied."),
                Err(conflict) => SharedString::from(conflict.message),
            });
        }
        cx.notify();
    }

    /// Escape closes the overlay when no rebind is pending.
    pub(super) fn settings_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.settings_ui.is_none() {
            return false;
        }
        if event.keystroke.key == "escape" && !self.capturing() {
            self.close_settings(window, cx);
            return true;
        }
        false
    }

    // ---- rendering ---------------------------------------------------------

    pub(super) fn render_settings(
        &mut self,
        t: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        self.settings_ui.as_ref()?;
        self.settings_hex_sync(window, cx);
        let (tab, capture, message) = {
            let ui = self.settings_ui.as_ref()?;
            (ui.tab, ui.capture, ui.message.clone())
        };
        let body = match tab {
            Tab::Theme => self.settings_theme_body(t, cx),
            Tab::Keyboard => self.settings_keymap_body(t, cx),
            Tab::Notifications => self.settings_notifications_body(t, cx),
        };
        let tabs = [Tab::Theme, Tab::Keyboard, Tab::Notifications]
            .into_iter()
            .map(|candidate| {
                let active = candidate == tab;
                let label = match candidate {
                    Tab::Theme => "Theme",
                    Tab::Keyboard => "Keyboard",
                    Tab::Notifications => "Notifications",
                };
                div()
                    .id(SharedString::from(format!("settings-tab-{label}")))
                    .h(px(24.))
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .rounded(t.radius_sm)
                    .cursor_pointer()
                    .text_size(t.text_size_sm)
                    .text_color(if active {
                        t.text_primary
                    } else {
                        t.text_secondary
                    })
                    .when(active, |d| d.bg(t.control_bg))
                    .child(label)
                    .on_click(cx.listener(move |host, _, _, cx| {
                        if let Some(ui) = host.settings_ui.as_mut() {
                            ui.tab = candidate;
                            ui.capture = None;
                        }
                        cx.notify();
                    }))
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        let footer = self.settings_footer(t, cx);
        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .flex()
                .justify_center()
                .bg(gpui::black().opacity(0.4))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|host, _, window, cx| host.close_settings(window, cx)),
                )
                .child(
                    div()
                        .id("settings")
                        .mt(px(20.))
                        .w(px(720.))
                        .max_h(px(720.))
                        .flex()
                        .flex_col()
                        .rounded(t.radius_lg)
                        .bg(t.surface_raised)
                        .border_1()
                        .border_color(t.border)
                        .shadow_lg()
                        .text_color(t.text_primary)
                        .on_mouse_down(MouseButton::Left, |_, _, cx: &mut gpui::App| {
                            cx.stop_propagation()
                        })
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .px(px(14.))
                                .pt(px(12.))
                                .pb(px(8.))
                                .child(div().text_size(px(15.)).child("Settings").mr(px(8.)))
                                .children(tabs)
                                .child(div().flex_1())
                                .child(
                                    div()
                                        .id("settings-close")
                                        .cursor_pointer()
                                        .text_size(t.text_size_sm)
                                        .text_color(t.text_secondary)
                                        .child("Close")
                                        .on_click(cx.listener(|host, _, window, cx| {
                                            host.close_settings(window, cx)
                                        })),
                                ),
                        )
                        .when_some(capture, |d, action| {
                            let label = keymap::ACTIONS
                                .iter()
                                .find(|spec| spec.id == action)
                                .map(|spec| spec.label)
                                .unwrap_or(action);
                            d.child(
                                div()
                                    .px(px(14.))
                                    .py(px(6.))
                                    .bg(t.accent.opacity(0.18))
                                    .text_size(t.text_size_sm)
                                    .child(format!(
                                        "Press the new keys for “{label}” — Esc cancels."
                                    )),
                            )
                        })
                        .when_some(message, |d, message| {
                            d.child(
                                div()
                                    .id("settings-message")
                                    .px(px(14.))
                                    .py(px(6.))
                                    .text_size(t.text_size_sm)
                                    .text_color(t.warning)
                                    .child(message),
                            )
                        })
                        .child(
                            div()
                                .id("settings-body")
                                .px(px(14.))
                                .pb(px(10.))
                                .max_h(px(470.))
                                .overflow_y_scroll()
                                .child(body),
                        )
                        .child(footer),
                )
                .into_any_element(),
        )
    }

    fn settings_footer(&mut self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let tab = self
            .settings_ui
            .as_ref()
            .map(|ui| ui.tab)
            .unwrap_or(Tab::Theme);
        let settings = self.settings.clone();
        let (notes, path) = {
            let settings = settings.read(cx);
            let notes = settings
                .notes()
                .iter()
                .map(|note| note.to_string())
                .collect::<Vec<_>>()
                .join(" · ");
            (notes, settings.store_dir().display().to_string())
        };
        let save = self.settings.clone();
        let mut row = div()
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(14.))
            .py(px(10.))
            .border_t_1()
            .border_color(t.border)
            .text_size(t.text_size_sm)
            .text_color(t.text_tertiary)
            .child(div().flex_1().truncate().child(if notes.is_empty() {
                SharedString::from(path)
            } else {
                SharedString::from(notes)
            }))
            .child(
                primary_button("settings-save", "Save", t).on_click(cx.listener(
                    move |_, _, _, cx| {
                        save.update(cx, |settings, cx| {
                            settings.save(cx);
                            if tab == Tab::Keyboard {
                                settings.save_keymap(cx);
                            }
                        });
                        cx.notify();
                    },
                )),
            );
        row = match tab {
            Tab::Theme => row.child(secondary_button("settings-reset", "Reset", t).on_click(
                cx.listener(move |host, _, _, cx| {
                    let settings = host.settings.clone();
                    settings.update(cx, |settings, cx| settings.reset_theme(cx));
                    if let Some(ui) = host.settings_ui.as_mut() {
                        ui.message = Some(SharedString::from("Theme reset to the defaults."));
                    }
                    cx.notify();
                }),
            )),
            Tab::Keyboard => row.child(
                secondary_button("settings-reset-keys", "Reset all", t).on_click(cx.listener(
                    move |host, _, _, cx| {
                        let settings = host.settings.clone();
                        settings.update(cx, |settings, cx| {
                            settings.reset_keymap(cx);
                            settings.save_keymap(cx);
                        });
                        if let Some(ui) = host.settings_ui.as_mut() {
                            ui.message = Some(SharedString::from("Keymap reset to defaults."));
                        }
                        cx.notify();
                    },
                )),
            ),
            Tab::Notifications => row,
        };
        if tab == Tab::Theme {
            row = row.child(
                secondary_button("settings-default", "Set as default", t).on_click(cx.listener(
                    move |host, _, _, cx| {
                        let settings = host.settings.clone();
                        settings.update(cx, |settings, cx| settings.set_as_default(cx));
                        if let Some(ui) = host.settings_ui.as_mut() {
                            ui.message =
                                Some(SharedString::from("This workspace is the default now."));
                        }
                        cx.notify();
                    },
                )),
            );
        }
        row.into_any_element()
    }

    fn settings_notifications_body(&mut self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let policy = self.settings.read(cx).notifications().clone();
        let mut body = div().flex().flex_col().gap(px(10.)).pt(px(8.)).child(
            div()
                .text_size(t.text_size_sm)
                .text_color(t.text_secondary)
                .child(SharedString::from(
                    "Desktop notifications are a Windows feature (spec §8.4). The inbox, in-app toasts and banner always stay on.",
                )),
        );
        for (class, title, detail) in [
            (
                EventClass::Completion,
                "Completion",
                "A turn or session finishes.",
            ),
            (
                EventClass::Input,
                "Approval or input",
                "A tool asks for approval or a question waits for an answer.",
            ),
        ] {
            let current = policy.delivery(class);
            body = body.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .child(div().text_size(t.text_size_sm).child(title))
                            .child(
                                div()
                                    .text_size(t.text_size_sm)
                                    .text_color(t.text_tertiary)
                                    .child(detail),
                            ),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("notify-{title}",)))
                            .px(px(10.))
                            .h(px(24.))
                            .flex()
                            .items_center()
                            .rounded(t.radius_sm)
                            .bg(t.control_bg)
                            .cursor_pointer()
                            .text_size(t.text_size_sm)
                            .child(current.label())
                            .on_click(cx.listener(move |host, _, _, cx| {
                                let next = current.next();
                                let settings = host.settings.clone();
                                settings.update(cx, |settings, cx| {
                                    settings.edit_notifications(
                                        |policy| policy.set_delivery(class, next),
                                        cx,
                                    )
                                });
                                cx.notify();
                            })),
                    ),
            );
        }
        let sounds = policy.sounds;
        body = body.child(
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .pt(px(6.))
                .child(div().flex_1().text_size(t.text_size_sm).child("Sounds"))
                .child(
                    div()
                        .id("notify-sounds")
                        .px(px(10.))
                        .h(px(24.))
                        .flex()
                        .items_center()
                        .rounded(t.radius_sm)
                        .bg(if sounds { t.accent } else { t.control_bg })
                        .text_color(if sounds {
                            gpui::white()
                        } else {
                            t.text_secondary
                        })
                        .cursor_pointer()
                        .text_size(t.text_size_sm)
                        .child(if sounds { "On" } else { "Off" })
                        .on_click(cx.listener(move |host, _, _, cx| {
                            let settings = host.settings.clone();
                            settings.update(cx, |settings, cx| {
                                settings
                                    .edit_notifications(|policy| policy.sounds = !policy.sounds, cx)
                            });
                            cx.notify();
                        })),
                ),
        );
        let defaults = NotificationSettings::default();
        let settings = self.settings.clone();
        body = body.child(div().pt(px(10.)).child(
            secondary_button("notify-defaults", "Restore defaults", t).on_click(cx.listener(
                move |host, _, _, cx| {
                    let settings = settings.clone();
                    settings.update(cx, |settings, cx| {
                        settings.edit_notifications(
                            |policy| *policy = NotificationSettings::default(),
                            cx,
                        );
                    });
                    if let Some(ui) = host.settings_ui.as_mut() {
                        ui.message = Some(SharedString::from(format!(
                            "Notifications: completion {}, input {}, sounds {}.",
                            defaults.completion.label(),
                            defaults.input.label(),
                            if defaults.sounds { "on" } else { "off" }
                        )));
                    }
                    cx.notify();
                },
            )),
        ));
        body.into_any_element()
    }

    fn settings_theme_body(&mut self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let ui_target = self
            .settings_ui
            .as_ref()
            .map(|ui| (ui.target, ui.editing_dark))
            .unwrap_or((Target::Background, true));
        let (target, editing_dark) = ui_target;
        let (mode, palette, terminal, fonts, saved, error) = {
            let settings = self.settings.read(cx);
            let workspace = settings.current();
            (
                workspace.theme.mode,
                *workspace.theme.palette(editing_dark),
                workspace.terminal.clone(),
                settings.client().fonts.clone(),
                settings.saved_workspaces(),
                settings.settings_error().cloned(),
            )
        };

        let modes = Mode::ALL
            .into_iter()
            .map(|candidate| {
                let settings = self.settings.clone();
                chip(
                    format!("settings-mode-{}", candidate.label()),
                    candidate.label(),
                    candidate == mode,
                    t,
                )
                .on_click(cx.listener(move |_, _, _, cx| {
                    settings.update(cx, |settings, cx| {
                        settings.edit_theme(|theme| theme.mode = candidate, cx)
                    });
                }))
                .into_any_element()
            })
            .collect::<Vec<_>>();

        let sides = [true, false]
            .into_iter()
            .map(|dark| {
                chip(
                    format!("settings-edit-{}", if dark { "dark" } else { "light" }),
                    if dark { "Edit: Dark" } else { "Edit: Light" },
                    editing_dark == dark,
                    t,
                )
                .on_click(cx.listener(move |host, _, _, cx| {
                    if let Some(ui) = host.settings_ui.as_mut() {
                        ui.editing_dark = dark;
                        ui.target = Target::Background;
                    }
                    cx.notify();
                }))
                .into_any_element()
            })
            .collect::<Vec<_>>();

        let targets = [
            (Target::Background, palette.background),
            (Target::Accent, palette.accent),
            (Target::Highlight, palette.highlight),
        ]
        .into_iter()
        .map(|(kind, color)| {
            let mut row = chip(
                format!("settings-target-{}", kind.label()),
                kind.label(),
                target == kind,
                t,
            )
            .child(div().flex_1())
            .child(
                div()
                    .w(px(16.))
                    .h(px(16.))
                    .rounded(px(3.))
                    .bg(gpui::rgb(color)),
            );
            row = row.on_click(cx.listener(move |host, _, _, cx| {
                if let Some(ui) = host.settings_ui.as_mut() {
                    ui.target = kind;
                }
                cx.notify();
            }));
            row.into_any_element()
        })
        .collect::<Vec<_>>();

        let mut canvas_rows = Vec::new();
        let cell = 20.0_f32;
        for row in 0..CANVAS_ROWS {
            let mut cells = Vec::new();
            for column in 0..CANVAS_COLS {
                let color = canvas_color(column, row);
                cells.push(
                    div()
                        .id(SharedString::from(format!("canvas-{row}-{column}")))
                        .w(px(cell))
                        .h(px(cell))
                        .bg(gpui::rgb(color))
                        .on_click(cx.listener(move |host, _, _, cx| {
                            host.settings_apply_color(color, cx);
                        }))
                        .into_any_element(),
                );
            }
            canvas_rows.push(div().flex().flex_row().children(cells).into_any_element());
        }

        let swatches = SWATCHES
            .into_iter()
            .map(|color| {
                div()
                    .id(SharedString::from(format!("swatch-{color:06X}")))
                    .w(px(20.))
                    .h(px(20.))
                    .rounded(px(4.))
                    .border_1()
                    .border_color(t.border)
                    .bg(gpui::rgb(color))
                    .on_click(cx.listener(move |host, _, _, cx| {
                        host.settings_apply_color(color, cx);
                    }))
                    .into_any_element()
            })
            .collect::<Vec<_>>();

        let copy_rows = [(true, "Dark → Light"), (false, "Light → Dark")]
            .into_iter()
            .map(|(from_dark, label)| {
                let settings = self.settings.clone();
                chip(format!("settings-copy-{}", label), label, false, t)
                    .on_click(cx.listener(move |_, _, _, cx| {
                        settings.update(cx, |settings, cx| {
                            settings.edit_theme(
                                |theme| {
                                    let source = *theme.palette_mut(from_dark);
                                    *theme.palette_mut(!from_dark) = source;
                                },
                                cx,
                            )
                        });
                    }))
                    .into_any_element()
            })
            .collect::<Vec<_>>();

        let copy_workspaces = saved
            .into_iter()
            .map(|(id, label)| {
                let settings = self.settings.clone();
                chip(format!("settings-copy-ws-{id}"), label, false, t)
                    .on_click(cx.listener(move |_, _, _, cx| {
                        let id = id.clone();
                        settings.update(cx, |settings, cx| settings.copy_from_workspace(&id, cx));
                    }))
                    .into_any_element()
            })
            .collect::<Vec<_>>();

        let sliders = [
            (
                SliderKind::Intensity,
                "Background intensity",
                palette.background_intensity,
            ),
            (
                SliderKind::Alpha,
                "Background alpha",
                palette.background_alpha,
            ),
            (
                SliderKind::Accent,
                "Accent prominence",
                palette.accent_prominence,
            ),
            (
                SliderKind::Highlight,
                "Highlight prominence",
                palette.highlight_prominence,
            ),
        ]
        .into_iter()
        .map(|(kind, label, value)| self.settings_slider(kind, label, value, t, cx))
        .collect::<Vec<_>>();

        let override_on = terminal.override_engine;
        let palette_toggle = chip(
            "settings-terminal-override",
            if override_on {
                "Terminal palette: custom (explicit override)"
            } else {
                "Terminal palette: engine (no override)"
            },
            override_on,
            t,
        )
        .on_click(cx.listener(move |host, _, _, cx| {
            let settings = host.settings.clone();
            settings.update(cx, |settings, cx| {
                settings.edit_terminal(|terminal| terminal.override_engine = !override_on, cx)
            });
            if let Some(ui) = host.settings_ui.as_mut() {
                ui.target = Target::Ansi(0);
            }
            cx.notify();
        }));

        let ansi = terminal
            .ansi
            .iter()
            .enumerate()
            .map(|(index, color)| {
                let color = *color;
                div()
                    .id(SharedString::from(format!("ansi-{index}")))
                    .w(px(18.))
                    .h(px(18.))
                    .rounded(px(3.))
                    .border_1()
                    .border_color(if target == Target::Ansi(index) {
                        t.accent
                    } else {
                        t.border
                    })
                    .bg(gpui::rgb(color))
                    .on_click(cx.listener(move |host, _, _, cx| {
                        let settings = host.settings.clone();
                        settings.update(cx, |settings, cx| {
                            settings.edit_terminal(|terminal| terminal.override_engine = true, cx)
                        });
                        if let Some(ui) = host.settings_ui.as_mut() {
                            ui.target = Target::Ansi(index);
                        }
                        cx.notify();
                    }))
                    .into_any_element()
            })
            .collect::<Vec<_>>();

        let editor_families = self.font_family_chips(false, &fonts, t, cx);
        let terminal_families = self.font_family_chips(true, &fonts, t, cx);
        let steppers = vec![
            self.font_stepper(
                FontField::Editor,
                "Editor / diff size",
                fonts.editor_size,
                t,
                cx,
            ),
            self.font_stepper(
                FontField::Terminal,
                "Terminal tab size",
                fonts.terminal_size,
                t,
                cx,
            ),
            self.font_stepper(
                FontField::PanelTerminal,
                "Right-panel terminal size",
                fonts.panel_terminal_size,
                t,
                cx,
            ),
            self.font_stepper(FontField::Chat, "Chat scale", fonts.chat_scale, t, cx),
        ];

        let hex = self
            .settings_ui
            .as_ref()
            .map(|ui| ui.hex.clone())
            .unwrap_or_else(|| unreachable!("the overlay is open"));
        let hex_row = div()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(div().w(px(120.)).child(format!("{} hex", target.label())))
            .child(div().w(px(150.)).child(hex))
            .child(
                div()
                    .text_size(t.text_size_sm)
                    .text_color(t.text_tertiary)
                    .child("Type #RRGGBB and press Enter."),
            );

        div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .children(vec![
                section(t, "Mode", vec![div().flex().gap(px(6.)).children(modes).into_any_element()]),
                section(
                    t,
                    "Colors (per mode)",
                    vec![
                        div().flex().gap(px(6.)).children(sides).into_any_element(),
                        div().flex().gap(px(6.)).children(targets).into_any_element(),
                        div().flex().flex_col().children(canvas_rows).into_any_element(),
                        hex_row.into_any_element(),
                        div().flex().flex_wrap().gap(px(4.)).children(swatches).into_any_element(),
                        div().flex().flex_wrap().gap(px(6.)).children(copy_rows).into_any_element(),
                        div().flex().flex_wrap().gap(px(6.)).children(copy_workspaces).into_any_element(),
                    ],
                ),
                section(t, "Sliders", sliders),
                section(
                    t,
                    "Terminal palette (independent of the theme)",
                    vec![
                        palette_toggle.into_any_element(),
                        div().flex().flex_wrap().gap(px(3.)).children(ansi).into_any_element(),
                    ],
                ),
                section(t, "Fonts and sizes (per surface)", {
                    let mut rows = vec![
                        div().flex().items_center().gap(px(8.))
                            .child(div().w(px(130.)).child("Editor family"))
                            .children(editor_families)
                            .into_any_element(),
                        div().flex().items_center().gap(px(8.))
                            .child(div().w(px(130.)).child("Terminal family"))
                            .children(terminal_families)
                            .into_any_element(),
                    ];
                    rows.extend(steppers);
                    rows
                }),
                section(t, "Workspace", {
                    let unreadable = self.settings.read(cx).file_unreadable();
                    let mut rows = vec![
                        div()
                            .text_size(t.text_size_sm)
                            .text_color(if error.is_some() { t.danger } else { t.text_tertiary })
                            .child(error.unwrap_or_else(|| SharedString::from(
                                "Saved locally beside hosts.json; applies live to every open surface.",
                            )))
                            .into_any_element(),
                    ];
                    if unreadable {
                        let replace = self.settings.clone();
                        rows.push(
                            chip(
                                "settings-replace-file",
                                "Replace the unreadable settings.json",
                                false,
                                t,
                            )
                            .on_click(cx.listener(move |_, _, _, cx| {
                                replace.update(cx, |settings, cx| settings.reset_file(cx));
                            }))
                            .into_any_element(),
                        );
                    }
                    rows
                }),
            ])
            .into_any_element()
    }

    fn settings_slider(
        &mut self,
        kind: SliderKind,
        label: &'static str,
        value: f32,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let filled = (value.clamp(0.0, 1.0) * SLIDER_STEPS as f32).round() as usize;
        let dark = self
            .settings_ui
            .as_ref()
            .map(|ui| ui.editing_dark)
            .unwrap_or(true);
        let segments = (0..SLIDER_STEPS)
            .map(|index| {
                let next = (index + 1) as f32 / SLIDER_STEPS as f32;
                div()
                    .id(SharedString::from(format!("slider-{label}-{index}")))
                    .w(px(8.))
                    .h(px(14.))
                    .bg(if index < filled {
                        t.accent
                    } else {
                        t.control_bg
                    })
                    .border_1()
                    .border_color(t.border)
                    .on_click(cx.listener(move |host, _, _, cx| {
                        let settings = host.settings.clone();
                        settings.update(cx, |settings, cx| {
                            settings.edit_theme(
                                |theme| {
                                    let palette = theme.palette_mut(dark);
                                    match kind {
                                        SliderKind::Intensity => {
                                            palette.background_intensity = next
                                        }
                                        SliderKind::Alpha => palette.background_alpha = next,
                                        SliderKind::Accent => palette.accent_prominence = next,
                                        SliderKind::Highlight => {
                                            palette.highlight_prominence = next
                                        }
                                    }
                                },
                                cx,
                            )
                        });
                    }))
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(div().w(px(150.)).child(label))
            .child(div().flex().children(segments))
            .child(
                div()
                    .w(px(40.))
                    .text_size(t.text_size_sm)
                    .text_color(t.text_secondary)
                    .child(format!("{value:.2}")),
            )
            .into_any_element()
    }

    fn font_family_chips(
        &mut self,
        terminal: bool,
        fonts: &flc_ui::theme_settings::SurfaceFonts,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        MONO_FAMILIES
            .into_iter()
            .map(|family| {
                let settings = self.settings.clone();
                let active = if terminal {
                    fonts.terminal_family == family
                } else {
                    fonts.editor_family == family
                };
                chip(
                    format!(
                        "settings-family-{}-{family}",
                        if terminal { "t" } else { "e" }
                    ),
                    family,
                    active,
                    t,
                )
                .on_click(cx.listener(move |_, _, _, cx| {
                    settings.update(cx, |settings, cx| {
                        settings.edit_fonts(
                            |fonts| {
                                if terminal {
                                    fonts.terminal_family = family.into();
                                } else {
                                    fonts.editor_family = family.into();
                                }
                            },
                            cx,
                        )
                    });
                }))
                .into_any_element()
            })
            .collect()
    }

    fn font_stepper(
        &mut self,
        field: FontField,
        label: &'static str,
        value: f32,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let apply = move |fonts: &mut flc_ui::theme_settings::SurfaceFonts, delta: f32| {
            field.apply(fonts, delta);
        };
        let dec = self.settings.clone();
        let dec_id = SharedString::from(format!("{label}-dec"));
        let dec = div()
            .id(dec_id)
            .w(px(22.))
            .h(px(22.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(t.radius_sm)
            .bg(t.control_bg)
            .cursor_pointer()
            .child("−")
            .on_click(cx.listener(move |_, _, _, cx| {
                dec.update(cx, |settings, cx| {
                    settings.edit_fonts(|fonts| apply(fonts, -field.delta()), cx)
                });
            }));
        let inc = self.settings.clone();
        let inc_id = SharedString::from(format!("{label}-inc"));
        let inc = div()
            .id(inc_id)
            .w(px(22.))
            .h(px(22.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(t.radius_sm)
            .bg(t.control_bg)
            .cursor_pointer()
            .child("+")
            .on_click(cx.listener(move |_, _, _, cx| {
                inc.update(cx, |settings, cx| {
                    settings.edit_fonts(|fonts| apply(fonts, field.delta()), cx)
                });
            }));
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(div().w(px(150.)).child(label))
            .child(dec)
            .child(
                div()
                    .w(px(46.))
                    .text_size(t.text_size_sm)
                    .text_color(t.text_secondary)
                    .child(format!("{value:.2}")),
            )
            .child(inc)
            .into_any_element()
    }

    fn settings_keymap_body(&mut self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let (rows, error) = {
            let settings = self.settings.read(cx);
            (settings.keymap().rows(), settings.keymap_error().cloned())
        };
        let mut actions = Vec::new();
        for row in rows {
            let id = row.id;
            let chord = chip(format!("settings-chord-{id}"), row.chord_label, false, t).on_click(
                cx.listener(move |host, _, _, cx| {
                    if let Some(ui) = host.settings_ui.as_mut() {
                        ui.capture = Some(id);
                        ui.message = None;
                    }
                    cx.notify();
                }),
            );
            let reset = self.settings.clone();
            let reset_id = SharedString::from(format!("settings-unbind-{id}"));
            actions.push(
                div()
                    .id(reset_id)
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .py(px(2.))
                    .child(div().w(px(190.)).text_size(t.text_size_sm).child(row.label))
                    .child(chord)
                    .child(
                        div()
                            .text_size(t.text_size_sm)
                            .text_color(t.text_tertiary)
                            .child(if row.custom { "custom" } else { "" }),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .id(SharedString::from(format!("settings-reset-{id}")))
                            .cursor_pointer()
                            .text_size(t.text_size_sm)
                            .text_color(t.text_secondary)
                            .child("Reset")
                            .on_click(cx.listener(move |_, _, _, cx| {
                                reset.update(cx, |settings, cx| settings.reset_binding(id, cx));
                            })),
                    )
                    .into_any_element(),
            );
        }
        let surfaces = super::super::settings::surface_rows()
            .iter()
            .take(14)
            .map(|binding| {
                div()
                    .flex()
                    .gap(px(10.))
                    .text_size(t.text_size_sm)
                    .text_color(t.text_tertiary)
                    .child(div().w(px(190.)).child(binding.surface))
                    .child(div().w(px(110.)).child(binding.chord))
                    .child(binding.action)
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .children(vec![
                section(
                    t,
                    "Actions — click a chord, then press the new keys",
                    vec![
                        div().flex().flex_col().children(actions).into_any_element(),
                        div()
                            .text_size(t.text_size_sm)
                            .text_color(t.text_tertiary)
                            .child(error.unwrap_or_else(|| SharedString::from(
                                "A refused binding is never written: the conflict is shown here.",
                            )))
                            .into_any_element(),
                    ],
                ),
                section(t, "Fixed surface handlers (not rebindable)", surfaces),
            ])
            .into_any_element()
    }
}

// Small rendering helpers shared by the two tabs.
fn section(t: &Theme, title: &'static str, children: Vec<AnyElement>) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .py(px(6.))
        .child(
            div()
                .text_size(t.text_size_sm)
                .text_color(t.text_tertiary)
                .child(title),
        )
        .children(children)
        .into_any_element()
}

fn chip(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    active: bool,
    t: &Theme,
) -> Stateful<Div> {
    let id: SharedString = id.into();
    div()
        .id(id)
        .h(px(24.))
        .px(px(8.))
        .flex()
        .items_center()
        .rounded(t.radius_sm)
        .border_1()
        .border_color(if active { t.accent } else { t.border })
        .bg(if active {
            t.control_bg
        } else {
            t.surface_raised
        })
        .text_size(t.text_size_sm)
        .text_color(if active {
            t.text_primary
        } else {
            t.text_secondary
        })
        .cursor_pointer()
        .child(label.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flc_ui::keymap::ConflictKind;
    use flc_ui::theme_settings::mix;

    #[test]
    fn the_canvas_covers_hues_and_brightness() {
        assert_eq!(canvas_color(0, 0), 0xFF0000, "red at the top left");
        let mid = canvas_color(CANVAS_COLS / 3, 0);
        assert!(mid & 0x00FF00 > 0x008000, "green around a third across");
        let dim = canvas_color(0, CANVAS_ROWS - 1);
        assert!((dim >> 16) < 0xFF, "the bottom row is darker");
    }

    #[test]
    fn stepped_values_stay_in_range() {
        assert_eq!(step(12.5, 0.5, 8.0, 28.0), 13.0);
        assert_eq!(step(8.0, -0.5, 8.0, 28.0), 8.0);
        assert_eq!(step(28.0, 0.5, 8.0, 28.0), 28.0);
        assert_eq!(step(0.95, 0.1, 0.0, 1.0), 1.0);
    }

    #[test]
    fn every_swatch_is_an_opaque_color() {
        for swatch in SWATCHES {
            assert!(swatch <= 0xFFFFFF);
        }
        assert_ne!(mix(SWATCHES[0], SWATCHES[11], 1.0), SWATCHES[0]);
    }

    #[test]
    fn conflict_kinds_have_a_message_for_the_ui() {
        let keymap = keymap::Keymap::defaults();
        let conflict = keymap
            .check("settings.open", &Chord::parse("ctrl-k").unwrap(), |chord| {
                chord.control && chord.key == "k"
            })
            .unwrap();
        assert_eq!(conflict.kind, ConflictKind::Terminal);
        assert!(!conflict.message.is_empty());
    }
}
