import { describe, expect, it } from 'vitest';
import type { QmpEvent } from '../qemu/driver.js';
import { DEFAULT_EVENT_BUFFER_SIZE, EventBuffer } from './event-buffer.js';

/** A minimal QMP event with a given name. */
const ev = (event: string, data?: Record<string, unknown>): QmpEvent => ({ event, data });

describe('EventBuffer capacity', () => {
  it('rejects a non-positive or non-integer capacity', () => {
    expect(() => new EventBuffer(0)).toThrow(RangeError);
    expect(() => new EventBuffer(-1)).toThrow(RangeError);
    expect(() => new EventBuffer(1.5)).toThrow(RangeError);
  });

  it('defaults to DEFAULT_EVENT_BUFFER_SIZE', () => {
    expect(new EventBuffer().capacity).toBe(DEFAULT_EVENT_BUFFER_SIZE);
  });
});

describe('EventBuffer append/read (cursor-based)', () => {
  it('stamps events with a monotonic seq and returns them oldest-first', () => {
    const buf = new EventBuffer(8);
    buf.append(ev('STOP'));
    buf.append(ev('RESET', { foo: 1 }));

    const { events, cursor } = buf.read();
    expect(events.map((e) => e.event)).toEqual(['STOP', 'RESET']);
    expect(events.map((e) => e.seq)).toEqual([1, 2]);
    expect(events[1]?.data).toEqual({ foo: 1 });
    expect(cursor).toBe(2);
  });

  it('pages from a cursor: read(since) returns only newer events', () => {
    const buf = new EventBuffer(8);
    buf.append(ev('A'));
    buf.append(ev('B'));
    const first = buf.read();
    expect(first.events.map((e) => e.event)).toEqual(['A', 'B']);

    buf.append(ev('C'));
    const next = buf.read(first.cursor);
    expect(next.events.map((e) => e.event)).toEqual(['C']);
    expect(next.cursor).toBe(3);

    // Nothing new since the latest cursor.
    expect(buf.read(next.cursor).events).toEqual([]);
  });

  it('is bounded: appending past capacity evicts the oldest (FIFO)', () => {
    const buf = new EventBuffer(3);
    for (const name of ['e1', 'e2', 'e3', 'e4', 'e5']) buf.append(ev(name));

    const { events, cursor } = buf.read();
    // Only the last 3 are retained; e1/e2 were evicted.
    expect(events.map((e) => e.event)).toEqual(['e3', 'e4', 'e5']);
    expect(buf.size).toBe(3);
    // seq keeps climbing and the cursor reflects the latest, regardless of eviction.
    expect(events.map((e) => e.seq)).toEqual([3, 4, 5]);
    expect(cursor).toBe(5);
  });
});

describe('EventBuffer waitFor (long-poll)', () => {
  it('resolves on a future matching (filtered) event', async () => {
    const buf = new EventBuffer(8);
    const pending = buf.waitFor({ eventName: 'SHUTDOWN', timeoutMs: 1_000 });
    buf.append(ev('STOP')); // non-matching, ignored
    buf.append(ev('SHUTDOWN', { guest: true }));

    const result = await pending;
    expect(result.timedOut).toBe(false);
    expect(result.event?.event).toBe('SHUTDOWN');
    expect(result.event?.data).toEqual({ guest: true });
    expect(result.cursor).toBe(result.event?.seq);
  });

  it('an unfiltered wait resolves on any event', async () => {
    const buf = new EventBuffer(8);
    const pending = buf.waitFor({ timeoutMs: 1_000 });
    buf.append(ev('RESET'));
    const result = await pending;
    expect(result.timedOut).toBe(false);
    expect(result.event?.event).toBe('RESET');
  });

  it('times out cleanly (no throw) when no matching event arrives', async () => {
    const buf = new EventBuffer(8);
    const pending = buf.waitFor({ eventName: 'SHUTDOWN', timeoutMs: 10 });
    buf.append(ev('STOP')); // never matches the SHUTDOWN filter
    const result = await pending;
    expect(result.timedOut).toBe(true);
    expect(result.event).toBeUndefined();
  });

  it('timeoutMs <= 0 polls without blocking and reports a clean timeout', async () => {
    const buf = new EventBuffer(8);
    const result = await buf.waitFor({ timeoutMs: 0 });
    expect(result.timedOut).toBe(true);
  });

  it('wakes every matching waiter on a single event', async () => {
    const buf = new EventBuffer(8);
    const a = buf.waitFor({ eventName: 'SHUTDOWN', timeoutMs: 1_000 });
    const b = buf.waitFor({ timeoutMs: 1_000 });
    buf.append(ev('SHUTDOWN'));
    const [ra, rb] = await Promise.all([a, b]);
    expect(ra.timedOut).toBe(false);
    expect(rb.timedOut).toBe(false);
  });

  it('is race-safe: sinceCursor replays an already-buffered event (not lost between calls)', async () => {
    const buf = new EventBuffer(8);
    // Event lands BEFORE the wait is issued.
    const landed = buf.append(ev('SHUTDOWN'));
    // A future-only wait would miss it; passing the prior cursor (0) recovers it.
    const result = await buf.waitFor({ eventName: 'SHUTDOWN', timeoutMs: 0, sinceCursor: 0 });
    expect(result.timedOut).toBe(false);
    expect(result.event?.seq).toBe(landed.seq);
  });

  it('sinceCursor does not replay events at or before the cursor', async () => {
    const buf = new EventBuffer(8);
    const first = buf.append(ev('SHUTDOWN'));
    // Caller already saw `first` (cursor == first.seq); a fresh wait must not re-fire on it.
    const result = await buf.waitFor({
      eventName: 'SHUTDOWN',
      timeoutMs: 10,
      sinceCursor: first.seq,
    });
    expect(result.timedOut).toBe(true);
  });
});

describe('EventBuffer reset', () => {
  it('clears events but keeps the cursor monotonic, and settles pending waiters as timeouts', async () => {
    const buf = new EventBuffer(8);
    buf.append(ev('A'));
    buf.append(ev('B'));
    const pending = buf.waitFor({ eventName: 'SHUTDOWN', timeoutMs: 5_000 });

    buf.reset();

    // Buffered events are gone, but seq does not rewind.
    expect(buf.read().events).toEqual([]);
    expect(buf.cursor).toBe(2);
    // The in-flight wait settled cleanly rather than dangling forever.
    const result = await pending;
    expect(result.timedOut).toBe(true);

    // New events continue from the monotonic cursor.
    const next = buf.append(ev('C'));
    expect(next.seq).toBe(3);
  });
});
