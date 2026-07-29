/**
 * Shared parity fixtures (ADR-0012): assert the TypeScript ffmpeg-argv builder reproduces the
 * language-neutral golden corpus at `testdata/ffmpeg-argv/*.json` byte-for-byte — the SAME corpus
 * the Rust loader (`rust/tests/ffmpeg_argv_fixtures.rs`) asserts. Any unintentional drift in the
 * recording argv on either side fails the fixture on whichever implementation changed.
 *
 * This suite lives OUTSIDE `src/` on purpose (mirrors `argv-parity.test.ts`): wired into vitest
 * via a `test/**` include so a fixtures-only change never retriggers the `src/**`-scoped jobs.
 *
 * Each fixture is `{ description?, codec, crf, maxFps, pixfmt, out, expectedArgv }`. `maxFps` is
 * part of the input tuple but must NOT appear in the argv (wallclock timestamps + VFR make it a
 * capture-loop pacing cap only) — the corpus pins exactly that.
 */

import { readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';
import { buildFfmpegArgs } from '../src/instance/recording.js';

const FIXTURES_DIR = fileURLToPath(new URL('../../testdata/ffmpeg-argv/', import.meta.url));

interface Fixture {
  description?: string;
  codec: string;
  crf: number;
  maxFps: number;
  pixfmt: string;
  out: string;
  expectedArgv: string[];
}

describe('ffmpeg-argv parity fixtures (ADR-0017 / ADR-0012)', () => {
  const files = readdirSync(FIXTURES_DIR)
    .filter((f) => f.endsWith('.json'))
    .sort();

  it('authors a representative corpus', () => {
    expect(files.length).toBeGreaterThanOrEqual(5);
  });

  for (const file of files) {
    it(`reproduces ${file}`, () => {
      const fixture = JSON.parse(readFileSync(join(FIXTURES_DIR, file), 'utf8')) as Fixture;
      const argv = buildFfmpegArgs(
        fixture.codec,
        fixture.crf,
        fixture.maxFps,
        fixture.pixfmt,
        fixture.out,
      );
      expect(argv).toEqual(fixture.expectedArgv);
      // maxFps must never leak into the argv as an input frame rate.
      expect(argv).not.toContain('-framerate');
    });
  }
});
