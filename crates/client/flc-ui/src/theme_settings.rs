//! Per-workspace theme settings (spec §8.2) and their resolution to tokens.
//!
//! The file format lives in the client (`settings.json`); this module only
//! owns the values, their bounds and the pure math that turns them into a
//! [`Theme`], so the editor, the persistence layer and the tests agree.
use serde::{Deserialize, Serialize};

use crate::theme::Theme;
use gpui::{Hsla, Rgba, px, rgba};

/// Theme mode chosen per workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    System,
    Light,
    Dark,
}

impl Mode {
    pub const ALL: [Mode; 3] = [Mode::System, Mode::Light, Mode::Dark];

    pub fn label(self) -> &'static str {
        match self {
            Mode::System => "System",
            Mode::Light => "Light",
            Mode::Dark => "Dark",
        }
    }
}

/// The three documented colors plus their sliders, for one mode.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Palette {
    /// Chrome background, `0xRRGGBB`.
    pub background: u32,
    /// Links, active tab underline, active branch.
    pub accent: u32,
    /// Selected rows and emphasised surfaces.
    pub highlight: u32,
    /// Separation between the chrome and the content surface, 0..1.
    pub background_intensity: f32,
    /// Alpha of the chrome and content surfaces, 0.35..1 (Mica tie-in).
    pub background_alpha: f32,
    /// How saturated the accent is, 0..1.
    pub accent_prominence: f32,
    /// How strong the highlight is, 0..1.
    pub highlight_prominence: f32,
}

impl Default for Palette {
    fn default() -> Self {
        Self::dark_default()
    }
}

impl Palette {
    pub fn dark_default() -> Self {
        Self {
            background: 0x27262B,
            accent: 0x3D8BFD,
            highlight: 0x2D2E33,
            background_intensity: 0.5,
            background_alpha: 1.0,
            accent_prominence: 1.0,
            highlight_prominence: 1.0,
        }
    }

    pub fn light_default() -> Self {
        Self {
            background: 0xE9E9ED,
            accent: 0x2F6FDF,
            highlight: 0xE3E3E9,
            background_intensity: 0.5,
            background_alpha: 1.0,
            accent_prominence: 1.0,
            highlight_prominence: 1.0,
        }
    }

    /// Clamp every slider into its documented range.
    pub fn sanitize(&mut self) {
        self.background_intensity = self.background_intensity.clamp(0.0, 1.0);
        self.background_alpha = self.background_alpha.clamp(0.35, 1.0);
        self.accent_prominence = self.accent_prominence.clamp(0.0, 1.0);
        self.highlight_prominence = self.highlight_prominence.clamp(0.0, 1.0);
    }

    fn resolved(&self, dark: bool) -> ResolvedPalette {
        let mut p = *self;
        p.sanitize();
        let surface = mix(
            p.background,
            if dark { 0x000000 } else { 0xFFFFFF },
            0.14 + 0.2 * p.background_intensity,
        );
        let raised = mix(surface, p.background, 0.5);
        let hover = mix(raised, if dark { 0xFFFFFF } else { 0x000000 }, 0.05);
        let accent = mix(p.background, p.accent, 0.4 + 0.6 * p.accent_prominence);
        let selected = mix(surface, p.highlight, 0.4 + 0.6 * p.highlight_prominence);
        let control = mix(raised, if dark { 0xDFE0E5 } else { 0x1D1E21 }, 0.10);
        ResolvedPalette {
            chrome_bg: p.background,
            surface_bg: surface,
            surface_raised: raised,
            raised_hover: hover,
            row_selected: selected,
            control_bg: control,
            accent,
            alpha: p.background_alpha,
        }
    }
}

struct ResolvedPalette {
    chrome_bg: u32,
    surface_bg: u32,
    surface_raised: u32,
    raised_hover: u32,
    row_selected: u32,
    control_bg: u32,
    accent: u32,
    alpha: f32,
}

/// Font family and size per surface (spec §8.2: not part of the theme colors).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SurfaceFonts {
    pub ui_font: String,
    pub editor_family: String,
    pub editor_size: f32,
    pub terminal_family: String,
    pub terminal_size: f32,
    /// Setup/Run/Teardown terminals in the right panel.
    pub panel_terminal_size: f32,
    /// Multiplier applied to chat text.
    pub chat_scale: f32,
}

impl Default for SurfaceFonts {
    fn default() -> Self {
        Self {
            ui_font: "Inter".into(),
            editor_family: "JetBrains Mono".into(),
            editor_size: 12.5,
            terminal_family: "Cascadia Mono".into(),
            terminal_size: 12.5,
            panel_terminal_size: 12.5,
            chat_scale: 1.0,
        }
    }
}

