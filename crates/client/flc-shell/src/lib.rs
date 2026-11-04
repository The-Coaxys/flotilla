//! Workspace shell: the view split tree, its tabs and panes, and the
//! terminal views inside them.
//!
//! Canonical layout is the daemon's; this crate renders
//! [`flc_sync::WorkspaceState::projected`], submits [`LayoutOp`]s and shows a
//! visible marker while an operation is still unacknowledged. Terminal state
//! comes from `flc-sync` mirrors through [`MirrorSource`], so nothing here
//! parses bytes or blocks the UI thread.
pub mod model;
pub mod source;
mod view;

pub use model::{Focus, Rect, ShellModel, SplitRects};
pub use source::MirrorSource;
pub use view::{ShellEvent, ShellView, ShellViewOptions};
