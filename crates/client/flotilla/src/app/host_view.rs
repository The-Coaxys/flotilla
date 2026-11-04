//! Center pane: empty state, host details, or the add-host form.
use flc_conn::supervisor::ConnPhase;
use flc_ui::theme::{ActiveTheme, Theme};
use gpui::prelude::*;
use gpui::{AnyElement, Context, Div, FontWeight, SharedString, Stateful, Window, div, px};

use super::titlebar::tone_color;
use super::{Center, Workspace};
use crate::connect::status_line;

pub(crate) fn primary_button(id: &'static str, label: &'static str, t: &Theme) -> Stateful<Div> {
    let hover = t.accent.opacity(0.85);
    div()
        .id(id)
        .h(px(30.))
        .px(px(14.))
        .rounded(t.radius_md)
        .flex()
        .items_center()
        .justify_center()
        .bg(t.accent)
        .text_color(gpui::white())
        .font_weight(FontWeight::MEDIUM)
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .child(label)
}

pub(crate) fn secondary_button(id: &'static str, label: &'static str, t: &Theme) -> Stateful<Div> {
    let hover = t.raised_hover;
    div()
        .id(id)
        .h(px(30.))
        .px(px(14.))
        .rounded(t.radius_md)
        .flex()
        .items_center()
        .justify_center()
        .bg(t.control_bg)
        .text_color(t.text_primary)
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .child(label)
}

fn kv(label: &'static str, value: impl Into<SharedString>, t: &Theme) -> impl IntoElement {
    div()
        .flex()
        .gap(px(12.))
        .child(div().w(px(90.)).text_color(t.text_tertiary).child(label))
        .child(div().flex_1().child(value.into()))
}

impl Workspace {
    pub(super) fn render_center(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if let Some(index) = self.deferred_connect.take() {
            self.connect(index, window, cx);
        }
        if let Center::Host = self.center
            && let Some(index) = self.selected
        {
            self.ensure_shell(index, cx);
        }
        let t = cx.theme().clone();
        let body: AnyElement = match &self.center {
            Center::AddHost(form) => form.clone().into_any_element(),
            // A connected host shows its live terminal workspace; the host card
            // stays available while connecting or when the link has failed.
            Center::Host => match self.selected {
                Some(i) if i < self.hosts.len() => match &self.hosts[i].workspace {
                    Some(workspace) => div()
                        .size_full()
                        .flex()
                        .flex_col()
                        .when_some(self.render_clipboard_request(i, cx), |d, prompt| {
                            d.child(prompt)
                        })
                        .child(div().flex_1().min_h(px(0.)).child(workspace.clone()))
                        .into_any_element(),
                    None => self.render_host(i, cx).into_any_element(),
                },
                _ => self.render_empty(cx).into_any_element(),
            },
            Center::Empty => self.render_empty(cx).into_any_element(),
        };
        div()
            .flex_1()
            .min_w(px(0.))
            .h_full()
            .overflow_hidden()
            .flex()
            .items_center()
            .justify_center()
            .bg(t.surface_bg)
            .child(body)
            .into_any_element()
    }

    /// The one-time OSC 52 question for this host, shown above its terminals.
    fn render_clipboard_request(&self, index: usize, cx: &mut Context<Self>) -> Option<Div> {
        let host = &self.hosts[index];
        let text = host.clipboard_request.as_ref()?;
        let t = cx.theme().clone();
        let chars = text.chars().count();
        Some(
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(12.))
                .py(px(8.))
                .bg(t.chrome_bg)
                .border_b_1()
                .border_color(t.border)
                .child(
                    div().flex_1().child(SharedString::from(format!(
                        "A program on {} wants to copy {chars} character{} to your clipboard. Allow programs on this host to set the clipboard?",
                        host.profile.name,
                        if chars == 1 { "" } else { "s" },
                    ))),
                )
                .child(
                    secondary_button("clipboard-deny", "Don't allow", &t).on_click(cx.listener(
                        move |ws, _, _, cx| ws.answer_clipboard(index, false, cx),
                    )),
                )
                .child(
                    primary_button("clipboard-allow", "Allow", &t).on_click(cx.listener(
                        move |ws, _, _, cx| ws.answer_clipboard(index, true, cx),
                    )),
                ),
        )
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme().clone();
        div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(10.))
            .child(div().text_size(px(20.)).font_weight(FontWeight::SEMIBOLD).child("Connect a workspace"))
            .child(
                div()
                    .max_w(px(420.))
                    .text_center()
                    .text_color(t.text_secondary)
                    .child("Flotilla runs on your server. Add an SSH host and Flotilla installs its daemon there for you."),
            )
            .child(
                primary_button("empty-add-host", "Add host", &t)
                    .mt(px(6.))
                    .on_click(cx.listener(|ws, _, window, cx| ws.open_add_host(window, cx))),
            )
    }

    fn render_host(&self, index: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme().clone();
        let h = &self.hosts[index];
        let (tone, status) = status_line(&h.phase);
        let (transport, latency, daemon) = match &h.phase {
            ConnPhase::Connected {
                kind,
                rtt_ms,
                daemon_version,
            } => (
                kind.to_string(),
                format!("{rtt_ms} ms"),
                format!("flotillad {daemon_version}"),
            ),
            _ => ("—".to_string(), "—".to_string(), "—".to_string()),
        };
        let problem = match &h.phase {
            ConnPhase::Failed { error } | ConnPhase::Reconnecting { error, .. } => {
                Some(error.clone())
            }
            _ => None,
        };
        let ssh = &h.profile.ssh;
        div()
            .w(px(520.))
            .p(px(20.))
            .rounded(t.radius_lg)
            .bg(t.surface_raised)
            .border_1()
            .border_color(t.border)
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(18.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(h.profile.name.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .text_color(t.text_secondary)
                            .child(div().size(px(7.)).rounded_full().bg(tone_color(tone, &t)))
                            .child(status),
                    ),
            )
            .child(kv(
                "Address",
                format!("{}@{}:{}", ssh.user, ssh.host, ssh.port),
                &t,
            ))
            .child(kv("Transport", transport, &t))
            .child(kv("Latency", latency, &t))
            .child(kv("Daemon", daemon, &t))
            .when_some(problem, |d, error| {
                d.child(div().text_color(t.danger).child(error))
            })
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .pt(px(4.))
                    .child(secondary_button("reconnect", "Reconnect", &t).on_click(
                        cx.listener(move |ws, _, window, cx| ws.connect(index, window, cx)),
                    ))
                    .child(
                        secondary_button("remove", "Remove", &t)
                            .text_color(t.danger)
                            .on_click(cx.listener(move |ws, _, _, cx| ws.remove_host(index, cx))),
                    ),
            )
    }
}
