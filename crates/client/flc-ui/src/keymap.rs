//! Effective key bindings, their file format and conflict detection (§8.3).
//!
//! The window-free half of the keymap editor: chord parsing and canonical
//! spelling, the table of actions the client actually dispatches, surface-local
//! handlers a binding can collide with, and the checks a rebind must pass.
//! Applying a [`Keymap`] to GPUI and persisting `keymap.json` live in the
//! client; nothing here needs a window.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Where an action is dispatched from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    /// Window and connection actions that are not tied to a workspace.
    Application,
    /// Overlays and navigation.
    Navigation,
    /// Reserved for a later slice; listed so its chord cannot be stolen.
    Reserved,
}

impl Group {
    pub fn label(self) -> &'static str {
        match self {
            Group::Application => "Application",
            Group::Navigation => "Navigation",
            Group::Reserved => "Reserved",
        }
    }
}

/// One rebindable action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionSpec {
    pub id: &'static str,
    pub label: &'static str,
    pub group: Group,
    pub default: &'static str,
    /// Shown under the row (e.g. why the key is reserved).
    pub note: &'static str,
}

/// Every action the client binds. Keep in step with `app::install_keymap`.
pub const ACTIONS: &[ActionSpec] = &[
    ActionSpec {
        id: "app.quit",
        label: "Quit Flotilla",
        group: Group::Application,
        default: "alt-f4",
        note: "Also closes the window on Windows.",
    },
    ActionSpec {
        id: "host.add",
        label: "Add host",
        group: Group::Application,
        default: "ctrl-shift-n",
        note: "",
    },
    ActionSpec {
        id: "palette.global",
        label: "Command palette",
        group: Group::Navigation,
        default: "ctrl-shift-k",
        note: "Spec §8.3: Global; Ctrl+1…6 switch modes inside.",
    },
    ActionSpec {
        id: "palette.files",
        label: "Palette: Files",
        group: Group::Navigation,
        default: "ctrl-shift-o",
        note: "",
    },
    ActionSpec {
        id: "settings.open",
        label: "Settings and keymap",
        group: Group::Navigation,
        default: "ctrl-,",
        note: "Theme editor and keyboard editor.",
    },
    ActionSpec {
        id: "performance.toggle",
        label: "Performance HUD",
        group: Group::Navigation,
        default: "ctrl-alt-shift-p",
        note: "Live frame cadence, terminal output rate, connection and resident memory.",
    },
    ActionSpec {
        id: "pip.toggle",
        label: "Picture-in-picture",
        group: Group::Reserved,
        default: "ctrl-shift-p",
        note: "Reserved by spec §8.3; the PiP window lands in its own slice.",
    },
];

/// A handler a binding can shadow: these fire only when their surface has
/// focus, so an application action on the same chord silently loses there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceBinding {
    pub chord: &'static str,
    pub surface: &'static str,
    pub action: &'static str,
}

