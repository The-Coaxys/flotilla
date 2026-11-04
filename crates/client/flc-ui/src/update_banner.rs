//! Non-modal update banner (spec §8.4): available, staged and failed states.
//!
//! The updater decides the words and whether a restart is currently allowed;
//! this module only draws the row and forwards clicks. Nothing here can start
//! an update on its own, and a restart button is only present when the caller
//! supplied an action.

use std::rc::Rc;

use crate::theme::Theme;
use gpui::prelude::*;
use gpui::{AnyElement, App, FontWeight, Hsla, SharedString, Window, div, px};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Info,
    Success,
    Danger,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Apply the staged update and restart.
    Restart,
    /// Ask the release feed again.
    Retry,
}

/// A click on the banner's action button.
pub type ActionHandler = Rc<dyn Fn(Action, &mut Window, &mut App)>;

/// A click on "Later".
pub type DismissHandler = Rc<dyn Fn(&mut Window, &mut App)>;

/// Everything the banner draws. Owned strings so a render never borrows the
/// updater's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Banner {
    pub tone: Tone,
    pub title: SharedString,
    pub detail: SharedString,
    /// Button label and the action it sends, when the caller allows it.
    pub action: Option<(Action, SharedString)>,
    pub dismissible: bool,
}

impl Banner {
    pub fn new(
        tone: Tone,
        title: impl Into<SharedString>,
        detail: impl Into<SharedString>,
    ) -> Self {
        Self {
            tone,
            title: title.into(),
            detail: detail.into(),
            action: None,
            dismissible: true,
        }
    }

    pub fn action(mut self, action: Action, label: impl Into<SharedString>) -> Self {
        self.action = Some((action, label.into()));
        self
    }

    pub fn dismissible(mut self, dismissible: bool) -> Self {
        self.dismissible = dismissible;
        self
    }
}

pub fn tone_color(tone: Tone, theme: &Theme) -> Hsla {
    match tone {
        Tone::Info => theme.accent,
        Tone::Success => theme.success,
        Tone::Danger => theme.danger,
    }
}

/// Draw the banner row. `on_action` runs only from a click on the action
/// button, and `on_dismiss` from the "Later" button.
pub fn update_banner(
    id: &'static str,
    banner: &Banner,
    theme: &Theme,
    on_action: ActionHandler,
    on_dismiss: DismissHandler,
) -> AnyElement {
    let accent = tone_color(banner.tone, theme);
    let mut row = div()
        .id(id)
        .w_full()
        .flex_none()
        .flex()
        .items_center()
        .gap(px(10.))
        .px(px(14.))
        .py(px(8.))
        .bg(accent.opacity(0.12))
        .border_b_1()
        .border_color(theme.border)
        .child(div().flex_none().size(px(7.)).rounded_full().bg(accent))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(
                    div()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.text_primary)
                        .child(banner.title.clone()),
                )
                .child(
                    div()
                        .text_size(theme.text_size_sm)
                        .text_color(theme.text_secondary)
                        .child(banner.detail.clone()),
                ),
        );
    if let Some((action, label)) = &banner.action {
        let handler = on_action.clone();
        let action = *action;
        row = row.child(
            div()
                .id(SharedString::from(format!("{id}-action")))
                .flex_none()
                .h(px(24.))
                .px(px(10.))
                .flex()
                .items_center()
                .rounded(theme.radius_sm)
                .bg(theme.control_bg)
                .text_color(theme.text_primary)
                .cursor_pointer()
                .hover({
                    let hover = theme.raised_hover;
                    move |style| style.bg(hover)
                })
                .child(label.clone())
                .on_click(move |_, window, cx| handler(action, window, cx)),
        );
    }
    if banner.dismissible {
        let handler = on_dismiss.clone();
        row = row.child(
            div()
                .id(SharedString::from(format!("{id}-dismiss")))
                .flex_none()
                .h(px(24.))
                .px(px(8.))
                .flex()
                .items_center()
                .rounded(theme.radius_sm)
                .text_color(theme.text_secondary)
                .cursor_pointer()
                .hover({
                    let hover = theme.raised_hover;
                    move |style| style.bg(hover)
                })
                .child("Later")
                .on_click(move |_, window, cx| handler(window, cx)),
        );
    }
    row.into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tones_map_to_theme_colors() {
        let theme = Theme::dark();
        assert_eq!(tone_color(Tone::Info, &theme), theme.accent);
        assert_eq!(tone_color(Tone::Success, &theme), theme.success);
        assert_eq!(tone_color(Tone::Danger, &theme), theme.danger);
    }

    #[test]
    fn a_banner_only_offers_the_action_it_was_given() {
        let banner = Banner::new(Tone::Success, "Ready", "Restart to switch versions.");
        assert_eq!(banner.action, None);
        assert!(banner.dismissible);

        let banner = banner
            .action(Action::Restart, "Restart now")
            .dismissible(false);
        assert_eq!(banner.action, Some((Action::Restart, "Restart now".into())));
        assert!(!banner.dismissible);
    }
}
