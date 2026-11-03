//! Flotilla's native terminal surface (M1 Task 7).
//!
//! [`TerminalView`] renders a [`TerminalSource`]'s published frames with a
//! custom GPUI element and turns keyboard, IME, mouse, clipboard and wheel
//! input into terminal bytes. The view holds presentation state only
//! (focus, selection, search, composition, viewport requests); canonical
//! terminal state is parsed elsewhere, off the UI thread.
//!
//! Window-free logic lives in plain modules with behavioral tests:
//! [`keys`] (routing table), [`grid`] (metrics, hit testing, resize
//! debounce), [`mouse`] (reporting and wheel), [`selection`], [`runs`]
//! (text-run batching and background merging), [`links`] and [`search`].
pub mod grid;
pub mod keys;
pub mod links;
pub mod mouse;
pub mod runs;
pub mod search;
pub mod selection;
mod source;
mod view;

pub use source::{Exited, TerminalSource};
pub use view::{SearchStatus, TerminalEvent, TerminalView, TerminalViewOptions};
