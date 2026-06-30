import { readdirSync, readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';

/**
 * Guards the operator-facing `.env.example` against drift: every `QMP_MCP_*`
 * environment variable the source actually reads must be documented there, so the
 * file stays an exhaustive, copy-pasteable reference rather than rotting behind the
 * code. A new env var added to a future slice fails this test until it is listed.
 */

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = dirname(here);
const ENV_VAR = /QMP_MCP_[A-Z0-9_]+/g;

/** Recursively collect non-test `.ts` source files under a directory. */
function sourceFiles(dir: string): string[] {
  const out: string[] = [];
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = join(dir, entry.name);
    if (entry.isDirectory()) {
      out.push(...sourceFiles(full));
    } else if (entry.name.endsWith('.ts') && !entry.name.endsWith('.test.ts')) {
      out.push(full);
    }
  }
  return out;
}

/** Every QMP_MCP_* token referenced anywhere in the (non-test) source tree. */
function referencedEnvVars(): Set<string> {
  const vars = new Set<string>();
  for (const file of sourceFiles(here)) {
    for (const match of readFileSync(file, 'utf8').matchAll(ENV_VAR)) {
      vars.add(match[0]);
    }
  }
  return vars;
}

describe('.env.example completeness', () => {
  it('documents every QMP_MCP_* variable the source reads', () => {
    const documented = readFileSync(join(repoRoot, '.env.example'), 'utf8');
    const missing = [...referencedEnvVars()].filter((name) => !documented.includes(name)).sort();
    expect(missing).toEqual([]);
  });
});
