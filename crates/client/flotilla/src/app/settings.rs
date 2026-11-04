//! The settings overlay's state: theme edits, keymap edits and their files.
//!
//! Every edit applies live — the theme is rebuilt into the GPUI global and
//! windows are refreshed, and a rebind is pushed through `install_keymap` —
//! while `Save` is what touches the disk. A file that failed to parse is
//! reported and never overwritten until the user explicitly resets it.
use flc_ui::keymap::{self, Chord, Conflict, Keymap};
use flc_ui::theme_settings::{SurfaceFonts, ThemeSettings};
use gpui::{Context, SharedString};

use crate::settings::{ClientSettings, SettingsStore, WorkspaceSettings};

pub struct Settings {
    store: SettingsStore,
    client: ClientSettings,
    keymap: Keymap,
    /// The daemon workspace id whose theme is showing; `None` is the default.
    active: Option<String>,
    /// No system appearance integration yet: the native client is dark unless
    /// the workspace says otherwise.
    system_dark: bool,
    settings_error: Option<SharedString>,
    keymap_error: Option<SharedString>,
    notes: Vec<SharedString>,
    /// The file could not be read; saving is refused until an explicit reset.
    settings_unreadable: bool,
    keymap_unreadable: bool,
}

impl Settings {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let store = SettingsStore::default_dir()
            .map(SettingsStore::new)
            .unwrap_or_else(|| SettingsStore::new(std::env::temp_dir().join("Flotilla")));
        let (client, settings_error) = match store.load_settings() {
            Ok(client) => (client, None),
            Err(error) => (ClientSettings::default(), Some(format!("{error:#}"))),
        };
        let (keymap, notes, keymap_error) = match store.load_keymap() {
            Ok(Some((keymap, load_notes))) => (keymap, load_notes, None),
            Ok(None) => (Keymap::defaults(), Vec::new(), None),
            Err(error) => (Keymap::defaults(), Vec::new(), Some(format!("{error:#}"))),
        };
        let mut settings = Self {
            store,
            client,
            keymap,
            active: None,
            system_dark: true,
            settings_unreadable: settings_error.is_some(),
            settings_error: settings_error.map(SharedString::from),
            keymap_unreadable: keymap_error.is_some(),
            keymap_error: keymap_error.map(SharedString::from),
            notes: notes.into_iter().map(SharedString::from).collect(),
        };
        settings.apply_theme(cx);
        settings
    }

    // ---- reads ---------------------------------------------------------------

    pub fn client(&self) -> &ClientSettings {
        &self.client
    }

    pub fn keymap(&self) -> &Keymap {
        &self.keymap
    }

    pub fn settings_error(&self) -> Option<&SharedString> {
        self.settings_error.as_ref()
    }

    pub fn keymap_error(&self) -> Option<&SharedString> {
        self.keymap_error.as_ref()
    }

    pub fn notes(&self) -> &[SharedString] {
        &self.notes
    }

    /// True when `settings.json` exists but could not be read.
    pub fn file_unreadable(&self) -> bool {
        self.settings_unreadable
    }

    pub fn store_dir(&self) -> &std::path::Path {
        self.store.dir()
    }

    /// Settings of the workspace currently on screen.
    pub fn current(&self) -> &WorkspaceSettings {
        self.client.for_workspace(self.active.as_deref())
    }

    // ---- live application ----------------------------------------------------

    /// Follow the visible workspace; its theme becomes the global one.
    pub fn set_active(&mut self, workspace: Option<String>, cx: &mut Context<Self>) {
        if self.active == workspace {
            return;
        }
        self.active = workspace;
        self.apply_theme(cx);
    }

    /// Rebuild the theme tokens from the active workspace's settings.
    pub fn apply_theme(&mut self, cx: &mut Context<Self>) {
        let settings = self.client.for_workspace(self.active.as_deref());
        let theme = settings.theme.resolve(&self.client.fonts, self.system_dark);
        cx.set_global(theme);
        cx.refresh_windows();
        cx.notify();
    }

    /// Push the current keymap into GPUI, replacing the previous bindings.
    pub fn apply_keymap(&self, cx: &mut Context<Self>) {
        crate::app::install_keymap(&self.keymap, cx);
    }

    // ---- theme edits ---------------------------------------------------------

    pub fn edit_theme(&mut self, f: impl FnOnce(&mut ThemeSettings), cx: &mut Context<Self>) {
        let active = self.active.clone();
        f(&mut self.client.workspace_mut(active.as_deref()).theme);
        self.client.sanitize();
        self.apply_theme(cx);
    }

    pub fn edit_fonts(&mut self, f: impl FnOnce(&mut SurfaceFonts), cx: &mut Context<Self>) {
        f(&mut self.client.fonts);
        self.client.sanitize();
        self.apply_theme(cx);
    }

    /// The desktop notification policy (spec §8.4).
    pub fn notifications(&self) -> &flc_ui::notify::NotificationSettings {
        &self.client.notifications
    }

    pub fn edit_notifications(
        &mut self,
        f: impl FnOnce(&mut flc_ui::notify::NotificationSettings),
        cx: &mut Context<Self>,
    ) {
        f(&mut self.client.notifications);
        cx.notify();
    }

    pub fn edit_terminal(
        &mut self,
        f: impl FnOnce(&mut crate::settings::TerminalPaletteSettings),
        cx: &mut Context<Self>,
    ) {
        let active = self.active.clone();
        f(&mut self.client.workspace_mut(active.as_deref()).terminal);
        self.apply_theme(cx);
    }

    /// Restore this workspace's theme to the shipped defaults.
    pub fn reset_theme(&mut self, cx: &mut Context<Self>) {
        let active = self.active.clone();
        let settings = self.client.workspace_mut(active.as_deref());
        settings.theme = ThemeSettings::default();
        settings.terminal = Default::default();
        self.apply_theme(cx);
    }

    /// Make this workspace's settings the default for workspaces without one.
    pub fn set_as_default(&mut self, cx: &mut Context<Self>) {
        let current = self.current().clone();
        self.client.copy_from(None, &current);
        self.apply_theme(cx);
    }

    /// Copy another workspace's whole theme (and terminal palette) here.
    pub fn copy_from_workspace(&mut self, source: &str, cx: &mut Context<Self>) {
        let Some(settings) = self.client.workspaces.get(source).cloned() else {
            self.notes = vec![SharedString::from(format!(
                "No saved settings for workspace {source}."
            ))];
            return;
        };
        let active = self.active.clone();
        self.client.copy_from(active.as_deref(), &settings);
        self.apply_theme(cx);
    }

    /// Workspace ids that have saved settings, for the copy-from list.
    pub fn saved_workspaces(&self) -> Vec<(String, String)> {
        self.client
            .workspaces
            .iter()
            .map(|(id, settings)| {
                let short = id.chars().take(8).collect::<String>();
                let label = format!("{short} · {}", settings.theme.mode.label());
                (id.clone(), label)
            })
            .collect()
    }

    // ---- keymap edits --------------------------------------------------------

    /// Bind `chord` to `id` if it conflicts with nothing. Returns the
    /// explanation when it is refused; nothing is changed in that case.
    pub fn bind(&mut self, id: &str, chord: Chord, cx: &mut Context<Self>) -> Result<(), Conflict> {
        if let Some(conflict) = self.keymap.check(id, &chord, terminal_owned) {
            return Err(conflict);
        }
        self.keymap.set(id, chord);
        self.keymap_error = None;
        self.apply_keymap(cx);
        cx.notify();
        Ok(())
    }

    pub fn reset_binding(&mut self, id: &str, cx: &mut Context<Self>) {
        self.keymap.reset(id);
        self.apply_keymap(cx);
        cx.notify();
    }

    pub fn reset_keymap(&mut self, cx: &mut Context<Self>) {
        self.keymap.reset_all();
        self.keymap_unreadable = false;
        self.keymap_error = None;
        self.apply_keymap(cx);
        cx.notify();
    }

    // ---- persistence ---------------------------------------------------------

    pub fn save(&mut self, cx: &mut Context<Self>) {
        if self.settings_unreadable {
            self.settings_error = Some(SharedString::from(
                "settings.json is unreadable; fix it or press Reset to replace it.",
            ));
            cx.notify();
            return;
        }
        self.client.sanitize();
        match self.store.save_settings(&self.client) {
            Ok(()) => {
                self.settings_error = None;
                self.notes = vec![SharedString::from(format!(
                    "Saved {}",
                    self.store.settings_path().display()
                ))];
            }
            Err(error) => self.settings_error = Some(SharedString::from(format!("{error:#}"))),
        }
        cx.notify();
    }

    pub fn save_keymap(&mut self, cx: &mut Context<Self>) {
        if self.keymap_unreadable {
            self.keymap_error = Some(SharedString::from(
                "keymap.json is unreadable; fix it or press Reset to replace it.",
            ));
            cx.notify();
            return;
        }
        match self.store.save_keymap(&self.keymap) {
            Ok(()) => {
                self.keymap_error = None;
                self.notes = vec![SharedString::from(format!(
                    "Saved {}",
                    self.store.keymap_path().display()
                ))];
            }
            Err(error) => self.keymap_error = Some(SharedString::from(format!("{error:#}"))),
        }
        cx.notify();
    }

    /// Explicitly replace an unreadable settings file with the current values.
    pub fn reset_file(&mut self, cx: &mut Context<Self>) {
        self.settings_unreadable = false;
        self.settings_error = None;
        self.save(cx);
    }
}

