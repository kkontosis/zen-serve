// Enrollment and the worker's start (spec/sw.md §8), in Node: the two sides
// of the conversation over fake windows that deliver as a browser does (by
// target origin, structured-cloned, with the sender's origin and window),
// then `startSite` over the store the page kept.
import { createServer, type Server } from 'node:net';
import {
  bytes,
  credentialVault,
  type LocalStore,
  memoryLocalStore,
  ROOT,
  sealedStore,
  Tree,
  type UnlockedFs,
  zw,
} from '@zen/client';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { type World, world } from '../../client/test/helpers.js';
import {
  decodeMounts,
  enrollPage,
  enrollViewer,
  handle,
  type MessageLike,
  MOUNTS_KEY,
  MOUNTS_STORE,
  type Offer,
  type PageResult,
  type PopupLike,
  type PostTarget,
  startSite,
  type WindowLike,
} from '../src/index.js';

const APP = 'http://app.test';
let w: World;
let admin: UnlockedFs;
let tree: Uint8Array;
const files = new Map<string, Uint8Array>();

beforeAll(async () => {
  w = await world();
  admin = await w.session.fs(1).init((k) => [zw.createDeviceSlot(k, w.admin.device.devicePublic)]);
  tree = Tree.newId();
  const t = admin.tree(tree);
  const site = await t.mkdir(ROOT, 'site');
  for (const [name, data] of [
    ['index.html', bytes.utf8('<h1>home</h1>')],
    ['a.txt', bytes.utf8('alpha')],
    ['b.txt', bytes.utf8('beta')],
  ] as const) {
    const n = await t.create(site, name);
    await t.writeFile(n, data);
    files.set(name, data);
  }
});
afterAll(() => w?.server.stop());

/** A window: `message` listeners, fed asynchronously as a browser does. */
class FakeWindow implements WindowLike {
  readonly fns = new Set<(e: MessageLike) => void>();
  constructor(readonly origin: string) {}
  addEventListener(_: 'message', fn: (e: MessageLike) => void) {
    this.fns.add(fn);
  }
  removeEventListener(_: 'message', fn: (e: MessageLike) => void) {
    this.fns.delete(fn);
  }
  deliver(data: unknown, origin: string, source: unknown) {
    const copy = structuredClone(data);
    setTimeout(() => {
      for (const f of [...this.fns]) f({ data: copy, origin, source });
    }, 0);
  }
}

/** The app's window and its popup, the page's window and its opener. */
function windows(content: string, app = APP) {
  const appWin = new FakeWindow(app);
  const page = new FakeWindow(content);
  const opener: PostTarget = {
    postMessage(m, target) {
      if (target === app) appWin.deliver(m, content, popup);
    },
  };
  const popup: { -readonly [K in keyof PopupLike]: PopupLike[K] } & { opened?: string } = {
    closed: false,
    close() {
      this.closed = true;
    },
    postMessage(m, target) {
      if (target === content) page.deliver(m, app, opener);
    },
  };
  return { appWin, page, opener, popup };
}

const MOUNT = { name: 's', fs: 1, base: '/site' };

interface Run {
  local: LocalStore;
  page: Promise<PageResult>;
  viewer: Promise<{ fp: Uint8Array; inAcl: boolean }>;
  offers: Offer[];
}

/** One enrollment, both sides. */
function enroll(
  o: { admin?: boolean; confirm?: boolean; local?: LocalStore; mounts?: object[] } = {},
): Run {
  const content = w.client.origin;
  const local = o.local ?? memoryLocalStore();
  const { appWin, page, opener, popup } = windows(content);
  const offers: Offer[] = [];
  const pageDone = enrollPage({
    win: page,
    opener,
    appOrigins: [APP],
    local,
    confirm: async (offer) => {
      offers.push(offer);
      return o.confirm ?? true;
    },
  });
  const viewer = enrollViewer({
    session: w.session,
    identity: w.admin.identity,
    ...(o.admin === false ? {} : { admin: w.admin.identity }),
    fs: [admin],
    mounts: (o.mounts ?? [{ ...MOUNT, tree }]) as never,
    contentOrigin: content,
    win: appWin,
    open: (url) => {
      popup.opened = url;
      return popup;
    },
    pollMs: 20,
  });
  return { local, page: pageDone, viewer, offers };
}

/** The ACL's device certificates of the admin. */
async function adminCerts(): Promise<Uint8Array[]> {
  const head = await w.session.acl.head();
  const me = w.admin.identity.fingerprint;
  return head.doc.members.find((m) => bytes.equal(zw.fingerprint(m.identity), me))!.devices;
}

const get = (path: string) => new Request(`${w.client.origin}${path}`);
const text = async (r: Response) => bytes.fromUtf8(new Uint8Array(await r.arrayBuffer()));

