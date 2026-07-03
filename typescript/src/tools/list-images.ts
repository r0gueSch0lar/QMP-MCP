import { MCPTool } from 'mcp-framework';
import { z } from 'zod';
import { imageStoreFromEnv } from '../instance/image-store.js';

/**
 * Lists the disk images present in the configured Image Store (ADR-0006),
 * referenced by name. Takes no input. Read-only. Fails closed with an actionable
 * message if the Image Store directory is missing. Auto-discovered from
 * `dist/tools`.
 */
export default class ListImagesTool extends MCPTool {
  name = 'list_images';
  description =
    'List the guest disk images available in the Image Store, by name. These names are what a disk in ' +
    'the Hardware Spec references. Read-only.';
  schema = z.object({});
  annotations = {
    title: 'List Images',
    readOnlyHint: true,
    openWorldHint: false,
  };

  async execute(): Promise<unknown> {
    return imageStoreFromEnv().list();
  }
}
