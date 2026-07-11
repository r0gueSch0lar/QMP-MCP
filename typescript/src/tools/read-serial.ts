import { MCPTool } from 'mcp-framework';
import { z } from 'zod';
import { orchestrator } from '../instance/orchestrator.js';

/** Validated input for {@link ReadSerialTool}. */
const schema = z.object({
  maxBytes: z
    .number()
    .int()
    .min(1)
    .optional()
    .describe(
      'Maximum bytes to return; omit to read up to the full ring-buffer size ' +
        '(QMP_MCP_SERIAL_BUFFER_BYTES).',
    ),
  format: z
    .enum(['utf8', 'base64'])
    .default('utf8')
    .describe('Output encoding: utf8 (default, text) or base64 for non-UTF8 early-boot bytes.'),
});

/**
 * Reads the running Guest's Serial Port output (ADR-0015). Each call DRAINS the ringbuf: it
 * returns the output produced since the last read and clears it (poll it like get_events).
 * Requires `create_instance` with `serial: true`. Does not change VM state. Fails if no Instance
 * is running. Auto-discovered from `dist/tools`.
 */
export default class ReadSerialTool extends MCPTool {
  name = 'read_serial';
  description =
    "Read the running Guest's Serial Port output. Each call DRAINS the ring buffer: it returns the " +
    'output produced since the last read and clears it (poll it like get_events). Requires ' +
    'create_instance serial: true. Optional maxBytes caps the return; format is utf8 (default) or ' +
    'base64 for non-UTF8 bytes. Fails if no Instance is running.';
  schema = schema;
  annotations = {
    title: 'Read Serial',
    readOnlyHint: true,
    openWorldHint: false,
  };

  async execute(input: z.infer<typeof schema>): Promise<unknown> {
    return orchestrator.readSerial(input.maxBytes, input.format);
  }
}
