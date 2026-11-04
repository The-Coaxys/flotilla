//! The GPUI application: one window with a title bar, a host sidebar and a center pane.
mod add_host;
mod git_actions;
mod host_view;
mod host_workspace;
mod hover_card;
mod hud;
mod palette;
mod pip;
mod settings;
mod sidebar;
mod titlebar;

use std::sync::Arc;

use flc_conn::profile::HostProfile;
use flc_conn::supervisor::{ConnPhase, Supervisor};
use flc_ui::theme::ActiveTheme;
use gpui::prelude::*;
use gpui::{
    AnyWindowHandle, App, Application, AsyncApp, Bounds, ClipboardItem, Context, Entity,
    FocusHandle, Global, KeyBinding, SharedString, Subscription, TitlebarOptions, WeakEntity,
    Window, WindowBounds, WindowDecorations, WindowKind, WindowOptions, actions, div, px, size,
};

use crate::hosts::HostStore;
use crate::secrets::SecretStore;
use crate::updater::{UpdateState, Updater};

actions!(
    flotilla,
    [
        Quit,
        AddHost,
        PaletteGlobal,
        PaletteFiles,
        SettingsOpen,
        PipToggle,
        PerformanceToggle,
        GitPrimary,
        GitAlternate,
        InboxOpen,
        FindScrollback,
        RunTarget,
        NextTab,
        PreviousTab,
    ]
);

/// The networking runtime, reachable from every view.
pub struct Tokio(pub tokio::runtime::Handle);

impl Global for Tokio {}

/// Rebuild every application key binding from `keymap`. GPUI keeps old
/// bindings alive, so they are cleared first: a rebind must actually replace a
/// chord, and the text-input editing keys are reinstalled afterwards.
pub(crate) fn install_keymap(keymap: &flc_ui::keymap::Keymap, cx: &mut App) {
    cx.clear_key_bindings();
    flc_ui::input::bind_keys(cx);
    let mut bindings = Vec::new();
    for action in flc_ui::keymap::ACTIONS {
        let Some(chord) = keymap.chord(action.id) else {
            continue;
        };
        let stroke = chord.canonical();
        match action.id {
            "app.quit" => bindings.push(KeyBinding::new(&stroke, Quit, None)),
            "host.add" => bindings.push(KeyBinding::new(&stroke, AddHost, None)),
            "palette.global" => bindings.push(KeyBinding::new(&stroke, PaletteGlobal, None)),
            "palette.files" => bindings.push(KeyBinding::new(&stroke, PaletteFiles, None)),
            "settings.open" => bindings.push(KeyBinding::new(&stroke, SettingsOpen, None)),
            "pip.toggle" => bindings.push(KeyBinding::new(&stroke, PipToggle, None)),
            "performance.toggle" => {
                bindings.push(KeyBinding::new(&stroke, PerformanceToggle, None))
            }
            "git.primary" => bindings.push(KeyBinding::new(&stroke, GitPrimary, None)),
            "git.alternate" => bindings.push(KeyBinding::new(&stroke, GitAlternate, None)),
            "inbox.open" => bindings.push(KeyBinding::new(&stroke, InboxOpen, None)),
            "terminal.find" => bindings.push(KeyBinding::new(&stroke, FindScrollback, None)),
            "run.target" => bindings.push(KeyBinding::new(&stroke, RunTarget, None)),
            "tab.next" => bindings.push(KeyBinding::new(&stroke, NextTab, None)),
            "tab.previous" => bindings.push(KeyBinding::new(&stroke, PreviousTab, None)),
            other => debug_assert!(false, "keymap action {other} has no GPUI binding"),
        }
    }
    cx.bind_keys(bindings);
}

