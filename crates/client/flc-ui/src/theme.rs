//! Design tokens (spec §8.2) and font registration.
use std::borrow::Cow;

use gpui::{App, Global, Hsla, Pixels, SharedString, px, rgb, rgba};

/// Bundled fonts (SIL OFL 1.1; licenses in assets/fonts).
pub static FONTS: &[&[u8]] = &[
    include_bytes!("../assets/fonts/Inter-Regular.ttf"),
    include_bytes!("../assets/fonts/Inter-Medium.ttf"),
    include_bytes!("../assets/fonts/Inter-SemiBold.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMono-Medium.ttf"),
];

#[derive(Debug, Clone)]
pub struct Theme {
    pub chrome_bg: Hsla,
    pub surface_bg: Hsla,
    pub surface_raised: Hsla,
    pub raised_hover: Hsla,
    pub row_selected: Hsla,
    pub row_selected_border: Hsla,
    pub control_bg: Hsla,
    pub border: Hsla,
    pub text_primary: Hsla,
    pub text_secondary: Hsla,
    pub text_tertiary: Hsla,
    pub accent: Hsla,
    pub success: Hsla,
    pub danger: Hsla,
    pub warning: Hsla,
    pub radius_lg: Pixels,
    pub radius_md: Pixels,
    pub radius_sm: Pixels,
    pub font_ui: SharedString,
    pub font_mono: SharedString,
    pub font_terminal: SharedString,
    pub text_size: Pixels,
    pub text_size_sm: Pixels,
    pub mono_size: Pixels,
    /// Editor and diff font (per-surface setting, spec §8.2).
    pub editor_font: SharedString,
    pub editor_size: Pixels,
    /// Terminal-tab font size; `font_terminal` is its family.
    pub terminal_size: Pixels,
    /// Chat text multiplier (per-surface setting).
    pub chat_scale: f32,
}

impl Theme {
    pub fn dark() -> Self {
        Self {
            chrome_bg: rgb(0x27262B).into(),
            surface_bg: rgb(0x1D1E21).into(),
            surface_raised: rgb(0x25262B).into(),
            raised_hover: rgb(0x2A2B30).into(),
            row_selected: rgb(0x2D2E33).into(),
            row_selected_border: rgba(0xFFFFFF14).into(),
            control_bg: rgb(0x35363B).into(),
            border: rgba(0xFFFFFF0F).into(),
            text_primary: rgb(0xDFE0E5).into(),
            text_secondary: rgb(0x838285).into(),
            text_tertiary: rgb(0x6C6D72).into(),
            accent: rgb(0x3D8BFD).into(),
            success: rgb(0x5DBB63).into(),
            danger: rgb(0xE5534B).into(),
            warning: rgb(0xD9A53C).into(),
            radius_lg: px(10.),
            radius_md: px(8.),
            radius_sm: px(6.),
            font_ui: "Inter".into(),
            font_mono: "JetBrains Mono".into(),
            font_terminal: "Cascadia Mono".into(),
            text_size: px(13.),
            text_size_sm: px(11.),
            mono_size: px(12.5),
            editor_font: "JetBrains Mono".into(),
            editor_size: px(12.5),
            terminal_size: px(12.5),
            chat_scale: 1.0,
        }
    }

    /// The light theme derived from the same tokens (spec §8.2).
    pub fn light() -> Self {
        Self {
            chrome_bg: rgb(0xE9E9ED).into(),
            surface_bg: rgb(0xF7F7F9).into(),
            surface_raised: rgb(0xFFFFFF).into(),
            raised_hover: rgb(0xF1F1F5).into(),
            row_selected: rgb(0xE3E3E9).into(),
            row_selected_border: rgba(0x00000014).into(),
            control_bg: rgb(0xDDDDE3).into(),
            border: rgba(0x00000014).into(),
            text_primary: rgb(0x1D1E21).into(),
            text_secondary: rgb(0x5C5D63).into(),
            text_tertiary: rgb(0x83848A).into(),
            accent: rgb(0x2F6FDF).into(),
            success: rgb(0x3C9B44).into(),
            danger: rgb(0xD0453D).into(),
            warning: rgb(0xB27E1E).into(),
            radius_lg: px(10.),
            radius_md: px(8.),
            radius_sm: px(6.),
            font_ui: "Inter".into(),
            font_mono: "JetBrains Mono".into(),
            font_terminal: "Cascadia Mono".into(),
            text_size: px(13.),
            text_size_sm: px(11.),
            mono_size: px(12.5),
            editor_font: "JetBrains Mono".into(),
            editor_size: px(12.5),
            terminal_size: px(12.5),
            chat_scale: 1.0,
        }
    }
}

impl Global for Theme {}

pub trait ActiveTheme {
    fn theme(&self) -> &Theme;
}

impl ActiveTheme for App {
    fn theme(&self) -> &Theme {
        self.global::<Theme>()
    }
}

/// Install the theme, bundled fonts and text-input key bindings.
pub fn init(cx: &mut App) {
    cx.set_global(Theme::dark());
    let fonts = FONTS.iter().map(|f| Cow::Borrowed(*f)).collect();
    if let Err(e) = cx.text_system().add_fonts(fonts) {
        eprintln!("flotilla: failed to register bundled fonts: {e:#}");
    }
    crate::input::bind_keys(cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_theme_matches_spec_tokens() {
        let t = Theme::dark();
        let c = |hex: u32| -> Hsla { rgb(hex).into() };
        assert_eq!(t.chrome_bg, c(0x27262B));
        assert_eq!(t.surface_bg, c(0x1D1E21));
        assert_eq!(t.surface_raised, c(0x25262B));
        assert_eq!(t.raised_hover, c(0x2A2B30));
        assert_eq!(t.row_selected, c(0x2D2E33));
        assert_eq!(t.control_bg, c(0x35363B));
        assert_eq!(t.text_primary, c(0xDFE0E5));
        assert_eq!(t.text_secondary, c(0x838285));
        assert_eq!(t.text_tertiary, c(0x6C6D72));
        assert_eq!(t.accent, c(0x3D8BFD));
        assert_eq!(t.success, c(0x5DBB63));
        assert_eq!(t.danger, c(0xE5534B));
        let border: Hsla = rgba(0xFFFFFF14).into();
        assert_eq!(t.row_selected_border, border);
        assert!((t.row_selected_border.a - 0.08).abs() < 0.01);
        assert_eq!(
            (t.radius_lg, t.radius_md, t.radius_sm),
            (px(10.), px(8.), px(6.))
        );
        assert_eq!(
            (t.text_size, t.text_size_sm, t.mono_size),
            (px(13.), px(11.), px(12.5))
        );
        assert_eq!(t.font_ui.as_ref(), "Inter");
        assert_eq!(t.font_mono.as_ref(), "JetBrains Mono");
    }

    #[test]
    fn bundled_fonts_are_truetype() {
        assert_eq!(FONTS.len(), 5);
        for f in FONTS {
            assert!(
                f.len() > 50_000,
                "font suspiciously small: {} bytes",
                f.len()
            );
            assert!(
                f.starts_with(&[0, 1, 0, 0]) || f.starts_with(b"true"),
                "not a TrueType file"
            );
        }
    }
}
