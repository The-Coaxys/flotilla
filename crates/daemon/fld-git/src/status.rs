//! Working-tree status and the per-file +/- counts the sidebar shows.
use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use fl_model::workspace::{ChangeKind, FileChange, GitSummary};

use crate::cmd::{self, line};
use crate::{GitError, Result};

/// Bytes of an untracked file read to count its lines.
pub const UNTRACKED_READ_BYTES: u64 = 1024 * 1024;

/// One path from `git status --porcelain=v2`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PorcelainEntry {
    pub path: String,
    /// Source of a rename or copy.
    pub old_path: Option<String>,
    pub kind: ChangeKind,
    /// The `X` letter (index); `.` when unchanged, `?` for untracked.
    pub index: char,
    /// The `Y` letter (worktree); `.` when unchanged, `?` for untracked.
    pub worktree: char,
}

/// Parsed `git status --porcelain=v2 -z --branch`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Porcelain {
    /// HEAD commit; `None` before the first commit (`(initial)`).
    pub oid: Option<String>,
    /// Current branch; `None` when detached.
    pub head: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub entries: Vec<PorcelainEntry>,
}

fn utf8(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn xy(field: &[u8], record: &str) -> Result<(char, char)> {
    match field {
        [x, y] => Ok((char::from(*x), char::from(*y))),
        _ => Err(GitError::Parse(format!(
            "malformed status letters in porcelain record `{record}`"
        ))),
    }
}

fn ordinary_kind(index: char, worktree: char) -> ChangeKind {
    if worktree == 'D' {
        return ChangeKind::Deleted;
    }
    let either = |c: char| index == c || worktree == c;
    if either('A') {
        ChangeKind::Added
    } else if either('D') {
        ChangeKind::Deleted
    } else if either('T') {
        ChangeKind::TypeChanged
    } else {
        ChangeKind::Modified
    }
}

/// Splits `record` into `n` space-separated fields, the last one being the
/// (possibly space-containing) rest.
fn fields(record: &[u8], n: usize) -> Result<Vec<&[u8]>> {
    let parts: Vec<&[u8]> = record.splitn(n, |&b| b == b' ').collect();
    if parts.len() == n {
        Ok(parts)
    } else {
        Err(GitError::Parse(format!(
            "malformed porcelain record `{}`",
            String::from_utf8_lossy(record)
        )))
    }
}

/// Parses `git status --porcelain=v2 -z --branch` output: ordinary (`1`),
/// renamed/copied (`2`, original path in the next NUL field), unmerged
/// (`u`) and untracked (`?`) records plus `# branch.*` headers. Ignored
/// (`!`) records are skipped.
pub fn parse_porcelain_v2(output: &[u8]) -> Result<Porcelain> {
    let mut porcelain = Porcelain::default();
    let mut records = output.split(|&b| b == 0);
    while let Some(record) = records.next() {
        if record.is_empty() {
            continue;
        }
        let text = String::from_utf8_lossy(record);
        match record[0] {
            b'#' => {
                let header = text.strip_prefix("# ").unwrap_or_default();
                let (key, value) = header.split_once(' ').unwrap_or((header, ""));
                match key {
                    "branch.oid" => {
                        porcelain.oid = (value != "(initial)").then(|| value.to_owned());
                    }
                    "branch.head" => {
                        porcelain.head = (value != "(detached)").then(|| value.to_owned());
                    }
                    "branch.upstream" => porcelain.upstream = Some(value.to_owned()),
                    "branch.ab" => {
                        let mut parts = value.split(' ');
                        let ahead = parts.next().and_then(|a| a.strip_prefix('+'));
                        let behind = parts.next().and_then(|b| b.strip_prefix('-'));
                        let parse = |n: Option<&str>| {
                            n.and_then(|n| n.parse::<u32>().ok()).ok_or_else(|| {
                                GitError::Parse(format!("malformed porcelain header `{text}`"))
                            })
                        };
                        porcelain.ahead = parse(ahead)?;
                        porcelain.behind = parse(behind)?;
                    }
                    // `# stash <n>` and future headers.
                    _ => {}
                }
            }
            b'1' => {
                let parts = fields(record, 9)?;
                let (index, worktree) = xy(parts[1], &text)?;
                porcelain.entries.push(PorcelainEntry {
                    path: utf8(parts[8]),
                    old_path: None,
                    kind: ordinary_kind(index, worktree),
                    index,
                    worktree,
                });
            }
            b'2' => {
                let parts = fields(record, 10)?;
                let (index, worktree) = xy(parts[1], &text)?;
                let old = records.next().filter(|o| !o.is_empty()).ok_or_else(|| {
                    GitError::Parse(format!("rename record `{text}` without its original path"))
                })?;
                let kind = if index == 'C' || worktree == 'C' {
                    ChangeKind::Copied
                } else {
                    ChangeKind::Renamed
                };
                porcelain.entries.push(PorcelainEntry {
                    path: utf8(parts[9]),
                    old_path: Some(utf8(old)),
                    kind,
                    index,
                    worktree,
                });
            }
            b'u' => {
                let parts = fields(record, 11)?;
                let (index, worktree) = xy(parts[1], &text)?;
                porcelain.entries.push(PorcelainEntry {
                    path: utf8(parts[10]),
                    old_path: None,
                    kind: ChangeKind::Conflicted,
                    index,
                    worktree,
                });
            }
            b'?' => {
                let path = record.strip_prefix(b"? ").ok_or_else(|| {
                    GitError::Parse(format!("malformed porcelain record `{text}`"))
                })?;
                porcelain.entries.push(PorcelainEntry {
                    path: utf8(path),
                    old_path: None,
                    kind: ChangeKind::Untracked,
                    index: '?',
                    worktree: '?',
                });
            }
            b'!' => {}
            _ => {
                return Err(GitError::Parse(format!(
                    "unknown porcelain record `{text}`"
                )));
            }
        }
    }
    Ok(porcelain)
}

/// One path from `git diff --numstat -z`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumstatEntry {
    pub path: String,
    pub old_path: Option<String>,
    /// `None` for binary files.
    pub added: Option<u32>,
    pub removed: Option<u32>,
}

