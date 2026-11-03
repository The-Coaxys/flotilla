//! The narrow boundary between a terminal mirror session and its views.
//!
//! The session (flc-sync) parses terminal bytes off the UI thread and publishes
//! immutable, generation-tagged [`RenderFrame`]s. A view never parses, never
//! owns canonical state and never blocks on the network: every method here
//! must be cheap and non-blocking when called from the UI thread.
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use fl_term::{HistoryLine, RenderFrame, Size, TermMode, TextPoint};

/// A terminal's child process has exited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exited {
    /// Exit status, when the daemon observed one (absent after a signal).
    pub code: Option<i32>,
}

/// Published state of one terminal, shared by every view showing it.
///
/// # Generation contract
///
/// [`frame`](Self::frame) returns a publication generation that increases on
/// *every* observable change: a new frame, a scroll of this source's
/// viewport, a status change ([`stale`](Self::stale) /
/// [`exited`](Self::exited)) or a mode change. A view compares only that
/// number to decide whether to redraw.
///
/// # Wake-up contract
///
/// [`subscribe`](Self::subscribe) registers a one-shot waker. After the next
/// publication the source calls `wake_by_ref` on every registered waker and
/// forgets them. A view re-subscribes before re-reading the generation, so a
/// publication racing with the read is never lost. Registering a waker that
/// [`Waker::will_wake`] an already registered one must replace it rather than
/// grow the set. Wakers may be called from any thread.
pub trait TerminalSource: Send + Sync + 'static {
    /// Latest published frame with its publication generation. A cheap read
    /// (an `Arc` clone under a short lock); never parses or copies cells.
    fn frame(&self) -> (u64, Arc<RenderFrame>);

    /// Wake `waker` once after the next publication. See the type docs.
    fn subscribe(&self, waker: &Waker);

    /// Already-encoded terminal input: key, paste, mouse report, focus report
    /// or committed IME text. Queued in order; never written twice.
    fn send_input(&self, bytes: Vec<u8>);

    /// Request a new grid size, including the cell's pixel size. The session
    /// forwards it under the daemon's resize-ownership rules; the published
    /// frame keeps the canonical size until the daemon approves a change.
    /// The view re-sends its laid-out size with input while the frame size
    /// differs (input makes it the owner), so repeating the latest request
    /// must be cheap and must not queue duplicate resize records.
    fn resize(&self, size: Size);

    /// Move this source's local viewport into (positive) or out of
    /// (negative) scrollback. The viewport is client-local presentation state
    /// and is never sent to the daemon.
    fn scroll_display(&self, lines: i32);

    /// Current terminal modes; the input encoders need them for each event.
    fn modes(&self) -> TermMode;

    /// Text between two grid points (inclusive, either order), joining soft
    /// wraps without a newline. Lines are grid-relative: scrollback is
    /// negative and the first visible row at the bottom of history is zero.
    /// Same semantics as [`fl_term::Engine::text_between`].
    fn text_between(&self, start: TextPoint, end: TextPoint) -> String;

    /// At most `count` (≤ 1000) consecutive lines starting at grid line
    /// `start`, clamped to the retained history. Same semantics as
    /// [`fl_term::Engine::history_page`].
    fn history_page(&self, start: i32, count: u16) -> Vec<HistoryLine>;

    /// The frame is a cached or disconnected copy that may lag the daemon.
    fn stale(&self) -> bool;

    /// The child process exited; the last frame stays readable.
    fn exited(&self) -> Option<Exited>;

    /// Canonical PTY echo state, when the session tracks it. `None` disables
    /// predictive echo: guessing on a password prompt would print secrets.
    fn echo(&self) -> Option<bool> {
        None
    }

    /// Current control-link round-trip time, for `EchoPolicy::Auto`.
    fn rtt(&self) -> Option<std::time::Duration> {
        None
    }
}

/// Resolves once the source has published past generation `seen`; the
/// view awaits it on the UI executor, so no thread polls for frames.
pub(crate) struct Publication {
    source: Arc<dyn TerminalSource>,
    seen: u64,
}

