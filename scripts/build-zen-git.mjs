#!/usr/bin/env node
// Build zen-git's two bins (packages/git/src/cli.ts, git-remote-zen.ts) into
// packages/git/dist/{zen-git,git-remote-zen}.mjs, with a shared chunk:
// @zen/client and @zen/db from their sources, as for zen-connect. @zen/wasm
// loads from node_modules (it finds its .wasm file there). The helper runs
// the hook as dist/zen-git.mjs, beside itself.
//
//   node scripts/build-zen-git.mjs
import { chmod } from 'node:fs/promises';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { build } from 'esbuild';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const dist = join(root, 'packages/git/dist');

await build({
  entryPoints: {
    'zen-git': join(root, 'packages/git/src/cli.ts'),
    'git-remote-zen': join(root, 'packages/git/src/git-remote-zen.ts'),
  },
  outdir: dist,
  outExtension: { '.js': '.mjs' },
  bundle: true,
  splitting: true,
  chunkNames: 'chunk-[hash]',
  format: 'esm',
  platform: 'node',
  target: 'node22',
  sourcemap: true,
  logLevel: 'warning',
  alias: {
    '@zen/client/node': join(root, 'packages/client/src/node.ts'),
    '@zen/client': join(root, 'packages/client/src/index.ts'),
    '@zen/db': join(root, 'packages/db/src/index.ts'),
  },
  external: ['node:*', '@zen/wasm'],
});
for (const b of ['zen-git', 'git-remote-zen']) await chmod(join(dist, `${b}.mjs`), 0o755);
console.log('built packages/git/dist/{zen-git,git-remote-zen}.mjs');
