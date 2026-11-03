//! The narrow bridge from a `flc-sync` mirror to a `flc-terminal` view.
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::task::Waker;
use std::time::Duration;

use fl_term::{HistoryLine, RenderFrame, Size, TermMode, TextPoint};
use flc_sync::MirrorHandle;
use flc_terminal::{Exited, TerminalSource};

/// One terminal incarnation as the renderer sees it.
///
/// The mirror owns parsing, input ordering, reconnection and the offline
/// cache; this type only forwards cheap reads and lets the shell publish the
/// link latency that drives `EchoPolicy::Auto`.
#[derive(Debug)]
pub struct MirrorSource {
    handle: MirrorHandle,
    /// Round-trip time in milliseconds, updated by the connection supervisor.
    rtt_ms: AtomicU32,
}

impl MirrorSource {
    pub fn new(handle: MirrorHandle) -> Arc<Self> {
        Arc::new(Self {
            handle,
            rtt_ms: AtomicU32::new(u32::MAX),
        })
    }

    pub fn handle(&self) -> &MirrorHandle {
        &self.handle
    }

    /// Publish the current link latency. `None` clears it, which disables
    /// automatic prediction until a measurement returns.
    pub fn set_rtt(&self, rtt: Option<Duration>) {
        self.rtt_ms.store(
            rtt.map_or(u32::MAX, |rtt| {
                u32::try_from(rtt.as_millis()).unwrap_or(u32::MAX - 1)
            }),
            Ordering::Relaxed,
        );
    }
}

impl TerminalSource for MirrorSource {
    fn frame(&self) -> (u64, Arc<RenderFrame>) {
        self.handle.frame()
    }

    fn subscribe(&self, waker: &Waker) {
        self.handle.subscribe(waker);
    }

    fn send_input(&self, bytes: Vec<u8>) {
        self.handle.send_input(bytes);
    }

    fn resize(&self, size: Size) {
        self.handle.resize(size);
    }

    fn scroll_display(&self, lines: i32) {
        self.handle.scroll_display(lines);
    }

    fn modes(&self) -> TermMode {
        self.handle.modes()
    }

    fn text_between(&self, start: TextPoint, end: TextPoint) -> String {
        self.handle.text_between(start, end)
    }

    fn history_page(&self, start: i32, count: u16) -> Vec<HistoryLine> {
        self.handle.history_page(start, count)
    }

    fn stale(&self) -> bool {
        self.handle.stale()
    }

    fn exited(&self) -> Option<Exited> {
        self.handle
            .exited()
            .map(|status| Exited { code: status.code })
    }

    fn echo(&self) -> Option<bool> {
        Some(self.handle.state().echo)
    }

    fn rtt(&self) -> Option<Duration> {
        match self.rtt_ms.load(Ordering::Relaxed) {
            u32::MAX => None,
            ms => Some(Duration::from_millis(ms.into())),
        }
    }
}