pub const SURFACE_BINDINGS: &[SurfaceBinding] = &[
    SurfaceBinding {
        chord: "backspace",
        surface: "Text input",
        action: "Delete backwards",
    },
    SurfaceBinding {
        chord: "delete",
        surface: "Text input",
        action: "Delete forwards",
    },
    SurfaceBinding {
        chord: "left",
        surface: "Text input",
        action: "Move left",
    },
    SurfaceBinding {
        chord: "right",
        surface: "Text input",
        action: "Move right",
    },
    SurfaceBinding {
        chord: "shift-left",
        surface: "Text input",
        action: "Select left",
    },
    SurfaceBinding {
        chord: "shift-right",
        surface: "Text input",
        action: "Select right",
    },
    SurfaceBinding {
        chord: "home",
        surface: "Text input",
        action: "Line start",
    },
    SurfaceBinding {
        chord: "end",
        surface: "Text input",
        action: "Line end",
    },
    SurfaceBinding {
        chord: "ctrl-a",
        surface: "Text input",
        action: "Select all",
    },
    SurfaceBinding {
        chord: "ctrl-c",
        surface: "Text input",
        action: "Copy",
    },
    SurfaceBinding {
        chord: "ctrl-v",
        surface: "Text input",
        action: "Paste",
    },
    SurfaceBinding {
        chord: "ctrl-x",
        surface: "Text input",
        action: "Cut",
    },
    SurfaceBinding {
        chord: "enter",
        surface: "Text input",
        action: "Submit",
    },
    SurfaceBinding {
        chord: "escape",
        surface: "Text input",
        action: "Cancel",
    },
    SurfaceBinding {
        chord: "ctrl-1",
        surface: "Palette",
        action: "Global mode",
    },
    SurfaceBinding {
        chord: "ctrl-2",
        surface: "Palette",
        action: "Commands mode",
    },
    SurfaceBinding {
        chord: "ctrl-3",
        surface: "Palette",
        action: "Files mode",
    },
    SurfaceBinding {
        chord: "ctrl-4",
        surface: "Palette",
        action: "Worktrees mode",
    },
    SurfaceBinding {
        chord: "ctrl-5",
        surface: "Palette",
        action: "Tabs mode",
    },
    SurfaceBinding {
        chord: "ctrl-6",
        surface: "Palette",
        action: "Conversations mode",
    },
    SurfaceBinding {
        chord: "tab",
        surface: "Palette",
        action: "Next mode",
    },
    SurfaceBinding {
        chord: "shift-tab",
        surface: "Palette",
        action: "Previous mode",
    },
    SurfaceBinding {
        chord: "up",
        surface: "Palette",
        action: "Previous row",
    },
    SurfaceBinding {
        chord: "down",
        surface: "Palette",
        action: "Next row",
    },
    SurfaceBinding {
        chord: "enter",
        surface: "Palette",
        action: "Run the row",
    },
    SurfaceBinding {
        chord: "escape",
        surface: "Palette",
        action: "Close",
    },
    SurfaceBinding {
        chord: "ctrl-s",
        surface: "Editor",
        action: "Save",
    },
    SurfaceBinding {
        chord: "ctrl-f",
        surface: "Editor",
        action: "Find",
    },
    SurfaceBinding {
        chord: "ctrl-z",
        surface: "Editor",
        action: "Undo",
    },
    SurfaceBinding {
        chord: "ctrl-shift-z",
        surface: "Editor",
        action: "Redo",
    },
    SurfaceBinding {
        chord: "ctrl-y",
        surface: "Editor",
        action: "Redo",
    },
];

/// A parsed chord. `key` is GPUI's key name: `a`, `1`, `f4`, `,`, `left`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Chord {
    pub control: bool,
    pub shift: bool,
    pub alt: bool,
    pub platform: bool,
    pub key: String,
}

impl Chord {
    pub fn new(
        key: impl Into<String>,
        control: bool,
        shift: bool,
        alt: bool,
        platform: bool,
    ) -> Self {
        Self {
            control,
            shift,
            alt,
            platform,
            key: key.into(),
        }
    }

    /// Parse `ctrl-shift-k`, `alt-f4`, `ctrl-,`. Modifier order is free; the
    /// key is the last component.
    pub fn parse(source: &str) -> Result<Self, String> {
        let source = source.trim();
        if source.is_empty() {
            return Err("A binding cannot be empty.".into());
        }
        let mut chord = Chord::new("", false, false, false, false);
        let components: Vec<&str> = source.split('-').collect();
        let (key, modifiers) = match components.split_last() {
            Some((key, modifiers)) if !key.is_empty() => (key.to_string(), modifiers),
            _ => return Err(format!("`{source}` does not name a key.")),
        };
        for component in modifiers {
            match component.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => chord.control = true,
                "shift" => chord.shift = true,
                "alt" | "option" => chord.alt = true,
                "cmd" | "super" | "win" | "platform" => chord.platform = true,
                other => {
                    return Err(format!("Unknown modifier `{other}` in `{source}`."));
                }
            }
        }
        if key.len() > 12 {
            return Err(format!("`{source}` is not a key name."));
        }
        chord.key = key.to_ascii_lowercase();
        if chord.key.chars().all(|c| c.is_ascii_whitespace()) {
            return Err(format!("`{source}` does not name a key."));
        }
        Ok(chord)
    }

    /// The spelling written to `keymap.json` and passed to GPUI.
    pub fn canonical(&self) -> String {
        let mut out = String::new();
        for (set, name) in [
            (self.control, "ctrl"),
            (self.alt, "alt"),
            (self.shift, "shift"),
            (self.platform, "cmd"),
        ] {
            if set {
                out.push_str(name);
                out.push('-');
            }
        }
        out.push_str(&self.key);
        out
    }

    /// Upper-case spelling for the editor.
    pub fn label(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if self.control {
            parts.push("Ctrl");
        }
        if self.alt {
            parts.push("Alt");
        }
        if self.shift {
            parts.push("Shift");
        }
        if self.platform {
            parts.push("Super");
        }
        let key = match self.key.as_str() {
            "left" => "←".to_string(),
            "right" => "→".to_string(),
            "up" => "↑".to_string(),
            "down" => "↓".to_string(),
            "enter" => "Enter".to_string(),
            "escape" => "Esc".to_string(),
            "tab" => "Tab".to_string(),
            "backspace" => "Backspace".to_string(),
            "delete" => "Delete".to_string(),
            "space" => "Space".to_string(),
            other => other.to_uppercase(),
        };
        parts.push(&key);
        parts.join("+")
    }

    pub fn has_modifier(&self) -> bool {
        self.control || self.alt || self.shift || self.platform
    }
}

