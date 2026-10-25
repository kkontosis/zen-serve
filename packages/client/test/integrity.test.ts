// Integrity (formats.md §5.1, §12.1, §14): device checkpoints, pins, signed
// headers and event chains, against the test adversary and forked servers.
// Each attack test first shows the attack works (the server serves the stale
// or forked answer), then that it is detected.

import type { EventBody } from '@zen/wasm';
import { afterEach, describe, expect, it } from 'vitest';
import { equal, randomBytes, utf8 } from '../src/bytes.js';
import {
  connect,
  credentialVault,
  type Fs,
  fullGrants,
  type Integrity,
  IntegrityError,
  type IntegrityOptions,
  isCode,
  member,
  memoryLocalStore,
  type SealedStore,
  type Session,
  type Signer,
  sealedStore,
  type Topic,
  type TopicEvent,
  type UnlockedFs,
  zw,
} from '../src/index.js';
import { lie, type TestServer, type TestUser, testUser, truth } from '../src/testing/index.js';
import { type World, world } from './helpers.js';

const text = (b: Uint8Array) => new TextDecoder().decode(b);

async function all<T>(it: AsyncIterable<T>): Promise<T[]> {
  const out: T[] = [];
  for await (const x of it) out.push(x);
  return out;
}

async function until(f: () => boolean | Promise<boolean>, ms = 5000): Promise<void> {
  const end = Date.now() + ms;
  while (!(await f())) {
    if (Date.now() > end) throw new Error('timed out');
    await new Promise((r) => setTimeout(r, 20));
  }
}

/** Whether `p` rejects with an integrity alarm of this kind (and object). */
async function alarmOf(p: Promise<unknown>): Promise<IntegrityError> {
  try {
    await p;
  } catch (e) {
    if (e instanceof IntegrityError) return e;
    throw e;
  }
  throw new Error('no alarm');
}

const signer = (u: TestUser): Signer => ({ device: u.device, cert: u.cert });

interface Env {
  w: World;
  bob: TestUser;
  bobSession: Session;
  recoveryKey: Uint8Array;
  servers: TestServer[];
}

let env: Env | undefined;

/** A fresh server (the adversary on), fs 1 created by the admin, and a second member, bob. */
async function setup(opts: { signedHeader?: boolean } = {}): Promise<Env> {
  const w = await world({ adversary: true, auth: { api_tokens: true } });
  const bob = testUser(2);
  await w.session.acl.update(w.admin.identity, (doc) => {
    doc.members.push(member(bob.identity, [bob.cert]));
    doc.grants.push(...fullGrants(1, bob.fp));
  });
  if (opts.signedHeader) w.session.useIdentity(w.admin.identity);
  let recoveryKey: Uint8Array = new Uint8Array(0);
  const u = await w.session.fs(1).init(
    (k) => {
      const r = zw.createRecoverySlot(k);
      recoveryKey = r.recoveryKey;
      return [r.slot];
    },
    { signed: opts.signedHeader ?? false },
  );
  u.close();
  const bobSession = await w.client.signInDevice(bob);
  env = { w, bob, bobSession, recoveryKey, servers: [w.server] };
  return env;
}

afterEach(async () => {
  if (!env) return;
  for (const s of env.servers) await s.stop();
  env = undefined;
});

interface Side {
  fs: Fs;
  integ: Integrity;
  u: UnlockedFs;
}

/** Open fs 1 with integrity attached (`device` signs, unless verify-only). */
async function open(
  e: Env,
  session: Session,
  device: TestUser | Signer | undefined,
  opts: Partial<IntegrityOptions> = {},
): Promise<Side> {
  const fs = session.fs(1);
  const s = device && 'identity' in device ? signer(device) : device;
  const integ = await fs.integrity({ ...(s ? { device: s } : {}), ...opts });
  const u = await fs.unlock({ recoveryKey: e.recoveryKey });
  return { fs, integ, u };
}

const pins = (): SealedStore => sealedStore(memoryLocalStore(), { device: false, plaintext: true });

