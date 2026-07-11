import { MCPTool } from 'mcp-framework';
import { type HardwareSpec, hardwareSpecSchema } from '../instance/hardware-spec.js';
import { orchestrator } from '../instance/orchestrator.js';

/**
 * Builds, launches, and negotiates the QMP Session for a new Instance from a
 * Hardware Spec, bringing it to `RUNNING` and reporting the chosen accelerator.
 * Rejected while an Instance already exists (only one runs at a time). The
 * schema is the validated Hardware Spec; every field has a default, so an empty
 * input launches a sensible minimal machine. Auto-discovered from `dist/tools`.
 */
export default class CreateInstanceTool extends MCPTool {
  name = 'create_instance';
  description =
    'Build and launch the single QEMU Instance from a Hardware Spec and negotiate its QMP session. ' +
    'Reports the chosen accelerator (KVM or TCG). Fails if an Instance already exists. The Guest ' +
    'auto-starts to RUNNING by default; run the server with QMP_MCP_AUTO_START=false to load it ' +
    'PAUSED (CPUs frozen at the -S startup pause for inspection) and start it later with resume_instance.';
  schema = hardwareSpecSchema;
  annotations = {
    title: 'Create Instance',
    readOnlyHint: false,
    destructiveHint: false,
    idempotentHint: false,
    openWorldHint: false,
  };

  async execute(input: HardwareSpec): Promise<unknown> {
    return orchestrator.createInstance(input);
  }
}
