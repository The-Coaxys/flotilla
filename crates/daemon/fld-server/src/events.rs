//! Push events to every authenticated control session.
//!
//! Each session has a bounded queue. A session that does not drain it loses
//! events instead of slowing the daemon; events are hints and a reconnect
//! refetches everything.
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use fl_model::workspace::DaemonEvent;
use tokio::sync::mpsc;

/// Events buffered per session before newer ones are dropped.
pub(crate) const SESSION_EVENT_QUEUE: usize = 256;

#[derive(Default)]
pub(crate) struct EventBus {
    next: AtomicU64,
    sessions: Mutex<HashMap<u64, mpsc::Sender<DaemonEvent>>>,
}

/// Unregisters its session when dropped.
pub(crate) struct Subscription<'a> {
    bus: &'a EventBus,
    id: u64,
    pub(crate) events: mpsc::Receiver<DaemonEvent>,
}

impl EventBus {
    pub(crate) fn subscribe(&self) -> Subscription<'_> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (sender, events) = mpsc::channel(SESSION_EVENT_QUEUE);
        self.sessions
            .lock()
            .expect("event sessions poisoned")
            .insert(id, sender);
        Subscription {
            bus: self,
            id,
            events,
        }
    }

    /// Never waits: a full session queue drops this event for that session.
    pub(crate) fn publish(&self, event: DaemonEvent) {
        let sessions = self.sessions.lock().expect("event sessions poisoned");
        for (id, sender) in sessions.iter() {
            if let Err(mpsc::error::TrySendError::Full(_)) = sender.try_send(event.clone()) {
                tracing::debug!(session = id, "event queue full; dropping event");
            }
        }
    }
}

impl Drop for Subscription<'_> {
    fn drop(&mut self) {
        self.bus
            .sessions
            .lock()
            .expect("event sessions poisoned")
            .remove(&self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_session_queue_drops_events_without_affecting_other_sessions() {
        let bus = EventBus::default();
        let mut slow = bus.subscribe();
        let mut fast = bus.subscribe();
        for rev in 0..(SESSION_EVENT_QUEUE as u64 + 10) {
            bus.publish(DaemonEvent::WorkspaceTreeChanged { rev });
            if let Ok(DaemonEvent::WorkspaceTreeChanged { rev: got }) = fast.events.try_recv() {
                assert_eq!(got, rev);
            } else {
                panic!("fast session missed event {rev}");
            }
        }
        let mut received = 0;
        while slow.events.try_recv().is_ok() {
            received += 1;
        }
        assert_eq!(received, SESSION_EVENT_QUEUE);
        drop(slow);
        assert_eq!(bus.sessions.lock().unwrap().len(), 1);
    }
}
