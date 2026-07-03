import { MCPTool } from 'mcp-framework';
import { z } from 'zod';
import { orchestrator } from '../instance/orchestrator.js';

/**
 * Pauses the running Instance's Guest CPUs by issuing QMP `stop`, moving the
 * lifecycle RUNNING → PAUSED (reflected by get_status). Takes no input. Fails if
 * no Instance is running. Reversible with resume_instance. Auto-discovered from
 * `dist/tools`.
 */
export default class PauseInstanceTool extends MCPTool {
  name = 'pause_instance';
  description =
    "Pause the running QEMU Instance's Guest CPUs (QMP stop), moving its lifecycle state to PAUSED. " +
    'Reversible with resume_instance. Fails if no Instance is running.';
  schema = z.object({});
  annotations = {
    title: 'Pause Instance',
    readOnlyHint: false,
    destructiveHint: false,
    idempotentHint: true,
    openWorldHint: false,
  };

  async execute(): Promise<unknown> {
    return orchestrator.pauseInstance();
  }
}
