/// Every failure this crate reports. Messages are shown to the user verbatim.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    /// A missing path, file, ref, or a repository where one is required.
    #[error("{0}")]
    NotFound(String),
    /// A bad argument: invalid branch name, path escaping its root,
    /// destination that already exists, not a directory, not a regular file.
    #[error("{0}")]
    Invalid(String),
    /// A safety refusal (dirty worktree, unpushed commits).
    #[error("{0}")]
    Refused(String),
    /// The file's content no longer matches the expected hash; `current` is
    /// the hash now on disk, `None` when the file is gone.
    #[error("file changed since it was read")]
    Conflict { current: Option<[u8; 32]> },
    #[error("git {args} failed ({status}): {stderr}")]
    Command {
        args: String,
        status: String,
        stderr: String,
    },
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
    /// Unparseable git output.
    #[error("{0}")]
    Parse(String),
}

pub type Result<T, E = GitError> = std::result::Result<T, E>;

impl GitError {
    pub(crate) fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}
