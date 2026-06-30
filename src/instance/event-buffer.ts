/**
 * The Event Buffer (CONTEXT.md): a bounded, server-side ring buffer of the
 * current Instance's recent QMP async events. The agent reads it PULL-style —
 * {@link EventBuffer.read} (non-blocking, behind `get_events`) and
 * {@link EventBuffer.waitFor} (long-poll, behind `wait_for_event`). A pushed MCP
 * notification, if ever added, is a secondary surface; these pull methods are the
 * contract.
 *
 * Read model — CURSOR-BASED. Every captured event is stamped with a monotonic,
 * server-lifetime sequence number (`seq`). `read(since)` returns the buffered
 * events with `seq > since` (or all of them when `since` is omitted) plus a
 * `cursor` (the latest `seq` assigned). The agent pages by passing the previous
 * `cursor` back as `since`. The cursor is monotonic across Instances (it never
 * resets on {@link reset}), so a stale cursor can only ever *skip already-seen*
 * events — never hide a current Instance's event.
 *
 * Bounded memory — the buffer holds at most `capacity` events; appending past
 * capacity evicts the oldest (FIFO). An agent that polls slower than events are
 * produced may therefore miss evicted events (a gap the monotonic cursor makes
 * visible: the oldest retained `seq` may be greater than `since + 1`).
 *
 * Single Instance — the Orchestrator owns one EventBuffer for the server's
 * lifetime and {@link reset}s it on every create/destroy, so events never bleed
 * across Instances. Pull access is additionally gated by the Orchestrator on a
 * running Instance.
 */

import type { QmpEvent } from '../qemu/driver.js';

/** A captured QMP async event, stamped with its buffer sequence number. */
export interface BufferedEvent {
  /**
   * Monotonic, server-lifetime sequence number — the cursor `get_events` /
   * `wait_for_event` page by. Strictly increasing; never reused, even after
   * eviction or a {@link EventBuffer.reset}.
   */
  seq: number;
  /** The QMP event name (e.g. `SHUTDOWN`, `STOP`, `RESET`, `POWERDOWN`). */
  event: string;
  /** The event's QMP `data` payload, if any. */
  data?: Record<string, unknown>;
  /** The QMP event timestamp, if QEMU supplied one. */
  timestamp?: { seconds: number; microseconds: number };
}

/** The result of {@link EventBuffer.read} (and the `get_events` tool). */
export interface ReadResult {
  /** The matching buffered events, oldest first. */
  events: BufferedEvent[];
  /**
   * The latest `seq` assigned so far. Pass this back as `since` on the next
   * `get_events` (or as `sinceCursor` to `wait_for_event`) to page forward
   * without missing or re-seeing events.
   */
  cursor: number;
}

/** Input to {@link EventBuffer.waitFor}. */
export interface WaitForEventOptions {
  /** Only resolve on this QMP event name; omitted = resolve on the next event. */
  eventName?: string;
  /** How long to wait before resolving as timed out. <= 0 polls without blocking. */
  timeoutMs: number;
  /**
   * Race-safe replay: also consider events ALREADY buffered with `seq > sinceCursor`.
   * An event that arrived between the agent's last read and this wait is therefore
   * NOT lost. Omitted = future events only (from the moment of the call).
   */
  sinceCursor?: number;
}

/** The result of {@link EventBuffer.waitFor} (and the `wait_for_event` tool). */
export interface WaitForEventResult {
  /** True when no matching event arrived within the timeout — a NORMAL outcome. */
  timedOut: boolean;
  /** The matching event, present iff `timedOut` is false. */
  event?: BufferedEvent;
  /** The latest `seq`, so the agent can resume paging after a match or a timeout. */
  cursor: number;
}

/** An in-flight {@link EventBuffer.waitFor}, resolvable by a match or a timeout. */
interface Waiter {
  matches(event: BufferedEvent): boolean;
  /** Resolve with a matching event; removes the waiter and clears its timer. */
  fulfill(event: BufferedEvent): void;
  /** Resolve as timed-out (or Instance-gone); removes the waiter and clears its timer. */
  expire(): void;
}

/** The default Event Buffer capacity when none is configured (issue #12). */
export const DEFAULT_EVENT_BUFFER_SIZE = 256;

/**
 * A bounded ring buffer of {@link BufferedEvent}s with cursor-based reads and a
 * race-safe long-poll. Not concurrency-safe across real threads, but Node is
 * single-threaded so append/read/waitFor interleave only at await points.
 */