/// Parses `git diff --numstat -z -M`: `added\tremoved\tpath\0`, or for
/// renames `added\tremoved\t\0old\0new\0`; binary files report `-\t-`.
pub fn parse_numstat(output: &[u8]) -> Result<Vec<NumstatEntry>> {
    let mut entries = Vec::new();
    let mut fields = output.split(|&b| b == 0);
    while let Some(record) = fields.next() {
        if record.is_empty() {
            continue;
        }
        let text = String::from_utf8_lossy(record);
        let malformed = || GitError::Parse(format!("malformed numstat record `{text}`"));
        let mut parts = record.splitn(3, |&b| b == b'\t');
        let (Some(added), Some(removed), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            return Err(malformed());
        };
        let count = |field: &[u8]| -> Result<Option<u32>> {
            if field == b"-" {
                return Ok(None);
            }
            std::str::from_utf8(field)
                .ok()
                .and_then(|f| f.parse::<u32>().ok())
                .map(Some)
                .ok_or_else(malformed)
        };
        let (added, removed) = (count(added)?, count(removed)?);
        let (path, old_path) = if path.is_empty() {
            let old = fields
                .next()
                .filter(|f| !f.is_empty())
                .ok_or_else(malformed)?;
            let new = fields
                .next()
                .filter(|f| !f.is_empty())
                .ok_or_else(malformed)?;
            (utf8(new), Some(utf8(old)))
        } else {
            (utf8(path), None)
        };
        entries.push(NumstatEntry {
            path,
            old_path,
            added,
            removed,
        });
    }
    Ok(entries)
}

/// Parses `git diff --name-status -z -M` into `(letter, path, old_path)`.
fn parse_name_status(output: &[u8]) -> Result<Vec<(char, String, Option<String>)>> {
    let mut entries = Vec::new();
    let mut fields = output.split(|&b| b == 0);
    while let Some(status) = fields.next() {
        if status.is_empty() {
            continue;
        }
        let malformed = || {
            GitError::Parse(format!(
                "malformed name-status record `{}`",
                String::from_utf8_lossy(status)
            ))
        };
        let letter = char::from(status[0]);
        let first = fields
            .next()
            .filter(|f| !f.is_empty())
            .ok_or_else(malformed)?;
        if matches!(letter, 'R' | 'C') {
            let new = fields
                .next()
                .filter(|f| !f.is_empty())
                .ok_or_else(malformed)?;
            entries.push((letter, utf8(new), Some(utf8(first))));
        } else {
            entries.push((letter, utf8(first), None));
        }
    }
    Ok(entries)
}

