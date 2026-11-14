#!/usr/bin/env node
// Build the content origin's files (spec/sw.md §1) into packages/sw/content/:
// the worker (sw.js, a module worker), the bootstrap and enrollment pages
// with their scripts and stylesheet, and the wasm the bundles load next to
// themselves. A deployment copies the directory to `[content] dir`, served
// at /_zen/.
//
//   node scripts/build-sw.mjs [--out DIR]
//
// The @zen packages resolve to their sources, as in build-apps.mjs.
import { cp, mkdir, rm } from 'node:fs/promises';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { build } from 'esbuild';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const src = (p) => join(root, 'packages', p);
const web = src('sw/web');
const args = process.argv.slice(2);
const at = args.indexOf('--out');
const out = at >= 0 ? resolve(args[at + 1]) : src('sw/content');

const alias = {
  '@zen/client': src('client/src/index.ts'),
  '@zen/fs': src('fs/src/index.ts'),
  '@zen/sw': src('sw/src/index.ts'),
};

async function main() {
  await rm(out, { recursive: true, force: true });
  await mkdir(out, { recursive: true });
  await build({
    entryPoints: ['sw', 'boot', 'enroll'].map((n) => join(web, `${n}.ts`)),
    outdir: out,
    bundle: true,
    format: 'esm',
    platform: 'browser',
    target: 'es2022',
    sourcemap: true,
    logLevel: 'warning',
    alias,
    // The client imports these only on its Node paths, never reached here.
    external: ['node:*'],
  });
  for (const f of ['boot.html', 'enroll.html', 'pages.css']) await cp(join(web, f), join(out, f));
  await cp(src('zen-wasm/pkg/zen_wasm_bg.wasm'), join(out, 'zen_wasm_bg.wasm'));
  console.log(`built ${out}`);
}

main().catch((e) => {
  console.error(e.message ?? e);
  process.exit(1);
});
