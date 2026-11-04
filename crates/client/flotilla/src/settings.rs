//! Local client settings: `settings.json` and `keymap.json` under the config
//! directory that holds `hosts.json`.
//!
//! Reading is tolerant (a missing file is the defaults). A corrupt file is
//! reported and left untouched; saving refuses to replace it until the user
//! explicitly resets, so a typo can never destroy the file silently.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use flc_terminal::default_ansi;
use flc_ui::keymap::Keymap;
use flc_ui::theme_settings::{SurfaceFonts, ThemeSettings};
use serde::{Deserialize, Serialize};

pub const SETTINGS_VERSION: u32 = 1;

/// An explicit override of the terminal's own palette. Rendering keeps using
/// the engine's 269 colors unless `override_engine` is set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalPaletteSettings {
    pub override_engine: bool,
    /// The 16 ANSI colors; the cube, greys and named defaults stay the
    /// engine's.
    pub ansi: [u32; 16],
}

impl Default for TerminalPaletteSettings {
    fn default() -> Self {
        Self {
            override_engine: false,
            ansi: default_ansi(),
        }
    }
}

impl TerminalPaletteSettings {
    /// The full rendering palette, or `None` when the engine's is in use.
    pub fn palette(&self) -> Option<Vec<u32>> {
        self.override_engine
            .then(|| flc_terminal::palette_with_ansi(&self.ansi))
    }
}

/// Everything the theme editor edits for one workspace.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceSettings {
    pub theme: ThemeSettings,
    pub terminal: TerminalPaletteSettings,
}

impl WorkspaceSettings {
    pub fn sanitize(&mut self) {
        self.theme.sanitize();
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientSettings {
    pub version: u32,
    /// Used by workspaces without an entry of their own.
    pub default: WorkspaceSettings,
    /// Per-workspace overrides, keyed by the daemon's workspace id.
    pub workspaces: BTreeMap<String, WorkspaceSettings>,
    pub fonts: SurfaceFonts,
    /// Desktop notification policy (spec §8.4); in-app toasts are unaffected.
    pub notifications: flc_ui::notify::NotificationSettings,
}

impl Default for ClientSettings {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            default: WorkspaceSettings::default(),
            workspaces: BTreeMap::new(),
            fonts: SurfaceFonts::default(),
            notifications: flc_ui::notify::NotificationSettings::default(),
        }
    }
}

impl ClientSettings {
    pub fn for_workspace(&self, workspace: Option<&str>) -> &WorkspaceSettings {
        workspace
            .and_then(|id| self.workspaces.get(id))
            .unwrap_or(&self.default)
    }

    pub fn workspace_mut(&mut self, workspace: Option<&str>) -> &mut WorkspaceSettings {
        match workspace {
            Some(id) => self
                .workspaces
                .entry(id.to_string())
                .or_insert_with(|| self.default.clone()),
            None => &mut self.default,
        }
    }

    pub fn sanitize(&mut self) {
        self.default.sanitize();
        for settings in self.workspaces.values_mut() {
            settings.sanitize();
        }
        self.fonts.sanitize();
    }

    /// Give this workspace the settings currently in `source`.
    pub fn copy_from(&mut self, workspace: Option<&str>, source: &WorkspaceSettings) {
        let mut copied = source.clone();
        copied.sanitize();
        *self.workspace_mut(workspace) = copied;
    }
}

/// The directory that holds `hosts.json`, `settings.json` and `keymap.json`.
#[derive(Debug, Clone)]
pub struct SettingsStore {
    dir: PathBuf,
}

impl SettingsStore {
    pub fn default_dir() -> Option<PathBuf> {
        dirs::config_dir().map(|dir| dir.join("Flotilla"))
    }

    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn settings_path(&self) -> PathBuf {
        self.dir.join("settings.json")
    }

    pub fn keymap_path(&self) -> PathBuf {
        self.dir.join("keymap.json")
    }

    pub fn load_settings(&self) -> Result<ClientSettings> {
        let path = self.settings_path();
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ClientSettings::default());
            }
            Err(error) => {
                return Err(error).with_context(|| format!("cannot read {}", path.display()));
            }
        };
        let mut settings: ClientSettings = serde_json::from_slice(&bytes)
            .with_context(|| format!("{} is corrupt; fix or delete it", path.display()))?;
        settings.sanitize();
        Ok(settings)
    }

    pub fn save_settings(&self, settings: &ClientSettings) -> Result<()> {
        write_atomic(&self.settings_path(), &serde_json::to_vec_pretty(settings)?)
    }

    /// `None` when there is no file yet: the caller keeps the defaults and
    /// writes on the first save.
    pub fn load_keymap(&self) -> Result<Option<(Keymap, Vec<String>)>> {
        let path = self.keymap_path();
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("cannot read {}", path.display()));
            }
        };
        let text = std::str::from_utf8(&bytes)
            .with_context(|| format!("{} is not UTF-8", path.display()))?;
        let (keymap, notes) = Keymap::from_json(text)
            .map_err(|error| anyhow::anyhow!("{} is corrupt: {error}", path.display()))?;
        Ok(Some((keymap, notes)))
    }

    pub fn save_keymap(&self, keymap: &Keymap) -> Result<()> {
        write_atomic(&self.keymap_path(), keymap.to_json().as_bytes())
    }
}