impl SurfaceFonts {
    /// Clamp the documented ranges: 8..28 px, 0.6..2.0 chat scale.
    pub fn sanitize(&mut self) {
        self.editor_size = clamp_size(self.editor_size, 12.5);
        self.terminal_size = clamp_size(self.terminal_size, 12.5);
        self.panel_terminal_size = clamp_size(self.panel_terminal_size, 12.5);
        self.chat_scale = self.chat_scale.clamp(0.6, 2.0);
        if self.ui_font.trim().is_empty() {
            self.ui_font = "Inter".into();
        }
        if self.editor_family.trim().is_empty() {
            self.editor_family = "JetBrains Mono".into();
        }
        if self.terminal_family.trim().is_empty() {
            self.terminal_family = "Cascadia Mono".into();
        }
    }

    pub fn terminal_family(&self) -> &str {
        &self.terminal_family
    }

    pub fn editor_family(&self) -> &str {
        &self.editor_family
    }
}

fn clamp_size(size: f32, fallback: f32) -> f32 {
    if size.is_finite() {
        size.clamp(8.0, 28.0)
    } else {
        fallback
    }
}

/// Every setting the theme editor edits for one workspace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeSettings {
    pub mode: Mode,
    pub light: Palette,
    pub dark: Palette,
}

impl Default for ThemeSettings {
    fn default() -> Self {
        Self {
            mode: Mode::System,
            light: Palette::light_default(),
            dark: Palette::dark_default(),
        }
    }
}

impl ThemeSettings {
    pub fn palette(&self, dark: bool) -> &Palette {
        if dark { &self.dark } else { &self.light }
    }

    pub fn palette_mut(&mut self, dark: bool) -> &mut Palette {
        if dark {
            &mut self.dark
        } else {
            &mut self.light
        }
    }

    /// Sanitize every slider so a hand-edited file cannot push the UI out of
    /// range.
    pub fn sanitize(&mut self) {
        self.light.sanitize();
        self.dark.sanitize();
    }

    /// Resolve to concrete tokens. `system_dark` is only consulted for
    /// [`Mode::System`].
    pub fn resolve(&self, fonts: &SurfaceFonts, system_dark: bool) -> Theme {
        let dark = match self.mode {
            Mode::Dark => true,
            Mode::Light => false,
            Mode::System => system_dark,
        };
        let base = if dark { Theme::dark() } else { Theme::light() };
        let resolved = self.palette(dark).resolved(dark);
        let alpha = |c: u32| color(c, resolved.alpha);
        let surface_bg = color(resolved.surface_bg, resolved.alpha);
        let text_primary = ensure_contrast(rgb_to_u32(base.text_primary), resolved.surface_bg, 4.5);
        let text_secondary =
            ensure_contrast(rgb_to_u32(base.text_secondary), resolved.surface_bg, 3.0);
        let text_tertiary =
            ensure_contrast(rgb_to_u32(base.text_tertiary), resolved.surface_bg, 2.2);
        let mut fonts = fonts.clone();
        fonts.sanitize();
        Theme {
            chrome_bg: alpha(resolved.chrome_bg),
            surface_bg,
            surface_raised: alpha(resolved.surface_raised),
            raised_hover: alpha(resolved.raised_hover),
            row_selected: alpha(resolved.row_selected),
            row_selected_border: color(text_primary, 0.08),
            control_bg: alpha(resolved.control_bg),
            border: color(text_primary, 0.07),
            text_primary: color(text_primary, 1.0),
            text_secondary: color(text_secondary, 1.0),
            text_tertiary: color(text_tertiary, 1.0),
            accent: color(resolved.accent, 1.0),
            success: base.success,
            danger: base.danger,
            warning: base.warning,
            radius_lg: base.radius_lg,
            radius_md: base.radius_md,
            radius_sm: base.radius_sm,
            font_ui: fonts.ui_font.clone().into(),
            font_mono: fonts.editor_family.clone().into(),
            font_terminal: fonts.terminal_family.clone().into(),
            text_size: base.text_size,
            text_size_sm: base.text_size_sm,
            mono_size: base.mono_size,
            editor_font: fonts.editor_family.clone().into(),
            editor_size: px(clamp_size(fonts.editor_size, 12.5)),
            terminal_size: px(clamp_size(fonts.terminal_size, 12.5)),
            chat_scale: fonts.chat_scale,
        }
    }
}

/// `0xRRGGBB` with an alpha channel.
fn color(rgb: u32, alpha: f32) -> Hsla {
    let a = (alpha.clamp(0.0, 1.0) * 255.0).round() as u32;
    rgba((rgb << 8) | a).into()
}

