/**
 * Shared parity fixtures (ADR-0012): assert the TypeScript `QMP_MCP_LOG_FILTER` parser
 * reproduces the language-neutral golden corpus at `testdata/log-filter/*.json` — the SAME
 * corpus the Rust loader (`rust/tests/log_filter_fixtures.rs`) asserts. Drift here would make
 * the two variants accept different filter strings or emit different per-subsystem levels.
 *
 * Each fixture is `{ description?, filter, expected }` for a parse, or
 * `{ description?, filter, expectError, errorContains }` for a fail-closed rejection whose
 * message must contain every listed substring.
 */

import { readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';
import { parseLogFilter } from '../src/config.js';

const FIXTURES_DIR = fileURLToPath(new URL('../../testdata/log-filter/', import.meta.url));

interface Fixture {
  description?: string;
  filter: string;
  expected?: Record<string, string>;
  expectError?: boolean;
  errorContains?: string[];
}

describe('log-filter parity fixtures (ADR-0012)', () => {
  const files = readdirSync(FIXTURES_DIR)
    .filter((f) => f.endsWith('.json'))
    .sort();

  it('authors a representative corpus', () => {
    expect(files.length).toBeGreaterThanOrEqual(5);
  });

  for (const file of files) {
    it(`reproduces ${file}`, () => {
      const fixture = JSON.parse(readFileSync(join(FIXTURES_DIR, file), 'utf8')) as Fixture;
      if (fixture.expectError) {
        let message = '';
        try {
          parseLogFilter('QMP_MCP_LOG_FILTER', fixture.filter);
        } catch (err) {
          message = err instanceof Error ? err.message : String(err);
        }
        expect(message, 'expected the filter to be rejected').not.toBe('');
        for (const substring of fixture.errorContains ?? []) {
          expect(message).toContain(substring);
        }
      } else {
        expect(parseLogFilter('QMP_MCP_LOG_FILTER', fixture.filter)).toEqual(
          fixture.expected ?? {},
        );
      }
    });
  }
});
