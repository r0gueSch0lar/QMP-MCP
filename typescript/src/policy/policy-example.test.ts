import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';
import { loadPolicyFile } from './command-policy.js';

/**
 * Guards the shared, operator-facing `policy.example.yaml` (the Command Policy file format
 * reference): it must always parse cleanly with this implementation's loader, and yield exactly
 * the lists it documents. The Rust suite (`rust/tests/policy_example.rs`) asserts the SAME file
 * with the SAME expectation, so the example cannot drift out of either parser's accepted shape —
 * the ADR-0012 posture applied to the config surface.
 */

const here = dirname(fileURLToPath(import.meta.url));
// This file lives at typescript/src/policy/; the shared example is three levels up at the root.
const EXAMPLE = join(here, '..', '..', '..', 'policy.example.yaml');

describe('policy.example.yaml parity', () => {
  it('parses with the TypeScript loader into exactly the documented lists', () => {
    expect(loadPolicyFile(EXAMPLE)).toEqual({
      allow: ['query-pci', 'query-fdsets', 'query-tpm', 'query-rocker'],
      deny: ['system_powerdown'],
    });
  });
});
