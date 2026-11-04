//! Shared helpers: isolated git configuration and repository fixtures.
#![allow(dead_code)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Once;

/// Isolates every git process this test binary starts (including the ones
/// the library spawns) from the host's global and system configuration.
pub fn setup() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let vars = [
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_CONFIG_COUNT", "5"),
            ("GIT_CONFIG_KEY_0", "user.name"),
            ("GIT_CONFIG_VALUE_0", "Flotilla Test"),
            ("GIT_CONFIG_KEY_1", "user.email"),
            ("GIT_CONFIG_VALUE_1", "test@flotilla.invalid"),
            ("GIT_CONFIG_KEY_2", "init.defaultBranch"),
            ("GIT_CONFIG_VALUE_2", "main"),
            // Local submodule URLs in tests.
            ("GIT_CONFIG_KEY_3", "protocol.file.allow"),
            ("GIT_CONFIG_VALUE_3", "always"),
            ("GIT_CONFIG_KEY_4", "commit.gpgsign"),
            ("GIT_CONFIG_VALUE_4", "false"),
        ];
        for (key, value) in vars {
            // SAFETY: std serialises its own environment access (including
            // the environment capture in `Command::spawn`) behind a lock, and
            // nothing in these tests reads the environment through libc.
            unsafe { std::env::set_var(key, value) };
        }
    });
}

/// Runs git in `dir` and returns its raw output.
pub fn git_output(dir: &Path, args: &[&str]) -> Output {
    setup();
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .expect("git runs")
}

/// Runs git in `dir`, asserts success and returns trimmed stdout.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = git_output(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} in {} failed: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// Writes `content` to `dir/relative`, creating parent directories.
pub fn write(dir: &Path, relative: &str, content: impl AsRef<[u8]>) {
    let path = dir.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

pub fn chmod(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

pub fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

/// Stages everything and commits.
pub fn commit_all(dir: &Path, message: &str) -> String {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", message]);
    git(dir, &["rev-parse", "HEAD"])
}

/// A new repository at `dir/name` on branch `main` with one commit
/// containing `files`. Returns its canonical path.
pub fn repo_with(dir: &Path, name: &str, files: &[(&str, &str)]) -> PathBuf {
    let path = dir.join(name);
    std::fs::create_dir_all(&path).unwrap();
    git(&path, &["init", "-q"]);
    for (relative, content) in files {
        write(&path, relative, content);
    }
    if files.is_empty() {
        git(&path, &["commit", "-q", "--allow-empty", "-m", "initial"]);
    } else {
        commit_all(&path, "initial");
    }
    std::fs::canonicalize(path).unwrap()
}

/// A canonical temporary directory (the temp root may be a symlink).
pub fn tempdir() -> (tempfile::TempDir, PathBuf) {
    setup();
    let dir = tempfile::tempdir().unwrap();
    let path = std::fs::canonicalize(dir.path()).unwrap();
    (dir, path)
}

/// Ten distinct lines, enough for git's rename detection.
pub const LONG_TEXT: &str = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n";
