import { MCPTool } from 'mcp-framework';
import { z } from 'zod';
import { orchestrator } from '../instance/orchestrator.js';

/** Validated input for {@link WriteSerialTool}. */
const schema = z.object({
  data: z
    .string()
    .describe(
      'Raw bytes to write to the guest console. No newline is appended — include \\n to submit a line.',
    ),
  format: z
    .enum(['utf8', 'base64'])
    .default('utf8')
    .describe('Encoding of data: utf8 (default) or base64.'),
});

/**
 * Types raw bytes into the running Guest's Serial Port console (ADR-0015) via QMP `ringbuf-write`.
 * DISABLED by default: the server must run with `QMP_MCP_ALLOW_SERIAL_WRITE=true`, else it fails.
 * Requires `create_instance` with `serial: true`. Fails if no Instance is running. Auto-discovered
 * from `dist/tools`.
 */
export default class WriteSerialTool extends MCPTool {
  name = 'write_serial';
  description =
    "Type raw bytes into the running Guest's Serial Port console (QMP ringbuf-write). DISABLED by " +
    'default: the server must run with QMP_MCP_ALLOW_SERIAL_WRITE=true, else this fails. Requires ' +
    'create_instance serial: true. No newline is appended — include \\n in data to submit a line. ' +
    'format is utf8 (default) or base64. Fails if no Instance is running.';
  schema = schema;
  annotations = {
    title: 'Write Serial',
    readOnlyHint: false,
    destructiveHint: false,
    idempotentHint: false,
    openWorldHint: false,
  };

  async execute(input: z.infer<typeof schema>): Promise<unknown> {
    return orchestrator.writeSerial(input.data, input.format);
  }
}
