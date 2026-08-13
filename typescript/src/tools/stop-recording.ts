import { MCPTool } from 'mcp-framework';
import { z } from 'zod';
import { orchestrator } from '../instance/orchestrator.js';

/**
 * Stops the active Display recording (ADR-0017): aborts the screendump frame loop, ends the
 * ffmpeg input (bounded — force-kills the encoder if it overruns finalize), and awaits the
 * container being finalized so the file on disk is a valid, complete video. Takes no input.
 * Returns the host path, file name, codec, size in bytes (sizeBytes), and duration in
 * milliseconds (durationMs). Fails if no recording is running. Auto-discovered from `dist/tools`.
 */
export default class StopRecordingTool extends MCPTool {
  name = 'stop_recording';
  description =
    "Stop the active Display recording and finalize the video file. Returns the recording's host " +
    'path, file name, codec, size in bytes (sizeBytes), and duration in milliseconds (durationMs). ' +
    'Fails if no recording is running.';
  schema = z.object({});
  annotations = {
    title: 'Stop Recording',
    readOnlyHint: false,
    openWorldHint: false,
  };

  async execute(): Promise<unknown> {
    return orchestrator.stopRecording();
  }
}
