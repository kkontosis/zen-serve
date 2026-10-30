// Tree checkpoints (fs.md §9, formats.md §14): replicas compute the tree
// state, check it where they are caught up, and name it in their device
// checkpoints. Honest replicas agree (purges included); a rollback, a fork
// and a wrong merge are detected. Each attack test first shows the attack
// works (the replica holds the stale or wrong state), then that it is caught.
import {
  bytes,
  type Client,
  connect,
  fullGrants,
  type Integrity,
  IntegrityError,
  type IntegrityOptions,
  member,
  memoryLocalStore,
  ROOT,
  type SealedStore,
  type Session,
  sealedStore,
  Tree,
  type UnlockedFs,
  zw,
} from '@zen/client';
import type { CheckpointEntry } from '@zen/wasm';
import { afterEach, describe, expect, it } from 'vitest';
import type { ServerOptions, TestServer, TestUser } from '../../client/src/testing/index.js';
import { testUser } from '../../client/src/testing/index.js';
import { type World, world } from '../../client/test/helpers.js';
import { openReplica, type Replica, type ReplicaOptions } from '../src/index.js';
import { until } from './helpers.js';

interface Env {
  w: World;
  bob: TestUser;
  recoveryKey: Uint8Array;
  servers: TestServer[];
  closers: (() => Promise<unknown> | unknown)[];
}

let env: Env | undefined;

/** A server, fs 1 created by the admin (a recovery slot), and a second member, bob. */
async function setup(opts: ServerOptions = {}): Promise<Env> {
  const w = await world(opts);
  const bob = testUser(2);
  await w.session.acl.update(w.admin.identity, (doc) => {
    doc.members.push(member(bob.identity, [bob.cert]));
    doc.grants.push(...fullGrants(1, bob.fp));
  });
  let recoveryKey: Uint8Array = new Uint8Array(0);
  const u = await w.session.fs(1).init((k) => {
    const r = zw.createRecoverySlot(k);
    recoveryKey = r.recoveryKey;
    return [r.slot];
  });
  u.close();
  env = { w, bob, recoveryKey, servers: [w.server], closers: [] };
  return env;
}

afterEach(async () => {
  if (!env) return;
  for (const c of env.closers.reverse()) await Promise.resolve(c()).catch(() => {});
  for (const s of env.servers) await s.stop();
  env = undefined;
});

const pins = (): SealedStore => sealedStore(memoryLocalStore(), { device: false, plaintext: true });
const store = (): SealedStore => sealedStore(memoryLocalStore());
const solo = (local: SealedStore): ReplicaOptions => ({
  local,
  locks: null,
  channel: null,
  waitMs: 300,
});

interface Side {
  client: Client;
  session: Session;
  integ: Integrity;
  ufs: UnlockedFs;
}

/** Sign `user` in on `url` and open fs 1 with integrity (`signer`: the user's device signs). */
async function side(
  e: Env,
  url: string,
  user: TestUser,
  o: {
    signer?: boolean;
    local?: SealedStore;
    fetch?: typeof fetch;
  } & Partial<IntegrityOptions> = {},
): Promise<Side> {
  const client = await connect(url, {
    origin: e.w.client.origin,
    ...(o.fetch ? { fetch: o.fetch } : {}),
  });
  const session = await client.signInDevice(user);
  const fs = session.fs(1);
  const integ = await fs.integrity({
    ...(o.signer === false ? {} : { device: { device: user.device, cert: user.cert } }),
    ...(o.local ? { local: o.local } : {}),
    ...(o.sigEvery ? { sigEvery: o.sigEvery } : {}),
  });
  const ufs = await fs.unlock({ recoveryKey: e.recoveryKey });
  e.closers.push(() => ufs.close());
  return { client, session, integ, ufs };
}

async function replica(e: Env, s: Side, id: Uint8Array, local = store()): Promise<Replica> {
  const r = await openReplica(s.ufs, id, solo(local));
  e.closers.push(() => r.close());
  return r;
}

const keyOf = (id: Uint8Array) => `2${bytes.hex(id)}`;

/** The tree's floor on `integ` (its own checks and verified checkpoints). */
function treeEntry(integ: Integrity, id: Uint8Array) {
  return integ.floor(keyOf(id)) as Extract<CheckpointEntry, { type: 'tree' }> | undefined;
}

/** Wait until `integ` has checked (or verified) the tree at `ops`. */
async function checkedAt(integ: Integrity, r: Replica, id: Uint8Array, ops: bigint) {
  return until(async () => {
    await r.sync();
    const t = treeEntry(integ, id);
    if (t?.ops !== ops) throw new Error(`at ${t?.ops}, not ${ops}`);
    return t;
  });
}

async function alarmOf(p: Promise<unknown>): Promise<IntegrityError> {
  try {
    await p;
  } catch (e) {
    if (e instanceof IntegrityError) return e;
    throw e;
  }
  throw new Error('no alarm');
}

/** A new tree with one directory, made on `s`. */
async function newTree(s: Side): Promise<{ id: Uint8Array; t: Tree }> {
  const id = Tree.newId();
  const t = s.ufs.tree(id);
  await t.mkdir(ROOT, 'base');
  return { id, t };
}

