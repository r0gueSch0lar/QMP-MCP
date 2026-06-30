import { MCPTool } from 'mcp-framework';
import { z } from 'zod';
import { isoStoreFromEnv } from '../instance/iso-store.js';

/**
 * Lists the installation/boot ISO media present in the configured read-only ISO
 * Store (ADR-0006), referenced by name. Takes no input. Read-only. Fails closed
 * with an actionable message naming `QMP_MCP_ISO_DIR` if the ISO Store directory
 * is missing. Auto-discovered from `dist/tools`.
 */
export default class ListIsosTool extends MCPTool {
  name = 'list_isos';
  description =
    'List the installation/boot ISO media available in the read-only ISO Store, by name. These names ' +
    'are what a Hardware Spec cdrom references. Read-only.';
  schema = z.object({});
  annotations = {
    title: 'List ISOs',
    readOnlyHint: true,
    openWorldHint: false,
  };

  async execute(): Promise<unknown> {
    return isoStoreFromEnv().list();
  }
}
