#!/usr/bin/env node
// Build the example apps (apps/<name>/ui/main.ts) into browser bundles under
// apps/dist/<name>/: main.js, the files of ui/ (index.html, CSS), and the
// WASM modules the bundle loads next to itself. zen-serve serves apps/dist as
// its unencrypted_dir, so an app is at /unencrypted/<name>/index.html.
// Node workers (apps/<name>/workers/*.ts) become apps/dist/<name>/workers/*.mjs.
//
//   node scripts/build-apps.mjs [name…] [--watch]
//
// The @zen packages resolve to their sources, as in vitest. `node:` imports
// are left external: the client imports them only on its Node paths.
import { cp, mkdir, readdir, rm, stat } from 'node:fs/promises';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { context } from 'esbuild';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const apps = join(root, 'apps');
const out = join(apps, 'dist');
const src = (p) => join(root, 'packages', p);

const alias = {
  '@zen/client/node': src('client/src/node.ts'),
  '@zen/client': src('client/src/index.ts'),
  '@zen/db': src('db/src/index.ts'),
  '@zen/fs': src('fs/src/index.ts'),
  '@zen/loro': src('loro/src/index.ts'),
};

// WASM files a bundle fetches relative to its own URL (`new URL(…, import.meta.url)`).
const wasm = [
  { from: src('zen-wasm/pkg/zen_wasm_bg.wasm'), when: 'zen-wasm/pkg/zen_wasm.js' },
  {
    from: join(root, 'node_modules/loro-crdt/browser/loro_wasm_bg.wasm'),
    when: 'loro-crdt/browser/',
  },
];

const exists = (p) =>
  stat(p).then(
    () => true,
    () => false,
  );

async function appNames(only) {
  const names = [];
  for (const e of await readdir(apps, { withFileTypes: true })) {
    if (!e.isDirectory() || e.name === 'dist' || e.name === 'shared') continue;
    if (only.length && !only.includes(e.name)) continue;
    if (
      (await exists(join(apps, e.name, 'ui', 'main.ts'))) ||
      (await exists(join(apps, e.name, 'workers')))
    )
      names.push(e.name);
  }
  return names;
}

/** Copy ui/ (minus TypeScript) and the WASM modules the bundle uses. */
function assets(name) {
  return {
    name: 'zen-assets',
    setup(b) {
      b.onEnd(async (r) => {
        if (r.errors.length) return;
        const dir = join(out, name);
        const ui = join(apps, name, 'ui');
        for (const f of await readdir(ui)) {
          if (!f.endsWith('.ts')) await cp(join(ui, f), join(dir, f), { recursive: true });
        }
        const inputs = Object.keys(r.metafile.inputs).join('\n');
        for (const w of wasm) {
          if (inputs.includes(w.when)) await cp(w.from, join(dir, w.from.split('/').pop()));
        }
        console.log(`built apps/dist/${name}`);
      });
    },
  };
}

async function main() {
  const args = process.argv.slice(2);
  const watch = args.includes('--watch');
  const names = await appNames(args.filter((a) => !a.startsWith('--')));
  if (!names.length) throw new Error('no app to build (apps/<name>/ui/main.ts)');
  const ctxs = [];
  for (const name of names) {
    await rm(join(out, name), { recursive: true, force: true });
    await mkdir(join(out, name), { recursive: true });
    const workers = join(apps, name, 'workers');
    if (await exists(workers)) {
      const entries = (await readdir(workers))
        .filter((f) => f.endsWith('.ts') && !f.endsWith('.d.ts'))
        .map((f) => join(workers, f));
      ctxs.push(
        await context({
          entryPoints: entries,
          outdir: join(out, name, 'workers'),
          outExtension: { '.js': '.mjs' },
          bundle: true,
          format: 'esm',
          platform: 'node',
          target: 'node22',
          sourcemap: true,
          logLevel: 'warning',
          alias,
          // Packages from node_modules load from there (the WASM finds its file).
          external: ['node:*', '@zen/wasm', 'loro-crdt'],
        }),
      );
    }
    if (!(await exists(join(apps, name, 'ui', 'main.ts')))) continue;
    ctxs.push(
      await context({
        entryPoints: [join(apps, name, 'ui', 'main.ts')],
        outfile: join(out, name, 'main.js'),
        bundle: true,
        format: 'esm',
        platform: 'browser',
        target: 'es2022',
        sourcemap: true,
        metafile: true,
        logLevel: 'warning',
        alias,
        external: ['node:*'],
        plugins: [assets(name)],
      }),
    );
  }
  if (watch) {
    await Promise.all(ctxs.map((c) => c.watch()));
    console.log(`watching ${names.join(', ')}`);
    return;
  }
  try {
    await Promise.all(ctxs.map((c) => c.rebuild()));
  } finally {
    await Promise.all(ctxs.map((c) => c.dispose()));
  }
}

main().catch((e) => {
  console.error(e.message ?? e);
  process.exit(1);
});
