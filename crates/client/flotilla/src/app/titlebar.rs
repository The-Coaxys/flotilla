//! 36 px client-drawn title bar: drag area, connection status, caption buttons.
use flc_ui::theme::{ActiveTheme, Theme};
use gpui::prelude::*;
use gpui::{Context, FontWeight, Hsla, Window, WindowControlArea, div, px, rgb};

use super::Workspace;
use crate::connect::{Tone, status_line};

pub(crate) fn tone_color(tone: Tone, t: &Theme) -> Hsla {
    match tone {
        Tone::Good => t.success,
        Tone::Busy => t.warning,
        Tone::Bad => t.danger,
        Tone::Neutral => t.text_tertiary,
    }
}

impl Workspace {
    pub(super) fn render_titlebar(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = cx.theme().clone();
        let (tone, text) = match self.selected_phase() {
            Some(phase) => status_line(phase),
            None => (Tone::Neutral, "No host".to_string()),
        };
        let maximized = window.is_maximized();
        div()
            .h(px(36.))
            .w_full()
            .flex_none()
            .flex()
            .items_center()
            .bg(t.chrome_bg)
            .border_b_1()
            .border_color(t.border)
            .child(
                div()
                    .id("titlebar-drag")
                    .flex_1()
                    .h_full()
                    .flex()
                    .items_center()
                    .pl(px(14.))
                    .window_control_area(WindowControlArea::Drag)
                    .child(
                        div()
                            .text_color(t.text_secondary)
                            .font_weight(FontWeight::MEDIUM)
                            .child("Flotilla"),
                    ),
            )
            .child(
                div()
                    .id("status-pill")
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .px(px(10.))
                    .h(px(22.))
                    .mr(px(8.))
                    .rounded(px(11.))
                    .bg(t.surface_raised)
                    .text_size(t.text_size_sm)
                    .text_color(t.text_secondary)
                    .child(div().size(px(7.)).rounded_full().bg(tone_color(tone, &t)))
                    .child(text),
            )
            .child(caption_button(
                "caption-min",
                "\u{E921}",
                WindowControlArea::Min,
                t.raised_hover,
                &t,
            ))
            .child(caption_button(
                "caption-max",
                if maximized { "\u{E923}" } else { "\u{E922}" },
                WindowControlArea::Max,
                t.raised_hover,
                &t,
            ))
            .child(caption_button(
                "caption-close",
                "\u{E8BB}",
                WindowControlArea::Close,
                rgb(0xC42B1C).into(),
                &t,
            ))
    }
}

/// 46×36 Windows caption button using the Segoe MDL2 Assets glyphs.
fn caption_button(
    id: &'static str,
    glyph: &'static str,
    area: WindowControlArea,
    hover_bg: Hsla,
    t: &Theme,
) -> impl IntoElement {
    div()
        .id(id)
        .w(px(46.))
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .font_family("Segoe MDL2 Assets")
        .text_size(px(10.))
        .text_color(t.text_secondary)
        .hover(move |s| s.bg(hover_bg).text_color(gpui::white()))
        .window_control_area(area)
        .child(glyph)
}
