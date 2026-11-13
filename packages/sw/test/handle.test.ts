// handle() over a real replica (spec/sw.md §3–§7): files, ranges against a
// model, directories and listings, validators, the error mapping, and the
// version an ETag names.
import {
  bytes,
  IntegrityError,
  memoryLocalStore,
  ROOT,
  sealedStore,
  Tree,
  type UnlockedFs,
  ZenError,
  zw,
} from '@zen/client';
import { encodeXattrs, openReplica, type Replica, type ReplicaOptions } from '@zen/fs';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { type World, world } from '../../client/test/helpers.js';
import { installation, type Net, until } from '../../fs/test/helpers.js';
import {
  etagOf,
  handle,
  type Mount,
  parseMounts,
  SANDBOX_HEADERS,
  type Site,
} from '../src/index.js';

let w: World;
let admin: UnlockedFs;
beforeAll(async () => {
  // Small values: chunks of 1,984 bytes, 127 ids a page, about 55 chunks inline.
  w = await world({ limits: { max_value_bytes: 2048 } });
  admin = await w.session.fs(1).init((k) => [zw.createDeviceSlot(k, w.admin.device.devicePublic)]);
});
afterAll(() => w?.server.stop());

const text = (s: string) => bytes.utf8(s);
const blob = (n: number, seed = 1) =>
  Uint8Array.from({ length: n }, (_, i) => (i * 31 + seed + (i >> 8)) & 0xff);
const solo = (local = sealedStore(memoryLocalStore())): ReplicaOptions => ({
  local,
  locks: null,
  channel: null,
  waitMs: 200,
});
const ct = (type: string) => encodeXattrs(new Map([['zen.content_type', text(type)]]));

const PAGED = 400 * 1984 + 7;
const SMALL = 3 * 1984 + 11;

/** The site's tree: under `/site`, every kind of entry sw.md names. */
async function makeSite(): Promise<{ id: Uint8Array; t: Tree; files: Map<string, Uint8Array> }> {
  const id = Tree.newId();
  const t = admin.tree(id);
  const files = new Map<string, Uint8Array>();
  const site = await t.mkdir(ROOT, 'site');
  const put = async (dir: Uint8Array, path: string, data: Uint8Array, xattrs?: Uint8Array) => {
    const name = path.split('/').at(-1)!;
    const n = await t.create(dir, name, xattrs ? { xattrs } : {});
    await t.writeFile(n, data);
    files.set(path, data);
    return n;
  };
  await put(site, '/site/index.html', text('<h1>home</h1>'));
  await put(site, '/site/small.bin', blob(SMALL));
  await put(site, '/site/paged.bin', blob(PAGED, 3));
  await put(site, '/site/sp ace%.txt', text('spaces'));
  await put(site, '/site/α.txt', text('alpha'));
  const same1 = await t.create(site, 'same.txt');
  await t.writeFile(same1, text('first'));
  await put(site, '/site/typed.dat', text('typed'), ct('text/x-good; a="b c"'));
  await put(site, '/site/badtype.txt', text('bad'), ct('not a type'));
  await t.create(site, 'empty.txt'); // never written: no version
  const a = await t.mkdir(site, 'a');
  await put(a, '/site/a/<b>&"q\'.txt', text('escaped'));
  await put(a, '/site/a/b.css', text('b{}'));
  await t.mkdir(a, 'sub');
  const l = await t.create(a, 'link', { type: 'symlink' });
  await t.writeFile(l, text('b.css'));
  const both = await t.mkdir(site, 'both');
  await put(both, '/site/both/index.htm', text('htm'));
  await put(both, '/site/both/index.html', text('html'));
  // A second node of the same name: one of the two is shown as `same (2).txt`
  // (fs.md §8 orders them by node id).
  const dup = await t.create(site, 'same.txt');
  await t.writeFile(dup, text('second'));
  // Siblings: a conflict copy.
  const c = await t.create(site, 'notes.md');
  await t.writeFile(c, text('one'), { replaces: [] });
  await t.writeFile(c, text('two'), { replaces: [] });
  return { id, t, files };
}

const mounts = (extra: Record<string, unknown> = {}): Mount[] =>
  parseMounts([{ name: 's', fs: 1, tree: new Uint8Array(16), base: '/site', ...extra }]);