/// The commit `worktree` diffs against: `merge-base HEAD <target>` (target
/// as given, then `origin/<target>`), else HEAD itself, else `None` when
/// HEAD is unborn.
pub fn diff_base(worktree: &Path, target: Option<&str>) -> Result<Option<String>> {
    let Some(head) = cmd::resolve_commit(worktree, "HEAD")? else {
        return Ok(None);
    };
    if let Some(target) = target.filter(|t| !t.is_empty() && !cmd::is_option_like(t)) {
        for candidate in [target.to_owned(), format!("origin/{target}")] {
            let out = cmd::output(worktree, &["merge-base", "HEAD", candidate.as_str()])?;
            if out.status.success() {
                let base = line(&out.stdout);
                if !base.is_empty() {
                    return Ok(Some(base));
                }
            }
        }
    }
    Ok(Some(head))
}

/// The empty tree's oid in this repository's hash algorithm.
fn empty_tree(worktree: &Path) -> Result<String> {
    let out = cmd::run(worktree, &["hash-object", "-t", "tree", "--stdin"])?;
    Ok(line(&out))
}

/// Line counts for an untracked file treated as entirely added; `None` for
/// binary files, special files and files that vanished.
fn untracked_counts(path: &Path) -> (Option<u32>, Option<u32>) {
    let Ok(meta) = path.symlink_metadata() else {
        return (None, None);
    };
    if meta.file_type().is_symlink() {
        // Git diffs a symlink as one line: its target.
        return (Some(1), Some(0));
    }
    if !meta.is_file() {
        return (None, None);
    }
    let mut bytes = Vec::new();
    let read = std::fs::File::open(path)
        .and_then(|f| f.take(UNTRACKED_READ_BYTES).read_to_end(&mut bytes));
    if read.is_err() {
        return (None, None);
    }
    let sniff = &bytes[..bytes.len().min(crate::files::BINARY_SNIFF_BYTES)];
    if sniff.contains(&0) {
        return (None, None);
    }
    let newlines = bytes.iter().filter(|&&b| b == b'\n').count();
    let lines = newlines + usize::from(!bytes.is_empty() && !bytes.ends_with(b"\n"));
    (Some(u32::try_from(lines).unwrap_or(u32::MAX)), Some(0))
}

fn name_status_kind(letter: char) -> ChangeKind {
    match letter {
        'A' => ChangeKind::Added,
        'D' => ChangeKind::Deleted,
        'R' => ChangeKind::Renamed,
        'C' => ChangeKind::Copied,
        'T' => ChangeKind::TypeChanged,
        'U' => ChangeKind::Conflicted,
        _ => ChangeKind::Modified,
    }
}

