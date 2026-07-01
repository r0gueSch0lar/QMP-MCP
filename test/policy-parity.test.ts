/**
 * Shared parity fixtures (ADR-0012): assert the TypeScript Command Policy reproduces
 * the language-neutral golden corpus at `testdata/policy/*.json` verdict-for-verdict —
 * the SAME corpus the Rust loader (`rust/tests/policy_fixtures.rs`) asserts. Any
 * unintentional policy drift on either side fails the fixture on whichever
 * implementation changed.
 *
 * This suite lives OUTSIDE `src/` on purpose: it is wired into vitest via the
 * `test/**` include so a fixtures-only change never retriggers the `src/**`-scoped
 * docker-build CI job (nor is it typechecked/linted as `src/` is).
 *
 * Each fixture is `{ description?, command, arguments?, config?, expectedVerdict }`:
 *   - `command`     — the QMP command name to decide (may carry stray case/whitespace).
 *   - `arguments`   — informational only: the policy gates NAMES, not arguments, so the
 *                     loader deliberately ignores this. Fixtures carry it to document
 *                     that a dangerous argument never changes a name-based verdict.
 *   - `config`      — optional `{ allow?, deny? }` overrides, representing the resolved
 *                     effect of QMP_MCP_ALLOW/DENY OR the YAML policy file (both feed
 *                     `buildPolicy`). File-specific error handling stays in the
 *                     command-policy unit tests.
 *   - `expectedVerdict` — `{ allowed, command, hardDenied?, reasonContains? }`.
 */

import { readFileSync, readdirSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';
import { buildPolicy, decideCommand } from '../src/policy/command-policy.js';

const FIXTURES_DIR = fileURLToPath(new URL('../testdata/policy/', import.meta.url));

interface Fixture {
  command: string;
  config?: { allow?: string[]; deny?: string[] };
  expectedVerdict: {
    allowed: boolean;
    command: string;
    hardDenied?: boolean;
    reasonContains?: string[];
  };
}

describe('Command Policy parity fixtures (ADR-0012)', () => {
  const files = readdirSync(FIXTURES_DIR)
    .filter((f) => f.endsWith('.json'))
    .sort();

  it('authors a representative corpus', () => {
    expect(files.length).toBeGreaterThanOrEqual(12);
  });

  for (const file of files) {
    it(`reproduces ${file}`, () => {
      const fixture = JSON.parse(readFileSync(`${FIXTURES_DIR}${file}`, 'utf8')) as Fixture;
      const policy = buildPolicy({
        allow: fixture.config?.allow,
        deny: fixture.config?.deny,
      });
      const verdict = decideCommand(policy, fixture.command);
      const expected = fixture.expectedVerdict;

      expect(verdict.allowed).toBe(expected.allowed);
      expect(verdict.command).toBe(expected.command);

      if (!verdict.allowed) {
        if (expected.hardDenied !== undefined) {
          expect(verdict.hardDenied).toBe(expected.hardDenied);
        }
        for (const needle of expected.reasonContains ?? []) {
          expect(verdict.reason).toContain(needle);
        }
      } else {
        expect(expected.reasonContains ?? []).toHaveLength(0);
      }
    });
  }
});