impl Publication {
    pub(crate) fn new(source: Arc<dyn TerminalSource>, seen: u64) -> Self {
        Self { source, seen }
    }
}

impl Future for Publication {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        // Subscribe before reading so a publication racing with the read
        // still wakes this task.
        self.source.subscribe(cx.waker());
        if self.source.frame().0 != self.seen {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;

    use fl_term::{EffectMode, Engine};

    use super::*;

    /// A mirror publishing frames from a real engine, as the session does.
    struct EngineSource {
        state: Mutex<(Engine, u64, Arc<RenderFrame>, Vec<Waker>)>,
    }

    impl EngineSource {
        fn new() -> Arc<Self> {
            let engine = Engine::new(Size::new(20, 4), EffectMode::Mirror).unwrap();
            let mut frame = RenderFrame::default();
            engine.render_into(&mut frame);
            Arc::new(Self {
                state: Mutex::new((engine, 0, Arc::new(frame), Vec::new())),
            })
        }

        /// Parse output off the UI thread and publish a new frame.
        fn output(&self, bytes: &[u8]) {
            let mut state = self.state.lock().unwrap();
            let offset = state.0.offset();
            state.0.feed(offset, bytes).unwrap();
            let mut frame = RenderFrame::default();
            state.0.render_into(&mut frame);
            state.1 += 1;
            state.2 = Arc::new(frame);
            for waker in state.3.drain(..) {
                waker.wake();
            }
        }
    }

    impl TerminalSource for EngineSource {
        fn frame(&self) -> (u64, Arc<RenderFrame>) {
            let state = self.state.lock().unwrap();
            (state.1, state.2.clone())
        }
        fn subscribe(&self, waker: &Waker) {
            let wakers = &mut self.state.lock().unwrap().3;
            if !wakers.iter().any(|w| w.will_wake(waker)) {
                wakers.push(waker.clone());
            }
        }
        fn send_input(&self, _: Vec<u8>) {}
        fn resize(&self, _: Size) {}
        fn scroll_display(&self, _: i32) {}
        fn modes(&self) -> TermMode {
            self.state.lock().unwrap().0.modes()
        }
        fn text_between(&self, start: TextPoint, end: TextPoint) -> String {
            self.state.lock().unwrap().0.text_between(start, end)
        }
        fn history_page(&self, start: i32, count: u16) -> Vec<HistoryLine> {
            self.state.lock().unwrap().0.history_page(start, count)
        }
        fn stale(&self) -> bool {
            false
        }
        fn exited(&self) -> Option<Exited> {
            None
        }
    }

    struct CountingWaker(AtomicUsize);

    impl Wake for CountingWaker {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn publication_wakes_the_view_once_per_new_frame_without_polling() {
        let source = EngineSource::new();
        let counter = Arc::new(CountingWaker(AtomicUsize::new(0)));
        let waker = Waker::from(counter.clone());
        let mut cx = Context::from_waker(&waker);
        let mut publication = Publication::new(source.clone(), 0);
        assert!(Pin::new(&mut publication).poll(&mut cx).is_pending());
        // Re-polling re-registers the same waker without duplicating it.
        assert!(Pin::new(&mut publication).poll(&mut cx).is_pending());

        // Output parsed on another thread publishes and wakes the task.
        let writer = source.clone();
        std::thread::spawn(move || writer.output(b"hello"))
            .join()
            .unwrap();
        assert_eq!(counter.0.load(Ordering::SeqCst), 1);
        assert!(Pin::new(&mut publication).poll(&mut cx).is_ready());
        let (generation, frame) = source.frame();
        assert_eq!(generation, 1);
        assert_eq!(frame.cells[0].c, 'h');
    }

    #[test]
    fn a_publication_before_the_first_poll_is_not_lost() {
        let source = EngineSource::new();
        let (seen, _) = source.frame();
        source.output(b"x");
        let waker = Waker::from(Arc::new(CountingWaker(AtomicUsize::new(0))));
        let mut publication = Publication::new(source, seen);
        assert!(
            Pin::new(&mut publication)
                .poll(&mut Context::from_waker(&waker))
                .is_ready()
        );
    }
}