/// The opaque `0xRRGGBB` behind an `Hsla`, for contrast math.
pub fn rgb_to_u32(color: Hsla) -> u32 {
    let rgba = Rgba::from(color);
    let channel = |c: f32| (c.clamp(0.0, 1.0) * 255.0).round() as u32;
    (channel(rgba.r) << 16) | (channel(rgba.g) << 8) | channel(rgba.b)
}

/// Mix two colors; `t` 0 keeps `a`, 1 keeps `b`.
pub fn mix(a: u32, b: u32, t: f32) -> u32 {
    let t = t.clamp(0.0, 1.0);
    let channel = |shift: u32| {
        let from = ((a >> shift) & 0xFF) as f32;
        let to = ((b >> shift) & 0xFF) as f32;
        (from + (to - from) * t).round().clamp(0.0, 255.0) as u32
    };
    (channel(16) << 16) | (channel(8) << 8) | channel(0)
}

fn linear(channel: u32) -> f32 {
    let c = channel as f32 / 255.0;
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// WCAG relative luminance of an `0xRRGGBB` color.
pub fn relative_luminance(c: u32) -> f32 {
    0.2126 * linear((c >> 16) & 0xFF) + 0.7152 * linear((c >> 8) & 0xFF) + 0.0722 * linear(c & 0xFF)
}

/// WCAG contrast ratio between two `0xRRGGBB` colors (1..21).
pub fn contrast_ratio(a: u32, b: u32) -> f32 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    let (light, dark) = if la > lb { (la, lb) } else { (lb, la) };
    (light + 0.05) / (dark + 0.05)
}

/// Nudge `fg` toward the far end of the luminance range until it reaches
/// `minimum` contrast on `bg`.
pub fn ensure_contrast(fg: u32, bg: u32, minimum: f32) -> u32 {
    if contrast_ratio(fg, bg) >= minimum {
        return fg;
    }
    // Walk toward white and toward black, returning the first candidate that
    // clears the floor. A mid-grey background can only be satisfied by one of
    // the two ends, so both are tried before giving up.
    let mut best = fg;
    let mut best_ratio = contrast_ratio(fg, bg);
    for step in 1..=20 {
        for target in [0x000000, 0xFFFFFF] {
            let candidate = mix(fg, target, step as f32 / 20.0);
            let ratio = contrast_ratio(candidate, bg);
            if ratio >= minimum {
                return candidate;
            }
            if ratio > best_ratio {
                best_ratio = ratio;
                best = candidate;
            }
        }
    }
    best
}

/// Parse `RRGGBB` or `#RRGGBB` (also `RRGGBBAA` is rejected: colors carry no
/// alpha here).
pub fn parse_hex(text: &str) -> Option<u32> {
    let text = text.trim().trim_start_matches('#');
    if text.len() != 6 || !text.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(text, 16).ok()
}

