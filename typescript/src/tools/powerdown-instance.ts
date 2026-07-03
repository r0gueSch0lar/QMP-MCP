import { MCPTool } from 'mcp-framework';
import { z } from 'zod';
import { orchestrator } from '../instance/orchestrator.js';

/**
 * Requests a graceful Guest shutdown by issuing QMP `system_powerdown` (sends an
 * ACPI power-button event). This only asks the Guest to power off; the Instance
 * keeps running until the Guest acts, so the lifecycle state is unchanged. Takes
 * no input. Fails if no Instance is running. Auto-discovered from `dist/tools`.
 */
export default class PowerdownInstanceTool extends MCPTool {
  name = 'powerdown_instance';
  description =
    'Request a graceful Guest shutdown of the running QEMU Instance via an ACPI power-button event ' +
    '(QMP system_powerdown). The Guest decides when to power off. Fails if no Instance is running.';
  schema = z.object({});
  annotations = {
    title: 'Power Down Instance',
    readOnlyHint: false,
    destructiveHint: true,
    idempotentHint: true,
    openWorldHint: false,
  };

  async execute(): Promise<unknown> {
    return orchestrator.powerdownInstance();
  }
}
