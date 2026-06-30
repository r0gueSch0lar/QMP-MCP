import { MCPTool } from 'mcp-framework';
import { z } from 'zod';
import { orchestrator } from '../instance/orchestrator.js';

/** Validated input for {@link QmpExecuteTool}. */
const schema = z.object({
  command: z
    .string()
    .min(1)
    .describe(
      'The QMP command name to run (e.g. "query-pci", "query-fdsets"). Subject to the Command ' +
        'Policy: a default-safe allowlist with an immutable hard denylist. Dangerous commands ' +
        '(human-monitor-command, migrate, dump-guest-memory, device_add, …) are permanently denied.',
    ),
  arguments: z
    .record(z.unknown())
    .optional()
    .describe("The QMP command's arguments object, if it takes any (the QMP `arguments` field)."),
});

/**
 * The single generic QMP escape hatch (ADR-0003): run an arbitrary QMP command
 * against the running Instance, gated by the Command Policy. The command is
 * checked against the policy BEFORE it can reach the QMP Session — a denied
 * command returns an actionable error and never touches QEMU; hard-denied
 * commands can never be enabled. Requires a running Instance. NOT read-only and
 * open-world (it can run effectively any allowlisted QMP command). Auto-discovered
 * from `dist/tools`.
 */
export default class QmpExecuteTool extends MCPTool {
  name = 'qmp_execute';
  description =
    'Run an arbitrary QMP command against the running QEMU Instance, subject to the Command Policy ' +
    '(a default-safe allowlist plus an immutable hard denylist). Provide the QMP command name and ' +
    'optional arguments object. Dangerous commands (e.g. human-monitor-command, migrate, ' +
    'dump-guest-memory, device_add) are permanently denied and cannot be enabled. Fails if no ' +
    'Instance is running or the command is not permitted.';
  schema = schema;
  annotations = {
    title: 'QMP Execute',
    readOnlyHint: false,
    openWorldHint: true,
  };

  async execute(input: z.infer<typeof schema>): Promise<unknown> {
    return orchestrator.executeCommand(input.command, input.arguments);
  }
}
