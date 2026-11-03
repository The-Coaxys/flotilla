//! The GPUI application: one window with a title bar, a host sidebar and a center pane.
mod add_host;
mod host_view;
mod sidebar;
mod titlebar;

use std::sync::Arc;

use flc_conn::profile::HostProfile;
use flc_conn::supervisor::{ConnPhase, Supervisor};
use flc_ui::theme::ActiveTheme;
use gpui::prelude::*;
use gpui::{
    App, Application, AsyncApp, Bounds, Context, Entity, FocusHandle, Global, KeyBinding,
    SharedString, Subscription, TitlebarOptions, WeakEntity, Window, WindowBounds,
    WindowDecorations, WindowOptions, actions, div, px, size,
};

use crate::hosts::HostStore;
use crate::secrets::SecretStore;

actions!(flotilla, [Quit, AddHost]);

/// The networking runtime, reachable from every view.
pub struct Tokio(pub tokio::runtime::Handle);

impl Global for Tokio {}

pub fn run(rt: tokio::runtime::Handle) {
    Application::new().run(move |cx: &mut App| {
        flc_ui::theme::init(cx);
        cx.set_global(Tokio(rt));
        cx.bind_keys([
            KeyBinding::new("alt-f4", Quit, None),
            KeyBinding::new("ctrl-shift-n", AddHost, None),
        ]);
        cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
        let bounds = Bounds::centered(None, size(px(1200.), px(780.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("Flotilla".into()),
                appears_transparent: true,
                traffic_light_position: None,
            }),
            window_decorations: Some(WindowDecorations::Client),
            window_min_size: Some(size(px(760.), px(480.))),
            app_id: Some("Coaxys.Flotilla".into()),
            ..Default::default()
        };
        cx.open_window(options, |window, cx| {
            let workspace = cx.new(|cx| Workspace::new(window, cx));
            window.focus(&workspace.read(cx).focus);
            workspace
        })
        .expect("cannot open the main window");
        cx.on_window_closed(|cx: &mut App| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
}

pub(crate) struct HostEntry {
    pub profile: HostProfile,
    pub phase: ConnPhase,
    supervisor: Option<Supervisor>,
    /// Terminal session for this host; `None` until the first connection.
    pub(crate) sync: Option<Arc<flc_sync::Sync>>,
    pub(crate) shell: Option<Entity<flc_shell::ShellView>>,
    /// Bumped on every (re)connect so stale phase watchers are ignored.
    generation: u64,
}

pub(crate) enum Center {
    Empty,
    Host,
    AddHost(Entity<add_host::AddHostForm>),
}

pub struct Workspace {
    store: HostStore,
    secrets: Arc<dyn SecretStore>,
    pub(crate) hosts: Vec<HostEntry>,
    pub(crate) selected: Option<usize>,
    pub(crate) center: Center,
    banner: Option<SharedString>,
    focus: FocusHandle,
    // Keep onboarding alive even when the center pane displays another host.
    add_host: Option<Entity<add_host::AddHostForm>>,
    add_host_subscription: Option<Subscription>,
    /// A host added from the form connects on the next render, where a window
    /// is available to build its shell view.
    deferred_connect: Option<usize>,
    /// Window activation triggers an immediate reconnect attempt.
    activation: Option<Subscription>,
}

impl Workspace {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let path = HostStore::default_path()
            .unwrap_or_else(|| std::env::temp_dir().join("Flotilla").join("hosts.json"));
        let store = HostStore::new(path);
        let secrets: Arc<dyn SecretStore> = Arc::from(crate::secrets::default_store());
        let (profiles, banner) = match store.load() {
            Ok(hosts) => (hosts, None),
            Err(e) => (Vec::new(), Some(SharedString::from(format!("{e:#}")))),
        };
        let mut ws = Self {
            store,
            secrets,
            hosts: profiles
                .into_iter()
                .map(|profile| HostEntry {
                    profile,
                    phase: ConnPhase::Idle,
                    supervisor: None,
                    sync: None,
                    shell: None,
                    generation: 0,
                })
                .collect(),
            selected: None,
            center: Center::Empty,
            banner,
            focus: cx.focus_handle(),
            add_host: None,
            add_host_subscription: None,
            deferred_connect: None,
            activation: None,
        };
        // Waking the machine or returning to the window retries immediately
        // instead of waiting out the reconnect backoff.
        ws.activation = Some(cx.observe_window_activation(window, |ws, _window, _cx| {
            for host in &ws.hosts {
                if let Some(supervisor) = &host.supervisor {
                    supervisor.retry_now();
                }
            }
        }));
        if !ws.hosts.is_empty() {
            ws.selected = Some(0);
            ws.center = Center::Host;
        }
        for i in 0..ws.hosts.len() {
            ws.connect(i, window, cx);
        }
        ws
    }

    /// Build the shell view for a connected host on first display. Requires a
    /// window (the shell takes focus), so it runs from the render path.
    pub(crate) fn ensure_shell(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.hosts.get_mut(index) else {
            return;
        };
        if entry.shell.is_some() {
            return;
        }
        let Some(sync) = entry.sync.clone() else {
            return;
        };
        if !matches!(entry.phase, ConnPhase::Connected { .. }) {
            return;
        }
        let shell = cx.new(|cx| {
            flc_shell::ShellView::new(sync, flc_shell::ShellViewOptions::default(), window, cx)
        });
        // The phase already reached Connected, so no watch update will carry
        // the latency: publish it now or Auto prediction stays disabled.
        if let ConnPhase::Connected { rtt_ms, .. } = entry.phase {
            shell.update(cx, |shell, cx| {
                shell.set_rtt(Some(std::time::Duration::from_millis(rtt_ms.into())), cx);
            });
        }
        entry.shell = Some(shell);
    }

    pub(crate) fn selected_phase(&self) -> Option<&ConnPhase> {
        self.selected
            .and_then(|i| self.hosts.get(i))
            .map(|h| &h.phase)
    }

    pub(crate) fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        self.selected = Some(index);
        self.center = Center::Host;
        cx.notify();
    }

    /// (Re)start the supervisor for host `index`.
    pub(crate) fn connect(&mut self, index: usize, _window: &mut Window, cx: &mut Context<Self>) {
        let token = match self.hosts.get(index) {
            Some(entry) => self.secrets.get(&entry.profile.id).unwrap_or_else(|e| {
                tracing::warn!("cannot read the device token: {e:#}");
                None
            }),
            None => return,
        };
        let rt = cx.global::<Tokio>().0.clone();
        let entry = &mut self.hosts[index];
        if let Some(old) = entry.supervisor.take() {
            old.stop();
        }
        // A new connection gets a new session and shell bound to it.
        entry.shell = None;
        entry.sync = None;
        entry.generation += 1;
        let Some(spec) = crate::connect::connect_spec(&entry.profile, token) else {
            entry.phase = ConnPhase::Failed {
                error: "This host was never set up. Remove it and add it again.".into(),
            };
            cx.notify();
            return;
        };
        let supervisor = Supervisor::spawn_on(&rt, spec);
        let rx = supervisor.phase();
        // The session must be created inside the runtime: it spawns the
        // channel and layout tasks that survive individual transports.
        let cache_dir = dirs::data_local_dir()
            .map(|dir| dir.join("Flotilla").join("cache"))
            .unwrap_or_else(|| std::env::temp_dir().join("Flotilla").join("cache"));
        let links = supervisor.links();
        let sync = {
            let _guard = rt.enter();
            Arc::new(flc_sync::Sync::new(
                links,
                Some(cache_dir),
                flc_sync::Config::default(),
            ))
        };
        entry.sync = Some(sync);
        entry.shell = None;
        entry.supervisor = Some(supervisor);
        entry.phase = ConnPhase::Connecting;
        let (host_id, generation) = (entry.profile.id.clone(), entry.generation);
        cx.spawn(
            async move |this: WeakEntity<Workspace>, cx: &mut AsyncApp| {
                watch_phase(this, cx, host_id, generation, rx).await;
            },
        )
        .detach();
        cx.notify();
    }

    pub(crate) fn remove_host(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.hosts.len() {
            return;
        }
        let entry = self.hosts.remove(index);
        if let Some(sup) = entry.supervisor {
            sup.stop();
        }
        if let Err(e) = self.secrets.delete(&entry.profile.id) {
            tracing::warn!("cannot delete the device token: {e:#}");
        }
        if let Err(e) = self.store.remove(&entry.profile.id) {
            self.banner = Some(format!("{e:#}").into());
        }
        self.selected = if self.hosts.is_empty() {
            None
        } else {
            Some(index.min(self.hosts.len() - 1))
        };
        self.center = if self.selected.is_some() {
            Center::Host
        } else {
            Center::Empty
        };
        cx.notify();
    }

    pub(crate) fn open_add_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let form = if let Some(form) = &self.add_host {
            form.clone()
        } else {
            let form = cx.new(add_host::AddHostForm::new);
            self.add_host_subscription = Some(
                cx.subscribe(&form, |ws, _form, event: &add_host::AddHostEvent, cx| {
                    ws.on_add_host(event, cx)
                }),
            );
            self.add_host = Some(form.clone());
            form
        };
        form.read(cx).focus_first(window, cx);
        self.center = Center::AddHost(form);
        cx.notify();
    }

    fn on_add_host(&mut self, event: &add_host::AddHostEvent, cx: &mut Context<Self>) {
        self.add_host = None;
        self.add_host_subscription = None;
        match event {
            add_host::AddHostEvent::Cancelled => {
                self.center = if self.selected.is_some() {
                    Center::Host
                } else {
                    Center::Empty
                };
            }
            add_host::AddHostEvent::Completed { profile, token } => {
                if let Err(e) = self.secrets.set(&profile.id, token) {
                    self.banner = Some(format!("Could not store the device token: {e:#}").into());
                }
                if let Err(e) = self.store.upsert(profile.clone()) {
                    self.banner = Some(format!("{e:#}").into());
                }
                self.hosts.push(HostEntry {
                    profile: profile.clone(),
                    phase: ConnPhase::Idle,
                    supervisor: None,
                    sync: None,
                    shell: None,
                    generation: 0,
                });
                let index = self.hosts.len() - 1;
                self.selected = Some(index);
                self.center = Center::Host;
                self.deferred_connect = Some(index);
            }
        }
        cx.notify();
    }
}