describe('tree checkpoints', () => {
  it('honest replicas agree: incrementally, from scratch, across devices and a reopen', async () => {
    const e = await setup();
    const a = await side(e, e.w.server.url, e.w.admin);
    const b = await side(e, e.w.server.url, e.bob);
    const { id, t } = await newTree(a);
    const ra = await replica(e, a, id);
    const rb = await replica(e, b, id);
    await ra.sync();
    await rb.sync();
    // Both write, concurrently, and follow each other.
    await Promise.all([
      (async () => {
        await ra.mkdir('/a');
        for (let i = 0; i < 5; i++) await ra.writeFile(`/a/f${i}`, bytes.utf8(`a${i}`));
        await ra.rename('/a/f0', '/a/g0');
        await ra.flush();
      })(),
      (async () => {
        await rb.mkdir('/b');
        for (let i = 0; i < 5; i++) await rb.writeFile(`/b/f${i}`, bytes.utf8(`b${i}`));
        await rb.unlink('/b/f1');
        await rb.flush();
      })(),
    ]);
    const { ops } = await t.chain();
    const ea = await checkedAt(a.integ, ra, id, ops);
    const eb = await checkedAt(b.integ, rb, id, ops);
    expect(bytes.equal(ea.state, eb.state)).toBe(true);
    expect(bytes.equal(ea.chain, eb.chain)).toBe(true);
    // A third replica, a full sync from scratch, verify-only.
    const c = await side(e, e.w.server.url, e.w.admin, { signer: false });
    const rc = await replica(e, c, id);
    const ec = await checkedAt(c.integ, rc, id, ops);
    expect(bytes.equal(ec.state, ea.state)).toBe(true);
    // Each device's checkpoint names the tree; each verifies the other's.
    expect(await a.integ.checkpoint()).toBeDefined();
    expect(await b.integ.checkpoint()).toBeDefined();
    await a.integ.sync();
    await b.integ.sync();
    await c.integ.sync();
    const own = a.integ.deviceHead(e.w.admin.device.fingerprint)!.body;
    const named = own.objects.find((o) => o.type === 'tree' && bytes.equal(o.tree, id));
    expect(named).toMatchObject({ type: 'tree', ops });
    expect([...a.integ.alarms, ...b.integ.alarms, ...c.integ.alarms]).toEqual([]);
    // Reopened from its store, a replica rebuilds the same state and checks again.
    const local = store();
    const ra2 = await replica(e, a, id, local);
    await ra2.sync();
    await ra2.close();
    const a2 = await side(e, e.w.server.url, e.w.admin, { signer: false });
    const ra3 = await replica(e, a2, id, local);
    const e3 = await checkedAt(a2.integ, ra3, id, ops);
    expect(bytes.equal(e3.state, ea.state)).toBe(true);
    expect((await ra3.status()).alarm).toBeUndefined();
  });

  it('a purge extends the op chain: replicas before and after it agree', async () => {
    const e = await setup({
      limits: { crdt_horizon_secs: 1, crdt_max_skew_ms: 100, sweep_interval_secs: 1 },
    });
    const a = await side(e, e.w.server.url, e.w.admin);
    const { id, t } = await newTree(a);
    const ra = await replica(e, a, id);
    await ra.mkdir('/d');
    await ra.writeFile('/d/f', bytes.utf8('x'));
    await ra.unlink('/d/f');
    await ra.flush();
    const n = (await t.chain()).ops;
    const before = await checkedAt(a.integ, ra, id, n);
    // The sweeper purges the trashed file: one more record in the chain.
    await until(async () => {
      if ((await t.chain()).ops !== n + 1n) throw new Error('not purged yet');
    }, 20_000);
    // A replica from scratch after the purge, on another device.
    const b = await side(e, e.w.server.url, e.bob);
    const rb = await replica(e, b, id);
    const eb = await checkedAt(b.integ, rb, id, n + 1n);
    expect(bytes.equal(eb.state, before.state)).toBe(false);
    // The replica that held the trashed file follows the tombstone.
    const ea = await checkedAt(a.integ, ra, id, n + 1n);
    expect(bytes.equal(ea.state, eb.state)).toBe(true);
    await a.integ.checkpoint();
    await b.integ.checkpoint();
    await a.integ.sync();
    await b.integ.sync();
    expect([...a.integ.alarms, ...b.integ.alarms]).toEqual([]);
  });

  it('rollback: a replica served an older copy of the store stops following it', async () => {
    const e = await setup();
    const la = pins();
    const lb = pins();
    const a = await side(e, e.w.server.url, e.w.admin, { local: la });
    const { id, t } = await newTree(a);
    const ra = await replica(e, a, id);
    await ra.flush();
    const lrb = store();
    let b = await side(e, e.w.server.url, e.bob, { local: lb });
    let rb = await replica(e, b, id, lrb);
    const n = (await t.chain()).ops;
    await checkedAt(b.integ, rb, id, n);
    await rb.close();
    await ra.close();
    a.ufs.close();
    b.ufs.close();

    const { self, copy } = await e.w.server.fork();
    e.servers = [self, copy];
    // A writes twice on the original and checkpoints; B verifies it there.
    const a1 = await side(e, self.url, e.w.admin, { local: la });
    const ra1 = await replica(e, a1, id);
    await ra1.mkdir('/x');
    await ra1.mkdir('/y');
    await ra1.flush();
    await checkedAt(a1.integ, ra1, id, n + 2n);
    await a1.integ.checkpoint();
    const b1 = await side(e, self.url, e.bob, { local: lb });
    await b1.integ.sync();
    expect(treeEntry(b1.integ, id)?.ops).toBe(n + 2n);
    b1.ufs.close();

    // B's replica, with its pins and store, is served the copy.
    b = await side(e, copy.url, e.bob, { local: lb });
    rb = await replica(e, b, id, lrb);
    const err = await alarmOf(rb.sync());
    expect(err.alarm).toMatchObject({ kind: 'rollback', object: 'tree' });
    expect(bytes.equal(err.alarm.id!, id)).toBe(true);
    // The attack worked (the copy has neither directory), and the feed stopped.
    expect(await rb.readdir('/')).not.toContain('x');
    expect((await rb.status()).alarm?.alarm.kind).toBe('rollback');
    await expect(rb.sync()).rejects.toBeInstanceOf(IntegrityError);
  });

  it('fork: two servers on one history; a replica that moves across sees two states at one ops', async () => {
    const e = await setup();
    const a = await side(e, e.w.server.url, e.w.admin);
    const { id, t } = await newTree(a);
    const n = (await t.chain()).ops;
    a.ufs.close();

    const { self, copy } = await e.w.server.fork();
    e.servers = [self, copy];
    const a1 = await side(e, self.url, e.w.admin);
    const b2 = await side(e, copy.url, e.bob);
    const ra = await replica(e, a1, id);
    const rb = await replica(e, b2, id);
    await ra.mkdir('/on-s1');
    await rb.mkdir('/on-s2');
    await ra.flush();
    await rb.flush();
    // Each side is consistent alone.
    const ea = await checkedAt(a1.integ, ra, id, n + 1n);
    const eb = await checkedAt(b2.integ, rb, id, n + 1n);
    expect(bytes.equal(ea.chain, eb.chain)).toBe(false);
    await a1.integ.checkpoint();
    await b2.integ.checkpoint();
    await a1.integ.sync();
    await b2.integ.sync();
    expect([...a1.integ.alarms, ...b2.integ.alarms]).toEqual([]);

    // A third client verifies A's checkpoint on S1, then follows the tree on S2.
    const lc = pins();
    const c1 = await side(e, self.url, e.w.admin, { signer: false, local: lc });
    await c1.integ.sync();
    expect(treeEntry(c1.integ, id)?.ops).toBe(n + 1n);
    c1.ufs.close();
    const c2 = await side(e, copy.url, e.w.admin, { signer: false, local: lc });
    const rc = await replica(e, c2, id);
    const err = await alarmOf(rc.sync());
    expect(err.alarm).toMatchObject({ kind: 'fork', object: 'tree' });
    expect(await rc.readdir('/')).toContain('on-s2'); // the attack worked
  });

  it('a wrong merge: the same ops with another state is a fork', async () => {
    const e = await setup();
    const a = await side(e, e.w.server.url, e.w.admin);
    const { id, t } = await newTree(a);
    const ra = await replica(e, a, id);
    const node = await ra.writeFile('/name', bytes.utf8('x'), { wait: true });
    const oldMeta = (await t.getOne(node))?.state.meta;
    expect(oldMeta).toBeDefined();
    await ra.rename('/name', '/renamed');
    await ra.flush();
    const ops = (await t.chain()).ops;
    await checkedAt(a.integ, ra, id, ops);
    await a.integ.checkpoint();

    // B's transport serves the node's first meta (its old name) in place of the current one.
    const tamper: typeof fetch = async (input, init) => {
      const resp = await fetch(input, init);
      if (!String(input).endsWith('/v1/fs/tree/changes') || !resp.ok) return resp;
      const c = zw.decodeChanges(new Uint8Array(await resp.arrayBuffer()));
      for (const ch of c.changes) {
        if (ch.state && bytes.equal(ch.node, node)) ch.state.meta = oldMeta!;
      }
      return new Response(zw.encodeChanges(c) as Uint8Array<ArrayBuffer>, {
        status: 200,
        headers: { 'content-type': resp.headers.get('content-type') ?? 'application/cbor' },
      });
    };
    const b = await side(e, e.w.server.url, e.bob, { fetch: tamper });
    await b.integ.sync(); // A's checkpoint: the floor at `ops`
    const rb = await replica(e, b, id);
    const err = await alarmOf(rb.sync());
    expect(err.alarm).toMatchObject({ kind: 'fork', object: 'tree' });
    expect(err.alarm.detail).toBe('tree state changed at the same ops');
    expect(await rb.readdir('/')).toContain('name'); // the attack worked
  });
});
