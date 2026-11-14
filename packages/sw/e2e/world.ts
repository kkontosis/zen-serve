// The browser tests' world (spec/sw.md): one zen-serve on a fixed port P
// answering two hosts, the app origin `http://localhost:P` (its
// `public_origins`, serving the test app from `unencrypted_dir`) and the
// content origin `http://view.localhost:P` (`[content]`, serving a copy of
// packages/sw/content). Chromium treats `*.localhost` as a secure context
// and resolves it to the loopback address, so both are real origins.
//
// fs 1 holds a site under `/site` (a tree of its own), written by the
// admin from Node. This file imports the client by relative path to its
// sources: Playwright has no @zen aliases.
import { cp, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { build } from 'esbuild';
import { hex } from '../../client/src/bytes.js';
import {
  claim,
  connect,
  fullGrants,
  member,
  ROOT,
  Tree,
  type UnlockedFs,
  zw,
} from '../../client/src/index.js';
import {
  freePort,
  spawnServer,
  type TestServer,
  testUser,
} from '../../client/src/testing/index.js';
import { initWasm } from '../../client/src/wasm.js';

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, '../../..');
/** Built by scripts/build-sw.mjs (the Playwright global setup runs it). */
export const CONTENT = join(root, 'packages/sw/content');

/** A 1×1 PNG. */
export const PNG = Uint8Array.from(
  atob(
    'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg==',
  ),
  (c) => c.charCodeAt(0),
);

/** A mono 16-bit WAV of a sine tone, `seconds` long at 22,050 Hz (about 43 KiB a second). */
export function wav(seconds: number): Uint8Array {
  const rate = 22_050;
  const n = rate * seconds;
  const out = new Uint8Array(44 + n * 2);
  const v = new DataView(out.buffer);
  const str = (at: number, s: string) => {
    for (let i = 0; i < s.length; i++) out[at + i] = s.charCodeAt(i);
  };
  str(0, 'RIFF');
  v.setUint32(4, 36 + n * 2, true);
  str(8, 'WAVE');
  str(12, 'fmt ');
  v.setUint32(16, 16, true);
  v.setUint16(20, 1, true); // PCM
  v.setUint16(22, 1, true); // mono
  v.setUint32(24, rate, true);
  v.setUint32(28, rate * 2, true);
  v.setUint16(32, 2, true);
  v.setUint16(34, 16, true);
  str(36, 'data');
  v.setUint32(40, n * 2, true);
  for (let i = 0; i < n; i++) {
    v.setInt16(44 + i * 2, Math.round(Math.sin((2 * Math.PI * 440 * i) / rate) * 8000), true);
  }
  return out;
}

/** The site's files, by path below `/site`. */
export function siteFiles(appOrigin: string): Map<string, Uint8Array> {
  const t = (s: string) => new TextEncoder().encode(s);
  return new Map<string, Uint8Array>([
    [
      'index.html',
      t(
        '<!doctype html><html><head><title>home</title><link rel="stylesheet" href="style.css"></head>' +
          '<body><h1 id="h">home</h1><img id="img" src="img/dot.png" alt=""></body></html>',
      ),
    ],
    ['style.css', t('h1 { color: rgb(1, 2, 3); }')],
    ['img/dot.png', PNG],
    ['docs/a/b/deep.txt', t('deep')],
    ['never.html', t('<!doctype html><p>never read</p>')],
    [
      'scripts.html',
      t(
        '<!doctype html><html><head><title>sandboxed</title><link rel="stylesheet" href="style.css">' +
          '<script>document.title = "inline ran"; window.ran = "inline";</script>' +
          '<script src="x.js"></script></head><body><h1 id="h">scripts</h1>' +
          '<img id="bad" src="missing.png" onerror="document.title = \'onerror ran\'" alt="">' +
          '<img id="local" src="img/dot.png" alt="">' +
          `<img id="remote" src="${appOrigin}/unencrypted/sw-app/dot.png" alt="">` +
          '<a id="js" href="javascript:document.title = \'link ran\'">link</a></body></html>',
      ),
    ],
    ['x.js', t('document.title = "external ran"; window.ran = "external";')],
    [
      'media.html',
      t('<!doctype html><body><audio id="a" src="tone.wav" preload="auto"></audio></body>'),
    ],
    ['tone.wav', wav(72)],
  ]);
}