/** Append by hand: a version-2 body with any chain fields (what a forging writer could seal). */
async function forge(
  t: Topic,
  sender: Uint8Array,
  chainId: Uint8Array,
  dseq: bigint,
  dprev: Uint8Array,
  payload: string,
  key?: string,
): Promise<EventBody> {
  const keys = t.fs.sealKeys();
  const body: EventBody = {
    sender,
    hlc: t.fs.session.clock.tick(),
    causation: new Uint8Array(0),
    payload: utf8(payload),
    chainId,
    dseq,
    dprev,
  };
  const keyToken = key === undefined ? undefined : t.keyToken(key);
  await t.fs.session.call('/v1/log/append', zw.encodeLogAppend, zw.decodeCommitResult, {
    commit_id: randomBytes(16),
    append: [
      {
        fs: t.fs.id,
        topic: t.id,
        ...(keyToken ? { key_token: keyToken } : {}),
        envelope: keys.sealEvent(t.keysAt(keys.epoch), keyToken, body),
      },
    ],
  });
  return body;
}

/** The raw bodies of a topic, unchecked. */
async function bodies(t: Topic): Promise<EventBody[]> {
  const r = await t.fs.session.call('/v1/log/read', zw.encodeLogRead, zw.decodeLogEvents, {
    fs: t.fs.id,
    topic: t.id,
  });
  return r.events.map((e) => t.open(e.key_token, e.envelope));
}