function siteOf(r: Replica, ms: Mount[], onError?: (e: unknown) => void): Site {
  return { mounts: ms, replica: async () => r, ...(onError ? { onError } : {}) };
}

function req(path: string, headers: Record<string, string> = {}, method = 'GET'): Request {
  return new Request(`https://view.test${path}`, { method, headers });
}

/** sw.md §4's headers, on every response. */
function sandboxed(res: Response): void {
  for (const [k, v] of SANDBOX_HEADERS) expect(res.headers.get(k), k).toBe(v);
}

async function body(res: Response): Promise<Uint8Array> {
  return new Uint8Array(await res.arrayBuffer());
}

/** Bytes equal (vitest's deep equality is slow on large arrays). */
function same(got: Uint8Array, want: Uint8Array, what = ''): void {
  expect(got.length, what).toBe(want.length);
  expect(bytes.equal(got, want), what).toBe(true);
}

/** Encode a tree path's segments for a URL. */
const urlOf = (path: string) => path.split('/').map(encodeURIComponent).join('/');

describe('handle (sw.md §3–§7)', () => {
  let r: Replica;
  let site: Awaited<ReturnType<typeof makeSite>>;
  let s: Site;
  const get = (path: string, h?: Record<string, string>, m?: string) => handle(req(path, h, m), s);

  beforeAll(async () => {
    site = await makeSite();
    const { ufs } = await installation(w);
    r = await openReplica(ufs, site.id, solo());
    await r.sync();
    s = siteOf(r, mounts());
  });
  afterAll(() => r?.close());

  it('files: bodies, types, ETags and headers', async () => {
    const copy = (await r.readdir('/site')).find((n) => n.startsWith('notes (conflict '))!;
    expect(copy).toBeDefined();
    site.files.set(`/site/${copy}`, await r.readFile(`/site/${copy}`));
    site.files.set('/site/notes.md', await r.readFile('/site/notes.md'));
    for (const p of ['/site/same.txt', '/site/same (2).txt']) {
      site.files.set(p, await r.readFile(p));
    }
    expect(
      new Set(
        ['/site/same.txt', '/site/same (2).txt'].map((p) => bytes.fromUtf8(site.files.get(p)!)),
      ),
    ).toEqual(new Set(['first', 'second']));
    const types: Record<string, string> = {
      '/site/index.html': 'text/html; charset=utf-8',
      '/site/small.bin': 'application/octet-stream',
      '/site/typed.dat': 'text/x-good; a="b c"',
      '/site/badtype.txt': 'text/plain; charset=utf-8',
      '/site/a/b.css': 'text/css; charset=utf-8',
      '/site/notes.md': 'text/markdown; charset=utf-8',
    };
    for (const [path, data] of site.files) {
      const res = await get(`/s${urlOf(path.slice('/site'.length))}`);
      expect(res.status, path).toBe(200);
      sandboxed(res);
      same(await body(res), data, path);
      expect(res.headers.get('Content-Length'), path).toBe(String(data.length));
      expect(res.headers.get('Accept-Ranges')).toBe('bytes');
      const e = await r.lookup(path);
      const v = await r.shown(e);
      expect(res.headers.get('ETag'), path).toBe(`"${bytes.hex(e.node.id)}.${bytes.hex(v!.dot)}"`);
      if (types[path]) expect(res.headers.get('Content-Type'), path).toBe(types[path]);
      same(await r.readFile(path), data, path);
    }
    // An empty file with no version: no ETag.
    const empty = await get('/s/empty.txt');
    expect(empty.status).toBe(200);
    sandboxed(empty);
    expect(empty.headers.get('ETag')).toBeNull();
    expect(empty.headers.get('Content-Length')).toBe('0');
    expect((await body(empty)).length).toBe(0);
    // The root of the mount serves its index.
    const home = await get('/s/');
    expect(await home.text()).toBe('<h1>home</h1>');
    expect(home.headers.get('Content-Type')).toBe('text/html; charset=utf-8');
  });

  it('HEAD: the same status and headers, no body', async () => {
    const g = await get('/s/paged.bin');
    const h = await get('/s/paged.bin', {}, 'HEAD');
    expect(h.status).toBe(200);
    sandboxed(h);
    for (const k of ['ETag', 'Content-Length', 'Content-Type', 'Accept-Ranges']) {
      expect(h.headers.get(k), k).toBe(g.headers.get(k));
    }
    expect(h.body).toBeNull();
    await g.body?.cancel();
    const hr = await get('/s/paged.bin', { Range: 'bytes=10-19' }, 'HEAD');
    expect(hr.status).toBe(206);
    expect(hr.headers.get('Content-Range')).toBe(`bytes 10-19/${PAGED}`);
    expect(hr.headers.get('Content-Length')).toBe('10');
    expect(hr.body).toBeNull();
    const h404 = await get('/s/nothing', {}, 'HEAD');
    expect(h404.status).toBe(404);
    expect(h404.body).toBeNull();
  });

  it('ranges against a model, the edges and 416', async () => {
    let seed = 11;
    const rnd = (n: number) => {
      seed = (seed * 1103515245 + 12345) & 0x7fffffff;
      return seed % n;
    };
    for (const [path, size] of [
      ['/site/paged.bin', PAGED],
      ['/site/small.bin', SMALL],
    ] as const) {
      const data = site.files.get(path)!;
      const url = `/s${path.slice('/site'.length)}`;
      const cases: string[] = [
        'bytes=0-0',
        `bytes=${size - 1}-`,
        `bytes=${size - 1}-${size - 1}`,
        'bytes=-1',
        `bytes=-${size + 5}`,
        `bytes=10-${size * 2}`,
        'bytes=1983-1984',
        'bytes=1984-3967',
      ];
      for (let i = 0; i < 40; i++) {
        const a = rnd(size);
        const form = rnd(3);
        cases.push(
          form === 0
            ? `bytes=${a}-${a + rnd(size - a)}`
            : form === 1
              ? `bytes=${a}-`
              : `bytes=-${a + 1}`,
        );
      }
      for (const h of cases) {
        // The model: RFC 9110 §14.1.2.
        const m = /^bytes=(\d*)-(\d*)$/.exec(h)!;
        let first: number;
        let last: number;
        if (m[1] === '') {
          first = Math.max(0, size - Number(m[2]));
          last = size - 1;
        } else {
          first = Number(m[1]);
          last = m[2] === '' ? size - 1 : Math.min(Number(m[2]), size - 1);
        }
        const res = await get(url, { Range: h });
        expect(res.status, h).toBe(206);
        sandboxed(res);
        expect(res.headers.get('Content-Range'), h).toBe(`bytes ${first}-${last}/${size}`);
        expect(res.headers.get('Content-Length'), h).toBe(String(last - first + 1));
        same(await body(res), data.slice(first, last + 1), h);
      }
      for (const h of [`bytes=${size}-`, `bytes=${size + 9}-${size + 20}`, 'bytes=-0']) {
        const res = await get(url, { Range: h });
        expect(res.status, h).toBe(416);
        sandboxed(res);
        expect(res.headers.get('Content-Range'), h).toBe(`bytes */${size}`);
        await res.body?.cancel();
      }
      for (const h of ['bytes=0-1,5-6', 'bytes=x', 'items=0-1', 'bytes=9-2']) {
        const res = await get(url, { Range: h });
        expect(res.status, h).toBe(200);
        expect((await body(res)).length, h).toBe(size);
      }
    }
    const empty = await get('/s/empty.txt', { Range: 'bytes=0-' });
    expect(empty.status).toBe(416);
    expect(empty.headers.get('Content-Range')).toBe('bytes */0');
  });

  it('validators: If-None-Match and If-Range', async () => {
    const first = await get('/s/small.bin');
    const etag = first.headers.get('ETag')!;
    await first.body?.cancel();
    for (const inm of [etag, `"x", ${etag}`, `W/${etag}`, '*']) {
      const res = await get('/s/small.bin', { 'If-None-Match': inm });
      expect(res.status, inm).toBe(304);
      sandboxed(res);
      expect(res.headers.get('ETag')).toBe(etag);
      expect(res.body).toBeNull();
    }
    const other = await get('/s/small.bin', { 'If-None-Match': '"other"' });
    expect(other.status).toBe(200);
    await other.body?.cancel();
    const match = await get('/s/small.bin', { Range: 'bytes=0-9', 'If-Range': etag });
    expect(match.status).toBe(206);
    await match.body?.cancel();
    for (const ir of ['"other"', `W/${etag}`, 'Tue, 01 Jan 2030 00:00:00 GMT']) {
      const res = await get('/s/small.bin', { Range: 'bytes=0-9', 'If-Range': ir });
      expect(res.status, ir).toBe(200);
      expect((await body(res)).length).toBe(SMALL);
    }
    // No ETag (no version): If-Range never matches.
    const empty = await get('/s/empty.txt', { Range: 'bytes=0-', 'If-Range': '""' });
    expect(empty.status).toBe(200);
  });

  it('directories: the redirect, the index, 404 without either', async () => {
    for (const [path, loc] of [
      ['/s', '/s/'],
      ['/s?x=1', '/s/?x=1'],
      ['/s/a', '/s/a/'],
      ['/s/a?q=%20', '/s/a/?q=%20'],
      ['/s/a/sub', '/s/a/sub/'],
    ]) {
      const res = await get(path!);
      expect(res.status, path).toBe(301);
      sandboxed(res);
      expect(res.headers.get('Location'), path).toBe(loc);
    }
    expect(await (await get('/s/both/')).text()).toBe('html');
    const s2 = siteOf(r, mounts({ index: ['index.htm', 'index.html'] }));
    expect(await (await handle(req('/s/both/'), s2)).text()).toBe('htm');
    for (const path of ['/s/a/', '/s/a/sub/', '/s/a/b.css/', '/s/a/link', '/s/nothing', '/x/']) {
      const res = await get(path);
      expect(res.status, path).toBe(404);
      sandboxed(res);
      expect(res.headers.get('Content-Type')).toBe('text/html; charset=utf-8');
      expect(res.headers.get('Zen-Reason')).toBeNull();
    }
  });

  it('listings: sorted, escaped, no scripts, no symlinks', async () => {
    const s2 = siteOf(r, mounts({ listing: true }));
    const res = await handle(req('/s/a/'), s2);
    expect(res.status).toBe(200);
    sandboxed(res);
    expect(res.headers.get('ETag')).toBeNull();
    const html = await res.text();
    expect(html).not.toMatch(/<script/i);
    expect(html).not.toContain('<b>&');
    expect(html).toContain('&lt;b&gt;&amp;&quot;q&#39;.txt');
    expect(html).toContain(`href="./${encodeURIComponent('<b>&"q\'.txt').replace(/'/g, '&#39;')}"`);
    const links = [...html.matchAll(/<a href="([^"]*)">/g)].map((m) => m[1]);
    expect(links).toEqual(['../', './sub/', './%3Cb%3E%26%22q&#39;.txt', './b.css']);
    // At the mount's base: no `../`.
    const base = await (
      await handle(req('/s/'), siteOf(r, mounts({ listing: true, index: [] })))
    ).text();
    expect(base).not.toContain('href="../"');
    expect(base.indexOf('./a/')).toBeLessThan(base.indexOf('./badtype.txt'));
    expect(base).toContain('./same%20(2).txt');
  });

  it('refusals: 400 and 405', async () => {
    for (const path of ['/s/a//b', '/s/%zz', '/s/%FF', '/s/a%2Fb', '/s/a%00']) {
      const res = await get(path);
      expect(res.status, path).toBe(400);
      sandboxed(res);
    }
    for (const m of ['POST', 'PUT', 'DELETE', 'OPTIONS', 'PATCH']) {
      const res = await get('/s/small.bin', {}, m);
      expect(res.status, m).toBe(405);
      expect(res.headers.get('Allow')).toBe('GET, HEAD');
      sandboxed(res);
    }
  });
});

