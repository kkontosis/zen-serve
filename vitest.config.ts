import { fileURLToPath } from 'node:url';
import { defineConfig } from 'vitest/config';

// Node tests of @zen/client and @zen/fuse against a spawned embedded zen-serve
// (packages/client/src/testing/server.ts). Build first:
//   cargo build -p zen-server && scripts/build-wasm.sh
export default defineConfig({
  // Tests run on the client's sources, never a stale build of it.
  resolve: {
    alias: { '@zen/client': fileURLToPath(new URL('packages/client/src/index.ts', import.meta.url)) },
  },
  test: {
    include: ['packages/*/test/**/*.test.ts'],
    testTimeout: 60_000,
    hookTimeout: 120_000,
    pool: 'forks',
  },
});
