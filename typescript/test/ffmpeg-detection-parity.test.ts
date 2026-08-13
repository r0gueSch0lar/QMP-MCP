/**
 * Shared parity fixtures (ADR-0012): assert the TypeScript `ffmpeg -encoders` parser reproduces
 * the language-neutral golden corpus at `testdata/ffmpeg-detection/*.json` — the SAME corpus the
 * Rust loader (`rust/tests/ffmpeg_detection_fixtures.rs`) asserts. The recording capability gate
 * (ADR-0017) keys off this parse, so drift here would make the two variants disagree about
 * whether a codec is available in the same ffmpeg build.
 *
 * Each fixture is `{ description?, output, expectedEncoders }`, where `output` is captured
 * `ffmpeg -encoders` text and `expectedEncoders` is the sorted, de-duplicated name set — the
 * Rust side parses into a set, so the corpus pins set semantics, not list order.
 */

import { readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';
import { parseFfmpegEncoders } from '../src/instance/recording.js';

const FIXTURES_DIR = fileURLToPath(new URL('../../testdata/ffmpeg-detection/', import.meta.url));

interface Fixture {
  description?: string;
  output: string;
  expectedEncoders: string[];
}

describe('ffmpeg-detection parity fixtures (ADR-0017 / ADR-0012)', () => {
  const files = readdirSync(FIXTURES_DIR)
    .filter((f) => f.endsWith('.json'))
    .sort();

  it('authors a representative corpus', () => {
    expect(files.length).toBeGreaterThanOrEqual(5);
  });

  for (const file of files) {
    it(`reproduces ${file}`, () => {
      const fixture = JSON.parse(readFileSync(join(FIXTURES_DIR, file), 'utf8')) as Fixture;
      const names = parseFfmpegEncoders(fixture.output);
      expect([...new Set(names)].sort()).toEqual(fixture.expectedEncoders);
    });
  }
});