/** The admin of the test world: a member's seeds as the test app takes them. */
export const ADMIN = {
  identitySeed: Array(32).fill(1),
  deviceSeed: Array(32).fill(1 ^ 0x80),
  certUnix: 1_700_000_000,
};

export interface SwWorld {
  server: TestServer;
  port: number;
  /** `http://localhost:P`. */
  app: string;
  /** `http://view.localhost:P`. */
  content: string;
  /** `http://evil.localhost:P`: an origin neither trusted nor content. */
  evil: string;
  /** The site's tree, hex. */
  tree: string;
  files: Map<string, Uint8Array>;
  /** The copy of the content dir the server serves (the update test edits it). */
  contentDir: string;
  /** The test app's page on `origin`. */
  appPage(origin?: string): string;
  stop(): Promise<void>;
}

async function writeSite(ufs: UnlockedFs, files: Map<string, Uint8Array>): Promise<Uint8Array> {
  const id = Tree.newId();
  const t = ufs.tree(id);
  const dirs = new Map<string, Uint8Array>([['', await t.mkdir(ROOT, 'site')]]);
  const dirOf = async (path: string): Promise<Uint8Array> => {
    let at = '';
    let node = dirs.get('')!;
    for (const seg of path.split('/').slice(0, -1)) {
      at = at ? `${at}/${seg}` : seg;
      let d = dirs.get(at);
      if (!d) {
        d = await t.mkdir(node, seg);
        dirs.set(at, d);
      }
      node = d;
    }
    return node;
  };
  for (const [path, data] of files) {
    const n = await t.create(await dirOf(path), path.split('/').at(-1)!);
    await t.writeFile(n, data);
  }
  return id;
}

/** Start the world. */
export async function startSwWorld(): Promise<SwWorld> {
  await initWasm();
  const port = await freePort();
  const app = `http://localhost:${port}`;
  const content = `http://view.localhost:${port}`;
  const evil = `http://evil.localhost:${port}`;
  const tmp = await mkdtemp(join(tmpdir(), 'zen-sw-e2e-'));
  const contentDir = join(tmp, 'content');
  await cp(CONTENT, contentDir, { recursive: true });
  // The test app, bundled with the @zen sources.
  const appDir = join(tmp, 'unencrypted', 'sw-app');
  const src = (p: string) => join(root, 'packages', p);
  await build({
    entryPoints: [join(here, 'app/main.ts')],
    outfile: join(appDir, 'main.js'),
    bundle: true,
    format: 'esm',
    platform: 'browser',
    target: 'es2022',
    logLevel: 'warning',
    alias: {
      '@zen/client': src('client/src/index.ts'),
      '@zen/fs': src('fs/src/index.ts'),
      '@zen/sw': src('sw/src/index.ts'),
    },
    external: ['node:*'],
  });
  await cp(join(here, 'app/index.html'), join(appDir, 'index.html'));
  await cp(join(CONTENT, 'zen_wasm_bg.wasm'), join(appDir, 'zen_wasm_bg.wasm'));
  await writeFile(join(appDir, 'dot.png'), PNG);

  const server = await spawnServer({
    port,
    top: { public_origins: [app], unencrypted_dir: join(tmp, 'unencrypted') },
    extra: `[content]\norigins = ["${content}"]\ndir = "${contentDir}"\n`,
  });
  const client = await connect(server.url, { origin: app });
  const admin = testUser(1);
  await claim(client, server.claimToken, admin.identity, {
    members: [member(admin.identity, [admin.cert])],
    grants: [...fullGrants(1, admin.fp)],
  });
  const session = await client.signInDevice(admin);
  const ufs = await session.fs(1).init((k) => [zw.createDeviceSlot(k, admin.device.devicePublic)]);
  const files = siteFiles(app);
  const tree = await writeSite(ufs, files);
  ufs.close();
  return {
    server,
    port,
    app,
    content,
    evil,
    tree: hex(tree),
    files,
    contentDir,
    appPage: (origin = app) => `${origin}/unencrypted/sw-app/index.html`,
    async stop() {
      await server.stop();
      await rm(tmp, { recursive: true, force: true });
    },
  };
}

/** Replace `from` with `to` in the served worker (the update test). */
export async function editWorker(w: SwWorld, from: string, to: string): Promise<void> {
  const p = join(w.contentDir, 'sw.js');
  const s = await readFile(p, 'utf8');
  if (!s.includes(from)) throw new Error(`sw.js has no ${from}`);
  await writeFile(p, s.replaceAll(from, to));
}