export class EventBuffer {
  readonly capacity: number;
  /** Buffered events, oldest first. Length never exceeds {@link capacity}. */
  #events: BufferedEvent[] = [];
  /** Last assigned sequence number; monotonic for the buffer's whole lifetime. */
  #seq = 0;
  #waiters = new Set<Waiter>();

  constructor(capacity: number = DEFAULT_EVENT_BUFFER_SIZE) {
    if (!Number.isInteger(capacity) || capacity < 1) {
      throw new RangeError(`EventBuffer capacity must be an integer >= 1 (got ${capacity}).`);
    }
    this.capacity = capacity;
  }

  /** The latest sequence number assigned (the cursor a fresh read returns). */
  get cursor(): number {
    return this.#seq;
  }

  /** Number of events currently retained (<= {@link capacity}). */
  get size(): number {
    return this.#events.length;
  }

  /**
   * Capture a QMP event: stamp it with the next `seq`, append it, evict the
   * oldest if over capacity, and wake every waiter it matches. Returns the
   * buffered form.
   */
  append(event: QmpEvent): BufferedEvent {
    const buffered: BufferedEvent = {
      seq: ++this.#seq,
      event: event.event,
      data: event.data,
      timestamp: event.timestamp,
    };
    this.#events.push(buffered);
    // Bound memory: evict oldest beyond capacity (FIFO ring).
    while (this.#events.length > this.capacity) this.#events.shift();
    // Wake ALL matching waiters (e.g. several agents awaiting SHUTDOWN). Snapshot
    // first: fulfill() mutates the set as each waiter settles.
    for (const waiter of [...this.#waiters]) {
      if (waiter.matches(buffered)) waiter.fulfill(buffered);
    }
    return buffered;
  }

  /**
   * Return the buffered events with `seq > since` (or all of them when `since`
   * is omitted), oldest first, plus the latest cursor. Never blocks.
   */
  read(since?: number): ReadResult {
    const events =
      since === undefined ? [...this.#events] : this.#events.filter((e) => e.seq > since);
    return { events, cursor: this.#seq };
  }

  /**
   * Long-poll for a matching event. Resolves (never rejects) with
   * `{ timedOut: false, event }` on the first matching event, or
   * `{ timedOut: true }` once `timeoutMs` elapses — a timeout is a normal result.
   *
   * Race-safe: when `sinceCursor` is given, an already-buffered matching event
   * newer than it satisfies the wait immediately, so an event arriving between
   * the agent's last read and this call is not lost. Without `sinceCursor` the
   * wait is future-only (from this moment).
   */
  waitFor(opts: WaitForEventOptions): Promise<WaitForEventResult> {
    const { eventName, sinceCursor, timeoutMs } = opts;
    const matches = (e: BufferedEvent): boolean => eventName === undefined || e.event === eventName;

    // Replay already-buffered events so a cursor-based caller cannot miss one
    // that landed between its last read and now.
    if (sinceCursor !== undefined) {
      const hit = this.#events.find((e) => e.seq > sinceCursor && matches(e));
      if (hit) return Promise.resolve({ timedOut: false, event: hit, cursor: hit.seq });
    }

    // Non-blocking poll (or invalid timeout): nothing buffered matched, so report
    // a clean timeout without ever registering a waiter.
    if (!(timeoutMs > 0)) {
      return Promise.resolve({ timedOut: true, cursor: this.#seq });
    }

    return new Promise<WaitForEventResult>((resolve) => {
      let timer: ReturnType<typeof setTimeout> | undefined;
      const waiter: Waiter = {
        matches,
        fulfill: (event) => {
          if (timer) clearTimeout(timer);
          this.#waiters.delete(waiter);
          resolve({ timedOut: false, event, cursor: event.seq });
        },
        expire: () => {
          if (timer) clearTimeout(timer);
          this.#waiters.delete(waiter);
          resolve({ timedOut: true, cursor: this.#seq });
        },
      };
      this.#waiters.add(waiter);
      timer = setTimeout(() => waiter.expire(), timeoutMs);
      // Don't let a pending long-poll keep the process alive on its own.
      timer.unref();
    });
  }

  /**
   * Drop all buffered events and settle every pending {@link waitFor} as a clean
   * timeout. Called on each Instance create/destroy so events never bleed across
   * Instances and no long-poll is left dangling. The monotonic cursor is
   * deliberately NOT reset.
   */
  reset(): void {
    this.#events = [];
    for (const waiter of [...this.#waiters]) waiter.expire();
  }
}
