#!/usr/bin/env node
// Build zen-connect (packages/svc/src/connect-cli.ts) into one file,
// packages/svc/dist/zen-connect.mjs: @zen/client from its sources, as for
// the apps' workers, so it needs no other package built. @zen/wasm loads
// from node_modules (it finds its .wasm file there).
//
//   node scripts/build-zen-connect.mjs
import { chmod } from 'node:fs/promises';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { build } from 'esbuild';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const out = join(root, 'packages/svc/dist/zen-connect.mjs');

await build({
  entryPoints: [join(root, 'packages/svc/src/connect-cli.ts')],
  outfile: out,
  bundle: true,
  format: 'esm',
  platform: 'node',
  target: 'node22',
  sourcemap: true,
  logLevel: 'warning',
  alias: {
    '@zen/client/node': join(root, 'packages/client/src/node.ts'),
    '@zen/client': join(root, 'packages/client/src/index.ts'),
  },
  external: ['node:*', '@zen/wasm'],
});
await chmod(out, 0o755);
console.log('built packages/svc/dist/zen-connect.mjs');
