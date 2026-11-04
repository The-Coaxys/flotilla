//! Ordered terminal input with exactly-once delivery.
//!
//! At most one input per terminal is outstanding. Keystrokes typed while it is
//! unacknowledged coalesce (per view) into the next input, so throughput stays
//! one 64 KiB input per round trip while recovery stays exact: after any link
//! change the single outstanding input is resent with its original identity,
//! bytes and absolute deadline. The process host deduplicates by `InputId`,
//! writes only the uncommitted suffix and never writes after the original
//! deadline, so a replay cannot repeat or resurrect input. A deeper pipeline
//! could not distinguish "committed" from "expired" for an input superseded
//! on the daemon while its acknowledgment was lost.
//!
//! Queued input expires `INPUT_TTL` after it was typed and is never sent after
//! an outage of `INPUT_TTL` or longer; both cases are reported as discards.
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use fl_model::{ClientSessionId, InputId, ViewId};
use fl_proto::InputAck;
use fl_proto::validation::MAX_INPUT_BYTES;

use crate::clock::DaemonClock;

/// Input never reaches a PTY later than this after it was typed. Equal to
/// the daemon's admission limit for absolute deadlines.
pub const INPUT_TTL: Duration = Duration::from_secs(5);
/// Unsent input retained per terminal (outstanding input excluded).
pub const MAX_QUEUED_INPUT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscardReason {
    /// Not written before its deadline.
    Expired,
    /// The connection was down for at least `INPUT_TTL`.
    Outage { duration: Duration },
    /// The terminal's child process was replaced or removed.
    IncarnationEnded,
    /// The unsent queue is full.
    QueueFull,
    /// The child has exited; the PTY accepts no input.
    Exited,
    /// The daemon refused the input.
    Rejected(String),
    /// The mirror was detached while input was outstanding.
    Detached,
}

/// Bytes that will never be written: `committed` leading bytes of the input
/// reached the PTY before it was discarded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discard {
    pub bytes: usize,
    pub committed: usize,
    pub reason: DiscardReason,
}

struct Queued {
    view: ViewId,
    bytes: Vec<u8>,
    typed: Instant,
    /// Daemon-clock deadline fixed when typed, if a clock sample existed. It
    /// keeps a local suspend (which can stop `Instant`) from extending the TTL.
    deadline_ns: Option<u64>,
}

/// The single outstanding input.
#[derive(Debug, Clone)]
pub(crate) struct InFlight {
    pub input: InputId,
    pub view: ViewId,
    pub bytes: Vec<u8>,
    pub deadline_ns: u64,
    pub accepted: u32,
}

pub(crate) enum AckOutcome {
    /// Every byte is committed.
    Delivered(InFlight),
    /// The deadline passed after `accepted` bytes; the suffix is discarded.
    Expired(Discard),
    /// The daemon reported progress without finishing: resend the same input.
    Partial,
    /// Not the outstanding input (stale or duplicate acknowledgment).
    Ignored,
}

#[derive(Default)]
pub(crate) struct InputQueue {
    in_flight: Option<InFlight>,
    queue: VecDeque<Queued>,
    queued_bytes: usize,
}

impl InputQueue {
    /// Bytes not yet acknowledged, including the outstanding input.
    pub fn pending_bytes(&self) -> usize {
        self.queued_bytes + self.in_flight.as_ref().map_or(0, |f| f.bytes.len())
    }

    pub fn in_flight(&self) -> Option<&InFlight> {
        self.in_flight.as_ref()
    }

    pub fn has_queued(&self) -> bool {
        !self.queue.is_empty()
    }

    /// Queue `bytes` typed at `now`, coalescing with the last unsent input of
    /// the same view. On overflow nothing is queued.
    pub fn push(
        &mut self,
        view: ViewId,
        mut bytes: &[u8],
        now: Instant,
        clock: Option<&DaemonClock>,
    ) -> Result<(), Discard> {
        if bytes.is_empty() {
            return Ok(());
        }
        if self.queued_bytes + bytes.len() > MAX_QUEUED_INPUT_BYTES {
            return Err(Discard {
                bytes: bytes.len(),
                committed: 0,
                reason: DiscardReason::QueueFull,
            });
        }
        self.queued_bytes += bytes.len();
        if let Some(last) = self.queue.back_mut()
            && last.view == view
            && last.bytes.len() < MAX_INPUT_BYTES
        {
            // The older entry's deadline also bounds the appended bytes.
            let room = MAX_INPUT_BYTES - last.bytes.len();
            let (head, rest) = bytes.split_at(room.min(bytes.len()));
            last.bytes.extend_from_slice(head);
            bytes = rest;
        }
        let deadline_ns = clock.map(|clock| clock.lower_bound(now + INPUT_TTL));
        for chunk in bytes.chunks(MAX_INPUT_BYTES) {
            self.queue.push_back(Queued {
                view,
                bytes: chunk.to_vec(),
                typed: now,
                deadline_ns,
            });
        }
        Ok(())
    }