/// The worktree's summary and changed files against the merge-base with
/// `target`: commits on the branch since the merge-base, staged, unstaged
/// and untracked changes, sorted by path. Totals cover every file; the list
/// is truncated to `max_files`.
pub fn status(
    worktree: &Path,
    target: Option<&str>,
    max_files: usize,
) -> Result<(GitSummary, Vec<FileChange>)> {
    let porcelain = parse_porcelain_v2(&cmd::run(
        worktree,
        &[
            "status",
            "--porcelain=v2",
            "-z",
            "--branch",
            "--untracked-files=all",
        ],
    )?)?;
    let base = match diff_base(worktree, target)? {
        Some(base) => base,
        None => empty_tree(worktree)?,
    };
    let numstat = parse_numstat(&cmd::run(
        worktree,
        &["diff", "-z", "--numstat", "-M", base.as_str(), "--"],
    )?)?;
    let name_status = parse_name_status(&cmd::run(
        worktree,
        &["diff", "-z", "--name-status", "-M", base.as_str(), "--"],
    )?)?;

    let mut files: BTreeMap<String, FileChange> = BTreeMap::new();
    for entry in numstat {
        files.insert(
            entry.path.clone(),
            FileChange {
                path: entry.path,
                old_path: entry.old_path,
                kind: ChangeKind::Modified,
                added: entry.added,
                removed: entry.removed,
            },
        );
    }
    for (letter, path, old_path) in name_status {
        let file = files.entry(path.clone()).or_insert_with(|| FileChange {
            path,
            old_path: None,
            kind: ChangeKind::Modified,
            added: Some(0),
            removed: Some(0),
        });
        file.kind = name_status_kind(letter);
        if old_path.is_some() {
            file.old_path = old_path;
        }
    }
    for entry in porcelain.entries.iter() {
        match files.get_mut(&entry.path) {
            Some(file) => {
                if matches!(entry.kind, ChangeKind::Conflicted | ChangeKind::Untracked) {
                    file.kind = entry.kind;
                }
            }
            None => {
                let (added, removed) = if entry.kind == ChangeKind::Untracked {
                    untracked_counts(&worktree.join(&entry.path))
                } else {
                    // Changed against HEAD but identical to the base.
                    (Some(0), Some(0))
                };
                files.insert(
                    entry.path.clone(),
                    FileChange {
                        path: entry.path.clone(),
                        old_path: entry.old_path.clone(),
                        kind: entry.kind,
                        added,
                        removed,
                    },
                );
            }
        }
    }

    let mut files: Vec<FileChange> = files.into_values().collect();
    let added = files.iter().filter_map(|f| f.added).map(u64::from).sum();
    let removed = files.iter().filter_map(|f| f.removed).map(u64::from).sum();
    let conflicted = files.iter().any(|f| f.kind == ChangeKind::Conflicted);
    files.truncate(max_files);
    let summary = GitSummary {
        branch: porcelain.head,
        head: porcelain.oid,
        upstream: porcelain.upstream,
        ahead: porcelain.ahead,
        behind: porcelain.behind,
        added,
        removed,
        dirty: !porcelain.entries.is_empty(),
        conflicted,
    };
    Ok((summary, files))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, kind: ChangeKind, index: char, worktree: char) -> PorcelainEntry {
        PorcelainEntry {
            path: path.into(),
            old_path: None,
            kind,
            index,
            worktree,
        }
    }

    #[test]
    fn parses_headers() {
        let out = b"# branch.oid 0123456789abcdef0123456789abcdef01234567\0# branch.head feature/x\0# branch.upstream origin/feature/x\0# branch.ab +3 -12\0# stash 2\0";
        let p = parse_porcelain_v2(out).unwrap();
        assert_eq!(
            p.oid.as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
        assert_eq!(p.head.as_deref(), Some("feature/x"));
        assert_eq!(p.upstream.as_deref(), Some("origin/feature/x"));
        assert_eq!((p.ahead, p.behind), (3, 12));
        assert!(p.entries.is_empty());
    }

    #[test]
    fn parses_initial_detached_and_missing_upstream() {
        let p = parse_porcelain_v2(b"# branch.oid (initial)\0# branch.head main\0").unwrap();
        assert_eq!(p.oid, None);
        assert_eq!(p.head.as_deref(), Some("main"));
        assert_eq!(p.upstream, None);
        assert_eq!((p.ahead, p.behind), (0, 0));

        let p = parse_porcelain_v2(b"# branch.oid abc\0# branch.head (detached)\0").unwrap();
        assert_eq!(p.oid.as_deref(), Some("abc"));
        assert_eq!(p.head, None);
    }

    #[test]
    fn parses_ordinary_records() {
        let h = "0".repeat(40);
        let out = format!(
            "1 .M N... 100644 100644 100644 {h} {h} src/a file.rs\0\
             1 A. N... 000000 100644 100644 {h} {h} new.rs\0\
             1 AM N... 000000 100644 100644 {h} {h} new2.rs\0\
             1 D. N... 100644 000000 000000 {h} {h} gone.rs\0\
             1 MD N... 100644 100644 000000 {h} {h} staged-then-deleted.rs\0\
             1 .T N... 100644 100644 120000 {h} {h} link\0\
             1 M. N... 100644 100644 100644 {h} {h} staged.rs\0"
        );
        let p = parse_porcelain_v2(out.as_bytes()).unwrap();
        assert_eq!(
            p.entries,
            vec![
                entry("src/a file.rs", ChangeKind::Modified, '.', 'M'),
                entry("new.rs", ChangeKind::Added, 'A', '.'),
                entry("new2.rs", ChangeKind::Added, 'A', 'M'),
                entry("gone.rs", ChangeKind::Deleted, 'D', '.'),
                entry("staged-then-deleted.rs", ChangeKind::Deleted, 'M', 'D'),
                entry("link", ChangeKind::TypeChanged, '.', 'T'),
                entry("staged.rs", ChangeKind::Modified, 'M', '.'),
            ]
        );
    }

    #[test]
    fn parses_renamed_and_copied_records() {
        let h = "0".repeat(40);
        let out = format!(
            "2 R. N... 100644 100644 100644 {h} {h} R100 new name.rs\0old name.rs\0\
             2 C. N... 100644 100644 100644 {h} {h} C75 copy.rs\0orig.rs\0\
             ? after.txt\0"
        );
        let p = parse_porcelain_v2(out.as_bytes()).unwrap();
        assert_eq!(p.entries.len(), 3);
        assert_eq!(p.entries[0].path, "new name.rs");
        assert_eq!(p.entries[0].old_path.as_deref(), Some("old name.rs"));
        assert_eq!(p.entries[0].kind, ChangeKind::Renamed);
        assert_eq!(p.entries[1].path, "copy.rs");
        assert_eq!(p.entries[1].old_path.as_deref(), Some("orig.rs"));
        assert_eq!(p.entries[1].kind, ChangeKind::Copied);
        assert_eq!(
            p.entries[2],
            entry("after.txt", ChangeKind::Untracked, '?', '?')
        );
    }

    #[test]
    fn parses_unmerged_untracked_and_skips_ignored() {
        let h = "0".repeat(40);
        let out = format!(
            "u UU N... 100644 100644 100644 100644 {h} {h} {h} both changed.rs\0\
             ? dir/untracked file\0\
             ! target/debug\0"
        );
        let p = parse_porcelain_v2(out.as_bytes()).unwrap();
        assert_eq!(
            p.entries,
            vec![
                entry("both changed.rs", ChangeKind::Conflicted, 'U', 'U'),
                entry("dir/untracked file", ChangeKind::Untracked, '?', '?'),
            ]
        );
    }

    #[test]
    fn rejects_malformed_porcelain() {
        assert!(matches!(
            parse_porcelain_v2(b"1 .M N...\0"),
            Err(GitError::Parse(_))
        ));
        let h = "0".repeat(40);
        let rename = format!("2 R. N... 100644 100644 100644 {h} {h} R100 new.rs\0");
        assert!(matches!(
            parse_porcelain_v2(rename.as_bytes()),
            Err(GitError::Parse(_))
        ));
        assert!(matches!(
            parse_porcelain_v2(b"# branch.ab +x -1\0"),
            Err(GitError::Parse(_))
        ));
        assert!(matches!(
            parse_porcelain_v2(b"Z what\0"),
            Err(GitError::Parse(_))
        ));
    }

    #[test]
    fn parses_numstat() {
        let out = b"3\t1\tsrc/lib.rs\0-\t-\timage.png\x000\t0\t\0old dir/a.rs\0new dir/a.rs\x0012\t0\twith\ttab.txt\0";
        let entries = parse_numstat(out).unwrap();
        assert_eq!(
            entries,
            vec![
                NumstatEntry {
                    path: "src/lib.rs".into(),
                    old_path: None,
                    added: Some(3),
                    removed: Some(1),
                },
                NumstatEntry {
                    path: "image.png".into(),
                    old_path: None,
                    added: None,
                    removed: None,
                },
                NumstatEntry {
                    path: "new dir/a.rs".into(),
                    old_path: Some("old dir/a.rs".into()),
                    added: Some(0),
                    removed: Some(0),
                },
                NumstatEntry {
                    path: "with\ttab.txt".into(),
                    old_path: None,
                    added: Some(12),
                    removed: Some(0),
                },
            ]
        );
        assert!(matches!(
            parse_numstat(b"x\t1\ta\0"),
            Err(GitError::Parse(_))
        ));
        assert!(matches!(
            parse_numstat(b"1\t1\t\0old\0"),
            Err(GitError::Parse(_))
        ));
    }

    #[test]
    fn parses_name_status() {
        let out = b"M\0a.rs\0R087\0old.rs\0new.rs\0A\0b.rs\0";
        assert_eq!(
            parse_name_status(out).unwrap(),
            vec![
                ('M', "a.rs".into(), None),
                ('R', "new.rs".into(), Some("old.rs".into())),
                ('A', "b.rs".into(), None),
            ]
        );
    }

    #[test]
    fn counts_untracked_lines() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::write(p.join("three"), "a\nb\nc\n").unwrap();
        std::fs::write(p.join("partial"), "a\nb").unwrap();
        std::fs::write(p.join("empty"), "").unwrap();
        std::fs::write(p.join("bin"), b"a\0b\n").unwrap();
        assert_eq!(untracked_counts(&p.join("three")), (Some(3), Some(0)));
        assert_eq!(untracked_counts(&p.join("partial")), (Some(2), Some(0)));
        assert_eq!(untracked_counts(&p.join("empty")), (Some(0), Some(0)));
        assert_eq!(untracked_counts(&p.join("bin")), (None, None));
        assert_eq!(untracked_counts(&p.join("missing")), (None, None));
    }
}