async fn watch_phase(
    this: WeakEntity<Workspace>,
    cx: &mut AsyncApp,
    host_id: String,
    generation: u64,
    mut rx: tokio::sync::watch::Receiver<ConnPhase>,
) {
    loop {
        let phase = rx.borrow_and_update().clone();
        // Predictive echo is gated on the measured link latency: publish it
        // whenever the link changes and clear it while there is no link.
        let rtt = match &phase {
            ConnPhase::Connected { rtt_ms, .. } => {
                Some(std::time::Duration::from_millis((*rtt_ms).into()))
            }
            _ => None,
        };
        let alive = this
            .update(cx, |ws, cx| {
                let Some(h) = ws
                    .hosts
                    .iter_mut()
                    .find(|h| h.profile.id == host_id && h.generation == generation)
                else {
                    return;
                };
                h.phase = phase;
                if let Some(shell) = &h.shell {
                    shell.update(cx, |shell, cx| shell.set_rtt(rtt, cx));
                }
                cx.notify();
            })
            .is_ok();
        if !alive || rx.changed().await.is_err() {
            break;
        }
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        div()
            .id("workspace")
            .key_context("Workspace")
            .track_focus(&self.focus)
            .on_action(cx.listener(|ws, _: &AddHost, window, cx| ws.open_add_host(window, cx)))
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.surface_bg)
            .text_color(theme.text_primary)
            .font_family(theme.font_ui.clone())
            .text_size(theme.text_size)
            .child(self.render_titlebar(window, cx))
            .when_some(self.banner.clone(), |d, message| {
                d.child(
                    div()
                        .id("banner")
                        .px(px(14.))
                        .py(px(8.))
                        .bg(theme.danger.opacity(0.15))
                        .text_color(theme.text_primary)
                        .child(message)
                        .on_click(cx.listener(|ws, _, _, cx| {
                            ws.banner = None;
                            cx.notify();
                        })),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(self.render_sidebar(cx))
                    .child(self.render_center(window, cx)),
            )
    }
}