    /// The next input to send, if none is outstanding. Expired entries are
    /// returned as discards (in order) before it. `sequence` is shared by every
    /// terminal of one client session, so sequences only ever increase.
    pub fn next(
        &mut self,
        now: Instant,
        clock: &DaemonClock,
        client: ClientSessionId,
        sequence: &AtomicU64,
        discards: &mut Vec<Discard>,
    ) -> Option<&InFlight> {
        if self.in_flight.is_some() {
            return None;
        }
        let daemon_now = clock.lower_bound(now);
        while let Some(entry) = self.queue.pop_front() {
            self.queued_bytes -= entry.bytes.len();
            let local_deadline = entry.typed + INPUT_TTL;
            let expired = now >= local_deadline
                || entry
                    .deadline_ns
                    .is_some_and(|deadline| daemon_now >= deadline);
            if expired {
                push_discard(discards, entry.bytes.len(), DiscardReason::Expired);
                continue;
            }
            // Never later than intended, and within the daemon's admission
            // limit of five seconds after the moment it receives the input.
            let deadline_ns = clock
                .lower_bound(local_deadline)
                .min(entry.deadline_ns.unwrap_or(u64::MAX))
                .min(daemon_now.saturating_add(INPUT_TTL.as_nanos() as u64));
            self.in_flight = Some(InFlight {
                input: InputId {
                    client,
                    sequence: sequence.fetch_add(1, Ordering::Relaxed) + 1,
                },
                view: entry.view,
                bytes: entry.bytes,
                deadline_ns,
                accepted: 0,
            });
            return self.in_flight.as_ref();
        }
        None
    }

    pub fn on_ack(&mut self, ack: &InputAck) -> AckOutcome {
        let Some(flight) = self.in_flight.as_mut() else {
            return AckOutcome::Ignored;
        };
        if ack
            .validate_for(flight.input, flight.bytes.len(), flight.accepted)
            .is_err()
        {
            return AckOutcome::Ignored;
        }
        flight.accepted = ack.accepted_bytes;
        if ack.accepted_bytes as usize == flight.bytes.len() {
            return AckOutcome::Delivered(self.in_flight.take().expect("checked above"));
        }
        if ack.expired {
            let flight = self.in_flight.take().expect("checked above");
            return AckOutcome::Expired(Discard {
                bytes: flight.bytes.len(),
                committed: flight.accepted as usize,
                reason: DiscardReason::Expired,
            });
        }
        AckOutcome::Partial
    }

    /// The daemon refused the outstanding input.
    pub fn reject_in_flight(&mut self, reason: DiscardReason) -> Option<Discard> {
        self.in_flight.take().map(|flight| Discard {
            bytes: flight.bytes.len(),
            committed: flight.accepted as usize,
            reason,
        })
    }

    /// Drop unsent input; the outstanding input stays to be resolved by resend.
    pub fn discard_queued(&mut self, reason: DiscardReason) -> Option<Discard> {
        if self.queue.is_empty() {
            return None;
        }
        let bytes = self.queued_bytes;
        self.queue.clear();
        self.queued_bytes = 0;
        Some(Discard {
            bytes,
            committed: 0,
            reason,
        })
    }

    /// Drop everything, e.g. when the incarnation ended.
    pub fn discard_all(&mut self, reason: DiscardReason) -> Vec<Discard> {
        let mut discards = Vec::new();
        if let Some(discard) = self.reject_in_flight(reason.clone()) {
            discards.push(discard);
        }
        discards.extend(self.discard_queued(reason));
        discards
    }
}

