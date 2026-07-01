import { defineConfig } from 'vitest/config';

export default defineConfig({
  test: {
    // `src/**` holds the unit tests; the top-level `test/**` holds cross-cutting
    // suites that must NOT live under `src/` — notably the shared argv parity
    // fixtures (ADR-0012), kept out of `src/` so a fixtures-only change never
    // retriggers the `src/**`-scoped docker-build CI job.
    include: ['src/**/*.test.ts', 'test/**/*.test.ts'],
    environment: 'node',
  },
});
