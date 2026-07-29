import { describe, expect, it } from 'vitest';
import { findEntry, idsHint, loadCatalog, parseCatalog } from './catalog.js';

describe('ISO catalog (ADR-0018)', () => {
  it('the built-in catalog parses and covers the expected distros', () => {
    const catalog = loadCatalog(undefined);
    expect(catalog.source).toBe('built-in');
    for (const id of [
      'ubuntu',
      'linuxmint',
      'zorinos',
      'popos',
      'elementaryos',
      'fedora',
      'opensuse',
      'archlinux',
      'manjaro',
      'endeavouros',
      'gentoo',
      'nixos',
      'voidlinux',
      'slackware',
      'debian',
      'rhel',
      'cachyos',
      'rockylinux',
      'alpine',
      'lubuntu',
      'xubuntu',
      'bodhi',
      'tails',
      'qubesos',
    ]) {
      expect(findEntry(catalog, id), `built-in catalog missing id ${id}`).toBeDefined();
    }
    expect(catalog.entries.length).toBe(24);
    for (const entry of catalog.entries) {
      expect(entry.mirrors.length).toBeGreaterThanOrEqual(1);
    }
  });

  it('rejects malformed, empty, duplicate, and unsafe entries', () => {
    expect(() => parseCatalog('{ not json', 't')).toThrowError(/not valid JSON/);
    expect(() => parseCatalog('{"isos":[]}', 't')).toThrowError(/no entries/);
    const dup =
      '{"isos":[{"id":"a","name":"A","filename":"a.iso","mirrors":["https://x/a.iso"]},' +
      '{"id":"a","name":"B","filename":"b.iso","mirrors":["https://x/b.iso"]}]}';
    expect(() => parseCatalog(dup, 't')).toThrowError(/duplicate id/);
    // A traversing filename is rejected by the shared store-name allowlist.
    const badFile =
      '{"isos":[{"id":"a","name":"A","filename":"../escape.iso","mirrors":["https://x/a.iso"]}]}';
    expect(() => parseCatalog(badFile, 't')).toThrowError(/invalid filename/);
    // A non-http mirror is rejected.
    const badUrl =
      '{"isos":[{"id":"a","name":"A","filename":"a.iso","mirrors":["file:///etc/passwd"]}]}';
    expect(() => parseCatalog(badUrl, 't')).toThrowError(/invalid mirror URL/);
  });

  it('finds entries and lists ids', () => {
    const catalog = loadCatalog(undefined);
    expect(findEntry(catalog, 'archlinux')?.name).toBe('Arch Linux');
    expect(findEntry(catalog, 'nope')).toBeUndefined();
    expect(idsHint(catalog)).toContain('ubuntu');
  });
});
