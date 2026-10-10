import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import {
  bytes,
  type ChangeBatch,
  displayNames,
  memoryClockStore,
  type OpRecord,
  ROOT,
  type Session,
  TRASH,
  Tree,
  TreeClock,
  type TreeNode,
  type UnlockedFs,
  ZenError,
  zw,
} from '../src/index.js';
import { type World, world } from './helpers.js';

const { equal, hex } = bytes;

/** Unlock fs 1 for `w.session` and for a second device (a password sign-in). */
async function setup(w: World): Promise<{ a: UnlockedFs; b: UnlockedFs; sb: Session }> {
  let rk: Uint8Array | undefined;
  const a = await w.session.fs(1).init((k) => {
    const r = zw.createRecoverySlot(k);
    rk = r.recoveryKey;
    return [r.slot];
  });
  await w.session.setPassword('alice', 'pw');
  const sb = await w.client.signInPassword('alice', 'pw');
  const b = await sb.fs(1).unlock({ recoveryKey: rk! });
  return { a, b, sb };
}

function random(n: number): Uint8Array {
  const out = new Uint8Array(n);
  for (let i = 0; i < n; i += 65536) crypto.getRandomValues(out.subarray(i, i + 65536));
  return out;
}

/** The error codes the server answered `s` with while `fn` ran. */
async function errorsDuring(s: Session, fn: () => Promise<unknown>): Promise<string[]> {
  const codes: string[] = [];
  const call = s.call.bind(s);
  s.call = (async (...args: Parameters<Session['call']>) => {
    try {
      return await call(...args);
    } catch (e) {
      if (e instanceof ZenError) codes.push(e.code);
      throw e;
    }
  }) as Session['call'];
  try {
    await fn();
  } finally {
    s.call = call;
  }
  return codes;
}

async function names(t: Tree, parent: Uint8Array): Promise<string[]> {
  return (await t.list(parent)).map((n) => n.meta?.name ?? '?').sort();
}

