//! Per-repository scripts from `.flotilla/config.json` in a worktree root.
//!
//! Lenient shapes: unknown keys are ignored, missing keys are empty, empty
//! command strings are dropped.
//!
//! ```json
//! {
//!   "setup": "npm ci",                       // string | [string]
//!   "run": {                                 // string | [string] | { name: target }
//!     "web": { "command": "npm run dev", "default": true, "stop_commands": ["pkill vite"] },
//!     "test": ["cargo test"]                 // target: string | [string] | object
//!   },
//!   "teardown": ["docker compose down"]      // string | [string]
//! }
//! ```
use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use serde_json::Value;

/// Location of the config file relative to the worktree root.
pub const CONFIG_PATH: &str = ".flotilla/config.json";
/// Largest config file accepted.
pub const MAX_CONFIG_BYTES: u64 = 256 * 1024;
/// Name of the target a plain string or array `run` becomes.
pub const DEFAULT_TARGET: &str = "default";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunTarget {
    pub commands: Vec<String>,
    /// Started when no target is named.
    pub default: bool,
    /// Run to stop the target instead of only signalling its processes.
    pub stop_commands: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoConfig {
    pub setup: Vec<String>,
    pub run: BTreeMap<String, RunTarget>,
    pub teardown: Vec<String>,
}

impl RepoConfig {
    /// The run target called `name`. With `None`: the target flagged
    /// default, else the only target, else one named `default`.
    pub fn run_target(&self, name: Option<&str>) -> Option<(&str, &RunTarget)> {
        let found = match name {
            Some(name) => self.run.get_key_value(name),
            None => self
                .run
                .iter()
                .find(|(_, target)| target.default)
                .or_else(|| {
                    if self.run.len() == 1 {
                        self.run.iter().next()
                    } else {
                        None
                    }
                })
                .or_else(|| self.run.get_key_value(DEFAULT_TARGET)),
        };
        found.map(|(name, target)| (name.as_str(), target))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {CONFIG_PATH}: {0}")]
    Io(#[from] std::io::Error),
    #[error("{CONFIG_PATH} line {line}, column {column}: {message}")]
    Json {
        line: usize,
        column: usize,
        message: String,
    },
    #[error("{CONFIG_PATH}: `{field}` must be {expected}")]
    Shape {
        field: String,
        expected: &'static str,
    },
}

const COMMANDS: &str = "a string or an array of strings";

fn shape(field: impl Into<String>, expected: &'static str) -> ConfigError {
    ConfigError::Shape {
        field: field.into(),
        expected,
    }
}

/// `string | [string]` (null = empty), empty strings dropped.
fn commands(value: Option<&Value>, field: &str) -> Result<Vec<String>, ConfigError> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(s)) => Ok(if s.is_empty() {
            Vec::new()
        } else {
            vec![s.clone()]
        }),
        Some(Value::Array(items)) => items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| match item {
                Value::String(s) if s.is_empty() => None,
                Value::String(s) => Some(Ok(s.clone())),
                _ => Some(Err(shape(format!("{field}[{i}]"), "a string"))),
            })
            .collect(),
        Some(_) => Err(shape(field, COMMANDS)),
    }
}

fn run_target(name: &str, value: &Value) -> Result<RunTarget, ConfigError> {
    let field = format!("run.{name}");
    match value {
        Value::Object(map) => {
            let default = match map.get("default") {
                None | Some(Value::Null) => false,
                Some(Value::Bool(b)) => *b,
                Some(_) => return Err(shape(format!("{field}.default"), "a boolean")),
            };
            Ok(RunTarget {
                commands: commands(map.get("command"), &format!("{field}.command"))?,
                default,
                stop_commands: commands(
                    map.get("stop_commands"),
                    &format!("{field}.stop_commands"),
                )?,
            })
        }
        Value::String(_) | Value::Array(_) => Ok(RunTarget {
            commands: commands(Some(value), &field)?,
            ..RunTarget::default()
        }),
        _ => Err(shape(
            field,
            "a string, an array of strings or an object with `command`",
        )),
    }
}

