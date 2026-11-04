//! Git repositories, worktrees, file access and change watching for the
//! daemon (M2 workspaces).
//!
//! Everything except [`watch::Watcher`] is synchronous and blocking; async
//! callers wrap calls in `tokio::task::spawn_blocking`. Git is always run as
//! a subprocess through [`cmd`].

pub mod cmd;
pub mod config;
mod error;
pub mod files;
pub mod repo;
pub mod status;
pub mod watch;
pub mod worktree;

pub use error::{GitError, Result};