describe('enrollment', () => {
  it('keeps the device and the mounts, and the worker serves the site', async () => {
    const before = (await adminCerts()).length;
    const run = enroll();
    const [p, v] = await Promise.all([run.page, run.viewer]);
    expect(p.saved).toBe(true);
    expect(p.origin).toBe(APP);
    expect(run.offers).toHaveLength(1);
    expect(run.offers[0]!.fp).toBe(bytes.hex(v.fp));
    expect(v.inAcl).toBe(true);
    expect(await adminCerts()).toHaveLength(before + 1);

    // The page kept the device, and it signs in.
    const sealed = sealedStore(run.local);
    const d = (await credentialVault(sealed).device())!;
    expect(bytes.equal(zw.fingerprint(d.device.devicePublic), v.fp)).toBe(true);
    d.device.free();
    const kept = decodeMounts((await sealed.get(MOUNTS_STORE, MOUNTS_KEY))!);
    expect(kept.map((m) => [m.name, m.fs, m.base])).toEqual([['s', 1, '/site']]);

    const viewer = startSite({
      origin: w.client.origin,
      local: run.local,
      locks: null,
      channel: null,
    });
    const r = await handle(get('/s/a.txt'), await viewer.site());
    expect(r.status).toBe(200);
    expect(await text(r)).toBe('alpha');
    const idx = await handle(get('/s/'), await viewer.site());
    expect(await text(idx)).toBe('<h1>home</h1>');
    await viewer.restart();
  });

  it('declined: nothing changes on the server', async () => {
    const before = await w.session.acl.head();
    const run = enroll({ confirm: false });
    await expect(run.viewer).rejects.toMatchObject({ code: 'enroll_failed' });
    const p = await run.page;
    expect(p).toMatchObject({ saved: false, reason: 'declined' });
    expect((await w.session.acl.head()).version).toBe(before.version);
    expect(await credentialVault(sealedStore(run.local)).device()).toBeUndefined();
  });

  it('a member without an admin: the device is certified but not in the ACL, so locked', async () => {
    const before = (await adminCerts()).length;
    const run = enroll({ admin: false });
    const [p, v] = await Promise.all([run.page, run.viewer]);
    expect(p.saved).toBe(true);
    expect(v.inAcl).toBe(false);
    expect(await adminCerts()).toHaveLength(before);
    const viewer = startSite({
      origin: w.client.origin,
      local: run.local,
      locks: null,
      channel: null,
    });
    const r = await handle(get('/s/a.txt'), await viewer.site());
    expect(r.status).toBe(503);
    expect(r.headers.get('Zen-Reason')).toBe('locked');
  });

  it('the page ignores other origins, other windows and bad offers', async () => {
    const content = w.client.origin;
    const { page, opener } = windows(content);
    const confirmed: Offer[] = [];
    const local = memoryLocalStore();
    void enrollPage({
      win: page,
      opener,
      appOrigins: [APP],
      local,
      confirm: async (o) => {
        confirmed.push(o);
        return true;
      },
    });
    const offer = { type: 'zen-enroll-offer', mounts: [{ ...MOUNT, tree }] };
    page.deliver(offer, 'http://evil.test', opener); // an untrusted origin
    page.deliver(offer, APP, {}); // not the opener
    page.deliver({ type: 'zen-enroll-offer', mounts: [{ ...MOUNT, name: 'Bad' }] }, APP, opener);
    page.deliver({ type: 'zen-enroll-done', cert: new Uint8Array(1) }, APP, opener); // out of order
    await new Promise((r) => setTimeout(r, 50));
    expect(confirmed).toHaveLength(0);

    // A done whose certificate is for another device is ignored too.
    page.deliver(offer, APP, opener);
    await new Promise((r) => setTimeout(r, 50));
    expect(confirmed).toHaveLength(1);
    const other = zw.DeviceSecret.fromSeed(new Uint8Array(32).fill(9));
    const cert = w.admin.identity.issueDeviceCert(other.devicePublic, 1_700_000_000n);
    page.deliver(
      { type: 'zen-enroll-done', cert, identityPublic: w.admin.identity.publicIdentity },
      APP,
      opener,
    );
    // And, once an origin offered, another trusted-looking origin is ignored.
    await new Promise((r) => setTimeout(r, 50));
    expect(await credentialVault(sealedStore(local)).device()).toBeUndefined();
  });

  it('the app ignores other origins and other windows', async () => {
    const content = w.client.origin;
    const { appWin, popup } = windows(content);
    const v = enrollViewer({
      session: w.session,
      identity: w.admin.identity,
      fs: [admin],
      mounts: [{ ...MOUNT, tree }] as never,
      contentOrigin: content,
      win: appWin,
      open: () => popup,
      pollMs: 20,
    });
    const d = zw.DeviceSecret.fromSeed(new Uint8Array(32).fill(7));
    const hello = {
      type: 'zen-enroll-hello',
      devicePublic: d.devicePublic,
      fp: new Uint8Array(32),
    };
    const sent: unknown[] = [];
    popup.postMessage = (m) => sent.push(m);
    appWin.deliver(hello, 'http://evil.test', popup);
    appWin.deliver(hello, content, {});
    await new Promise((r) => setTimeout(r, 50));
    expect(sent).toHaveLength(0);
    appWin.deliver(hello, content, popup);
    await new Promise((r) => setTimeout(r, 50));
    expect(sent).toMatchObject([{ type: 'zen-enroll-offer' }]);
    popup.closed = true;
    await expect(v).rejects.toMatchObject({ code: 'enroll_failed' });
  });
});

