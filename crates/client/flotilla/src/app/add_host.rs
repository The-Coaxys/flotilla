//! Add-host card: form → SSH connect → confirm host key → install with progress.
use std::sync::Arc;
use std::time::Duration;

use fl_transport::ssh::{HostKeyPolicy, SshSession};
use flc_conn::bootstrap::{BootstrapOptions, BootstrapStep, bootstrap};
use flc_conn::profile::HostProfile;
use flc_ui::input::TextInput;
use flc_ui::theme::{ActiveTheme, Theme};
use gpui::prelude::*;
use gpui::{
    AnyElement, App, AsyncApp, Context, Entity, EventEmitter, Focusable, FontWeight, SharedString,
    WeakEntity, Window, div, px,
};

use super::Tokio;
use super::host_view::{primary_button, secondary_button};
use crate::connect::{
    Draft, default_key_path, device_name, parse_draft, profile_from_enrollment, step_index,
    step_label,
};

// One event per install; the size gap between variants is harmless.
#[allow(clippy::large_enum_variant)]
pub enum AddHostEvent {
    Completed { profile: HostProfile, token: String },
    Cancelled,
}

enum Stage {
    Editing {
        error: Option<SharedString>,
    },
    Connecting,
    ConfirmHostKey {
        draft: Draft,
        session: Arc<SshSession>,
    },
    Installing {
        step: BootstrapStep,
    },
}

pub struct AddHostForm {
    name: Entity<TextInput>,
    host: Entity<TextInput>,
    port: Entity<TextInput>,
    user: Entity<TextInput>,
    key_path: Entity<TextInput>,
    use_agent: bool,
    stage: Stage,
}

impl EventEmitter<AddHostEvent> for AddHostForm {}