/// Parses the text of a config file.
pub fn parse_config(text: &str) -> Result<RepoConfig, ConfigError> {
    let value: Value = serde_json::from_str(text).map_err(|e| ConfigError::Json {
        line: e.line(),
        column: e.column(),
        message: e.to_string(),
    })?;
    let Value::Object(root) = value else {
        return Err(shape("(root)", "an object"));
    };
    let mut run = BTreeMap::new();
    match root.get("run") {
        None | Some(Value::Null) => {}
        Some(value @ (Value::String(_) | Value::Array(_))) => {
            let commands = commands(Some(value), "run")?;
            if !commands.is_empty() {
                run.insert(
                    DEFAULT_TARGET.to_owned(),
                    RunTarget {
                        commands,
                        default: true,
                        stop_commands: Vec::new(),
                    },
                );
            }
        }
        Some(Value::Object(targets)) => {
            for (name, target) in targets {
                run.insert(name.clone(), run_target(name, target)?);
            }
        }
        Some(_) => {
            return Err(shape(
                "run",
                "a string, an array of strings or an object of named targets",
            ));
        }
    }
    Ok(RepoConfig {
        setup: commands(root.get("setup"), "setup")?,
        run,
        teardown: commands(root.get("teardown"), "teardown")?,
    })
}