/// Write beside the target and rename, so a crash never leaves a half file.
fn write_atomic(path: &Path, body: &[u8]) -> Result<()> {
    let dir = path.parent().context("settings path has no parent")?;
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body).with_context(|| format!("cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("cannot replace {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flc_ui::theme_settings::{Mode, Palette};

    fn store(name: &str) -> SettingsStore {
        let dir = std::env::temp_dir().join(format!(
            "flotilla-settings-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        SettingsStore::new(dir)
    }

    #[test]
    fn a_missing_file_is_the_default_settings() {
        let store = store("missing");
        assert_eq!(store.load_settings().unwrap(), ClientSettings::default());
        assert!(store.load_keymap().unwrap().is_none());
        assert!(!store.settings_path().exists());
    }

    #[test]
    fn saving_writes_atomically_and_round_trips() {
        let store = store("roundtrip");
        let mut settings = ClientSettings::default();
        settings.default.theme.mode = Mode::Light;
        settings.workspace_mut(Some("ws-1")).theme.dark.accent = 0xFF0000;
        settings.fonts.terminal_size = 15.0;
        store.save_settings(&settings).unwrap();
        assert_eq!(store.load_settings().unwrap(), settings);
        assert!(!store.settings_path().with_extension("json.tmp").exists());

        let mut keymap = Keymap::defaults();
        keymap.set(
            "palette.global",
            flc_ui::keymap::Chord::parse("ctrl-shift-m").unwrap(),
        );
        store.save_keymap(&keymap).unwrap();
        let (loaded, notes) = store.load_keymap().unwrap().unwrap();
        assert!(notes.is_empty());
        assert_eq!(loaded, keymap);
    }

    #[test]
    fn a_corrupt_settings_file_is_reported_not_replaced() {
        let store = store("corrupt");
        std::fs::create_dir_all(store.dir()).unwrap();
        std::fs::write(store.settings_path(), "{ this is not json").unwrap();
        let error = store.load_settings().unwrap_err().to_string();
        assert!(error.contains("corrupt"), "{error}");
        // The bytes are untouched: only an explicit save writes.
        assert_eq!(
            std::fs::read_to_string(store.settings_path()).unwrap(),
            "{ this is not json"
        );
    }

    #[test]
    fn a_corrupt_keymap_is_reported_not_replaced() {
        let store = store("keymap-corrupt");
        std::fs::create_dir_all(store.dir()).unwrap();
        std::fs::write(store.keymap_path(), "nope").unwrap();
        let error = store.load_keymap().unwrap_err().to_string();
        assert!(error.contains("corrupt"), "{error}");
        assert_eq!(
            std::fs::read_to_string(store.keymap_path()).unwrap(),
            "nope"
        );
    }

    #[test]
    fn per_workspace_settings_fall_back_to_the_default_then_override() {
        let mut settings = ClientSettings::default();
        settings.default.theme.dark = Palette::dark_default();
        assert_eq!(
            settings.for_workspace(Some("unknown")),
            &settings.default,
            "an unknown workspace sees the default"
        );
        settings.workspace_mut(Some("ws-1")).theme.dark.accent = 0x00FF00;
        assert_eq!(
            settings.for_workspace(Some("ws-1")).theme.dark.accent,
            0x00FF00
        );
        assert_eq!(
            settings.for_workspace(Some("ws-2")).theme.dark.accent,
            settings.default.theme.dark.accent
        );
        assert_eq!(settings.for_workspace(None), &settings.default);
    }

    #[test]
    fn copying_from_another_workspace_keeps_them_independent() {
        let mut settings = ClientSettings::default();
        settings.workspace_mut(Some("a")).theme.dark.background = 0x112233;
        let source = settings.for_workspace(Some("a")).clone();
        settings.copy_from(Some("b"), &source);
        assert_eq!(
            settings.for_workspace(Some("b")).theme.dark.background,
            0x112233
        );
        settings.workspace_mut(Some("b")).theme.dark.background = 0x445566;
        assert_eq!(
            settings.for_workspace(Some("a")).theme.dark.background,
            0x112233
        );
    }

    #[test]
    fn the_terminal_palette_override_is_explicit() {
        let mut terminal = TerminalPaletteSettings::default();
        assert_eq!(
            terminal.palette(),
            None,
            "the engine palette is the default"
        );
        terminal.override_engine = true;
        let palette = terminal.palette().expect("override");
        assert_eq!(palette.len(), 269, "the engine palette is 269 colors");
        assert_eq!(&palette[..16], &default_ansi());
    }

    #[test]
    fn sliders_and_fonts_are_clamped_on_load() {
        let store = store("clamped");
        std::fs::create_dir_all(store.dir()).unwrap();
        std::fs::write(
            store.settings_path(),
            r#"{"version":1,"default":{"theme":{"dark":{"background_alpha":9.0,"background_intensity":-3.0}}},"fonts":{"terminal_size":400.0,"chat_scale":9.0}}"#,
        )
        .unwrap();
        let settings = store.load_settings().unwrap();
        assert_eq!(settings.default.theme.dark.background_alpha, 1.0);
        assert_eq!(settings.default.theme.dark.background_intensity, 0.0);
        assert_eq!(settings.fonts.terminal_size, 28.0);
        assert_eq!(settings.fonts.chat_scale, 2.0);
    }
}