describe('integrity', () => {
  it('enrollment: a session without a device key enrolls one that signs checkpoints', async () => {
    const e = await setup();
    const t = await e.w.session.createApiToken({ user: e.w.admin.fp, label: 'enroll' });
    const s = e.w.client.withApiToken(t.token);
    const vault = credentialVault(sealedStore(memoryLocalStore()));
    const { device, fp } = await s.enrollDevice(e.w.admin.identity, { vault });
    expect(equal(fp, device.device.fingerprint)).toBe(true);
    const [info] = await s.devices([fp]);
    expect(equal(info!.user, e.w.admin.fp)).toBe(true);
    expect(equal(info!.cert, device.cert)).toBe(true);

    // The vault gives it back as the signer.
    const kept = await vault.signer();
    expect(equal(kept!.device.fingerprint, fp)).toBe(true);
    const a = await open(e, s, { device: kept!.device, cert: kept!.cert });
    await a.u.topic('t').append(utf8('x'));
    expect(await a.integ.checkpoint()).toBe(1n);

    // Its checkpoints verify for another member (the cert comes from the registry).
    const b = await open(e, e.bobSession, e.bob);
    await b.integ.sync();
    expect(b.integ.deviceHead(fp)?.seq).toBe(1n);
    expect(b.integ.alarms).toEqual([]);

    // Another user's identity can't certify a device for this user.
    await expect(
      s.approveDevice(device.device.devicePublic, testUser(3).identity),
    ).rejects.toMatchObject({
      status: 400,
    });
  });

  it('honest checkpoints: cumulative, cross-linked, persistent, on cadence', async () => {
    const e = await setup();
    const la = pins();
    const a = await open(e, e.w.session, e.w.admin, { local: la });
    const b = await open(e, e.bobSession, e.bob);
    await a.u.topic('t1').append(utf8('a1'));
    expect(await a.integ.checkpoint()).toBe(1n);
    await a.u.topic('t2').append(utf8('a2'));
    expect(await a.integ.checkpoint()).toBe(2n);
    // Cumulative: the second body still names t1.
    const ah = a.integ.deviceHead(e.w.admin.device.fingerprint)!;
    expect(ah.body.objects.map((o) => o.type)).toEqual(['topic', 'topic']);
    expect(ah.body.seq).toBe(2n);
    expect(await a.integ.checkpoint()).toBeUndefined(); // nothing new

    await b.u.topic('t1').append(utf8('b1'));
    await b.integ.sync();
    expect(b.integ.deviceHead(e.w.admin.device.fingerprint)?.seq).toBe(2n);
    expect(await b.integ.checkpoint()).toBe(1n);
    const bh = b.integ.deviceHead(e.bob.device.fingerprint)!;
    expect(bh.body.seen.map((x) => x.seq)).toEqual([2n]);
    await a.integ.sync();
    expect(a.integ.deviceHead(e.bob.device.fingerprint)?.seq).toBe(1n);

    // Pins survive a new process on the same store, and a server restart.
    const a2fs = e.w.session.fs(1);
    const a2 = await a2fs.integrity({ device: signer(e.w.admin), local: la });
    expect(a2.deviceHead(e.bob.device.fingerprint)?.seq).toBe(1n);
    expect(a2.deviceHead(e.w.admin.device.fingerprint)?.seq).toBe(2n);
    e.servers[0] = await e.w.server.restart();
    const u2 = await a2fs.unlock({ recoveryKey: e.recoveryKey });
    await a2.sync();
    expect(a2.alarms).toEqual([]);
    u2.close();

    // Cadence: every 3 changes, and after a pause.
    const c = await open(e, e.bobSession, e.bob, { sigEvery: 3, delayMs: 150 });
    const head = () => c.integ.deviceHead(e.bob.device.fingerprint)?.seq ?? 0n;
    const t = c.u.topic('cadence');
    for (let i = 0; i < 3; i++) await t.append(utf8(`c${i}`));
    await until(() => head() === 2n);
    await t.append(utf8('c3'));
    await new Promise((r) => setTimeout(r, 50));
    expect(head()).toBe(2n);
    await until(() => head() === 3n);
    expect(a.integ.alarms).toEqual([]);
    expect(b.integ.alarms).toEqual([]);
    expect(c.integ.alarms).toEqual([]);
  });

  it('one device, two processes: puts race, merge, and both reach the head', async () => {
    const e = await setup();
    const p1 = await open(e, e.w.session, e.w.admin);
    const p2 = await open(e, e.w.session, e.w.admin);
    await p1.u.topic('one').append(utf8('1'));
    await p2.u.topic('two').append(utf8('2'));
    const seqs = await Promise.all([p1.integ.checkpoint(), p2.integ.checkpoint()]);
    expect(seqs.map(Number).sort()).toEqual([1, 2]);
    const v = await open(e, e.bobSession, e.bob);
    await v.integ.sync();
    const h = v.integ.deviceHead(e.w.admin.device.fingerprint)!;
    expect(h.seq).toBe(2n);
    const topics = h.body.objects.flatMap((o) => (o.type === 'topic' ? [o.topic] : []));
    const ids = [p1.u.topic('one').id, p1.u.topic('two').id];
    expect(ids.every((id) => topics.some((x) => equal(x, id)))).toBe(true);
    expect([...p1.integ.alarms, ...p2.integ.alarms, ...v.integ.alarms]).toEqual([]);
  });

  it('watch: pushed heads sync without polling, across a reconnection', async () => {
    const e = await setup();
    const a = await open(e, e.w.session, e.w.admin);
    const b = await open(e, e.bobSession, e.bob);
    const stream = await e.bobSession.stream({ backoffMs: [20, 100] });
    const sub = await b.integ.watch(stream);
    const seq = () => b.integ.deviceHead(e.w.admin.device.fingerprint)?.seq ?? 0n;
    await a.u.topic('w').append(utf8('1'));
    await a.integ.checkpoint();
    await until(() => seq() === 1n);
    stream.reconnect();
    await a.u.topic('w').append(utf8('2'));
    await a.integ.checkpoint();
    await until(() => seq() === 2n);
    await sub.close();
    stream.close();
    expect(b.integ.alarms).toEqual([]);
  });

  it('chains: v2 bodies, concurrent chains, retried transactions, filtered readers', async () => {
    const e = await setup();
    const a = await open(e, e.w.session, e.w.admin);
    const t = a.u.topic('chains');
    await t.append(utf8('s1'));
    await t.append(utf8('s2'));
    await Promise.all([1, 2, 3].map((i) => t.append(utf8(`c${i}`))));
    await t.append(utf8('s3'));
    const bs = await bodies(t);
    expect(bs.every((b) => b.chainId && b.dseq !== undefined)).toBe(true);
    expect(equal(bs[0]!.sender, e.w.admin.device.fingerprint)).toBe(true);
    expect(bs[1]!.dseq).toBe(2n);
    expect(new Set(bs.map((b) => Buffer.from(b.chainId!).toString('hex'))).size).toBe(3);

    // A transaction that conflicts and runs again reuses its dseq: no gap.
    let runs = 0;
    await a.u.transaction(async (tx) => {
      await tx.get(['k']);
      if (runs++ === 0) await a.u.kv.set(['k'], utf8('other'));
      tx.set(['k'], utf8('mine'));
      t.appendIn(tx, utf8('tx1'));
      t.appendIn(tx, utf8('tx2'));
    });
    expect(runs).toBe(2);
    const evs = await all(t.read());
    expect(evs.map((x) => text(x.payload)).slice(-2)).toEqual(['tx1', 'tx2']);

    // Readers of a subset: a key, a group.
    await t.append(utf8('k1'), { key: 'a' });
    await t.append(utf8('k2'), { key: 'b' });
    await t.append(utf8('k3'), { key: 'a' });
    expect((await all(t.read({ key: 'a' }))).map((x) => text(x.payload))).toEqual(['k1', 'k3']);
    const g = await t.group('g', { mode: 'per_key' });
    const ds = await g.next({ limit: 10 });
    expect(ds.length).toBeGreaterThan(0);

    // Another reader checks every chain.
    const b = await open(e, e.bobSession, e.bob);
    await b.integ.sync();
    expect((await all(b.u.topic('chains').read())).length).toBe(evs.length + 3);
    await b.u.topic('chains').verifyHead();
    expect([...a.integ.alarms, ...b.integ.alarms]).toEqual([]);
  });

  it('chain gap and rewrite are detected', async () => {
    const e = await setup();
    const a = await open(e, e.w.session, e.w.admin);
    const fp = e.w.admin.device.fingerprint;
    const z = new Uint8Array(32);

    // A gap: dseq 1, then 3.
    const g = a.u.topic('gap');
    const cid = randomBytes(12);
    const b1 = await forge(g, fp, cid, 1n, z, 'g1');
    await forge(g, fp, cid, 3n, zw.eventChainHash(b1), 'g3');
    const gap = await alarmOf(all(g.read()));
    expect(gap.alarm).toMatchObject({ kind: 'gap', object: 'topic' });
    const stream = await e.w.session.stream();
    const sub = await stream.subscribe(g, { after: new Uint8Array(12) });
    const seen: unknown[] = [];
    const subGap = await alarmOf(
      (async () => {
        for await (const x of sub) seen.push(x);
      })(),
    );
    expect(subGap.alarm.kind).toBe('gap');
    expect(seen.length).toBe(1);
    stream.close();

    // A rewrite: two bodies at dseq 1.
    const r = a.u.topic('rewrite');
    const cid2 = randomBytes(12);
    await forge(r, fp, cid2, 1n, z, 'r1');
    await forge(r, fp, cid2, 1n, z, 'r1-other');
    expect((await alarmOf(all(r.read()))).alarm.kind).toBe('rewrite');

    // A filtered reader against a pinned link: bob pins a's chain (from its
    // checkpoint), then meets another body at that dseq under the same key.
    const e2 = await open(e, e.w.session, e.w.admin);
    const p = e2.u.topic('pinned');
    await p.append(utf8('real'), { key: 'k' });
    await e2.integ.checkpoint();
    const [real] = await bodies(p);
    await forge(p, fp, real!.chainId!, 1n, z, 'fake', 'k');
    const b = await open(e, e.bobSession, e.bob);
    await b.integ.sync();
    const bp = b.u.topic('pinned');
    const got: TopicEvent[] = [];
    const filt = await alarmOf(
      (async () => {
        for await (const x of bp.read({ key: 'k' })) got.push(x);
      })(),
    );
    expect(filt.alarm.kind).toBe('rewrite');
    expect(got.map((x) => text(x.payload))).toEqual(['real']);
  });

  it('signed header: signed versions chain, verify and pin; a signer is required', async () => {
    const e = await setup({ signedHeader: true });
    const a = await open(e, e.w.session, e.w.admin);
    expect(a.u.signed).toBe(true);
    expect(a.integ.headerPin?.version).toBe(1n);
    await a.u.addRecovery();
    await a.u.addRecovery();
    expect(a.integ.headerPin?.version).toBe(3n);

    const b = await open(e, e.bobSession, e.bob);
    const h = await b.fs.header();
    expect(h?.signed?.version).toBe(3n);
    expect(equal(h!.signed!.signer, e.w.admin.fp)).toBe(true);
    expect(b.integ.headerPin?.version).toBe(3n);

    // Without the identity, an edit of a signed header stops before writing.
    const plain = await e.w.client.signInDevice(e.w.admin);
    const pu = await plain.fs(1).unlock({ recoveryKey: e.recoveryKey });
    await expect(pu.addRecovery()).rejects.toSatisfy((x) => isCode(x, 'signer_needed'));

    // A member who isn't an admin can't write it, signed or not.
    e.bobSession.useIdentity(e.bob.identity);
    const cur = await b.fs.header();
    await expect(
      b.fs.putHeader(cur!.header, cur!.version, { after: cur!.signed! }),
    ).rejects.toMatchObject({ status: 403 });
    expect(b.integ.alarms).toEqual([]);
  });

  it('header rollback: a frozen header is caught by its writer and by others', async () => {
    const e = await setup({ signedHeader: true });
    const a = await open(e, e.w.session, e.w.admin);
    const b = await open(e, e.bobSession, e.bob);
    expect(b.integ.headerPin?.version).toBe(1n);
    await lie(e.w.session, [{ kind: 'header', fs: 1 }]);
    await a.u.addRecovery(); // signs version 2; the server keeps it
    expect(a.integ.headerPin?.version).toBe(2n);
    await a.integ.checkpoint();
    // The attack works: version 1 is served.
    expect((await e.bobSession.fs(1).header())?.signed?.version).toBe(1n);
    // bob learns version 2 from a's checkpoint, and the served header is older.
    const bAlarm = await alarmOf(b.integ.sync());
    expect(bAlarm.alarm).toMatchObject({ kind: 'rollback', object: 'header' });
    const aAlarm = await alarmOf(a.fs.header());
    expect(aAlarm.alarm).toMatchObject({ kind: 'rollback', object: 'header' });
    await truth(e.w.session);
  });

  it('topic tail withheld: caught after syncing the writer’s checkpoint', async () => {
    const e = await setup();
    const a = await open(e, e.w.session, e.w.admin);
    const t = a.u.topic('tail');
    for (const p of ['e1', 'e2', 'e3']) await t.append(utf8(p));
    await lie(e.w.session, [{ kind: 'topic', fs: 1, topic: t.id }]);
    await t.append(utf8('e4'));
    await t.append(utf8('e5'));
    await a.integ.checkpoint();

    const b = await open(e, e.bobSession, e.bob);
    const bt = b.u.topic('tail');
    // The attack works: e4 and e5 are withheld, and alone bob can't tell.
    expect((await all(bt.read())).map((x) => text(x.payload))).toEqual(['e1', 'e2', 'e3']);
    expect(b.integ.alarms).toEqual([]);
    await b.integ.sync();
    expect((await alarmOf(all(bt.read()))).alarm).toMatchObject({
      kind: 'rollback',
      object: 'topic',
    });
    expect((await alarmOf(bt.verifyHead())).alarm.kind).toBe('rollback');
    // The writer itself sees its own tail missing too.
    expect((await alarmOf(all(t.read()))).alarm.kind).toBe('rollback');
    await truth(e.w.session);
  });

  it('checkpoints withheld: the writer sees its head rolled back; others by comparing pins', async () => {
    const e = await setup();
    const a = await open(e, e.w.session, e.w.admin);
    const t = a.u.topic('ck');
    for (let i = 1; i <= 2; i++) {
      await t.append(utf8(`${i}`));
      await a.integ.checkpoint();
    }
    await lie(e.w.session, [{ kind: 'checkpoints', fs: 1, device: e.w.admin.device.fingerprint }]);
    for (let i = 3; i <= 4; i++) {
      await t.append(utf8(`${i}`));
      expect(await a.integ.checkpoint()).toBe(BigInt(i)); // the server takes them
    }
    expect((await alarmOf(a.integ.sync())).alarm).toMatchObject({
      kind: 'rollback',
      object: 'checkpoint',
    });
    // The attack works on bob alone: he sees seq 2, no alarm.
    const b = await open(e, e.bobSession, e.bob);
    await b.integ.sync();
    expect(b.integ.deviceHead(e.w.admin.device.fingerprint)?.seq).toBe(2n);
    expect(b.integ.alarms).toEqual([]);
    const found = await b.integ.comparePins(a.integ.exportPins());
    expect(found.some((x) => x.kind === 'fork' && x.object === 'checkpoint')).toBe(true);
    await truth(e.w.session);
  });

  it('fork: two servers on one history; caught across, and by a moved client', async () => {
    const e = await setup();
    const la = pins();
    const lb = pins();
    const fpA = e.w.admin.device.fingerprint;
    const a = await open(e, e.w.session, e.w.admin, { local: la });
    const b = await open(e, e.bobSession, e.bob, { local: lb });
    await a.u.topic('f').append(utf8('before'));
    await a.integ.checkpoint();
    await b.integ.sync();
    a.u.close();
    b.u.close();

    const { self, copy } = await e.w.server.fork();
    e.servers = [self, copy];
    const c1 = await connect(self.url, { origin: e.w.client.origin });
    const c2 = await connect(copy.url, { origin: e.w.client.origin });
    // a stays on S1; b on S2. Each is consistent alone.
    const a1 = await open(e, await c1.signInDevice(e.w.admin), e.w.admin, { local: la });
    const b2 = await open(e, await c2.signInDevice(e.bob), e.bob, { local: lb });
    await a1.u.topic('f').append(utf8('on S1'));
    expect(await a1.integ.checkpoint()).toBe(2n);
    await b2.u.topic('f').append(utf8('on S2'));
    expect(await b2.integ.checkpoint()).toBe(1n);
    await a1.integ.sync();
    await b2.integ.sync();
    expect([...a1.integ.alarms, ...b2.integ.alarms]).toEqual([]);
    // Across: their pins name heads the other server can't produce.
    expect((await b2.integ.comparePins(a1.integ.exportPins())).map((x) => x.kind)).toContain(
      'fork',
    );
    expect((await a1.integ.comparePins(b2.integ.exportPins())).map((x) => x.kind)).toContain(
      'fork',
    );

    // a's device moved to S2 with its pins: its head there is below its pin.
    const lx = pins();
    const x1 = await open(e, await c1.signInDevice(e.bob), undefined, { local: lx });
    await x1.integ.sync(); // pins a@2 (S1)
    const moved = await open(e, await c2.signInDevice(e.w.admin), e.w.admin, { local: la });
    expect((await alarmOf(moved.integ.sync())).alarm).toMatchObject({
      kind: 'rollback',
      object: 'checkpoint',
    });
    // a fresh process of a on S2 (no pins) writes another seq 2.
    const fresh = await open(e, await c2.signInDevice(e.w.admin), e.w.admin);
    await fresh.u.topic('f').append(utf8('another 2'));
    expect(await fresh.integ.checkpoint()).toBe(2n);
    // A client that pinned a@2 on S1 moves to S2: two bodies of one seq.
    const x2 = await open(e, await c2.signInDevice(e.bob), undefined, { local: lx });
    const f = await alarmOf(x2.integ.sync());
    expect(f.alarm).toMatchObject({ kind: 'fork', object: 'checkpoint' });
    expect(equal(f.alarm.id!, fpA)).toBe(true);
  });

  it('whole-store rollback: an older copy of the store is caught', async () => {
    const e = await setup();
    const la = pins();
    const a = await open(e, e.w.session, e.w.admin, { local: la });
    await a.u.topic('w').append(utf8('1'));
    await a.integ.checkpoint();
    a.u.close();
    const { self, copy } = await e.w.server.fork();
    e.servers = [self, copy];
    // On the original: an ACL version, a signed header, a checkpoint.
    const c1 = await connect(self.url, { origin: e.w.client.origin });
    const s1 = await c1.signInDevice(e.w.admin);
    s1.useIdentity(e.w.admin.identity);
    const a1 = await open(e, s1, e.w.admin, { local: la });
    await s1.acl.update(e.w.admin.identity, (doc) => {
      doc.members.push(member(testUser(3).identity));
    });
    await a1.u.signHeader();
    await a1.u.topic('w').append(utf8('2'));
    await a1.integ.checkpoint();
    expect(a1.integ.alarms).toEqual([]);
    a1.u.close();

    // The client, with those pins, is served the older copy.
    const c2 = await connect(copy.url, { origin: e.w.client.origin });
    const s2 = await c2.signInDevice(e.w.admin);
    const fs = s2.fs(1);
    const integ = await fs.integrity({ device: signer(e.w.admin), local: la });
    expect((await alarmOf(fs.unlock({ recoveryKey: e.recoveryKey }))).alarm).toMatchObject({
      kind: 'rollback',
      object: 'header',
    });
    await expect(s2.acl.head()).rejects.toSatisfy((x) => isCode(x, 'acl_chain'));
    const objects = integ.alarms.map((x) => x.object);
    expect(objects).toContain('header');
  });

  it('honest runs report nothing', async () => {
    const e = await setup({ signedHeader: true });
    const a = await open(e, e.w.session, e.w.admin, { sigEvery: 2, delayMs: 100 });
    const b = await open(e, e.bobSession, e.bob, { sigEvery: 3, delayMs: 100 });
    const stream = await e.bobSession.stream({ backoffMs: [20, 100], connectTimeoutMs: 500 });
    const errors: unknown[] = [];
    const watch = await b.integ.watch(stream, { onError: (x) => errors.push(x) });
    const ta = a.u.topic('mix');
    const tb = b.u.topic('mix');
    for (let i = 0; i < 4; i++) {
      await Promise.all([ta.append(utf8(`a${i}`)), tb.append(utf8(`b${i}`))]);
      await ta.append(utf8(`k${i}`), { key: i % 2 ? 'x' : 'y' });
    }
    await a.u.addRecovery();
    await e.w.session.acl.update(e.w.admin.identity, (doc) => {
      doc.members.push(member(testUser(4).identity));
    });
    await a.integ.checkpoint();
    await b.integ.checkpoint();
    await a.integ.sync();
    await b.integ.sync();
    e.servers[0] = await e.w.server.restart();
    await ta.append(utf8('after restart'));
    await a.integ.checkpoint();
    await until(
      () =>
        (b.integ.deviceHead(e.w.admin.device.fingerprint)?.seq ?? 0n) ===
        a.integ.deviceHead(e.w.admin.device.fingerprint)?.seq,
    ).catch((x) => {
      throw new Error(`${x}: ${errors.join('; ')}`);
    });
    expect((await all(tb.read())).length).toBe(13);
    expect((await all(ta.read({ key: 'x' }))).length).toBe(2);
    await tb.verifyHead();
    await a.integ.sync();
    expect(await a.integ.comparePins(b.integ.exportPins())).toEqual([]);
    await watch.close();
    stream.close();
    await a.integ.close();
    await b.integ.close();
    expect([...a.integ.alarms, ...b.integ.alarms]).toEqual([]);
  });
});
