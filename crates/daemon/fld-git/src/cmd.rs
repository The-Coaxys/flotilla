//! Running the `git` binary.
//!
//! Every invocation is `git -C <dir> -c core.quotepath=false -c color.ui=false
//! --no-pager <args>` with a scrubbed, non-interactive environment and a null
//! stdin, so output is stable, uncoloured and never waits for a prompt.
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Output, Stdio};

use crate::{GitError, Result};

/// Upper bound on the stderr text carried in [`GitError::Command`].
pub const MAX_STDERR_BYTES: usize = 2000;

/// A configured `git` command for `dir`; the caller appends the subcommand.
pub fn command(dir: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "core.quotepath=false",
            "-c",
            "color.ui=false",
            "--no-pager",
        ])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C")
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null());
    command
}

/// Runs git and returns its raw output whatever the exit status, for callers
/// that interpret exit codes themselves.
pub fn output<S: AsRef<OsStr>>(dir: &Path, args: &[S]) -> Result<Output> {
    command(dir)
        .args(args)
        .output()
        .map_err(|e| GitError::io(format!("cannot run git {}", render_args(args)), e))
}

/// Runs git and returns stdout; a non-zero exit is [`GitError::Command`].
pub fn run<S: AsRef<OsStr>>(dir: &Path, args: &[S]) -> Result<Vec<u8>> {
    let out = output(dir, args)?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(command_error(args, &out))
    }
}

/// Runs git with `input` written to its stdin and returns the raw output
/// whatever the exit status. Stdin is fed from a separate thread so large
/// inputs and outputs cannot deadlock on full pipes.
pub fn output_with_input<S: AsRef<OsStr>>(dir: &Path, args: &[S], input: &[u8]) -> Result<Output> {
    let context = || format!("cannot run git {}", render_args(args));
    let mut child = command(dir)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| GitError::io(context(), e))?;
    let mut stdin = child.stdin.take().expect("stdin is piped");
    std::thread::scope(|scope| {
        let writer = scope.spawn(move || {
            // A broken pipe means git stopped reading early; its exit status
            // and stderr say why, so the write error itself is not reported.
            let _ = stdin.write_all(input);
        });
        let out = child
            .wait_with_output()
            .map_err(|e| GitError::io(context(), e));
        let _ = writer.join();
        out
    })
}

/// Runs git and streams stdout to `sink` in chunks, without buffering the
/// whole output. A non-zero exit is [`GitError::Command`].
pub fn stream<S: AsRef<OsStr>>(dir: &Path, args: &[S], mut sink: impl FnMut(&[u8])) -> Result<()> {
    let context = || format!("cannot run git {}", render_args(args));
    let mut child = command(dir)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| GitError::io(context(), e))?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let (read, stderr_bytes) = std::thread::scope(|scope| {
        let errors = scope.spawn(move || {
            let mut bytes = Vec::new();
            let _ = stderr.read_to_end(&mut bytes);
            bytes
        });
        let mut buf = vec![0u8; 64 * 1024];
        let read = loop {
            match stdout.read(&mut buf) {
                Ok(0) => break Ok(()),
                Ok(n) => sink(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => break Err(e),
            }
        };
        drop(stdout);
        (read, errors.join().unwrap_or_default())
    });
    let status = child.wait().map_err(|e| GitError::io(context(), e))?;
    read.map_err(|e| GitError::io(context(), e))?;
    if status.success() {
        Ok(())
    } else {
        Err(GitError::Command {
            args: render_args(args),
            status: status.to_string(),
            stderr: bounded_stderr(&stderr_bytes),
        })
    }
}

/// The [`GitError::Command`] for a failed invocation.
pub fn command_error<S: AsRef<OsStr>>(args: &[S], out: &Output) -> GitError {
    GitError::Command {
        args: render_args(args),
        status: out.status.to_string(),
        stderr: bounded_stderr(&out.stderr),
    }
}

/// Stderr as trimmed, lossy UTF-8 of at most [`MAX_STDERR_BYTES`].
pub fn bounded_stderr(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let text = text.trim();
    if text.len() <= MAX_STDERR_BYTES {
        return text.to_owned();
    }
    let mut end = MAX_STDERR_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn render_args<S: AsRef<OsStr>>(args: &[S]) -> String {
    args.iter()
        .map(|a| a.as_ref().to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Trims one trailing `\n` (and `\r`) from single-line git output.
pub(crate) fn line(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    text.trim_end_matches(['\n', '\r']).to_owned()
}

/// A ref, branch or revision argument that starts with `-` would be parsed
/// as an option, so such user input is never passed to git.
pub(crate) fn is_option_like(arg: &str) -> bool {
    arg.starts_with('-')
}

/// Resolves `rev` to a commit oid, `None` when it does not name a commit.
pub(crate) fn resolve_commit(dir: &Path, rev: &str) -> Result<Option<String>> {
    if rev.is_empty() || is_option_like(rev) {
        return Ok(None);
    }
    let spec = format!("{rev}^{{commit}}");
    let out = output(dir, &["rev-parse", "--verify", "--quiet", &spec])?;
    if out.status.success() {
        Ok(Some(line(&out.stdout)))
    } else if out.status.code() == Some(1) {
        Ok(None)
    } else {
        Err(command_error(&["rev-parse", "--verify", &spec], &out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stderr_is_trimmed_and_bounded() {
        assert_eq!(bounded_stderr(b"  fatal: nope\n"), "fatal: nope");
        let long = "é".repeat(MAX_STDERR_BYTES);
        let bounded = bounded_stderr(long.as_bytes());
        assert!(bounded.len() <= MAX_STDERR_BYTES + '…'.len_utf8());
        assert!(bounded.ends_with('…'));
    }

    #[test]
    fn failing_command_reports_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(dir.path(), &["rev-parse", "--show-toplevel"]).unwrap_err();
        match err {
            GitError::Command { args, stderr, .. } => {
                assert_eq!(args, "rev-parse --show-toplevel");
                assert!(stderr.contains("not a git repository"), "{stderr}");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn input_is_fed_to_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let out = output_with_input(dir.path(), &["hash-object", "--stdin"], b"hello\n").unwrap();
        assert!(out.status.success());
        assert_eq!(
            line(&out.stdout),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
    }
}
