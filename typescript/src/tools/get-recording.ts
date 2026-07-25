import { MCPTool } from 'mcp-framework';
import { z } from 'zod';
import { orchestrator } from '../instance/orchestrator.js';

/**
 * Reports the Display-recording capability and state (ADR-0017): whether recording is available
 * and why, the resolved codec/CRF/max-fps/pixel-format, which known codecs the ffmpeg build
 * carries, and whether a recording is currently active. Takes no input. Read-only — never changes
 * VM state. Recording itself is started/stopped with start_recording/stop_recording.
 * Auto-discovered from `dist/tools`.
 */
export default class GetRecordingTool extends MCPTool {
  name = 'get_recording';
  description =
    'Report the Display-recording capability and state: whether recording is available and why ' +
    '(ffmpeg present, recording directory configured, selected codec compiled in), the resolved ' +
    'codec/CRF/max-fps/pixel-format, which known codecs the ffmpeg build carries, and whether a ' +
    'recording is active. Start/stop recording with start_recording/stop_recording. Read-only.';
  schema = z.object({});
  annotations = {
    title: 'Get Recording',
    readOnlyHint: true,
    openWorldHint: false,
  };

  async execute(): Promise<unknown> {
    return orchestrator.describeRecording();
  }
}
