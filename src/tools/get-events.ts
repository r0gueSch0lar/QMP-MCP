import { MCPTool } from 'mcp-framework';
import { z } from 'zod';
import { orchestrator } from '../instance/orchestrator.js';

/** Validated input for {@link GetEventsTool}. */
const schema = z.object({
  since: z
    .number()
    .int()
    .min(0)
    .optional()
    .describe(
      'Cursor to page from: return only events whose `seq` is greater than this. ' +
        'Pass the `cursor` returned by a previous get_events (or wait_for_event) call to fetch ' +
        'only what is new. Omit to get all currently buffered events.',
    ),
});

/**
 * Returns the running Instance's recently buffered QMP async events WITHOUT
 * blocking — the pull half of the Event Buffer contract (issue #12). Cursor-based:
 * the response carries a `cursor` (the latest event sequence number); pass it back
 * as `since` to page forward without missing or repeating events. The buffer is
 * bounded, so an agent that polls slower than events are produced may miss evicted
 * events. Read-only. Fails if no Instance is running. Auto-discovered from
 * `dist/tools`.
 */
export default class GetEventsTool extends MCPTool {
  name = 'get_events';
  description =
    "Return the running QEMU Instance's recently buffered QMP async events (e.g. SHUTDOWN, STOP, " +
    'RESET, POWERDOWN) without blocking. Each event has { seq, event, data?, timestamp? }. The ' +
    'response includes a `cursor`; pass it back as `since` to fetch only newer events. The buffer ' +
    'is bounded (oldest events are evicted when full). For a blocking wait, use wait_for_event. ' +
    'Fails if no Instance is running.';
  schema = schema;
  annotations = {
    title: 'Get Events',
    readOnlyHint: true,
    openWorldHint: false,
  };

  async execute(input: z.infer<typeof schema>): Promise<unknown> {
    return orchestrator.getEvents(input.since);
  }
}
