import { MCPTool } from 'mcp-framework';
import { z } from 'zod';
import { orchestrator } from '../instance/orchestrator.js';

/** Upper bound on a single long-poll, so a wait can never hang indefinitely. */
const MAX_TIMEOUT_MS = 600_000;

/** Validated input for {@link WaitForEventTool}. */
const schema = z.object({
  eventName: z
    .string()
    .min(1)
    .optional()
    .describe(
      'QMP event name to wait for (e.g. "SHUTDOWN", "POWERDOWN", "RESET", "STOP"). ' +
        'Omit to resolve on the next event of any kind.',
    ),
  timeoutMs: z
    .number()
    .int()
    .min(0)
    .max(MAX_TIMEOUT_MS)
    .default(30_000)
    .describe(
      'How long to wait before returning a timed-out result (default 30000, max 600000). ' +
        'A timeout is a normal outcome, not an error. 0 checks without blocking.',
    ),
  sinceCursor: z
    .number()
    .int()
    .min(0)
    .optional()
    .describe(
      'Make the wait race-safe: also resolve on an already-buffered event whose `seq` is greater ' +
        'than this cursor (from a prior get_events/wait_for_event), so an event that arrived between ' +
        'calls is not missed. Omit for future-only (events arriving after this call).',
    ),
});

/**
 * Long-polls for a matching QMP async event — the blocking half of the Event
 * Buffer contract (issue #12). Resolves with the first matching event, or with a
 * `{ timedOut: true }` result once `timeoutMs` elapses (a timeout is a NORMAL
 * outcome, never an error). Ideal for "did the Guest finish booting / shut down
 * yet?". Pass `sinceCursor` to make it race-safe against events that landed
 * between calls; omit it for future-only. Fails only if no Instance is running.
 * Auto-discovered from `dist/tools`.
 */
export default class WaitForEventTool extends MCPTool {
  name = 'wait_for_event';
  description =
    'Block until the running QEMU Instance emits a matching QMP async event, then return it; or ' +
    'return { timedOut: true } if none arrives within timeoutMs (a timeout is a normal result, not ' +
    'an error). Provide eventName to filter (e.g. "SHUTDOWN"), or omit it to wait for any event. ' +
    'Pass sinceCursor (a prior cursor) to also catch events already buffered since then, so nothing ' +
    'is missed between calls. Useful for "has the Guest booted/shut down yet?". Fails if no Instance ' +
    'is running.';
  schema = schema;
  annotations = {
    title: 'Wait For Event',
    readOnlyHint: true,
    openWorldHint: false,
  };

  async execute(input: z.infer<typeof schema>): Promise<unknown> {
    return orchestrator.waitForEvent({
      eventName: input.eventName,
      timeoutMs: input.timeoutMs,
      sinceCursor: input.sinceCursor,
    });
  }
}