describe('the worker start', () => {
  it('locked with nothing enrolled, on every path', async () => {
    const viewer = startSite({ origin: w.client.origin, local: memoryLocalStore() });
    for (const path of ['/', '/s/a.txt', '/x/y/']) {
      const r = await handle(get(path), await viewer.site());
      expect(r.status, path).toBe(503);
      expect(r.headers.get('Zen-Reason'), path).toBe('locked');
    }
  });

  it('offline: what was read is served again, the rest is 503 offline', async () => {
    const run = enroll();
    await Promise.all([run.page, run.viewer]);
    const online = startSite({
      origin: w.client.origin,
      local: run.local,
      locks: null,
      channel: null,
    });
    expect((await handle(get('/s/a.txt'), await online.site())).status).toBe(200);
    // Let the replica write what it read, then stop as the browser stops a worker.
    await online.restart();

    // The same origin, but the server can't be reached.
    const off = startSite({
      origin: w.client.origin,
      url: 'http://127.0.0.1:1',
      local: run.local,
      locks: null,
      channel: null,
    });
    const a = await handle(get('/s/a.txt'), await off.site());
    expect(a.status).toBe(200);
    expect(await text(a)).toBe('alpha');
    const b = await handle(get('/s/b.txt'), await off.site());
    expect(b.status).toBe(503);
    expect(b.headers.get('Zen-Reason')).toBe('offline');
    await off.restart();
  });

  it('the device removed from the ACL: locked, though a session was kept', async () => {
    const run = enroll();
    const [, v] = await Promise.all([run.page, run.viewer]);
    const viewer = startSite({
      origin: w.client.origin,
      local: run.local,
      locks: null,
      channel: null,
    });
    expect((await handle(get('/s/a.txt'), await viewer.site())).status).toBe(200);
    await viewer.restart();
    await w.session.acl.update(w.admin.identity, (doc) => {
      for (const m of doc.members) {
        m.devices = m.devices.filter((c) => {
          const pub = zw.verifyDeviceCert(m.identity, c).devicePublic;
          return !bytes.equal(zw.fingerprint(pub), v.fp);
        });
      }
    });
    // Sessions are cached up to 10 s per node (api.md §3.3).
    const deadline = Date.now() + 15_000;
    let r: Response;
    for (;;) {
      r = await handle(get('/s/a.txt'), await viewer.site());
      if (r.status !== 200 || Date.now() > deadline) break;
      await viewer.restart();
      await new Promise((ok) => setTimeout(ok, 500));
    }
    expect(r.status).toBe(503);
    expect(r.headers.get('Zen-Reason')).toBe('locked');
  }, 30_000);

  it('a start that hangs answers 503 starting after the timeout', async () => {
    const run = enroll();
    await Promise.all([run.page, run.viewer]);
    // A server that accepts and never answers.
    const hang: Server = createServer(() => {});
    await new Promise<void>((ok) => hang.listen(0, '127.0.0.1', ok));
    const port = (hang.address() as { port: number }).port;
    try {
      // No kept session, so the sign-in must reach the server.
      await credentialVault(sealedStore(run.local)).forgetSession();
      const viewer = startSite({
        origin: w.client.origin,
        url: `http://127.0.0.1:${port}`,
        local: run.local,
        timeoutMs: 200,
      });
      const t0 = Date.now();
      const r = await handle(get('/s/a.txt'), await viewer.site());
      expect(r.status).toBe(503);
      expect(r.headers.get('Zen-Reason')).toBe('starting');
      expect(Date.now() - t0).toBeLessThan(5_000);
    } finally {
      hang.close();
    }
  });

  it('restart picks up new mounts', async () => {
    const local = memoryLocalStore();
    const first = enroll({ local });
    await Promise.all([first.page, first.viewer]);
    const viewer = startSite({ origin: w.client.origin, local, locks: null, channel: null });
    expect((await handle(get('/s/a.txt'), await viewer.site())).status).toBe(200);
    expect((await handle(get('/t/a.txt'), await viewer.site())).status).toBe(404);
    const again = enroll({ local, mounts: [{ ...MOUNT, name: 't', tree }] });
    await Promise.all([again.page, again.viewer]);
    // Still the old start until it is told.
    expect((await handle(get('/t/a.txt'), await viewer.site())).status).toBe(404);
    await viewer.restart();
    expect((await handle(get('/t/a.txt'), await viewer.site())).status).toBe(200);
    expect((await handle(get('/s/a.txt'), await viewer.site())).status).toBe(404);
    await viewer.restart();
  });
});