describe('tree operations', () => {
  let w: World;
  let a: UnlockedFs;
  let b: UnlockedFs;
  let sb: Session;
  beforeAll(async () => {
    w = await world();
    ({ a, b, sb } = await setup(w));
  });
  afterAll(() => w?.server.stop());

  it('creates, moves, renames and deletes', async () => {
    const t = a.tree(Tree.newId());
    const docs = await t.mkdir(ROOT, 'docs');
    const f = await t.create(docs, 'a.txt', { mode: 0o600, mtimeMs: 1234 });
    expect(await names(t, ROOT)).toEqual(['docs']);
    const n = (await t.getOne(f))!;
    expect(n.meta).toMatchObject({ type: 'file', name: 'a.txt', mode: 0o600, mtimeMs: 1234 });
    expect(n.parent).toEqual(docs);

    await t.move(f, ROOT, 'b.txt');
    expect(await names(t, ROOT)).toEqual(['b.txt', 'docs']);
    await t.rename(f, 'c.txt');
    await t.setMeta(f, { mode: 0o644 });
    expect((await t.getOne(f))!.meta).toMatchObject({ name: 'c.txt', mode: 0o644, mtimeMs: 1234 });

    await t.remove(docs);
    expect(await names(t, ROOT)).toEqual(['c.txt']);
    expect((await t.list(TRASH)).map((x) => hex(x.id))).toEqual([hex(docs)]);
    await t.move(docs, ROOT); // restore
    expect(await names(t, ROOT)).toEqual(['c.txt', 'docs']);

    expect((await Tree.list(a)).some((x) => equal(x.tree, t.id))).toBe(true);
    expect((await t.stat(docs))!.size).toBeUndefined();
  });

  it('batches several operations in one commit', async () => {
    const t = a.tree(Tree.newId());
    const batch = t.batch();
    const d = batch.mkdir(ROOT, 'd');
    const f = batch.create(d, 'x');
    await batch.rename(f, 'y');
    await batch.commit();
    expect(await names(t, d)).toEqual(['y']);
    expect((await t.chain()).ops).toBe(3n);
  });

  it('skips a cycle move', async () => {
    const t = a.tree(Tree.newId());
    const x = await t.mkdir(ROOT, 'x');
    const y = await t.mkdir(x, 'y');
    await t.move(x, y); // accepted, but skipped by the merge
    expect((await t.getOne(x))!.parent).toEqual(ROOT);
    expect((await t.getOne(y))!.parent).toEqual(x);
  });

  it('keeps a concurrent rename and move', async () => {
    const id = Tree.newId();
    const ta = a.tree(id);
    const tb = b.tree(id);
    const d = await ta.mkdir(ROOT, 'd');
    const f = await ta.create(ROOT, 'f');
    // Both devices have seen f at ROOT named "f".
    await tb.getOne(f);
    await tb.rename(f, 'renamed-by-b');
    await ta.move(f, d);
    const n = (await ta.getOne(f))!;
    expect(n.parent).toEqual(d);
    expect(n.meta?.name).toBe('renamed-by-b');
    expect(equal(n.state.meta_device!, sb.deviceFp!)).toBe(true);
    expect(equal(n.state.move_device, w.session.deviceFp!)).toBe(true);
  });

  it('keeps concurrent writes as siblings, then resolves them', async () => {
    const id = Tree.newId();
    const ta = a.tree(id);
    const tb = b.tree(id);
    const f = await ta.create(ROOT, 'f');
    const v1 = await ta.writeFile(f, bytes.utf8('one'));
    const [wa, wb] = await Promise.all([
      ta.writeFile(f, bytes.utf8('by a'), { replaces: [v1.dot] }),
      tb.writeFile(f, bytes.utf8('by b'), { replaces: [v1.dot] }),
    ]);
    const vs = await ta.versions(f);
    expect(vs.map((v) => hex(v.dot)).sort()).toEqual([hex(wa.dot), hex(wb.dot)].sort());
    expect((await ta.getOne(f))!.versions).toBe(2);
    const texts = await Promise.all(vs.map((v) => ta.readFile(f, { version: v })));
    expect(texts.map((x) => bytes.fromUtf8(x)).sort()).toEqual(['by a', 'by b']);
    const byB = vs.find((v) => equal(v.device, sb.deviceFp!))!;
    expect(bytes.fromUtf8(await ta.readFile(f, { version: byB.dot }))).toBe('by b');

    const dot = await ta.resolve(f, byB);
    const after = await ta.versions(f);
    expect(after.map((v) => hex(v.dot))).toEqual([hex(dot)]);
    expect(bytes.fromUtf8(await tb.readFile(f))).toBe('by b');
    // The default `replaces: 'current'` overwrites whatever is there.
    await tb.writeFile(f, bytes.utf8('last'));
    expect((await ta.versions(f)).length).toBe(1);
    expect((await ta.stat(f))!.size).toBe(4);
  });

  it('follows the change feed and wakes on a write', async () => {
    const t = a.tree(Tree.newId());
    const d = await t.mkdir(ROOT, 'd');
    let cursor: Uint8Array | undefined;
    const seen: TreeNode[] = [];
    for await (const batch of t.changes()) {
      cursor = batch.cursor;
      for (const c of batch.changes) if (c.state) seen.push(c.state);
    }
    expect(seen.map((n) => n.meta?.name)).toEqual(['d']);

    const it = t.changes(cursor, { waitMs: 10_000 })[Symbol.asyncIterator]();
    const next = it.next();
    await new Promise((r) => setTimeout(r, 300));
    const f = await t.create(d, 'woke');
    const r = await next;
    expect(r.done).toBe(false);
    const batch = r.value as ChangeBatch;
    expect(batch.changes.map((c) => c.state?.meta?.name)).toContain('woke');
    expect(batch.changes.some((c) => equal(c.node, f))).toBe(true);
    await it.return?.(undefined);
  });

  it('rebases after clock_skew from a clock persisted ahead', async () => {
    const ahead = zw.hlc(Date.now() + 10 * 60_000, 0);
    const clock = new TreeClock({ store: memoryClockStore(ahead) });
    const t = new Tree(a, Tree.newId(), { clock });
    let d!: Uint8Array;
    const codes = await errorsDuring(w.session, async () => {
      d = await t.mkdir(ROOT, 'skewed');
    });
    expect(codes).toEqual(['clock_skew']);
    expect((await t.getOne(d))!.meta?.name).toBe('skewed');
    expect(zw.hlcMs(clock.last)).toBeLessThan(Date.now() + 60_000);
  });

  it('corrects a wrong wall clock from the server time', async () => {
    const clock = new TreeClock({ now: () => Date.now() + 10 * 60_000 });
    const t = new Tree(a, Tree.newId(), { clock });
    await t.mkdir(ROOT, 'fast-clock');
    const slow = new TreeClock({ now: () => Date.now() - 8 * 86_400_000 });
    const t2 = new Tree(a, t.id, { clock: slow });
    await t2.mkdir(ROOT, 'slow-clock');
    expect(await names(t, ROOT)).toEqual(['fast-clock', 'slow-clock']);
  });

  it('verifies the op chain', async () => {
    const recs: OpRecord[] = [];
    const t = new Tree(a, Tree.newId(), { onOp: (r) => recs.push(r) });
    const d = await t.mkdir(ROOT, 'd');
    const f = await t.create(d, 'f');
    await t.rename(f, 'g');
    const w1 = await t.writeFile(f, bytes.utf8('hello'));
    await t.writeFile(f, bytes.utf8('hello again'), { replaces: [w1.dot] });
    await t.remove(d);
    expect(recs.length).toBe(6);
    expect(await t.verifyChain(recs)).toBe(true);
    expect(await t.verifyChain(recs.slice(0, 5))).toBe(false);
  });

  it('shows duplicate names as numbered copies', async () => {
    const t = a.tree(Tree.newId());
    for (const n of ['foo.txt', 'foo.txt', 'foo (2).txt', 'README', 'README']) {
      await t.create(ROOT, n);
    }
    const kids = await t.list(ROOT);
    const shown = displayNames(kids);
    expect([...shown.values()].sort()).toEqual([
      'README',
      'README (2)',
      'foo (2).txt',
      'foo (3).txt',
      'foo.txt',
    ]);
    // The lower node id keeps the name.
    const foos = kids
      .filter((k) => k.meta?.name === 'foo.txt')
      .sort((x, y) => bytes.compare(x.id, y.id));
    expect(shown.get(hex(foos[0]!.id))).toBe('foo.txt');
    expect(shown.get(hex(foos[1]!.id))).toBe('foo (3).txt');
  });
});

