//! 260 px host list.
use flc_ui::theme::ActiveTheme;
use gpui::prelude::*;
use gpui::{Context, FontWeight, div, px};

use super::Workspace;
use super::titlebar::tone_color;
use crate::connect::status_line;

impl Workspace {
    pub(super) fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme().clone();
        let rows: Vec<_> = self
            .hosts
            .iter()
            .enumerate()
            .map(|(i, h)| {
                let selected = self.selected == Some(i);
                let (tone, _) = status_line(&h.phase);
                let hover_bg = t.raised_hover;
                div()
                    .id(("host", i))
                    .h(px(30.))
                    .flex_none()
                    .px(px(8.))
                    .rounded(t.radius_sm)
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .cursor_pointer()
                    .when(selected, |d| {
                        d.bg(t.row_selected)
                            .border_1()
                            .border_color(t.row_selected_border)
                    })
                    .when(!selected, |d| d.hover(move |s| s.bg(hover_bg)))
                    .child(
                        div()
                            .size(px(7.))
                            .rounded_full()
                            .flex_none()
                            .bg(tone_color(tone, &t)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(h.profile.name.clone()),
                    )
                    .on_click(cx.listener(move |ws, _, _window, cx| ws.select(i, cx)))
            })
            .collect();
        let hover_bg = t.raised_hover;
        div()
            .w(px(260.))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(2.))
            .p(px(8.))
            .bg(t.chrome_bg)
            .border_r_1()
            .border_color(t.border)
            .child(
                div()
                    .px(px(8.))
                    .flex_none()
                    .pt(px(6.))
                    .pb(px(4.))
                    .text_size(t.text_size_sm)
                    .text_color(t.text_tertiary)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("HOSTS"),
            )
            .child(
                div()
                    .id("host-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .children(rows),
            )
            .child(
                div()
                    .id("add-host")
                    .mt(px(4.))
                    .h(px(30.))
                    .flex_none()
                    .px(px(8.))
                    .rounded(t.radius_sm)
                    .flex()
                    .items_center()
                    .text_color(t.text_secondary)
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover_bg))
                    .child("+ Add host")
                    .on_click(cx.listener(|ws, _, window, cx| ws.open_add_host(window, cx))),
            )
    }
}