/// Why a candidate binding was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictKind {
    /// No modifier at all: the focused surface (terminal, editor, text input)
    /// would consume it as typing or a key sequence.
    Unmodified,
    /// Plain Ctrl+letter (or an otherwise terminal-owned chord): the terminal
    /// consumes it before any application binding.
    Terminal,
    /// Another action already uses the chord.
    OtherAction,
    /// A focused surface handles the chord itself.
    Surface,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub kind: ConflictKind,
    pub chord: String,
    /// Human-readable explanation, shown in the editor.
    pub message: String,
}

/// The editor's row for one action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub id: &'static str,
    pub label: &'static str,
    pub group: Group,
    pub note: &'static str,
    pub chord: String,
    pub chord_label: String,
    /// True when the chord differs from the shipped default.
    pub custom: bool,
}

/// The action table plus its current chords.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keymap {
    bindings: BTreeMap<&'static str, Chord>,
}

fn spec(id: &str) -> Option<&'static ActionSpec> {
    ACTIONS.iter().find(|action| action.id == id)
}

impl Default for Keymap {
    fn default() -> Self {
        Self::defaults()
    }
}

impl Keymap {
    pub fn defaults() -> Self {
        Self {
            bindings: ACTIONS
                .iter()
                .map(|action| {
                    let chord =
                        Chord::parse(action.default).expect("shipped keymap defaults must parse");
                    (action.id, chord)
                })
                .collect(),
        }
    }

    pub fn chord(&self, id: &str) -> Option<&Chord> {
        self.bindings.get(id)
    }

    pub fn set(&mut self, id: &str, chord: Chord) -> bool {
        let Some(slot) = self.bindings.get_mut(id) else {
            return false;
        };
        *slot = chord;
        true
    }

    pub fn reset(&mut self, id: &str) -> bool {
        let Some(action) = spec(id) else {
            return false;
        };
        let Ok(chord) = Chord::parse(action.default) else {
            return false;
        };
        self.set(id, chord)
    }

    pub fn reset_all(&mut self) {
        *self = Self::defaults();
    }

    /// Rows for the editor, in table order.
    pub fn rows(&self) -> Vec<Row> {
        ACTIONS
            .iter()
            .filter_map(|action| {
                let chord = self.bindings.get(action.id)?;
                Some(Row {
                    id: action.id,
                    label: action.label,
                    group: action.group,
                    note: action.note,
                    chord: chord.canonical(),
                    chord_label: chord.label(),
                    custom: chord.canonical() != action.default,
                })
            })
            .collect()
    }