fn push_discard(discards: &mut Vec<Discard>, bytes: usize, reason: DiscardReason) {
    if let Some(last) = discards.last_mut()
        && last.reason == reason
        && last.committed == 0
    {
        last.bytes += bytes;
        return;
    }
    discards.push(Discard {
        bytes,
        committed: 0,
        reason,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_model::{IncarnationId, LogCursor, TerminalId};

    fn ack(flight: &InFlight, accepted: usize, expired: bool) -> InputAck {
        InputAck {
            id: 1,
            terminal: TerminalId::new(),
            incarnation: IncarnationId::new(),
            input: flight.input,
            accepted_bytes: accepted as u32,
            echo: true,
            cursor: LogCursor::default(),
            expired,
        }
    }

    #[test]
    fn one_outstanding_input_and_per_view_coalescing() {
        let now = Instant::now();
        let clock = DaemonClock::new(1_000_000_000_000, now);
        let client = ClientSessionId::new();
        let (a, b) = (ViewId::new(), ViewId::new());
        let mut queue = InputQueue::default();
        let sequence = AtomicU64::new(0);
        let mut discards = Vec::new();
        queue.push(a, b"ec", now, Some(&clock)).unwrap();
        let first = queue
            .next(now, &clock, client, &sequence, &mut discards)
            .unwrap()
            .clone();
        assert_eq!(first.bytes, b"ec");
        assert_eq!(first.input.sequence, 1);
        queue.push(a, b"ho", now, Some(&clock)).unwrap();
        queue.push(a, b" hi", now, Some(&clock)).unwrap();
        queue.push(b, b"\n", now, Some(&clock)).unwrap();
        assert!(
            queue
                .next(now, &clock, client, &sequence, &mut discards)
                .is_none()
        );
        assert_eq!(queue.pending_bytes(), 8);
        assert!(matches!(
            queue.on_ack(&ack(&first, 2, false)),
            AckOutcome::Delivered(_)
        ));
        let second = queue
            .next(now, &clock, client, &sequence, &mut discards)
            .unwrap()
            .clone();
        assert_eq!((second.bytes.as_slice(), second.view), (&b"ho hi"[..], a));
        assert_eq!(second.input.sequence, 2);
        // A partial acknowledgment keeps the same identity for a resend.
        assert!(matches!(
            queue.on_ack(&ack(&second, 3, false)),
            AckOutcome::Partial
        ));
        assert_eq!(queue.in_flight().unwrap().accepted, 3);
        // A regressing prefix or a foreign input is ignored.
        assert!(matches!(
            queue.on_ack(&ack(&second, 2, false)),
            AckOutcome::Ignored
        ));
        assert!(matches!(
            queue.on_ack(&ack(&first, 2, false)),
            AckOutcome::Ignored
        ));
        match queue.on_ack(&ack(&second, 4, true)) {
            AckOutcome::Expired(discard) => {
                assert_eq!((discard.bytes, discard.committed), (5, 4))
            }
            _ => panic!("expected expiry"),
        }
        let third = queue
            .next(now, &clock, client, &sequence, &mut discards)
            .unwrap();
        assert_eq!((third.bytes.as_slice(), third.view), (&b"\n"[..], b));
        assert!(discards.is_empty());
    }

    #[test]
    fn deadlines_are_conservative_and_expired_input_is_discarded() {
        let typed = Instant::now();
        let clock = DaemonClock::new(7_000_000_000, typed);
        let client = ClientSessionId::new();
        let view = ViewId::new();
        let mut queue = InputQueue::default();
        let sequence = AtomicU64::new(0);
        let mut discards = Vec::new();
        queue.push(view, b"late", typed, Some(&clock)).unwrap();
        let sent = typed + Duration::from_secs(2);
        let flight = queue
            .next(sent, &clock, client, &sequence, &mut discards)
            .unwrap();
        assert!(flight.deadline_ns <= 7_000_000_000 + 5_000_000_000);
        assert!(flight.deadline_ns > 7_000_000_000 + 4_990_000_000);
        queue.reject_in_flight(DiscardReason::Detached);

        queue.push(view, b"old", typed, Some(&clock)).unwrap();
        queue
            .push(ViewId::new(), b"older", typed, Some(&clock))
            .unwrap();
        let after = typed + INPUT_TTL;
        assert!(
            queue
                .next(after, &clock, client, &sequence, &mut discards)
                .is_none()
        );
        assert_eq!(
            discards,
            vec![Discard {
                bytes: 8,
                committed: 0,
                reason: DiscardReason::Expired
            }]
        );
        assert_eq!(queue.pending_bytes(), 0);
    }

    #[test]
    fn a_daemon_clock_deadline_survives_a_stopped_local_clock() {
        let typed = Instant::now();
        let before = DaemonClock::new(1_000_000_000, typed);
        let mut queue = InputQueue::default();
        queue
            .push(ViewId::new(), b"x", typed, Some(&before))
            .unwrap();
        // The host slept for an hour; `Instant` did not advance, the daemon's
        // boot clock did. A fresh sample reveals the expiry.
        let after = DaemonClock::new(1_000_000_000 + 3_600_000_000_000, typed);
        let mut discards = Vec::new();
        assert!(
            queue
                .next(
                    typed,
                    &after,
                    ClientSessionId::new(),
                    &AtomicU64::new(0),
                    &mut discards
                )
                .is_none()
        );
        assert_eq!(discards[0].reason, DiscardReason::Expired);
    }

    #[test]
    fn queue_is_bounded_and_large_input_is_split() {
        let now = Instant::now();
        let mut queue = InputQueue::default();
        let view = ViewId::new();
        let big = vec![b'x'; MAX_INPUT_BYTES * 2 + 10];
        queue.push(view, &big, now, None).unwrap();
        assert_eq!(queue.queue.len(), 3);
        let rest = vec![b'y'; MAX_QUEUED_INPUT_BYTES - big.len()];
        queue.push(view, &rest, now, None).unwrap();
        let overflow = queue.push(view, b"z", now, None).unwrap_err();
        assert_eq!(overflow.reason, DiscardReason::QueueFull);
        assert_eq!(queue.pending_bytes(), MAX_QUEUED_INPUT_BYTES);
        let discarded = queue
            .discard_queued(DiscardReason::Outage {
                duration: Duration::from_secs(6),
            })
            .unwrap();
        assert_eq!(discarded.bytes, MAX_QUEUED_INPUT_BYTES);
        assert_eq!(queue.pending_bytes(), 0);
    }
}
