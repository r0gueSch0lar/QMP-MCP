import { MCPTool } from 'mcp-framework';
import { z } from 'zod';
import { orchestrator } from '../instance/orchestrator.js';

/**
 * Resumes a paused Instance's Guest CPUs by issuing QMP `cont`, moving the
 * lifecycle PAUSED → RUNNING (reflected by get_status). Takes no input. Fails if
 * no Instance is running. Auto-discovered from `dist/tools`.
 */
export default class ResumeInstanceTool extends MCPTool {
  name = 'resume_instance';
  description =
    "Resume the paused QEMU Instance's Guest CPUs (QMP cont), moving its lifecycle state back to " +
    'RUNNING. Fails if no Instance is running.';
  schema = z.object({});
  annotations = {
    title: 'Resume Instance',
    readOnlyHint: false,
    destructiveHint: false,
    idempotentHint: true,
    openWorldHint: false,
  };

  async execute(): Promise<unknown> {
    return orchestrator.resumeInstance();
  }
}
