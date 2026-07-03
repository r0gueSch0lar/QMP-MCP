/**
 * Shared parity fixtures (ADR-0012): assert the TypeScript store-name allowlist
 * reproduces the language-neutral golden corpus at `testdata/store-name/*.json`
 * verdict-for-verdict — the SAME corpus the Rust loader
 * (`rust/tests/store_name_fixtures.rs`) asserts. The name allowlist is the
 * security-critical option-injection guard (ADR-0006): a name is a single safe path
 * segment with no comma/`=`/`:`/space/leading-dash that could inject QemuOpts
 * properties downstream. Any drift of that rule on either side fails the fixture on
 * whichever implementation changed.
 *
 * This suite lives OUTSIDE `src/` on purpose: it is wired into vitest via the
 * `test/**` include so a fixtures-only change never retriggers the `src/**`-scoped
 * docker-build CI job (nor is it typechecked/linted as `src/` is).
 *
 * The corpus exercises the pure, filesystem-free name rule ONLY, via the Image
 * Store's `assertValidImageName` — the very same allowlist the ISO Store shares — so
 * the reason substrings are store-label-agnostic. Realpath containment stays in each
 * implementation's unit tests, image FORMAT validation needs no fixture (a closed
 * enum on both sides), and the NUL-byte branch (not cleanly representable as JSON) is
 * pinned by a unit test on each side.
 *
 * Each fixture is `{ description?, name, expectedValid, reasonContains? }`.
 */

import { readFileSync, readdirSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';
import { assertValidImageName } from '../src/instance/image-store.js';

const FIXTURES_DIR = fileURLToPath(new URL('../../testdata/store-name/', import.meta.url));

interface Fixture {
  name: string;
  expectedValid: boolean;
  reasonContains?: string[];
}

describe('Store-name allowlist parity fixtures (ADR-0012)', () => {
  const files = readdirSync(FIXTURES_DIR)
    .filter((f) => f.endsWith('.json'))
    .sort();

  it('authors a representative corpus', () => {
    expect(files.length).toBeGreaterThanOrEqual(12);
  });

  for (const file of files) {
    it(`reproduces ${file}`, () => {
      const fixture = JSON.parse(readFileSync(`${FIXTURES_DIR}${file}`, 'utf8')) as Fixture;
      const validate = (): void => {
        assertValidImageName(fixture.name);
      };

      if (fixture.expectedValid) {
        expect(validate).not.toThrow();
        expect(fixture.reasonContains ?? []).toHaveLength(0);
      } else {
        expect(validate).toThrow();
        for (const needle of fixture.reasonContains ?? []) {
          expect(validate).toThrow(needle);
        }
      }
    });
  }
});
