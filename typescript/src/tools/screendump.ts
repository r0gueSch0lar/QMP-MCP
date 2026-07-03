import { MCPTool } from 'mcp-framework';
import { z } from 'zod';
import { orchestrator } from '../instance/orchestrator.js';

/** MCP image content block (what the framework forwards as a tool image result). */
interface ImageContent {
  type: 'image';
  data: string;
  mimeType: string;
}

/**
 * Captures a screenshot of the running Instance's display via QMP `screendump`
 * and returns it as MCP image content (base64 PNG). Takes no input — and notably
 * no path: the destination file is ALWAYS server-chosen (a single-use temp file
 * under a server-controlled directory), read back, and deleted, because QMP
 * screendump writes an arbitrary host file and must never be steered by the
 * agent. Fails if no Instance is running. Auto-discovered from `dist/tools`.
 */
export default class ScreendumpTool extends MCPTool {
  name = 'screendump';
  description =
    "Capture a screenshot of the running QEMU Instance's display and return it as a PNG image (QMP " +
    'screendump to a server-chosen path). Fails if no Instance is running.';
  schema = z.object({});
  annotations = {
    title: 'Screendump',
    readOnlyHint: true,
    openWorldHint: false,
  };

  async execute(): Promise<ImageContent> {
    const { data, mimeType } = await orchestrator.screendump();
    return { type: 'image', data, mimeType };
  }
}