/// Loads `<root>/.flotilla/config.json`; `Ok(None)` when it does not exist.
pub fn load_config(root: &Path) -> Result<Option<RepoConfig>, ConfigError> {
    let file = match std::fs::File::open(root.join(CONFIG_PATH)) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err(ConfigError::Io(std::io::Error::new(
            std::io::ErrorKind::FileTooLarge,
            format!("larger than {} KiB", MAX_CONFIG_BYTES / 1024),
        )));
    }
    let text = String::from_utf8(bytes).map_err(|e| ConfigError::Json {
        line: 1,
        column: 1,
        message: format!("not valid UTF-8: {e}"),
    })?;
    parse_config(&text).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(commands: &[&str], default: bool, stop: &[&str]) -> RunTarget {
        RunTarget {
            commands: commands.iter().map(|s| s.to_string()).collect(),
            default,
            stop_commands: stop.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn empty_object_is_empty_config() {
        assert_eq!(parse_config("{}").unwrap(), RepoConfig::default());
        assert_eq!(
            parse_config(r#"{"unknown": 1, "setup": null}"#).unwrap(),
            RepoConfig::default()
        );
    }

    #[test]
    fn string_and_array_commands() {
        let c = parse_config(r#"{"setup": "npm ci", "teardown": ["a", "", "b"]}"#).unwrap();
        assert_eq!(c.setup, vec!["npm ci"]);
        assert_eq!(c.teardown, vec!["a", "b"]);
        assert_eq!(
            parse_config(r#"{"setup": ""}"#).unwrap().setup,
            Vec::<String>::new()
        );
    }

    #[test]
    fn run_as_string_or_array_is_the_default_target() {
        let c = parse_config(r#"{"run": "npm run dev"}"#).unwrap();
        assert_eq!(c.run.len(), 1);
        assert_eq!(c.run["default"], target(&["npm run dev"], true, &[]));
        let c = parse_config(r#"{"run": ["a", "b"]}"#).unwrap();
        assert_eq!(c.run["default"], target(&["a", "b"], true, &[]));
        assert!(parse_config(r#"{"run": ""}"#).unwrap().run.is_empty());
    }

    #[test]
    fn run_as_named_targets() {
        let c = parse_config(
            r#"{"run": {
                "web": {"command": "npm run dev", "default": true, "stop_commands": "pkill vite", "x": 1},
                "test": ["cargo test", "cargo clippy"],
                "lint": "cargo fmt --check",
                "empty": {}
            }}"#,
        )
        .unwrap();
        assert_eq!(
            c.run["web"],
            target(&["npm run dev"], true, &["pkill vite"])
        );
        assert_eq!(
            c.run["test"],
            target(&["cargo test", "cargo clippy"], false, &[])
        );
        assert_eq!(c.run["lint"], target(&["cargo fmt --check"], false, &[]));
        assert_eq!(c.run["empty"], target(&[], false, &[]));
    }

    #[test]
    fn selects_run_targets() {
        let c =
            parse_config(r#"{"run": {"a": "x", "b": {"command": "y", "default": true}}}"#).unwrap();
        assert_eq!(c.run_target(None).map(|(n, _)| n), Some("b"));
        assert_eq!(c.run_target(Some("a")).map(|(n, _)| n), Some("a"));
        assert_eq!(c.run_target(Some("zzz")), None);

        let only = parse_config(r#"{"run": {"web": "x"}}"#).unwrap();
        assert_eq!(only.run_target(None).map(|(n, _)| n), Some("web"));

        let named = parse_config(r#"{"run": {"default": "x", "other": "y"}}"#).unwrap();
        assert_eq!(named.run_target(None).map(|(n, _)| n), Some("default"));

        let ambiguous = parse_config(r#"{"run": {"a": "x", "b": "y"}}"#).unwrap();
        assert_eq!(ambiguous.run_target(None), None);
        assert_eq!(RepoConfig::default().run_target(None), None);
    }

    #[test]
    fn shape_errors_name_the_field() {
        let field = |text: &str| match parse_config(text) {
            Err(ConfigError::Shape { field, .. }) => field,
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(field("[]"), "(root)");
        assert_eq!(field(r#"{"setup": 1}"#), "setup");
        assert_eq!(field(r#"{"teardown": ["a", 2]}"#), "teardown[1]");
        assert_eq!(field(r#"{"run": 5}"#), "run");
        assert_eq!(field(r#"{"run": [true]}"#), "run[0]");
        assert_eq!(field(r#"{"run": {"web": 1}}"#), "run.web");
        assert_eq!(
            field(r#"{"run": {"web": {"command": {}}}}"#),
            "run.web.command"
        );
        assert_eq!(
            field(r#"{"run": {"web": {"default": "yes"}}}"#),
            "run.web.default"
        );
        assert_eq!(
            field(r#"{"run": {"web": {"stop_commands": [1]}}}"#),
            "run.web.stop_commands[0]"
        );
        let err = parse_config(r#"{"run": {"web": {"command": {}}}}"#).unwrap_err();
        assert_eq!(
            err.to_string(),
            ".flotilla/config.json: `run.web.command` must be a string or an array of strings"
        );
    }

    #[test]
    fn json_errors_carry_position() {
        match parse_config("{\n  \"setup\": [\"a\",\n}") {
            Err(ConfigError::Json { line, column, .. }) => {
                assert_eq!(line, 3);
                assert!(column >= 1);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn loads_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_config(dir.path()).unwrap(), None);
        std::fs::create_dir(dir.path().join(".flotilla")).unwrap();
        std::fs::write(dir.path().join(CONFIG_PATH), r#"{"setup": "make"}"#).unwrap();
        assert_eq!(
            load_config(dir.path()).unwrap().unwrap().setup,
            vec!["make"]
        );

        let big = format!(
            r#"{{"setup": "{}"}}"#,
            "x".repeat(MAX_CONFIG_BYTES as usize)
        );
        std::fs::write(dir.path().join(CONFIG_PATH), big).unwrap();
        match load_config(dir.path()) {
            Err(ConfigError::Io(e)) => assert_eq!(e.kind(), std::io::ErrorKind::FileTooLarge),
            other => panic!("unexpected {other:?}"),
        }

        std::fs::write(dir.path().join(CONFIG_PATH), b"{\"setup\": \"\xff\"}").unwrap();
        assert!(matches!(
            load_config(dir.path()),
            Err(ConfigError::Json { .. })
        ));
    }
}
