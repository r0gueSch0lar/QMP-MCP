import { MCPTool } from 'mcp-framework';
import { z } from 'zod';
import { orchestrator } from '../instance/orchestrator.js';

/**
 * Hard-resets the running Instance by issuing QMP `system_reset` (equivalent to
 * the reset button): the Guest reboots in place and unsaved Guest state is lost.
 * Does not change the lifecycle state. Takes no input. Fails if no Instance is
 * running. Auto-discovered from `dist/tools`.
 */
export default class ResetInstanceTool extends MCPTool {
  name = 'reset_instance';
  description =
    'Hard-reset the running QEMU Instance (QMP system_reset), rebooting the Guest in place. Unsaved ' +
    'Guest state is lost. Fails if no Instance is running.';
  schema = z.object({});
  annotations = {
    title: 'Reset Instance',
    readOnlyHint: false,
    destructiveHint: true,
    idempotentHint: false,
    openWorldHint: false,
  };

  async execute(): Promise<unknown> {
    return orchestrator.resetInstance();
  }
}
