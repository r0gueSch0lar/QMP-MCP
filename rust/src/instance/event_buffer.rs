//! The Event Buffer (CONTEXT.md): a bounded, server-side ring buffer of the current
//! Instance's recent QMP async events (issue #12, slice #24). The agent reads it
//! PULL-style — [`EventBuffer::read`] (non-blocking, behind `get_events`) and
//! [`EventBuffer::wait_for`] (long-poll, behind `wait_for_event`). A pushed MCP
//! notification, if ever added, is a secondary surface; these pull methods are the
//! contract.
//!
//! A second implementation of the shared bounded context, mirroring
//! `../../src/instance/event-buffer.ts` behaviorally: the same cursor semantics, the
//! same FIFO overflow, the same race-safe long-poll, and the same clean-timeout
//! outcome (a timeout is never an error).
//!
//! Read model — CURSOR-BASED. Every captured event is stamped with a monotonic,
//! server-lifetime sequence number (`seq`). [`read`](EventBuffer::read) with a
//! `since` returns the buffered events with `seq > since` (or all of them when
//! `since` is `None`) plus a `cursor` (the latest `seq` assigned). The agent pages
//! by passing the previous `cursor` back as `since`. The cursor is monotonic across
//! Instances (it never resets on [`reset`](EventBuffer::reset)), so a stale cursor
//! can only ever *skip already-seen* events — never hide a current Instance's event.
//!
//! Bounded memory — the buffer holds at most `capacity` events; appending past
//! capacity evicts the oldest (FIFO). An agent that polls slower than events are
//! produced may therefore miss evicted events (a gap the monotonic cursor makes
//! visible: the oldest retained `seq` may exceed `since + 1`).
//!
//! Concurrency model (idiomatic tokio, not the TS single-threaded loop): the buffer
//! is internally synchronised behind a `std::sync::Mutex` (never held across an
//! `.await`), so it can live in an `Arc<EventBuffer>` shared between the Orchestrator
//! (which reads it under the orchestrator lock) and the background feeder task that
//! appends the live QMP events. A long-poll registers a waiter synchronously, then
//! awaits a [`oneshot`] woken by a matching [`append`](EventBuffer::append) — or a
//! timeout — without holding any lock.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::oneshot;

use crate::qemu::qmp_client::QmpEvent;

/// The default Event Buffer capacity when none is configured (`QMP_MCP_EVENT_BUFFER_SIZE`,
/// issue #12). Re-exported through [`crate::config::DEFAULT_EVENT_BUFFER_SIZE`], which
/// holds the canonical value the config resolver defaults to.
pub const DEFAULT_EVENT_BUFFER_SIZE: usize = crate::config::DEFAULT_EVENT_BUFFER_SIZE as usize;

/// A captured QMP async event, stamped with its buffer sequence number. Serialises to
/// the `{ seq, event, data?, timestamp? }` shape the `get_events` / `wait_for_event`
/// tools return, matching the TS `BufferedEvent`.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct BufferedEvent {
    /// Monotonic, server-lifetime sequence number — the cursor `get_events` /
    /// `wait_for_event` page by. Strictly increasing; never reused, even after
    /// eviction or a [`reset`](EventBuffer::reset).
    pub seq: u64,
    /// The QMP event name (e.g. `SHUTDOWN`, `STOP`, `RESET`, `POWERDOWN`).
    pub event: String,
    /// The event's QMP `data` payload, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    /// The QMP `{seconds, microseconds}` timestamp, if QEMU supplied one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<Value>,
}

/// The result of [`EventBuffer::read`] (and the `get_events` tool): the matching
/// buffered events oldest-first, plus the latest cursor to page from.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct ReadResult {
    /// The matching buffered events, oldest first.
    pub events: Vec<BufferedEvent>,
    /// The latest `seq` assigned so far. Pass this back as `since` on the next
    /// `get_events` (or as `sinceCursor` to `wait_for_event`) to page forward without
    /// missing or re-seeing events.
    pub cursor: u64,
}

/// Input to [`EventBuffer::wait_for`], mirroring the TS `WaitForEventOptions`.
#[derive(Debug, Clone)]
pub struct WaitForEventOptions {
    /// Only resolve on this QMP event name; `None` resolves on the next event of any
    /// kind.
    pub event_name: Option<String>,
    /// How long to wait before resolving as timed out. [`Duration::ZERO`] polls
    /// without blocking.
    pub timeout: Duration,
    /// Race-safe replay: also consider events ALREADY buffered with `seq > since_cursor`,
    /// so an event that arrived between the agent's last read and this wait is not lost.
    /// `None` = future events only (from the moment of the call).
    pub since_cursor: Option<u64>,
}