/** A replica whose calls can be made to fail. */
function wrapped(r: Replica, over: Partial<Record<'lookup' | 'shown' | 'streamOf', unknown>>) {
  return new Proxy(r, {
    get(target, k, recv) {
      if (k in over) return over[k as keyof typeof over];
      const v = Reflect.get(target, k, recv);
      return typeof v === 'function' ? v.bind(target) : v;
    },
  });
}

/** A stream of `before` bytes, then `err`. */
function failing(before: Uint8Array | undefined, err: unknown): ReadableStream<Uint8Array> {
  let sent = false;
  return new ReadableStream({
    pull(c) {
      if (before && !sent) {
        sent = true;
        c.enqueue(before);
      } else c.error(err);
    },
  });
}

function cryptoError(): Error {
  const e = new Error('decrypt');
  e.name = 'ZenCryptoError';
  return e;
}

describe('errors (sw.md §7)', () => {
  let site: Awaited<ReturnType<typeof makeSite>>;
  let r: Replica;
  let net: Net;
  beforeAll(async () => {
    site = await makeSite();
    const inst = await installation(w);
    net = inst.net;
    r = await openReplica(inst.ufs, site.id, solo());
    await r.sync();
  });
  afterAll(() => r?.close());

  async function expectError(res: Response, status: number, reason: string | null) {
    expect(res.status).toBe(status);
    sandboxed(res);
    expect(res.headers.get('Zen-Reason')).toBe(reason);
    expect(res.headers.get('Content-Type')).toBe('text/html; charset=utf-8');
    const html = await res.text();
    expect(html).not.toMatch(/<script/i);
    return html;
  }

  it('offline: what is kept is served, the rest is 503 offline', async () => {
    const s = siteOf(r, mounts());
    expect(await r.readFile('/site/small.bin')).toEqual(site.files.get('/site/small.bin')!);
    net.down = true;
    try {
      const kept = await handle(req('/s/small.bin'), s);
      expect(kept.status).toBe(200);
      same(await body(kept), site.files.get('/site/small.bin')!);
      const ranged = await handle(req('/s/small.bin', { Range: 'bytes=100-199' }), s);
      same(await body(ranged), site.files.get('/site/small.bin')!.slice(100, 200));
      await expectError(await handle(req('/s/paged.bin'), s), 503, 'offline');
      await expectError(await handle(req('/s/paged.bin', {}, 'HEAD'), s), 503, 'offline');
    } finally {
      net.down = false;
    }
  });

  it("the site's start-up: locked and starting", async () => {
    const reject = (code: string): Site => ({
      mounts: mounts(),
      replica: () => Promise.reject(new ZenError(0, code, 'test')),
    });
    const html = await expectError(await handle(req('/s/x'), reject('locked')), 503, 'locked');
    expect(html).toContain('href="/_zen/enroll.html"');
    await expectError(await handle(req('/s/x'), reject('starting')), 503, 'starting');
    // Routing comes first: a bad path is 400 whatever the replica.
    expect((await handle(req('/s/%zz'), reject('locked'))).status).toBe(400);
  });

  it('integrity: before the first byte 502, after it an errored body', async () => {
    const alarm = new IntegrityError({ kind: 'invalid', object: 'tree', detail: 'test' });
    const cases: [string, unknown, number, string | null][] = [
      ['lookup', alarm, 502, 'integrity'],
      ['lookup', new ZenError(0, 'format', 'bad'), 502, 'integrity'],
      ['lookup', new ZenError(0, 'closed', 'closed'), 503, 'starting'],
      ['lookup', new ZenError(0, 'network', 'down'), 503, 'offline'],
      ['lookup', new ZenError(0, 'not_dir', 'x'), 404, null],
      ['shown', cryptoError(), 502, 'integrity'],
    ];
    for (const [k, err, status, reason] of cases) {
      const rr = wrapped(r, { [k]: () => Promise.reject(err) });
      await expectError(await handle(req('/s/small.bin'), siteOf(rr, mounts())), status, reason);
    }
    // streamOf: an error before the first byte.
    for (const err of [cryptoError(), alarm]) {
      const rr = wrapped(r, { streamOf: () => failing(undefined, err) });
      await expectError(await handle(req('/s/small.bin'), siteOf(rr, mounts())), 502, 'integrity');
    }
    // After it: the status is sent, the body errors.
    const rr = wrapped(r, { streamOf: () => failing(new Uint8Array(10), cryptoError()) });
    const res = await handle(req('/s/small.bin'), siteOf(rr, mounts()));
    expect(res.status).toBe(200);
    await expect(res.arrayBuffer()).rejects.toThrow();
  });

  it('anything unexpected is 500, never 404, and reported', async () => {
    const seen: unknown[] = [];
    const rr = wrapped(r, { lookup: () => Promise.reject(new TypeError('boom')) });
    const html = await expectError(
      await handle(
        req('/s/small.bin'),
        siteOf(rr, mounts(), (e) => seen.push(e)),
      ),
      500,
      null,
    );
    expect(html).not.toContain('boom');
    expect(seen).toHaveLength(1);
    expect((seen[0] as Error).message).toBe('boom');
  });

  it('a chunk tampered in the local store: 502', async () => {
    const local = sealedStore(memoryLocalStore(), { device: false, plaintext: true });
    const one = await installation(w);
    const r1 = await openReplica(one.ufs, site.id, solo(local));
    await r1.sync();
    const s1 = siteOf(r1, mounts());
    same(await body(await handle(req('/s/small.bin'), s1)), site.files.get('/site/small.bin')!);
    await r1.close();
    const store = `zfs/1/${bytes.hex(site.id)}/chunks`;
    let n = 0;
    for await (const [k, v] of local.raw.list(store)) {
      const t = Uint8Array.from(v);
      t[t.length - 1]! ^= 1;
      await local.raw.put(store, k, t);
      n++;
    }
    expect(n).toBeGreaterThanOrEqual(3);
    const two = await installation(w);
    two.net.down = true; // the tampered chunks can't be fetched again
    const r2 = await openReplica(two.ufs, site.id, solo(local));
    try {
      await expectError(await handle(req('/s/small.bin'), siteOf(r2, mounts())), 502, 'integrity');
    } finally {
      await r2.close();
    }
  });
});

