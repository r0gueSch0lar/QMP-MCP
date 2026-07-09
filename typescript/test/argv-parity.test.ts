/**
 * Shared parity fixtures (ADR-0012): assert the TypeScript argv generator
 * reproduces the language-neutral golden corpus at `testdata/argv/*.json`
 * byte-for-byte — the SAME corpus the Rust loader (`rust/tests/argv_fixtures.rs`)
 * asserts. Any unintentional argv drift on either side fails the fixture on
 * whichever implementation changed.
 *
 * This suite lives OUTSIDE `src/` on purpose: it is wired into vitest via a
 * `test/**` include so a fixtures-only change never retriggers the `src/**`-scoped
 * docker-build CI job (nor is it typechecked/linted, which are `src/`-scoped).
 *
 * Each fixture is `{ description?, spec, options, expectedArgv }`. `expectedArgv`
 * uses placeholders for the non-deterministic fragments — `{{QMP_SOCKET}}` for the
 * QMP socket path and `{{IMAGE_DIR}}`/`{{ISO_DIR}}` for the realpath-resolved Store
 * directories — which this loader substitutes back before comparing.
 */

import { mkdtempSync, readFileSync, readdirSync, realpathSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { type Accel, buildArgv, parseHardwareSpec } from '../src/instance/hardware-spec.js';

/** A fixed, deterministic QMP socket path stand-in (interpolated verbatim). */
const SOCKET = '/run/qmp-mcp/qmp.sock';
const FIXTURES_DIR = fileURLToPath(new URL('../../testdata/argv/', import.meta.url));

interface FixtureOptions {
  accel: Accel;
  hostfwdPortRange?: { low: number; high: number };
  allowHostNet?: boolean;
  maxMemoryMb?: number;
  maxVcpus?: number;
  allowRawArgs?: boolean;
  // Guest folder sharing (ADR-0013). hostShareDir is emitted verbatim (not resolved
  // through a Store), so fixtures use a literal path — no placeholder needed.
  hostShareDir?: string;
  shareReadonly?: boolean;
}

interface Fixture {
  spec: unknown;
  options: FixtureOptions;
  expectedArgv: string[];
}

describe('argv parity fixtures (ADR-0012)', () => {
  let imageDir: string;
  let isoDir: string;
  let realImage: string;
  let realIso: string;

  beforeAll(() => {
    // Real, existing Store directories are required because the containment
    // boundary realpath-resolves them; the leaf files need not exist.
    imageDir = mkdtempSync(join(tmpdir(), 'qmp-argv-images-'));
    isoDir = mkdtempSync(join(tmpdir(), 'qmp-argv-isos-'));
    realImage = realpathSync(imageDir);
    realIso = realpathSync(isoDir);
  });

  afterAll(() => {
    rmSync(imageDir, { recursive: true, force: true });
    rmSync(isoDir, { recursive: true, force: true });
  });

  const files = readdirSync(FIXTURES_DIR)
    .filter((f) => f.endsWith('.json'))
    .sort();

  it('authors a representative corpus', () => {
    expect(files.length).toBeGreaterThanOrEqual(12);
  });

  for (const file of files) {
    it(`reproduces ${file}`, () => {
      const fixture = JSON.parse(readFileSync(join(FIXTURES_DIR, file), 'utf8')) as Fixture;
      const spec = parseHardwareSpec(fixture.spec);
      const argv = buildArgv(spec, {
        accel: fixture.options.accel,
        qmpSocketPath: SOCKET,
        imageDir,
        isoDir,
        hostShareDir: fixture.options.hostShareDir,
        shareReadonly: fixture.options.shareReadonly,
        hostfwdPortRange: fixture.options.hostfwdPortRange,
        allowHostNet: fixture.options.allowHostNet,
        maxMemoryMb: fixture.options.maxMemoryMb,
        maxVcpus: fixture.options.maxVcpus,
        allowRawArgs: fixture.options.allowRawArgs,
      });
      const got = argv.map((s) =>
        s
          .replaceAll(realImage, '{{IMAGE_DIR}}')
          .replaceAll(realIso, '{{ISO_DIR}}')
          .replaceAll(SOCKET, '{{QMP_SOCKET}}'),
      );
      expect(got).toEqual(fixture.expectedArgv);
    });
  }
});
