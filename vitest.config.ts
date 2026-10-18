import { fileURLToPath } from 'node:url';
import { defineConfig } from 'vitest/config';

// Node tests of @zen/client, @zen/fuse, @zen/db, @zen/fs, @zen/loro, @zen/svc and the apps against a spawned embedded zen-serve
// (packages/client/src/testing/server.ts). Build first:
//   cargo build -p zen-server && scripts/build-wasm.sh
export default defineConfig({
  // Tests run on the client's sources, never a stale build of it.
  resolve: {
    alias: {
      '@zen/client/node': fileURLToPath(new URL('packages/client/src/node.ts', import.meta.url)),
      '@zen/client': fileURLToPath(new URL('packages/client/src/index.ts', import.meta.url)),
      '@zen/db': fileURLToPath(new URL('packages/db/src/index.ts', import.meta.url)),
      '@zen/fs': fileURLToPath(new URL('packages/fs/src/index.ts', import.meta.url)),
      '@zen/loro': fileURLToPath(new URL('packages/loro/src/index.ts', import.meta.url)),
      '@zen/svc': fileURLToPath(new URL('packages/svc/src/index.ts', import.meta.url)),
    },
  },
  test: {
    include: ['packages/*/test/**/*.test.ts', 'apps/*/test/**/*.test.ts'],
    testTimeout: 60_000,
    hookTimeout: 120_000,
    pool: 'forks',
  },
});