/// The result of [`EventBuffer::wait_for`] (and the `wait_for_event` tool). Serialises
/// to `{ timedOut, event?, cursor }`, matching the TS `WaitForEventResult`.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WaitForEventResult {
    /// True when no matching event arrived within the timeout — a NORMAL outcome, not
    /// an error.
    pub timed_out: bool,
    /// The matching event, present iff `timed_out` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event: Option<BufferedEvent>,
    /// The latest `seq`, so the agent can resume paging after a match or a timeout.
    pub cursor: u64,
}

/// A future that resolves a long-poll: `Send + 'static` (it captures only owned
/// state), so the Orchestrator can register the waiter under its lock, return this,
/// and let the caller `.await` it after the lock is released.
pub type WaitFuture = Pin<Box<dyn Future<Output = WaitForEventResult> + Send>>;

/// An in-flight long-poll, resolvable by a matching [`append`](EventBuffer::append) or
/// dropped (settling as a clean timeout) by [`reset`](EventBuffer::reset) or the
/// waiter's own timeout.
struct Waiter {
    /// Identifies this waiter so its own timeout branch can deregister it.
    id: u64,
    /// The event name filter; `None` matches any event.
    event_name: Option<String>,
    /// Woken with the matching event; dropping it (reset / removal) wakes the waiter
    /// as a clean timeout.
    sender: oneshot::Sender<BufferedEvent>,
}

/// Mutable inner state, guarded by a single non-async mutex. Never locked across an
/// `.await`, so it is a plain `std::sync::Mutex`.
struct Inner {
    /// Buffered events, oldest first. Length never exceeds `capacity`.
    events: VecDeque<BufferedEvent>,
    /// Last assigned sequence number; monotonic for the buffer's whole lifetime.
    seq: u64,
    /// The registered long-polls awaiting a match.
    waiters: Vec<Waiter>,
}

/// A bounded ring buffer of [`BufferedEvent`]s with cursor-based reads and a race-safe
/// long-poll. Shared as an `Arc<EventBuffer>` between the Orchestrator and the feeder
/// task; every method is internally synchronised.
pub struct EventBuffer {
    /// The retained-event count ceiling; appending past it evicts the oldest (FIFO).
    capacity: usize,
    /// The guarded ring + cursor + waiters, in an `Arc` so a [`WaitFuture`] can hold a
    /// clone to read the current cursor / deregister on timeout.
    inner: Arc<Mutex<Inner>>,
    /// Source of unique waiter ids.
    next_waiter_id: AtomicU64,
}

impl EventBuffer {
    /// Construct an Event Buffer holding at most `capacity` events. Panics if
    /// `capacity` is zero — the config resolver already guarantees a positive integer
    /// (`QMP_MCP_EVENT_BUFFER_SIZE`), so this only fires on a programming error, and it
    /// mirrors the TS constructor's `RangeError` invariant.
    pub fn new(capacity: usize) -> Self {
        assert!(
            capacity >= 1,
            "EventBuffer capacity must be an integer >= 1 (got {capacity})."
        );
        Self {
            capacity,
            inner: Arc::new(Mutex::new(Inner {
                events: VecDeque::new(),
                seq: 0,
                waiters: Vec::new(),
            })),
            next_waiter_id: AtomicU64::new(1),
        }
    }

    /// The retained-event ceiling.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// The latest sequence number assigned (the cursor a fresh read returns).
    pub fn cursor(&self) -> u64 {
        self.lock().seq
    }

    /// Number of events currently retained (<= [`capacity`](Self::capacity)).
    pub fn size(&self) -> usize {
        self.lock().events.len()
    }

    /// Capture a QMP event: stamp it with the next `seq`, append it, evict the oldest
    /// if over capacity, and wake every waiter it matches. Returns the buffered form.
    pub fn append(&self, event: QmpEvent) -> BufferedEvent {
        let mut inner = self.lock();
        inner.seq += 1;
        let buffered = BufferedEvent {
            seq: inner.seq,
            event: event.event,
            data: event.data,
            timestamp: event.timestamp,
        };
        inner.events.push_back(buffered.clone());
        // Bound memory: evict oldest beyond capacity (FIFO ring).
        while inner.events.len() > self.capacity {
            inner.events.pop_front();
        }
        // Wake ALL matching waiters (e.g. several agents awaiting SHUTDOWN). Remove
        // each matched waiter as it settles; `send` consumes its sender.
        let mut i = 0;
        while i < inner.waiters.len() {
            if event_matches(&inner.waiters[i].event_name, &buffered) {
                let waiter = inner.waiters.remove(i);
                // The receiver may already be gone (caller dropped the wait); ignore.
                let _ = waiter.sender.send(buffered.clone());
            } else {
                i += 1;
            }
        }
        buffered
    }