describe('large files and limits', () => {
  let w: World;
  let a: UnlockedFs;
  let b: UnlockedFs;
  beforeAll(async () => {
    w = await world({
      limits: { max_commit_bytes: 300_000, max_value_bytes: 90_000, crdt_max_redo: 2 },
    });
    ({ a, b } = await setup(w));
  });
  afterAll(() => w?.server.stop());

  it('uploads over several commits, reuses unchanged chunks, reads ranges', async () => {
    const t = a.tree(Tree.newId());
    const f = await t.create(ROOT, 'big.bin');
    const data = random(10 * 65536 + 1000); // 11 chunks, 4 per commit
    const w1 = await t.writeFile(f, data);
    expect(w1.uploaded).toBe(11);
    expect(w1.commits).toBe(3);
    expect(w1.manifest.size).toBe(data.length);
    expect(equal(await t.readFile(f), data)).toBe(true);

    // Ranged reads: across a chunk boundary, at the end, past the end.
    expect(equal(await t.read(f, undefined, 65530, 20), data.subarray(65530, 65550))).toBe(true);
    const v = (await t.versions(f))[0]!;
    expect(equal(await t.read(f, v, data.length - 10, 100), data.subarray(data.length - 10))).toBe(
      true,
    );
    expect((await t.read(f, v, data.length + 5, 10)).length).toBe(0);

    // Change one byte in chunk 3: only that chunk is uploaded again.
    const next = data.slice();
    next[3 * 65536 + 7]! ^= 1;
    const w2 = await t.writeFile(f, next, { previous: w1.index, replaces: [w1.dot] });
    expect(w2.uploaded).toBe(1);
    expect(w2.commits).toBe(1);
    expect(w2.manifest.chunks.filter((c, i) => equal(c, w1.manifest.chunks[i]!)).length).toBe(10);
    expect(equal(await b.tree(t.id).readFile(f), next)).toBe(true);

    // An index built from a version read back reuses as well.
    const v2 = (await t.versions(f))[0]!;
    const idx = await t.chunkIndex(f, v2);
    const appended = new Uint8Array(next.length + 70000);
    appended.set(next);
    const w3 = await t.writeFile(f, appended, { previous: idx });
    // The old last chunk (1000 bytes) is now full; it and the new tail upload.
    expect(w3.uploaded).toBe(2);
    expect(equal(await t.readFile(f), appended)).toBe(true);
  });

  it('writes from a stream of uneven pieces', async () => {
    const t = a.tree(Tree.newId());
    const f = await t.create(ROOT, 's.bin');
    const data = random(300_000);
    async function* pieces() {
      for (let i = 0; i < data.length; i += 7777) yield data.subarray(i, i + 7777);
    }
    const r = await t.writeFile(f, pieces());
    expect(r.manifest.chunks.length).toBe(5);
    expect(equal(await t.readFile(f), data)).toBe(true);
    // An empty file.
    const e = await t.create(ROOT, 'empty');
    await t.writeFile(e, new Uint8Array(0));
    expect((await t.readFile(e)).length).toBe(0);
    expect((await t.stat(e))!.size).toBe(0);
  });

  it('rebases a move refused with stale_op', async () => {
    // b's clock runs 30 s ahead (within the skew): its moves are later than
    // a's next one, which the server would have to undo three of
    // (crdt_max_redo = 2).
    const id = Tree.newId();
    const ta = a.tree(id);
    const x = await ta.mkdir(ROOT, 'x');
    const y = await ta.mkdir(ROOT, 'y');
    const ahead = new TreeClock({ store: memoryClockStore(zw.hlc(Date.now() + 30_000, 0)) });
    const tb = new Tree(b, id, { clock: ahead });
    for (let i = 0; i < 3; i++) await tb.move(x, i % 2 ? ROOT : y);
    const before = ta.clock.last;
    // A plain move reads nothing first (a rename would read the node, and
    // observe b's timestamps).
    const codes = await errorsDuring(w.session, () => ta.move(x, ROOT));
    expect(codes).toEqual(['stale_op']); // then rebased past b's moves
    expect(ta.clock.last).toBeGreaterThan(ahead.last - 10n);
    expect(ta.clock.last).toBeGreaterThan(before);
    const n = (await ta.getOne(x))!;
    expect(n.parent).toEqual(ROOT);
    expect(equal(n.state.move_device, w.session.deviceFp!)).toBe(true);
  });
});