pub fn run(rt: tokio::runtime::Handle) {
    Application::new().run(move |cx: &mut App| {
        flc_ui::theme::init(cx);
        // Load settings before the first window so the workspace theme and the
        // user's keymap are the ones that first render.
        let settings = cx.new(settings::Settings::new);
        let keymap = settings.read(cx).keymap().clone();
        install_keymap(&keymap, cx);
        cx.set_global(Tokio(rt));
        let (notify_tx, notify_rx) = tokio::sync::mpsc::unbounded_channel();
        crate::notify::install(notify_tx);
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
        let main = cx
            .open_window(options, move |window, cx| {
                let workspace =
                    cx.new(|cx| Workspace::new(settings.clone(), notify_rx, window, cx));
                window.focus(&workspace.read(cx).focus);
                window.on_next_frame(|_, _| crate::updater::confirm_window_ready());
                workspace
            })
            .expect("cannot open the main window");
        let main: AnyWindowHandle = main.into();
        let main_id = main.window_id();
        // A PiP window keeps rendering after the main window closes, so quit
        // on the main window specifically and take the PiP down with the app.
        cx.on_window_closed(move |cx: &mut App| {
            if !cx
                .windows()
                .iter()
                .any(|window| window.window_id() == main_id)
            {
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
    /// Projects, worktrees and terminals of the connected host.
    pub(crate) workspace: Option<Entity<host_workspace::HostWorkspace>>,
    workspace_events: Option<Subscription>,
    /// Latest OSC 52 store waiting for the user's first answer for this host.
    pub(crate) clipboard_request: Option<String>,
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
    /// Theme, fonts, terminal palettes and the keymap; shared by every host.
    pub(crate) settings: Entity<settings::Settings>,
    secrets: Arc<dyn SecretStore>,
    pub(crate) hosts: Vec<HostEntry>,
    pub(crate) selected: Option<usize>,
    pub(crate) center: Center,
    banner: Option<SharedString>,
    /// Signed update channel and the state it last published.
    updater: Updater,
    update: UpdateState,
    /// Banner the user dismissed for this session, as [`update_key`] names it.
    update_dismissed: Option<String>,
    focus: FocusHandle,
    // Keep onboarding alive even when the center pane displays another host.
    add_host: Option<Entity<add_host::AddHostForm>>,
    add_host_subscription: Option<Subscription>,
    /// A host added from the form connects on the next render, where a window
    /// is available to build its shell view.
    deferred_connect: Option<usize>,
    /// Window activation triggers an immediate reconnect attempt.
    activation: Option<Subscription>,
    hud: Option<hud::Hud>,
    hud_sampler: Option<gpui::Task<()>>,
    /// The surface pinned into the second window, and that window's handle.
    pip: Option<pip::PipPin>,
    pip_window: Option<AnyWindowHandle>,
    /// Content waiting for the next render, where opening a window is safe.
    pip_pending: Option<pip::PipContent>,
    /// A host asked to pin its current surface on the next render.
    pip_request: bool,
    /// Toast clicks arrive from WinRT's callback thread.
    notify_activations: Option<tokio::sync::mpsc::UnboundedReceiver<crate::notify::Activation>>,
}

impl Workspace {
    fn new(
        settings: Entity<settings::Settings>,
        notify_activations: tokio::sync::mpsc::UnboundedReceiver<crate::notify::Activation>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let path = HostStore::default_path()
            .unwrap_or_else(|| std::env::temp_dir().join("Flotilla").join("hosts.json"));
        let store = HostStore::new(path);
        let secrets: Arc<dyn SecretStore> = Arc::from(crate::secrets::default_store());
        // Checking for a signed update never touches the network on a build
        // without a release feed, and it never replaces anything by itself.
        let updater = Updater::spawn(crate::updater::Config::detect());
        let update = updater.state();
        let (profiles, banner) = match store.load() {
            Ok(hosts) => (hosts, None),
            Err(e) => (Vec::new(), Some(SharedString::from(format!("{e:#}")))),
        };
        let mut ws = Self {
            store,
            settings,
            secrets,
            hosts: profiles
                .into_iter()
                .map(|profile| HostEntry {
                    profile,
                    phase: ConnPhase::Idle,
                    supervisor: None,
                    sync: None,
                    workspace: None,
                    workspace_events: None,
                    clipboard_request: None,
                    generation: 0,
                })
                .collect(),
            selected: None,
            center: Center::Empty,
            banner,
            updater,
            update,
            update_dismissed: None,
            focus: cx.focus_handle(),
            add_host: None,
            add_host_subscription: None,
            deferred_connect: None,
            activation: None,
            hud: None,
            hud_sampler: None,
            pip: None,
            pip_window: None,
            pip_pending: None,
            pip_request: false,
            notify_activations: Some(notify_activations),
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
        let mut updates = ws.updater.watch();
        cx.spawn(
            async move |this: WeakEntity<Workspace>, cx: &mut AsyncApp| {
                watch_updates(this, cx, &mut updates).await;
            },
        )
        .detach();
        if let Some(mut activations) = ws.notify_activations.take() {
            cx.spawn(
                async move |this: WeakEntity<Workspace>, cx: &mut AsyncApp| {
                    while let Some(activation) = activations.recv().await {
                        if this
                            .update(cx, |ws, cx| ws.activate_notification(activation, cx))
                            .is_err()
                        {
                            break;
                        }
                    }
                },
            )
            .detach();
        }
        ws
    }

    /// Build the workspace view for a connected host on first display. Runs
    /// from the render path, where a window exists for its shells.
    pub(crate) fn ensure_shell(&mut self, index: usize, cx: &mut Context<Self>) {
        let settings = self.settings.clone();
        let Some(entry) = self.hosts.get_mut(index) else {
            return;
        };
        if entry.workspace.is_some() {
            return;
        }
        let Some(sync) = entry.sync.clone() else {
            return;
        };
        if !matches!(entry.phase, ConnPhase::Connected { .. }) {
            return;
        }
        // The daemon socket lives under the remote home's .flotilla.
        let home_dir = entry
            .profile
            .socket_path
            .as_deref()
            .and_then(|socket| socket.split("/.flotilla/").next())
            .filter(|home| home.starts_with('/'))
            .unwrap_or("/")
            .to_string();
        let workspace =
            cx.new(|cx| host_workspace::HostWorkspace::new(sync, home_dir, settings, cx));
        // The phase already reached Connected, so no watch update will carry
        // the latency: publish it now or Auto prediction stays disabled.
        if let ConnPhase::Connected { rtt_ms, .. } = entry.phase {
            workspace.update(cx, |workspace, cx| {
                workspace.set_rtt(Some(std::time::Duration::from_millis(rtt_ms.into())), cx);
            });
        }
        let host_id = entry.profile.id.clone();
        entry.workspace_events = Some(cx.subscribe(
            &workspace,
            move |ws, _, event: &host_workspace::HostWorkspaceEvent, cx| {
                ws.on_workspace_event(&host_id, event, cx)
            },
        ));
        entry.workspace = Some(workspace);
    }

    fn on_workspace_event(
        &mut self,
        host_id: &str,
        event: &host_workspace::HostWorkspaceEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            host_workspace::HostWorkspaceEvent::OpenSettings => self.request_settings(cx),
            host_workspace::HostWorkspaceEvent::ToggleHud => self.toggle_hud(cx),
            host_workspace::HostWorkspaceEvent::ClipboardStore(text) => {
                let Some(entry) = self.hosts.iter_mut().find(|h| h.profile.id == host_id) else {
                    return;
                };
                match entry.profile.clipboard_consent {
                    Some(true) => cx.write_to_clipboard(ClipboardItem::new_string(text.clone())),
                    Some(false) => {}
                    // Ask once; a newer store replaces the pending one.
                    None => {
                        entry.clipboard_request = Some(text.clone());
                        cx.notify();
                    }
                }
            }
            host_workspace::HostWorkspaceEvent::Notify {
                class,
                title,
                session,
            } => self.on_notification(host_id, *class, title, *session, cx),
            host_workspace::HostWorkspaceEvent::PinRequest => {
                self.pip_request = true;
                cx.notify();
            }
        }
    }

    /// The in-app toast is already visible; a desktop toast additionally
    /// follows the per-class policy and the focused-window rule.
    fn on_notification(
        &self,
        host_id: &str,
        class: flc_ui::notify::EventClass,
        title: &str,
        session: uuid::Uuid,
        cx: &mut Context<Self>,
    ) {
        let policy = self.settings.read(cx).notifications().clone();
        let focused = cx.active_window().is_some();
        if policy.should_deliver(class, focused) {
            crate::notify::desktop(host_id, class, title, session, policy.sounds);
        }
    }

    /// A Windows toast was clicked: bring the app forward and open the source
    /// session's worktree and tab.
    pub(super) fn activate_notification(
        &mut self,
        activation: crate::notify::Activation,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self
            .hosts
            .iter()
            .position(|host| host.profile.id == activation.host)
        else {
            return;
        };
        let Some(workspace) = self.hosts[index].workspace.clone() else {
            return;
        };
        self.selected = Some(index);
        self.center = Center::Host;
        workspace.update(cx, |workspace, cx| {
            workspace.activate_session(activation.session, cx)
        });
        cx.activate(true);
        cx.notify();
    }

    fn selected_workspace(&self) -> Option<Entity<host_workspace::HostWorkspace>> {
        self.selected
            .and_then(|index| self.hosts.get(index))
            .and_then(|host| host.workspace.clone())
    }

    /// Ctrl+Shift+P: close the PiP if it is open, otherwise pin the current
    /// surface. Refuses with a banner when nothing eligible is showing.
    pub(super) fn toggle_pip(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(handle) = self.pip_window {
            if cx
                .windows()
                .iter()
                .any(|window| window.window_id() == handle.window_id())
            {
                let _ = handle.update(cx, |_, window, _| window.remove_window());
            }
            self.pip_window = None;
            self.pip = None;
            cx.notify();
            return;
        }
        let Some(index) = self.selected else {
            self.banner = Some(SharedString::from(
                "Connect to a host before pinning a surface.",
            ));
            cx.notify();
            return;
        };
        let Some(workspace) = self.hosts[index].workspace.clone() else {
            self.banner = Some(SharedString::from(
                "Connect to a host before pinning a surface.",
            ));
            cx.notify();
            return;
        };
        let host_id = self.hosts[index].profile.id.clone();
        let content = workspace.update(cx, |workspace, cx| workspace.pin_surface(window, cx));
        let Some(content) = content else {
            // Nothing eligible: no empty window, just the explanation.
            self.banner = Some(SharedString::from(
                "Open a terminal, chat, run or review surface to pin it.",
            ));
            cx.notify();
            return;
        };
        let pin = pip::PipPin {
            host: host_id,
            scope: workspace.read(cx).scope(),
            kind: content.kind(),
            session: content.session(),
        };
        self.pip = Some(pin);
        self.pip_pending = Some(content);
        cx.notify();
    }

    /// Open the second window once, from the render path where a window and
    /// the app exist together.
    fn open_pending_pip(&mut self, cx: &mut Context<Self>) {
        if self.pip_window.is_some() {
            return;
        }
        let Some(content) = self.pip_pending.take() else {
            return;
        };
        let settings = self.settings.clone();
        let this = cx.weak_entity();
        cx.defer(move |cx: &mut App| {
            let bounds = Bounds::centered(None, size(px(560.), px(380.)), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some(format!("Flotilla — {}", content.title()).into()),
                    appears_transparent: true,
                    traffic_light_position: None,
                }),
                window_decorations: Some(WindowDecorations::Client),
                window_min_size: Some(size(px(280.), px(180.))),
                // Floating is always-on-top on X11 and macOS. Windows needs
                // the SetWindowPos call below; gpui maps every kind there to
                // a normal WS_EX_APPWINDOW.
                kind: WindowKind::Floating,
                is_movable: true,
                is_resizable: true,
                app_id: Some(crate::notify::APP_USER_MODEL_ID.into()),
                ..Default::default()
            };
            match cx.open_window(options, move |window, cx| {
                let view = cx.new(|cx| pip::PipView::new(content, settings, window, cx));
                #[cfg(windows)]
                pip::make_topmost(window);
                view
            }) {
                Ok(handle) => {
                    let handle: AnyWindowHandle = handle.into();
                    let _ = this.update(cx, |workspace, cx| {
                        workspace.pip_window = Some(handle);
                        cx.notify();
                    });
                }
                Err(error) => {
                    let _ = this.update(cx, |workspace, cx| {
                        workspace.banner = Some(SharedString::from(format!(
                            "Cannot open the picture-in-picture window: {error}"
                        )));
                        workspace.pip = None;
                        cx.notify();
                    });
                }
            }
        });
    }

    fn run_primary_git(&mut self, alternate: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.selected_workspace() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            workspace.run_primary_git(alternate, window, cx)
        });
    }

    fn open_inbox(&mut self, cx: &mut Context<Self>) {
        let Some(workspace) = self.selected_workspace() else {
            return;
        };
        workspace.update(cx, |workspace, cx| workspace.open_inbox(cx));
    }

    fn find_scrollback(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.selected_workspace() else {
            return;
        };
        workspace.update(cx, |workspace, cx| workspace.find_scrollback(window, cx));
    }

    fn run_target(&mut self, cx: &mut Context<Self>) {
        let Some(workspace) = self.selected_workspace() else {
            return;
        };
        workspace.update(cx, |workspace, cx| workspace.run_target(cx));
    }

    fn cycle_tab(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(workspace) = self.selected_workspace() else {
            return;
        };
        workspace.update(cx, |workspace, cx| workspace.cycle_tab(forward, cx));
    }

    /// Record the user's answer for this host and apply the pending store
    /// when it was allowed.
    pub(crate) fn answer_clipboard(&mut self, index: usize, allow: bool, cx: &mut Context<Self>) {
        let Some(entry) = self.hosts.get_mut(index) else {
            return;
        };
        let pending = entry.clipboard_request.take();
        entry.profile.clipboard_consent = Some(allow);
        if allow && let Some(text) = pending {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
        if let Err(e) = self.store.upsert(entry.profile.clone()) {
            self.banner = Some(format!("Could not save the clipboard choice: {e:#}").into());
        }
        cx.notify();
    }

    pub(crate) fn selected_phase(&self) -> Option<&ConnPhase> {
        self.selected
            .and_then(|i| self.hosts.get(i))
            .map(|h| &h.phase)
    }

    /// Re-evaluated at click time: active/unknown chat work, pending layout
    /// mutations, syncing terminals and unsaved files prevent a restart.
    pub(crate) fn busy(&self, cx: &App) -> bool {
        self.hosts
            .iter()
            .filter_map(|host| host.workspace.as_ref())
            .any(|workspace| workspace.read(cx).update_busy(cx))
    }

    fn on_update_action(&mut self, action: flc_ui::update_banner::Action, cx: &mut Context<Self>) {
        match action {
            flc_ui::update_banner::Action::Restart => {
                if !self.updater.restart_if_idle(self.busy(cx)) {
                    self.banner = Some("The update waits until the current work finishes.".into());
                    cx.notify();
                }
            }
            flc_ui::update_banner::Action::Retry => self.updater.check_now(),
        }
    }

    fn dismiss_update(&mut self, cx: &mut Context<Self>) {
        self.update_dismissed = update_key(&self.update);
        cx.notify();
    }

    /// The non-modal update banner, if the updater has something to say and the
    /// user has not dismissed this state.
    fn render_update_banner(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let banner = crate::updater::banner(&self.update, self.busy(cx))?;
        if let Some(key) = update_key(&self.update)
            && self.update_dismissed.as_deref() == Some(key.as_str())
        {
            return None;
        }
        let theme = cx.theme().clone();
        let view = flc_ui::update_banner::Banner {
            tone: match banner.tone {
                crate::updater::Tone::Info => flc_ui::update_banner::Tone::Info,
                crate::updater::Tone::Success => flc_ui::update_banner::Tone::Success,
                crate::updater::Tone::Danger => flc_ui::update_banner::Tone::Danger,
            },
            title: banner.title.into(),
            detail: banner.detail.into(),
            action: banner.action.map(|action| match action {
                crate::updater::BannerAction::Restart => (
                    flc_ui::update_banner::Action::Restart,
                    SharedString::from("Restart now"),
                ),
                crate::updater::BannerAction::Retry => (
                    flc_ui::update_banner::Action::Retry,
                    SharedString::from("Try again"),
                ),
            }),
            dismissible: banner.dismissible,
        };
        let this = cx.weak_entity();
        let on_action: flc_ui::update_banner::ActionHandler =
            std::rc::Rc::new(move |action, _window, cx| {
                let _ = this.update(cx, |ws, cx| ws.on_update_action(action, cx));
            });
        let this = cx.weak_entity();
        let on_dismiss: flc_ui::update_banner::DismissHandler =
            std::rc::Rc::new(move |_window, cx| {
                let _ = this.update(cx, |ws, cx| ws.dismiss_update(cx));
            });
        Some(flc_ui::update_banner::update_banner(
            "update-banner",
            &view,
            &theme,
            on_action,
            on_dismiss,
        ))
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
        entry.workspace = None;
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
        entry.workspace = None;
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
                    workspace: None,
                    workspace_events: None,
                    clipboard_request: None,
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

/// How [`update_key`] names a state, so a dismissed banner stays dismissed
/// until the updater has something new to say.
fn update_key(state: &UpdateState) -> Option<String> {
    match state {
        UpdateState::Idle | UpdateState::Checking => None,
        UpdateState::Available { version }
        | UpdateState::Downloading { version, .. }
        | UpdateState::Staged { version }
        | UpdateState::Restarting { version } => Some(version.clone()),
        UpdateState::Failed { version, message } => Some(format!(
            "failed:{}:{message}",
            version.as_deref().unwrap_or("")
        )),
    }
}

/// Follow the updater: repaint on every change and quit once the replacement
/// build has been started.
async fn watch_updates(
    this: WeakEntity<Workspace>,
    cx: &mut AsyncApp,
    rx: &mut tokio::sync::watch::Receiver<UpdateState>,
) {
    loop {
        let state = rx.borrow_and_update().clone();
        let restarting = matches!(state, UpdateState::Restarting { .. });
        let alive = this
            .update(cx, |ws, cx| {
                ws.update = state;
                cx.notify();
            })
            .is_ok();
        if !alive {
            return;
        }
        if restarting {
            let _ = this.update(cx, |_, cx| cx.quit());
            return;
        }
        if rx.changed().await.is_err() {
            return;
        }
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
                if let Some(workspace) = &h.workspace {
                    workspace.update(cx, |workspace, cx| workspace.set_rtt(rtt, cx));
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
        if std::mem::take(&mut self.pip_request) {
            self.toggle_pip(window, cx);
        }
        self.open_pending_pip(cx);
        // The visible workspace owns the theme: switch the globals before any
        // child renders with them.
        let active = self.active_workspace_id(cx);
        self.settings
            .update(cx, |settings, cx| settings.set_active(active, cx));
        let theme = cx.theme().clone();
        // Focus is tracked below the title bar on purpose: GPUI's focus
        // transfer marks every mouse-down inside a focusable element as
        // handled, and on Windows a handled non-client button-down never
        // reaches the caption-button and drag logic, leaving minimize,
        // maximize, close and window dragging dead.
        div()
            .id("workspace")
            .key_context("Workspace")
            .on_action(cx.listener(|ws, _: &AddHost, window, cx| ws.open_add_host(window, cx)))
            .on_action(cx.listener(|ws, _: &PaletteGlobal, _, cx| {
                ws.request_palette(palette::Mode::Global, cx)
            }))
            .on_action(cx.listener(|ws, _: &PaletteFiles, _, cx| {
                ws.request_palette(palette::Mode::Files, cx)
            }))
            .on_action(cx.listener(|ws, _: &SettingsOpen, _, cx| ws.request_settings(cx)))
            .on_action(cx.listener(|ws, _: &PipToggle, window, cx| ws.toggle_pip(window, cx)))
            .on_action(cx.listener(|ws, _: &PerformanceToggle, _, cx| ws.toggle_hud(cx)))
            .on_action(
                cx.listener(|ws, _: &GitPrimary, window, cx| ws.run_primary_git(false, window, cx)),
            )
            .on_action(
                cx.listener(|ws, _: &GitAlternate, window, cx| {
                    ws.run_primary_git(true, window, cx)
                }),
            )
            .on_action(cx.listener(|ws, _: &InboxOpen, _, cx| ws.open_inbox(cx)))
            .on_action(
                cx.listener(|ws, _: &FindScrollback, window, cx| ws.find_scrollback(window, cx)),
            )
            .on_action(cx.listener(|ws, _: &RunTarget, _, cx| ws.run_target(cx)))
            .on_action(cx.listener(|ws, _: &NextTab, _, cx| ws.cycle_tab(true, cx)))
            .on_action(cx.listener(|ws, _: &PreviousTab, _, cx| ws.cycle_tab(false, cx)))
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(theme.surface_bg)
            .text_color(theme.text_primary)
            .font_family(theme.font_ui.clone())
            .text_size(theme.text_size)
            .child(self.render_titlebar(window, cx))
            .child(
                div()
                    .id("workspace-body")
                    .track_focus(&self.focus)
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .when_some(self.pip.clone(), |d, pin| {
                        let session = pin
                            .session
                            .map(|session| format!(" · {session}"))
                            .unwrap_or_default();
                        d.child(
                            div()
                                .id("pip-status")
                                .px(px(14.))
                                .py(px(4.))
                                .bg(theme.chrome_bg)
                                .text_size(theme.text_size_sm)
                                .text_color(theme.text_secondary)
                                .child(SharedString::from(format!(
                                    "Picture-in-picture: {} · {}{session}",
                                    pin.kind.label(),
                                    pin.host
                                ))),
                        )
                    })
                    .when_some(self.render_update_banner(cx), |d, banner| d.child(banner))
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
                    ),
            )
            .children(self.render_hover_card(cx))
            .children(self.render_hud(window, cx))
    }
}