/// Chords the focused terminal consumes before any application binding.
fn terminal_owned(chord: &Chord) -> bool {
    let stroke = flc_terminal::keys::Stroke {
        key: &chord.key,
        key_char: None,
        modifiers: flc_terminal::keys::StrokeModifiers {
            control: chord.control,
            alt: chord.alt,
            shift: chord.shift,
            platform: chord.platform,
        },
    };
    flc_terminal::keys::is_terminal_owned(&stroke)
}

/// The read-only surface rows the editor lists under the actions.
pub fn surface_rows() -> &'static [keymap::SurfaceBinding] {
    keymap::SURFACE_BINDINGS
}

impl super::Workspace {
    /// The daemon workspace whose theme the visible scope belongs to.
    pub(super) fn active_workspace_id(&self, cx: &gpui::App) -> Option<String> {
        let workspace = self
            .selected
            .and_then(|index| self.hosts.get(index))
            .and_then(|host| host.workspace.clone())?;
        workspace
            .read(cx)
            .active_workspace()
            .map(|id| id.to_string())
    }

    /// Open the settings overlay for the selected host on the next frame; the
    /// overlay needs a window to build its text inputs.
    pub(super) fn request_settings(&mut self, cx: &mut Context<Self>) {
        let Some(host) = self
            .selected
            .and_then(|index| self.hosts.get(index))
            .and_then(|host| host.workspace.clone())
        else {
            self.banner = Some(SharedString::from(
                "Connect to a host before editing its workspace settings.",
            ));
            cx.notify();
            return;
        };
        host.update(cx, |host, cx| host.request_settings(cx));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flc_ui::keymap::{ConflictKind, Group};

    #[test]
    fn the_terminal_ownership_bridge_refuses_plain_ctrl_letters() {
        for (chord, expected) in [
            ("ctrl-k", true),
            ("ctrl-shift-k", false),
            ("ctrl-1", false),
            ("alt-f4", false),
        ] {
            assert_eq!(
                terminal_owned(&Chord::parse(chord).unwrap()),
                expected,
                "{chord}"
            );
        }
    }

    #[test]
    fn the_pip_chord_is_reserved_for_pip_alone() {
        let keymap = Keymap::defaults();
        let action = keymap::ACTIONS
            .iter()
            .find(|action| action.id == "pip.toggle")
            .unwrap();
        assert_eq!(action.group, Group::Navigation);
        // The palette no longer answers it.
        assert_eq!(
            keymap.chord("palette.global").unwrap().canonical(),
            "ctrl-shift-k"
        );
        // And it is refused for any other action.
        let conflict = keymap
            .check(
                "settings.open",
                &Chord::parse("ctrl-shift-p").unwrap(),
                terminal_owned,
            )
            .unwrap();
        assert_eq!(conflict.kind, ConflictKind::OtherAction);
    }
}