describe('the body is pulled (sw.md §6)', () => {
  it('handle reads at most one piece ahead of the reader', async () => {
    const site = await makeSite();
    const { ufs } = await installation(w);
    const r = await openReplica(ufs, site.id, solo());
    await r.sync();
    let pulls = 0;
    const rr = wrapped(r, {
      streamOf: () =>
        new ReadableStream<Uint8Array>(
          {
            pull(c) {
              pulls++;
              if (pulls > 10) c.close();
              else c.enqueue(new Uint8Array(1024).fill(pulls));
            },
          },
          { highWaterMark: 0 },
        ),
    });
    const res = await handle(req('/s/paged.bin'), siteOf(rr, mounts()));
    expect(res.status).toBe(200);
    // The first piece decides the status; nothing more is read until asked.
    await new Promise((ok) => setTimeout(ok, 50));
    expect(pulls).toBe(1);
    const reader = res.body!.getReader();
    for (let i = 1; i <= 3; i++) {
      expect((await reader.read()).value?.[0]).toBe(i);
      await new Promise((ok) => setTimeout(ok, 20));
      expect(pulls).toBeLessThanOrEqual(i + 1);
    }
    await reader.cancel();
    await r.close();
  });
});

describe('the version an ETag names (sw.md §6)', () => {
  it('a version replacing it mid-request is not streamed', async () => {
    const site = await makeSite();
    const { ufs } = await installation(w);
    const r = await openReplica(ufs, site.id, solo());
    await r.sync();
    const old = site.files.get('/site/paged.bin')!;
    const e = await r.lookup('/site/paged.bin');
    const etag = etagOf(e, await r.shown(e));
    const next = blob(PAGED - 1000, 9);
    let wrote = false;
    // A write lands between the handler's lookup and its read.
    const rr = wrapped(r, {
      shown: async (x: Parameters<Replica['shown']>[0]) => {
        const v = await r.shown(x);
        if (!wrote) {
          wrote = true;
          await site.t.writeFile(e.node.id, next);
          await until(async () => expect((await r.stat('/site/paged.bin')).size).toBe(next.length));
        }
        return v;
      },
    });
    const res = await handle(req('/s/paged.bin'), siteOf(rr, mounts()));
    expect(wrote).toBe(true);
    expect(res.headers.get('ETag')).toBe(etag);
    expect(res.headers.get('Content-Length')).toBe(String(old.length));
    same(await body(res), old);
    // The next request sees the new version.
    const now = await handle(req('/s/paged.bin'), siteOf(r, mounts()));
    expect(now.headers.get('ETag')).not.toBe(etag);
    same(await body(now), next);
    await r.close();
  });
});