    /// The binding written to the file, in a stable order.
    pub fn to_file(&self) -> KeymapFile {
        KeymapFile {
            version: 1,
            bindings: self
                .bindings
                .iter()
                .map(|(id, chord)| ((*id).to_string(), chord.canonical()))
                .collect(),
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(&self.to_file())
            .unwrap_or_else(|_| "{\"version\":1,\"bindings\":{}}".into())
    }

    /// Load a file, keeping defaults for anything missing, unknown or
    /// unparseable. The notes explain every dropped entry so the caller can
    /// surface them instead of silently overwriting the file.
    pub fn from_json(text: &str) -> Result<(Self, Vec<String>), String> {
        let file: KeymapFile =
            serde_json::from_str(text).map_err(|e| format!("keymap.json: {e}"))?;
        let mut keymap = Self::defaults();
        let mut notes = Vec::new();
        for (id, value) in file.bindings {
            let Some(action) = spec(&id) else {
                notes.push(format!("Ignored unknown action `{id}`."));
                continue;
            };
            match Chord::parse(&value) {
                Ok(chord) => {
                    keymap.set(&id, chord);
                }
                Err(error) => notes.push(format!("{}: {error}", action.label)),
            }
        }
        Ok((keymap, notes))
    }

    /// Check a candidate for `id` without changing anything; `None` accepts.
    ///
    /// `terminal_owned` reports chords the focused terminal consumes before any
    /// application binding (plain Ctrl+letter): those are never accepted.
    pub fn check(
        &self,
        id: &str,
        chord: &Chord,
        terminal_owned: impl Fn(&Chord) -> bool,
    ) -> Option<Conflict> {
        if !chord.has_modifier() {
            return Some(Conflict {
                kind: ConflictKind::Unmodified,
                chord: chord.canonical(),
                message: format!(
                    "{} has no modifier: the focused surface would consume it. Use Ctrl, Alt or Shift with another key.",
                    chord.label()
                ),
            });
        }
        if terminal_owned(chord) {
            return Some(Conflict {
                kind: ConflictKind::Terminal,
                chord: chord.canonical(),
                message: format!(
                    "{} is sent to the terminal while it has focus (spec §8.3: the terminal comes first).",
                    chord.label()
                ),
            });
        }
        for (other, bound) in &self.bindings {
            if *other != id && bound == chord {
                let label = spec(other).map(|s| s.label).unwrap_or(other);
                return Some(Conflict {
                    kind: ConflictKind::OtherAction,
                    chord: chord.canonical(),
                    message: format!("{} is already bound to “{label}”.", chord.label()),
                });
            }
        }
        if let Some(surface) = SURFACE_BINDINGS
            .iter()
            .find(|binding| Chord::parse(binding.chord).ok().as_ref() == Some(chord))
        {
            return Some(Conflict {
                kind: ConflictKind::Surface,
                chord: chord.canonical(),
                message: format!(
                    "{} is handled by {} ({}) while it has focus.",
                    chord.label(),
                    surface.surface,
                    surface.action
                ),
            });
        }
        None
    }

    /// Every chord used by an action, for a duplicate scan at load time.
    pub fn duplicates(&self) -> Vec<(Chord, Vec<&'static str>)> {
        let mut seen: BTreeMap<&Chord, Vec<&'static str>> = BTreeMap::new();
        for (id, chord) in &self.bindings {
            seen.entry(chord).or_default().push(id);
        }
        seen.into_iter()
            .filter(|(_, ids)| ids.len() > 1)
            .map(|(chord, ids)| (chord.clone(), ids))
            .collect()
    }
}

/// `keymap.json` on disk.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeymapFile {
    pub version: u32,
    #[serde(default)]
    pub bindings: BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terminal_owned(chord: &Chord) -> bool {
        chord.control
            && !chord.alt
            && !chord.shift
            && !chord.platform
            && chord.key.len() == 1
            && chord.key.chars().all(|c| c.is_ascii_alphabetic())
    }