    /// Return the buffered events with `seq > since` (or all of them when `since` is
    /// `None`), oldest first, plus the latest cursor. Never blocks.
    pub fn read(&self, since: Option<u64>) -> ReadResult {
        let inner = self.lock();
        let events = match since {
            None => inner.events.iter().cloned().collect(),
            Some(since) => inner
                .events
                .iter()
                .filter(|e| e.seq > since)
                .cloned()
                .collect(),
        };
        ReadResult {
            events,
            cursor: inner.seq,
        }
    }

    /// Long-poll for a matching event. Registers synchronously (so it can be called
    /// under the orchestrator lock) and returns a [`WaitFuture`] that resolves — never
    /// rejects — with `{ timed_out: false, event }` on the first matching event, or
    /// `{ timed_out: true }` once the timeout elapses (a timeout is a NORMAL result).
    ///
    /// Race-safe: when `since_cursor` is given, an already-buffered matching event
    /// newer than it satisfies the wait immediately, so an event arriving between the
    /// agent's last read and this call is not lost. Without `since_cursor` the wait is
    /// future-only (from this moment). A zero timeout polls without ever registering a
    /// waiter.
    pub fn wait_for(&self, opts: WaitForEventOptions) -> WaitFuture {
        let mut inner = self.lock();

        // Replay already-buffered events so a cursor-based caller cannot miss one that
        // landed between its last read and now.
        if let Some(since) = opts.since_cursor {
            if let Some(hit) = inner
                .events
                .iter()
                .find(|e| e.seq > since && event_matches(&opts.event_name, e))
            {
                let hit = hit.clone();
                return Box::pin(async move {
                    WaitForEventResult {
                        timed_out: false,
                        cursor: hit.seq,
                        event: Some(hit),
                    }
                });
            }
        }

        // Non-blocking poll: nothing buffered matched, so report a clean timeout
        // without ever registering a waiter.
        if opts.timeout.is_zero() {
            let cursor = inner.seq;
            return Box::pin(async move {
                WaitForEventResult {
                    timed_out: true,
                    cursor,
                    event: None,
                }
            });
        }

        let id = self.next_waiter_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        inner.waiters.push(Waiter {
            id,
            event_name: opts.event_name,
            sender,
        });
        drop(inner);

        let timeout = opts.timeout;
        let shared = Arc::clone(&self.inner);
        Box::pin(async move {
            tokio::select! {
                // Bias the match arm so a same-instant match wins over the timeout.
                biased;
                recv = receiver => match recv {
                    Ok(event) => WaitForEventResult {
                        timed_out: false,
                        cursor: event.seq,
                        event: Some(event),
                    },
                    // Sender dropped (reset / removal): settle as a clean timeout.
                    Err(_) => {
                        let cursor = shared.lock().expect("event buffer mutex").seq;
                        WaitForEventResult { timed_out: true, cursor, event: None }
                    }
                },
                _ = tokio::time::sleep(timeout) => {
                    let mut guard = shared.lock().expect("event buffer mutex");
                    guard.waiters.retain(|w| w.id != id);
                    WaitForEventResult { timed_out: true, cursor: guard.seq, event: None }
                }
            }
        })
    }

    /// Drop all buffered events and settle every pending [`wait_for`](Self::wait_for)
    /// as a clean timeout. Called on each Instance create/destroy so events never bleed
    /// across Instances and no long-poll is left dangling. The monotonic cursor is
    /// deliberately NOT reset.
    pub fn reset(&self) {
        let mut inner = self.lock();
        inner.events.clear();
        // Dropping each waiter's sender wakes its future as a clean timeout.
        inner.waiters.clear();
    }

    /// Lock the inner state. The guard is never held across an `.await`, so a poisoned
    /// mutex is unreachable in practice; we surface it as a panic with a clear message.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("event buffer mutex")
    }
}