pub fn format_hex(c: u32) -> String {
    format!("{:06X}", c & 0xFFFFFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_defaults_reproduce_the_spec_tokens() {
        let settings = ThemeSettings {
            mode: Mode::Dark,
            ..Default::default()
        };
        let theme = settings.resolve(&SurfaceFonts::default(), true);
        assert_eq!(rgb_to_u32(theme.chrome_bg), 0x27262B);
        assert_eq!(rgb_to_u32(theme.row_selected), 0x2D2E33);
        assert_eq!(rgb_to_u32(theme.accent), 0x3D8BFD);
        let surface = rgb_to_u32(theme.surface_bg);
        assert_ne!(surface, 0x27262B, "content surface separates from chrome");
        assert!(relative_luminance(surface) < relative_luminance(0x27262B));
    }

    #[test]
    fn system_mode_follows_the_platform_preference() {
        let settings = ThemeSettings::default();
        let dark = settings.resolve(&SurfaceFonts::default(), true);
        let light = settings.resolve(&SurfaceFonts::default(), false);
        assert_eq!(rgb_to_u32(dark.chrome_bg), 0x27262B);
        assert_eq!(rgb_to_u32(light.chrome_bg), 0xE9E9ED);
        let explicit = ThemeSettings {
            mode: Mode::Light,
            ..Default::default()
        };
        assert_eq!(
            rgb_to_u32(explicit.resolve(&SurfaceFonts::default(), true).chrome_bg),
            0xE9E9ED
        );
    }

    #[test]
    fn intensity_separates_the_surface_and_alpha_is_bounded() {
        let mut settings = ThemeSettings {
            mode: Mode::Dark,
            ..Default::default()
        };
        settings.dark.background_intensity = 0.0;
        let flat = rgb_to_u32(settings.resolve(&SurfaceFonts::default(), true).surface_bg);
        settings.dark.background_intensity = 1.0;
        let deep = rgb_to_u32(settings.resolve(&SurfaceFonts::default(), true).surface_bg);
        assert!(relative_luminance(deep) < relative_luminance(flat));
        settings.dark.background_alpha = 0.0;
        let theme = settings.resolve(&SurfaceFonts::default(), true);
        assert!(
            (theme.chrome_bg.a - 0.35).abs() < 0.01,
            "alpha clamps at 0.35"
        );
        settings.dark.background_alpha = 2.0;
        let theme = settings.resolve(&SurfaceFonts::default(), true);
        assert!((theme.chrome_bg.a - 1.0).abs() < 0.01);
    }

    #[test]
    fn prominence_decides_how_much_of_the_accent_survives() {
        let mut settings = ThemeSettings {
            mode: Mode::Dark,
            ..Default::default()
        };
        settings.dark.accent = 0xFF0000;
        settings.dark.accent_prominence = 0.0;
        let muted = rgb_to_u32(settings.resolve(&SurfaceFonts::default(), true).accent);
        settings.dark.accent_prominence = 1.0;
        let full = rgb_to_u32(settings.resolve(&SurfaceFonts::default(), true).accent);
        assert_eq!(full, 0xFF0000);
        assert_ne!(muted, full);
        // A zero-prominence accent is a blend toward the background.
        assert_eq!(muted, mix(settings.dark.background, 0xFF0000, 0.4));
    }

    #[test]
    fn unreadable_text_is_nudged_to_the_contrast_floor() {
        let grey = 0x808080;
        let fixed = ensure_contrast(grey, grey, 4.5);
        assert!(contrast_ratio(fixed, grey) >= 4.5);
        // Already-contrasting text is untouched.
        assert_eq!(ensure_contrast(0xFFFFFF, 0x000000, 4.5), 0xFFFFFF);
    }

    #[test]
    fn contrast_math_matches_the_wcag_reference_points() {
        assert!((contrast_ratio(0xFFFFFF, 0x000000) - 21.0).abs() < 0.01);
        assert!((contrast_ratio(0x000000, 0x000000) - 1.0).abs() < 0.001);
        assert!((relative_luminance(0xFFFFFF) - 1.0).abs() < 0.001);
        assert!(relative_luminance(0x000000).abs() < 0.001);
        // 3D8BFD on 27262B is above the UI floor but below body-text floor.
        let ratio = contrast_ratio(0x3D8BFD, 0x27262B);
        assert!(ratio > 3.0 && ratio < 10.0);
    }

    #[test]
    fn hex_round_trips_and_rejects_junk() {
        assert_eq!(parse_hex("#3D8BFD"), Some(0x3D8BFD));
        assert_eq!(parse_hex("3d8bfd"), Some(0x3D8BFD));
        assert_eq!(format_hex(0x3D8BFD), "3D8BFD");
        for bad in ["", "#12345", "#1234567", "zzzzzz", "#-12345"] {
            assert_eq!(parse_hex(bad), None, "{bad}");
        }
    }

    #[test]
    fn surface_fonts_are_clamped_and_invalid_values_fall_back() {
        let mut fonts = SurfaceFonts {
            editor_size: 99.0,
            terminal_size: f32::NAN,
            chat_scale: 9.0,
            editor_family: "  ".into(),
            ..Default::default()
        };
        fonts.sanitize();
        assert_eq!(fonts.editor_size, 28.0);
        assert_eq!(fonts.terminal_size, 12.5);
        assert_eq!(fonts.chat_scale, 2.0);
        assert_eq!(fonts.editor_family, "JetBrains Mono");
        let theme = ThemeSettings::default().resolve(&fonts, true);
        assert_eq!(f32::from(theme.terminal_size), 12.5);
        assert_eq!(theme.font_terminal.as_ref(), "Cascadia Mono");
        assert_eq!(theme.chat_scale, 2.0);
    }

    #[test]
    fn settings_round_trip_through_json_and_tolerate_missing_fields() {
        let settings = ThemeSettings {
            mode: Mode::Light,
            ..Default::default()
        };
        let text = serde_json::to_string(&settings).unwrap();
        let back: ThemeSettings = serde_json::from_str(&text).unwrap();
        assert_eq!(back, settings);
        let sparse: ThemeSettings = serde_json::from_str(r#"{"mode":"dark"}"#).unwrap();
        assert_eq!(sparse.mode, Mode::Dark);
        assert_eq!(sparse.dark, Palette::dark_default());
        let fonts: SurfaceFonts = serde_json::from_str("{}").unwrap();
        assert_eq!(fonts, SurfaceFonts::default());
    }
}