    #[test]
    fn chords_parse_and_keep_a_canonical_spelling() {
        let chord = Chord::parse("SHIFT-ctrl-k").unwrap();
        assert_eq!(chord.canonical(), "ctrl-shift-k");
        assert_eq!(chord.label(), "Ctrl+Shift+K");
        let comma = Chord::parse("ctrl-,").unwrap();
        assert_eq!(comma.canonical(), "ctrl-,");
        assert_eq!(comma.label(), "Ctrl+,");
        let alt = Chord::parse("alt-f4").unwrap();
        assert_eq!((alt.alt, alt.key.as_str()), (true, "f4"));
        assert_eq!(Chord::parse("escape").unwrap().label(), "Esc");
        for bad in ["", "ctrl-", "hyper-k", "ctrl-shift-"] {
            assert!(Chord::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_shipped_defaults_are_unique_and_pass_their_own_check() {
        let keymap = Keymap::defaults();
        assert!(keymap.duplicates().is_empty(), "{:?}", keymap.duplicates());
        for action in ACTIONS {
            let chord = keymap.chord(action.id).unwrap().clone();
            assert_eq!(
                keymap.check(action.id, &chord, terminal_owned),
                None,
                "{}",
                action.id
            );
        }
    }

    #[test]
    fn a_plain_ctrl_letter_is_refused_because_the_terminal_owns_it() {
        let keymap = Keymap::defaults();
        let chord = Chord::parse("ctrl-p").unwrap();
        let conflict = keymap
            .check("palette.global", &chord, terminal_owned)
            .unwrap();
        assert_eq!(conflict.kind, ConflictKind::Terminal);
        assert!(conflict.message.contains("terminal"));
    }

    #[test]
    fn a_chord_used_by_another_action_is_refused() {
        let keymap = Keymap::defaults();
        let chord = Chord::parse("ctrl-shift-k").unwrap();
        let conflict = keymap
            .check("settings.open", &chord, terminal_owned)
            .unwrap();
        assert_eq!(conflict.kind, ConflictKind::OtherAction);
        assert!(conflict.message.contains("Command palette"));
    }

    #[test]
    fn surface_handlers_are_refused_and_explained() {
        let keymap = Keymap::defaults();
        let chord = Chord::parse("escape").unwrap();
        let conflict = keymap
            .check("settings.open", &chord, terminal_owned)
            .unwrap();
        assert_eq!(conflict.kind, ConflictKind::Unmodified);
        let chord = Chord::parse("ctrl-escape").unwrap();
        assert_eq!(
            keymap.check("settings.open", &chord, terminal_owned),
            None,
            "an unbound modified chord is accepted"
        );
        // A surface chord with a modifier is still reported.
        let chord = Chord::parse("ctrl-1").unwrap();
        let conflict = keymap
            .check("settings.open", &chord, terminal_owned)
            .unwrap();
        assert_eq!(conflict.kind, ConflictKind::Surface);
        assert!(conflict.message.contains("Palette"));
    }

    #[test]
    fn a_binding_without_any_modifier_is_refused() {
        let keymap = Keymap::defaults();
        let chord = Chord::parse("f5").unwrap();
        let conflict = keymap
            .check("palette.files", &chord, terminal_owned)
            .unwrap();
        assert_eq!(conflict.kind, ConflictKind::Unmodified);
        assert!(conflict.message.contains("no modifier"));
    }

    #[test]
    fn reset_and_file_round_trip_keep_every_action() {
        let mut keymap = Keymap::defaults();
        let custom = Chord::parse("ctrl-shift-m").unwrap();
        assert!(keymap.set("palette.global", custom.clone()));
        assert_eq!(keymap.chord("palette.global"), Some(&custom));
        let rows = keymap.rows();
        let row = rows.iter().find(|row| row.id == "palette.global").unwrap();
        assert!(row.custom);
        assert_eq!(row.chord_label, "Ctrl+Shift+M");

        let json = keymap.to_json();
        let (back, notes) = Keymap::from_json(&json).unwrap();
        assert!(notes.is_empty());
        assert_eq!(back, keymap);
        assert_eq!(back.chord("palette.global"), Some(&custom));

        assert!(keymap.reset("palette.global"));
        assert_eq!(
            keymap.chord("palette.global").unwrap().canonical(),
            "ctrl-shift-k"
        );
        keymap.reset_all();
        assert_eq!(keymap, Keymap::defaults());
    }

    #[test]
    fn loading_reports_unknown_actions_and_bad_chords_instead_of_failing() {
        let text = r#"{
            "version": 1,
            "bindings": {
                "palette.global": "ctrl-shift-m",
                "nope.nothing": "ctrl-shift-z",
                "palette.files": "ctrl-"
            }
        }"#;
        let (keymap, notes) = Keymap::from_json(text).unwrap();
        assert_eq!(notes.len(), 2);
        assert!(notes[0].contains("nope.nothing") || notes[1].contains("nope.nothing"));
        assert!(notes.iter().any(|n| n.contains("Palette: Files")));
        assert_eq!(
            keymap.chord("palette.global").unwrap().canonical(),
            "ctrl-shift-m"
        );
        // The rejected chord keeps its default instead of becoming empty.
        assert_eq!(
            keymap.chord("palette.files").unwrap().canonical(),
            "ctrl-shift-o"
        );
    }

    #[test]
    fn a_corrupt_file_is_an_error_the_caller_can_report() {
        let error = Keymap::from_json("{not json").unwrap_err();
        assert!(error.contains("keymap.json"));
    }

    #[test]
    fn the_palette_no_longer_claims_the_pip_chord() {
        let keymap = Keymap::defaults();
        assert_eq!(
            keymap.chord("pip.toggle").unwrap().canonical(),
            "ctrl-shift-p"
        );
        let pip = ACTIONS.iter().find(|a| a.id == "pip.toggle").unwrap();
        assert_eq!(pip.group, Group::Reserved);
        assert!(pip.note.contains("spec §8.3"));
        // No other action may use it: the collision that prompted this slice.
        let users: Vec<_> = keymap
            .bindings
            .iter()
            .filter(|(_, chord)| chord.canonical() == "ctrl-shift-p")
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(users, vec!["pip.toggle"]);
    }
}