describe('tree operations in transactions', () => {
  let w: World;
  let a: UnlockedFs;
  let b: UnlockedFs;
  const u = bytes.utf8;
  const s = (v: Uint8Array | undefined) => (v ? bytes.fromUtf8(v) : undefined);
  beforeAll(async () => {
    w = await world({ limits: { crdt_max_redo: 2 } });
    ({ a, b } = await setup(w));
  });
  afterAll(() => w?.server.stop());

  it('commits tree operations with KV writes; a conflict re-runs both', async () => {
    const t = a.tree(Tree.newId());
    await a.kv.set(['tx-tree'], u('0'));
    let runs = 0;
    await a.transaction(async (tx) => {
      runs++;
      const v = await tx.get(['tx-tree']);
      if (runs === 1) await a.kv.set(['tx-tree'], u('raced'));
      const batch = t.batch();
      batch.mkdir(ROOT, `d${runs}`);
      await batch.commitIn(tx);
      tx.set(['tx-tree'], u(`${s(v)}+dir`));
    });
    expect(runs).toBe(2);
    expect(await names(t, ROOT)).toEqual(['d2']);
    expect(s(await a.kv.get(['tx-tree']))).toBe('raced+dir');
  });

  it('rebases operations refused with stale_op inside the transaction', async () => {
    // As in `rebases a move refused with stale_op`, run in a transaction.
    const id = Tree.newId();
    const ta = a.tree(id);
    const x = await ta.mkdir(ROOT, 'x');
    const y = await ta.mkdir(ROOT, 'y');
    const ahead = new TreeClock({ store: memoryClockStore(zw.hlc(Date.now() + 30_000, 0)) });
    const tb = new Tree(b, id, { clock: ahead });
    for (let i = 0; i < 3; i++) await tb.move(x, i % 2 ? ROOT : y);
    let runs = 0;
    const codes = await errorsDuring(w.session, () =>
      a.transaction(async (tx) => {
        runs++;
        await ta.writeIn(tx, [{ op: 'move', node: x, parent: ROOT }]);
        tx.set(['rebased'], u(String(runs)));
      }),
    );
    expect(codes).toEqual(['stale_op']);
    expect(runs).toBe(2);
    const n = (await ta.getOne(x))!;
    expect(n.parent).toEqual(ROOT);
    expect(equal(n.state.move_device, w.session.deviceFp!)).toBe(true);
    expect(s(await a.kv.get(['rebased']))).toBe('2');
  });

  it('uploads a file, then writes it in a transaction', async () => {
    const t = a.tree(Tree.newId());
    const f = await t.create(ROOT, 'f');
    const data = random(150_000);
    const up = await t.upload(f, data);
    expect(up.uploaded).toBe(3);
    expect(up.commits).toBeGreaterThanOrEqual(1);
    // Not written yet.
    expect((await t.versions(f)).length).toBe(0);
    await a.transaction(async (tx) => {
      await t.writeIn(tx, [await t.writeOp(up)]);
      tx.set(['file-index'], up.index.hashes[0]!);
    });
    expect(equal(await t.readFile(f), data)).toBe(true);
    // The next write reuses unchanged chunks through `previous`.
    data[0] = (data[0]! + 1) & 0xff;
    const w2 = await t.writeFile(f, data, { previous: up.index });
    expect(w2.uploaded).toBe(1);
    expect(equal(await t.readFile(f), data)).toBe(true);
  });
});