impl AddHostForm {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let with_text = |cx: &mut Context<Self>, placeholder: &'static str, text: String| {
            cx.new(|cx| {
                let mut input = TextInput::new(cx, placeholder);
                if !text.is_empty() {
                    input.set_text(text, cx);
                }
                input
            })
        };
        Self {
            name: with_text(cx, "my-vps", String::new()),
            host: with_text(cx, "203.0.113.10 or vps.example.com", String::new()),
            port: with_text(cx, "22", "22".into()),
            user: with_text(cx, "root", "root".into()),
            key_path: with_text(
                cx,
                "C:\\Users\\you\\.ssh\\id_ed25519",
                default_key_path(dirs::home_dir()),
            ),
            use_agent: false,
            stage: Stage::Editing { error: None },
        }
    }

    pub fn focus_first(&self, window: &mut Window, cx: &App) {
        window.focus(&self.host.focus_handle(cx));
    }

    fn text(&self, input: &Entity<TextInput>, cx: &Context<Self>) -> String {
        input.read(cx).text().to_string()
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let draft = match parse_draft(
            &self.text(&self.name, cx),
            &self.text(&self.host, cx),
            &self.text(&self.port, cx),
            &self.text(&self.user, cx),
            &self.text(&self.key_path, cx),
            self.use_agent,
            dirs::home_dir(),
        ) {
            Ok(d) => d,
            Err(message) => {
                self.stage = Stage::Editing {
                    error: Some(message.into()),
                };
                cx.notify();
                return;
            }
        };
        self.stage = Stage::Connecting;
        cx.notify();
        let rt = cx.global::<Tokio>().0.clone();
        let (target, auth) = (draft.target.clone(), draft.auth.clone());
        let task = rt.spawn(async move {
            SshSession::connect(
                &target,
                &auth,
                &HostKeyPolicy::TrustOnFirstUse,
                Duration::from_secs(15),
            )
            .await
        });
        cx.spawn(
            async move |this: WeakEntity<AddHostForm>, cx: &mut AsyncApp| {
                let result = task.await;
                let _ = this.update(cx, |form, cx| {
                    form.stage = match result {
                        Ok(Ok(session)) => Stage::ConfirmHostKey {
                            draft,
                            session: Arc::new(session),
                        },
                        Ok(Err(e)) => Stage::Editing {
                            error: Some(e.to_string().into()),
                        },
                        Err(e) => Stage::Editing {
                            error: Some(format!("internal error: {e}").into()),
                        },
                    };
                    cx.notify();
                });
            },
        )
        .detach();
    }

    fn trust_and_install(&mut self, cx: &mut Context<Self>) {
        let Stage::ConfirmHostKey { draft, session } = std::mem::replace(
            &mut self.stage,
            Stage::Installing {
                step: BootstrapStep::Probing,
            },
        ) else {
            return;
        };
        cx.notify();
        let fingerprint = session.host_key_fingerprint().to_string();
        let rt = cx.global::<Tokio>().0.clone();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<BootstrapStep>();
        let task = rt.spawn(async move {
            let progress = move |step: BootstrapStep| {
                let _ = tx.send(step);
            };
            let options = BootstrapOptions::new(device_name());
            let result =
                bootstrap(&session, &crate::embedded::artifacts(), &options, &progress).await;
            session.disconnect().await;
            result
        });
        cx.spawn(
            async move |this: WeakEntity<AddHostForm>, cx: &mut AsyncApp| {
                while let Some(step) = rx.recv().await {
                    let updated = this.update(cx, |form, cx| {
                        form.stage = Stage::Installing { step };
                        cx.notify();
                    });
                    if updated.is_err() {
                        return;
                    }
                }
                let result = task.await;
                let _ = this.update(cx, |form, cx| {
                    match result {
                        Ok(Ok(outcome)) => match outcome.enrollment {
                            Some(enrollment) => {
                                let profile = profile_from_enrollment(
                                    uuid::Uuid::new_v4().to_string(),
                                    &draft,
                                    &fingerprint,
                                    &enrollment,
                                );
                                cx.emit(AddHostEvent::Completed {
                                    profile,
                                    token: enrollment.token.clone(),
                                });
                            }
                            None => {
                                form.stage = Stage::Editing {
                                    error: Some("The daemon did not return credentials.".into()),
                                }
                            }
                        },
                        Ok(Err(e)) => {
                            form.stage = Stage::Editing {
                                error: Some(e.to_string().into()),
                            }
                        }
                        Err(e) => {
                            form.stage = Stage::Editing {
                                error: Some(format!("internal error: {e}").into()),
                            }
                        }
                    }
                    cx.notify();
                });
            },
        )
        .detach();
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        cx.emit(AddHostEvent::Cancelled);
    }

    fn field(label: &'static str, input: &Entity<TextInput>, t: &Theme) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(
                div()
                    .text_size(t.text_size_sm)
                    .text_color(t.text_secondary)
                    .child(label),
            )
            .child(input.clone())
    }

    fn render_fields(
        &self,
        error: Option<SharedString>,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let agent = self.use_agent;
        div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(Self::field("Name", &self.name, t))
            .child(Self::field("Address", &self.host, t))
            .child(
                div()
                    .flex()
                    .gap(px(10.))
                    .child(div().w(px(90.)).child(Self::field("Port", &self.port, t)))
                    .child(div().flex_1().child(Self::field("User", &self.user, t))),
            )
            .when(!agent, |d| {
                d.child(Self::field("Private key file", &self.key_path, t))
            })
            .child(
                div()
                    .id("use-agent")
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .cursor_pointer()
                    .child(
                        div()
                            .size(px(14.))
                            .rounded(px(3.))
                            .border_1()
                            .border_color(if agent { t.accent } else { t.text_tertiary })
                            .when(agent, |d| d.bg(t.accent)),
                    )
                    .child(
                        div()
                            .text_color(t.text_secondary)
                            .child("Use SSH agent (OpenSSH agent or Pageant)"),
                    )
                    .on_click(cx.listener(|form, _, _, cx| {
                        form.use_agent = !form.use_agent;
                        cx.notify();
                    })),
            )
            .when_some(error, |d, e| d.child(div().text_color(t.danger).child(e)))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap(px(8.))
                    .pt(px(4.))
                    .child(
                        secondary_button("cancel", "Cancel", t)
                            .on_click(cx.listener(|form, _, _, cx| form.cancel(cx))),
                    )
                    .child(
                        primary_button("connect", "Connect", t)
                            .on_click(cx.listener(|form, _, _, cx| form.submit(cx))),
                    ),
            )
            .into_any_element()
    }

    fn render_confirm(
        &self,
        draft: &Draft,
        fingerprint: &str,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(div().text_color(t.text_secondary).child(format!(
                "First connection to {}. Check that this fingerprint matches your server's SSH host key:",
                draft.target.host
            )))
            .child(
                div()
                    .p(px(10.))
                    .rounded(t.radius_sm)
                    .bg(t.surface_bg)
                    .font_family(t.font_mono.clone())
                    .text_size(t.mono_size)
                    .child(fingerprint.to_string()),
            )
            .child(div().text_size(t.text_size_sm).text_color(t.text_tertiary).child(
                "On the server: ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub",
            ))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap(px(8.))
                    .child(secondary_button("cancel", "Cancel", t).on_click(cx.listener(|form, _, _, cx| form.cancel(cx))))
                    .child(
                        primary_button("trust", "Trust and install", t)
                            .on_click(cx.listener(|form, _, _, cx| form.trust_and_install(cx))),
                    ),
            )
            .into_any_element()
    }

    fn render_progress(&self, current: &BootstrapStep, t: &Theme) -> AnyElement {
        let now = step_index(current);
        let steps = [
            BootstrapStep::Probing,
            match current {
                BootstrapStep::Uploading { bytes } => BootstrapStep::Uploading { bytes: *bytes },
                _ => BootstrapStep::Uploading { bytes: 0 },
            },
            BootstrapStep::Starting,
            BootstrapStep::Enrolling,
        ];
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .children(steps.iter().map(|step| {
                let i = step_index(step);
                let (mark, color) = if i < now {
                    ("✓", t.success)
                } else if i == now {
                    ("●", t.accent)
                } else {
                    ("○", t.text_tertiary)
                };
                let label = match step {
                    BootstrapStep::Uploading { bytes: 0 } => "Uploading the daemon".to_string(),
                    other => step_label(other),
                };
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(div().w(px(14.)).text_color(color).child(mark))
                    .child(
                        div()
                            .text_color(if i <= now {
                                t.text_primary
                            } else {
                                t.text_tertiary
                            })
                            .child(label),
                    )
            }))
            .into_any_element()
    }
}

impl Render for AddHostForm {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme().clone();
        let body = match &self.stage {
            Stage::Editing { error } => self.render_fields(error.clone(), &t, cx),
            Stage::Connecting => div()
                .text_color(t.text_secondary)
                .child("Connecting to the server…")
                .into_any_element(),
            Stage::ConfirmHostKey { draft, session } => {
                let (draft, fingerprint) =
                    (draft.clone(), session.host_key_fingerprint().to_string());
                self.render_confirm(&draft, &fingerprint, &t, cx)
            }
            Stage::Installing { step } => self.render_progress(step, &t),
        };
        div()
            .w(px(460.))
            .p(px(24.))
            .rounded(t.radius_lg)
            .bg(t.surface_raised)
            .border_1()
            .border_color(t.border)
            .flex()
            .flex_col()
            .gap(px(16.))
            .child(
                div()
                    .text_size(px(18.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Add a host"),
            )
            .child(body)
    }
}