/// Whether `event` satisfies a name filter: an absent filter matches any event.
fn event_matches(filter: &Option<String>, event: &BufferedEvent) -> bool {
    match filter {
        None => true,
        Some(name) => event.event == *name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal QMP event with a given name (and optional data).
    fn ev(event: &str) -> QmpEvent {
        QmpEvent {
            event: event.to_string(),
            data: None,
            timestamp: None,
        }
    }

    fn ev_data(event: &str, data: Value) -> QmpEvent {
        QmpEvent {
            event: event.to_string(),
            data: Some(data),
            timestamp: None,
        }
    }

    #[test]
    #[should_panic(expected = "must be an integer >= 1")]
    fn rejects_a_zero_capacity() {
        let _ = EventBuffer::new(0);
    }

    #[test]
    fn defaults_constant_matches_the_config_default() {
        assert_eq!(DEFAULT_EVENT_BUFFER_SIZE, 256);
    }

    #[test]
    fn stamps_events_with_a_monotonic_seq_oldest_first() {
        let buf = EventBuffer::new(8);
        buf.append(ev("STOP"));
        buf.append(ev_data("RESET", serde_json::json!({ "foo": 1 })));

        let ReadResult { events, cursor } = buf.read(None);
        assert_eq!(
            events.iter().map(|e| e.event.as_str()).collect::<Vec<_>>(),
            ["STOP", "RESET"]
        );
        assert_eq!(events.iter().map(|e| e.seq).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(events[1].data, Some(serde_json::json!({ "foo": 1 })));
        assert_eq!(cursor, 2);
    }

    #[test]
    fn pages_from_a_cursor_returning_only_newer_events() {
        let buf = EventBuffer::new(8);
        buf.append(ev("A"));
        buf.append(ev("B"));
        let first = buf.read(None);
        assert_eq!(
            first
                .events
                .iter()
                .map(|e| e.event.as_str())
                .collect::<Vec<_>>(),
            ["A", "B"]
        );

        buf.append(ev("C"));
        let next = buf.read(Some(first.cursor));
        assert_eq!(
            next.events
                .iter()
                .map(|e| e.event.as_str())
                .collect::<Vec<_>>(),
            ["C"]
        );
        assert_eq!(next.cursor, 3);

        // Nothing new since the latest cursor.
        assert!(buf.read(Some(next.cursor)).events.is_empty());
    }

    #[test]
    fn is_bounded_evicting_the_oldest_fifo_past_capacity() {
        let buf = EventBuffer::new(3);
        for name in ["e1", "e2", "e3", "e4", "e5"] {
            buf.append(ev(name));
        }

        let ReadResult { events, cursor } = buf.read(None);
        // Only the last 3 are retained; e1/e2 were evicted.
        assert_eq!(
            events.iter().map(|e| e.event.as_str()).collect::<Vec<_>>(),
            ["e3", "e4", "e5"]
        );
        assert_eq!(buf.size(), 3);
        // seq keeps climbing and the cursor reflects the latest, regardless of eviction.
        assert_eq!(events.iter().map(|e| e.seq).collect::<Vec<_>>(), [3, 4, 5]);
        assert_eq!(cursor, 5);
    }

    #[test]
    fn overflow_leaves_a_visible_gap_the_cursor_exposes() {
        // A caller paging from an old cursor that has fallen off the back gets only
        // the retained events; the oldest retained seq exceeds `since + 1`, so the gap
        // is visible rather than hidden — matching the TS overflow contract.
        let buf = EventBuffer::new(2);
        for name in ["a", "b", "c", "d"] {
            buf.append(ev(name));
        }
        // The caller last saw seq 1 ("a"); seq 2 ("b") was also evicted.
        let result = buf.read(Some(1));
        assert_eq!(
            result.events.iter().map(|e| e.seq).collect::<Vec<_>>(),
            [3, 4]
        );
        // The gap is observable: the oldest retained seq (3) is greater than since+1 (2).
        assert_eq!(result.events.first().unwrap().seq, 3);
        assert_eq!(result.cursor, 4);
    }

    #[tokio::test]
    async fn wait_for_resolves_on_a_future_matching_filtered_event() {
        let buf = EventBuffer::new(8);
        let pending = buf.wait_for(WaitForEventOptions {
            event_name: Some("SHUTDOWN".to_string()),
            timeout: Duration::from_secs(1),
            since_cursor: None,
        });
        buf.append(ev("STOP")); // non-matching, ignored
        buf.append(ev_data("SHUTDOWN", serde_json::json!({ "guest": true })));

        let result = pending.await;
        assert!(!result.timed_out);
        let event = result.event.expect("a matching event");
        assert_eq!(event.event, "SHUTDOWN");
        assert_eq!(event.data, Some(serde_json::json!({ "guest": true })));
        assert_eq!(result.cursor, event.seq);
    }

    #[tokio::test]
    async fn an_unfiltered_wait_resolves_on_any_event() {
        let buf = EventBuffer::new(8);
        let pending = buf.wait_for(WaitForEventOptions {
            event_name: None,
            timeout: Duration::from_secs(1),
            since_cursor: None,
        });
        buf.append(ev("RESET"));
        let result = pending.await;
        assert!(!result.timed_out);
        assert_eq!(result.event.unwrap().event, "RESET");
    }

    #[tokio::test]
    async fn times_out_cleanly_when_no_matching_event_arrives() {
        let buf = EventBuffer::new(8);
        let pending = buf.wait_for(WaitForEventOptions {
            event_name: Some("SHUTDOWN".to_string()),
            timeout: Duration::from_millis(20),
            since_cursor: None,
        });
        buf.append(ev("STOP")); // never matches the SHUTDOWN filter
        let result = pending.await;
        assert!(result.timed_out);
        assert!(result.event.is_none());
    }

    #[tokio::test]
    async fn zero_timeout_polls_without_blocking_reporting_a_clean_timeout() {
        let buf = EventBuffer::new(8);
        let result = buf
            .wait_for(WaitForEventOptions {
                event_name: None,
                timeout: Duration::ZERO,
                since_cursor: None,
            })
            .await;
        assert!(result.timed_out);
    }

    #[tokio::test]
    async fn wakes_every_matching_waiter_on_a_single_event() {
        let buf = EventBuffer::new(8);
        let a = buf.wait_for(WaitForEventOptions {
            event_name: Some("SHUTDOWN".to_string()),
            timeout: Duration::from_secs(1),
            since_cursor: None,
        });
        let b = buf.wait_for(WaitForEventOptions {
            event_name: None,
            timeout: Duration::from_secs(1),
            since_cursor: None,
        });
        buf.append(ev("SHUTDOWN"));
        let (ra, rb) = tokio::join!(a, b);
        assert!(!ra.timed_out);
        assert!(!rb.timed_out);
    }

    #[tokio::test]
    async fn since_cursor_replays_an_already_buffered_event() {
        let buf = EventBuffer::new(8);
        // Event lands BEFORE the wait is issued.
        let landed = buf.append(ev("SHUTDOWN"));
        // A future-only wait would miss it; passing the prior cursor (0) recovers it.
        let result = buf
            .wait_for(WaitForEventOptions {
                event_name: Some("SHUTDOWN".to_string()),
                timeout: Duration::ZERO,
                since_cursor: Some(0),
            })
            .await;
        assert!(!result.timed_out);
        assert_eq!(result.event.unwrap().seq, landed.seq);
    }

    #[tokio::test]
    async fn since_cursor_does_not_replay_events_at_or_before_the_cursor() {
        let buf = EventBuffer::new(8);
        let first = buf.append(ev("SHUTDOWN"));
        // Caller already saw `first` (cursor == first.seq); a fresh wait must not
        // re-fire on it.
        let result = buf
            .wait_for(WaitForEventOptions {
                event_name: Some("SHUTDOWN".to_string()),
                timeout: Duration::from_millis(20),
                since_cursor: Some(first.seq),
            })
            .await;
        assert!(result.timed_out);
    }

    #[tokio::test]
    async fn reset_clears_events_keeps_the_cursor_and_settles_waiters() {
        let buf = EventBuffer::new(8);
        buf.append(ev("A"));
        buf.append(ev("B"));
        let pending = buf.wait_for(WaitForEventOptions {
            event_name: Some("SHUTDOWN".to_string()),
            timeout: Duration::from_secs(5),
            since_cursor: None,
        });

        buf.reset();

        // Buffered events are gone, but seq does not rewind.
        assert!(buf.read(None).events.is_empty());
        assert_eq!(buf.cursor(), 2);
        // The in-flight wait settled cleanly rather than dangling forever.
        let result = pending.await;
        assert!(result.timed_out);

        // New events continue from the monotonic cursor.
        let next = buf.append(ev("C"));
        assert_eq!(next.seq, 3);
    }

    #[test]
    fn serialises_to_the_expected_wire_shape() {
        let buf = EventBuffer::new(8);
        buf.append(ev_data("SHUTDOWN", serde_json::json!({ "guest": true })));
        let read = buf.read(None);
        let json = serde_json::to_value(&read).unwrap();
        assert_eq!(json["cursor"], 1);
        assert_eq!(json["events"][0]["seq"], 1);
        assert_eq!(json["events"][0]["event"], "SHUTDOWN");
        assert_eq!(json["events"][0]["data"]["guest"], true);
        // Absent optional fields are omitted, not null.
        assert!(json["events"][0].get("timestamp").is_none());

        let wait = WaitForEventResult {
            timed_out: true,
            event: None,
            cursor: 1,
        };
        let json = serde_json::to_value(&wait).unwrap();
        assert_eq!(json["timedOut"], true);
        assert_eq!(json["cursor"], 1);
        assert!(json.get("event").is_none());
    }
}
