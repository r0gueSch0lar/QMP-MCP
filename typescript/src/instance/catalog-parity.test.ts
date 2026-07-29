import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';

/**
 * The built-in ISO catalog ships as a copy in each variant's build context (the Rust crate
 * embeds its copy via `include_str!`; the TypeScript runtime reads its copy at startup), so the
 * two must stay byte-identical or the servers would offer different downloads. This guards them
 * exactly as the shared golden fixtures guard the argv builders (ADR-0012).
 */
const tsCatalog = fileURLToPath(new URL('../../assets/iso-catalog.json', import.meta.url));
const rustCatalog = fileURLToPath(
  new URL('../../../rust/assets/iso-catalog.json', import.meta.url),
);

describe('ISO catalog parity (ADR-0012/0018)', () => {
  it('the Rust and TypeScript bundled catalogs are byte-identical', () => {
    expect(readFileSync(tsCatalog, 'utf8')).toBe(readFileSync(rustCatalog, 'utf8'));
  });
});
