// Repack, gc and the public mirror (spec/git.md §9, §13), with real git: the
// geometric roll-up, its transaction and gc, a mirror covering roll-ups, a
// pack gone after gc, `zen-git worker`, and a dumb-HTTP clone of a published
// copy through the server's /unencrypted.
//
// The world's server purges trash after 3 s (crdt_horizon_secs), so a gc'd
// pack is really gone within the test.
import { execFileSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { UnlockedFs } from '@zen/client';
import { type Db, decodeMsg } from '@zen/db';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { until } from '../../../apps/shared/harness.js';
import type { WorkerContext } from '../../../apps/shared/node.js';
import { testUser } from '../../client/src/testing/index.js';
import { initMirror, refsOf, syncMirror, syncRepo } from '../src/mirror.js';
import {
  collect,
  PackGone,
  type PackRow,
  push,
  REPACK_AT,
  type RefUpdate,
  readState,
  repack,
  rollUp,
} from '../src/store.js';
import { DB_OPTS, GitWorld } from './world.js';

const USERS = ['alice', 'bob', 'ci'];
const READ_ONLY = [{ rights: ['read' as const] }];
/** Pushes in the many-pushes test; the bench sets more (`ZEN_GIT_BENCH_PUSHES`). */
const BENCH = Number(process.env.ZEN_GIT_BENCH_PUSHES ?? 0);
const PUSHES = BENCH || 300;

let t: GitWorld;
let pub: string;
let alice: WorkerContext;
/** alice's view of the database: she pushes and repacks in-process. */
let adb: Db;
let afs: UnlockedFs;
/** alice's device (testUser(2)), set once the wasm is up. */
let device: Uint8Array;

beforeAll(async () => {
  pub = mkdtempSync(join(tmpdir(), 'zen-git-pub-'));
  t = await GitWorld.start({
    users: USERS,
    grantsOf: { ci: READ_ONLY },
    writer: 'alice',
    reader: 'ci',
    server: {
      top: { unencrypted_dir: pub },
      limits: { crdt_horizon_secs: 3, crdt_max_skew_ms: 100, sweep_interval_secs: 1 },
    },
  });
  alice = await t.signIn('alice');
  adb = await alice.open('git', DB_OPTS);
  afs = alice.fs;
  device = testUser(2).device.fingerprint;
}, 180_000);

afterAll(async () => {
  await alice?.close();
  await t?.stop();
  if (pub) rmSync(pub, { recursive: true, force: true });
});

// ------------------------------------------------------------ helpers

const sh = (args: string[], cwd: string, input?: string | Buffer): string =>
  execFileSync('git', args, {
    cwd,
    encoding: 'utf8',
    maxBuffer: 1 << 28,
    stdio: ['pipe', 'pipe', 'pipe'],
    ...(input !== undefined ? { input } : {}),
    env: { ...process.env, GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_NOSYSTEM: '1' },
  });

/** A local repo the test pushes from in-process (store.push), as a hook would. */
class Local {
  readonly dir: string;
  tip: string | null = null;
  private n = 0;
  constructor(
    readonly repo: string,
    readonly format: 'sha1' | 'sha256' = 'sha1',
  ) {
    this.dir = mkdtempSync(join(t.dir, `local-${repo}-`));
    sh(['init', '-q', `--object-format=${format}`, '-b', 'main', '.'], this.dir);
    sh(['config', 'user.name', 'test'], this.dir);
    sh(['config', 'user.email', 'test@zen.test'], this.dir);
  }

  /** One commit (a new file, and a change to a shared one), pushed as one pack. */
  async commitAndPush(db: Db = adb, fs: UnlockedFs = afs): Promise<string> {
    this.n++;
    writeFileSync(join(this.dir, `f${this.n % 50}`), `${this.repo} ${this.n}\n`.repeat(40), {
      flag: 'a',
    });
    writeFileSync(join(this.dir, 'log'), `${this.n} ${'x'.repeat(this.n % 200)}\n`, { flag: 'a' });
    sh(['add', '-A'], this.dir);
    sh(['commit', '-q', '-m', `c${this.n}`], this.dir);
    const tip = sh(['rev-parse', 'HEAD'], this.dir).trim();
    const r = await this.pushTip(tip, db, fs);
    if (!r.ok) throw new Error(`push ${this.n}: ${r.why}`);
    return tip;
  }

  /** Push `tip` as main, its pack cut against the last pushed tip. */
  async pushTip(tip: string, db: Db = adb, fs: UnlockedFs = afs, against = this.tip) {
    const pack = this.cut(tip, against);
    const updates: RefUpdate[] = [{ ref: 'refs/heads/main', old: this.tip, new: tip }];
    const r = await push(db, fs, {
      repo: this.repo,
      format: this.format,
      updates,
      device,
      ...(pack ? { pack } : {}),
    });
    if (r.ok) this.tip = tip;
    return r;
  }

  /** `git pack-objects --revs` of `tip ^against`, never thin, indexed. */
  cut(tip: string, against: string | null) {
    const out = join(this.dir, '.git', 'cut');
    rmSync(out, { recursive: true, force: true });
    mkdirSync(out);
    const input = `${tip}\n${against ? `^${against}\n` : ''}`;
    const hash = sh(['pack-objects', '-q', '--revs', join(out, 'p')], this.dir, input).trim();
    const idxPath = join(out, `p-${hash}.idx`);
    const objects = sh(['show-index'], this.dir, readFileSync(idxPath))
      .split('\n')
      .filter(Boolean).length;
    if (!objects) return undefined;
    const packPath = join(out, `p-${hash}.pack`);
    return { packPath, idxPath, hash, size: readFileSync(packPath).length, objects };
  }
}

/** A repo with `n` pushes. */
async function seeded(repo: string, n: number): Promise<Local> {
  const l = new Local(repo);
  for (let i = 0; i < n; i++) await l.commitAndPush();
  return l;
}

/** Every `packs` row of a repo, live or replaced. */
function allPacks(db: Db, repo: string): Promise<PackRow[]> {
  return db.transaction(async (tx) =>
    (await tx.table<PackRow>('packs').query().where('repo', '=', repo).all()).filter(
      (p) => p.repo === repo,
    ),
  );
}

/** An fs that records the nodes whose content is read (pack downloads). */
function counting(fs: UnlockedFs, read: Uint8Array[]): UnlockedFs {
  return new Proxy(fs, {
    get(target, prop, recv) {
      if (prop !== 'tree') return Reflect.get(target, prop, recv);
      return (id: Uint8Array) => {
        const tree = target.tree(id);
        return new Proxy(tree, {
          get(tt, p, r) {
            if (p !== 'readStream') {
              const v = Reflect.get(tt, p, r);
              return typeof v === 'function' ? v.bind(tt) : v;
            }
            return (node: Uint8Array, ...rest: unknown[]) => {
              read.push(node);
              return (tt.readStream as (...a: unknown[]) => unknown)(node, ...rest);
            };
          },
        });
      };
    },
  });
}

/** A gc message's body (§9). */
const gcMsg = (repo: string, hashes: string[]) =>
  new Map<number, string | string[]>([
    [1, repo],
    [2, hashes],
  ]) as unknown as import('@zen/db').Value;

/** A fresh mirror directory. */
function freshMirror(): string {
  return join(mkdtempSync(join(t.dir, 'mirror-')), 'm.git');
}

/** Whether a pack's node can no longer be read (the sweeper purged it). */
async function gone(fs: UnlockedFs, tree: Uint8Array, node: Uint8Array): Promise<boolean> {
  try {
    // A purged node reads as no version: an empty stream.
    const r = fs.tree(tree).readStream(node).getReader();
    let n = 0;
    for (let c = await r.read(); !c.done; c = await r.read()) n += c.value.length;
    return n === 0;
  } catch {
    return true;
  }
}

// ------------------------------------------------------------ tests

describe('rollUp (§9)', () => {
  it('nothing at 16 packs or fewer; the newest two at least; the shortest geometric run', () => {
    expect(rollUp(new Array(16).fill(10))).toBeUndefined();
    // Equal sizes: no older pack is twice the rest, so everything rolls up.
    expect(rollUp(new Array(17).fill(10))).toBe(0);
    // A geometric history: only the newest two.
    // (Each pack is at least twice all the newer ones together: a ratio of 4 is.)
    const geo = Array.from({ length: 17 }, (_, i) => 4 ** (17 - i));
    expect(rollUp(geo)).toBe(15);
    // A large old pack and 16 small ones: the small ones, not the large one.
    expect(rollUp([1_000_000, ...new Array(16).fill(10)])).toBe(1);
    // Small ones grown past half the large one: the run reaches the oldest.
    expect(rollUp([100, ...new Array(16).fill(10)])).toBe(0);
    // A middle pack too small for what follows: the run starts there.
    expect(rollUp([1_000_000, 50_000, 10, 1_000, ...new Array(13).fill(1)])).toBe(2);
    expect(rollUp([10, 10], 1)).toBe(0);
    expect(rollUp([10], 1)).toBeUndefined();
  });
});

describe('a repack (§9)', () => {
  it('rolls packs up into one; a fresh clone works; a mirror that kept up downloads no roll-up', async () => {
    const l = await seeded('cover', 8);
    const m = freshMirror();
    const read: Uint8Array[] = [];
    const fs = counting(afs, read);
    await syncRepo(m, { db: adb, fs }, 'cover');
    expect(read).toHaveLength(8);

    const r1 = await repack(adb, afs, 'cover', { min: 1 });
    expect(r1).toMatchObject({ ok: true, rolled: 8, left: 1 });
    let st = await readState(adb, 'cover');
    expect(st.packs).toHaveLength(1);
    expect(st.packs[0]!.of).toHaveLength(8);
    expect(st.packs[0]!.seq).toBe(8);
    await syncRepo(m, { db: adb, fs }, 'cover');
    expect(read).toHaveLength(8);

    // Three more pushes, and a roll-up of the roll-up with them.
    for (let i = 0; i < 3; i++) await l.commitAndPush();
    await syncRepo(m, { db: adb, fs }, 'cover');
    expect(read).toHaveLength(11);
    const r2 = await repack(adb, afs, 'cover', { min: 1 });
    expect(r2.ok && r2.rolled).toBeGreaterThanOrEqual(2);
    st = await readState(adb, 'cover');
    await syncRepo(m, { db: adb, fs }, 'cover');
    expect(read, 'the roll-ups are covered').toHaveLength(11);
    expect(refsOf(m).get('refs/heads/main')).toBe(l.tip);
    sh(['--git-dir', m, 'fsck', '--strict', '--no-dangling'], t.dir);

    // The replaced rows stay until the gc, marked.
    const rows = await allPacks(adb, 'cover');
    const replaced = rows.filter((p) => p.state === 'replaced');
    expect(replaced.length).toBeGreaterThanOrEqual(9);
    for (const p of replaced) expect(p.by && p.replaced_ms).toBeTruthy();

    // A fresh clone through the helper, from the roll-ups.
    await t.g('bob', ['clone', '-q', t.url('cover'), 'cover']);
    expect(await t.g('bob', ['rev-parse', 'HEAD'], 'cover')).toBe(l.tip);
    await t.g('bob', ['fsck', '--strict', '--no-dangling'], 'cover');
    expect(st.packs.length).toBeLessThanOrEqual(2);
  });

  it('a stale listing syncs within the grace; after the gc it re-reads (PackGone)', async () => {
    const l = await seeded('stale', 5);
    const before = await readState(adb, 'stale');
    const r = await repack(adb, afs, 'stale', { min: 1, graceMs: 3_600_000 });
    expect(r.ok).toBe(true);
    // Within the grace: the old packs are still there.
    const m1 = freshMirror();
    initMirror(m1, 'sha1', before.repo!.tree);
    await syncMirror(m1, afs, before);
    expect(refsOf(m1).get('refs/heads/main')).toBe(l.tip);

    // The gc, as the job would run it at its message; then the sweeper purges.
    const hashes = before.packs.map((p) => p.hash);
    expect(await collect(adb, afs, gcMsg('stale', hashes))).toBe(5);
    expect(await collect(adb, afs, gcMsg('stale', hashes)), 'again: nothing').toBe(0);
    expect((await allPacks(adb, 'stale')).map((p) => p.state)).toEqual(['live']);
    const tree = before.repo!.tree;
    await until(
      async () => {
        for (const p of before.packs) if (!(await gone(afs, tree, p.nodes[0]!))) return false;
        return true;
      },
      60_000,
      'the purge of the replaced packs',
    );
    const m2 = freshMirror();
    initMirror(m2, 'sha1', tree);
    await expect(syncMirror(m2, afs, before)).rejects.toBeInstanceOf(PackGone);
    // syncRepo re-reads the state and syncs.
    const m3 = freshMirror();
    await syncRepo(m3, { db: adb, fs: afs }, 'stale', 'sha1', before);
    expect(refsOf(m3).get('refs/heads/main')).toBe(l.tip);
    sh(['--git-dir', m3, 'fsck', '--strict', '--no-dangling'], t.dir);
  }, 120_000);

  it('a push during a repack: both commit', async () => {
    const l = await seeded('during', 6);
    let pushed: Promise<unknown> | undefined;
    const r = await repack(adb, afs, 'during', {
      min: 1,
      afterUpload: async () => {
        pushed = l.commitAndPush();
        await pushed;
      },
    });
    expect(r).toMatchObject({ ok: true, rolled: 6 });
    await pushed;
    const st = await readState(adb, 'during');
    expect(st.packs).toHaveLength(2);
    expect(st.refs.get('refs/heads/main')).toBe(l.tip);
    await t.g('bob', ['clone', '-q', t.url('during'), 'during']);
    await t.g('bob', ['fsck', '--strict', '--no-dangling'], 'during');
    expect(await t.g('bob', ['rev-parse', 'HEAD'], 'during')).toBe(l.tip);
  });

  it('two repacks at once: one commits, the other is refused; nothing is lost', async () => {
    const l = await seeded('twice', 6);
    let waiting = 0;
    let open!: () => void;
    const both = new Promise<void>((r) => {
      open = r;
    });
    const barrier = async () => {
      if (++waiting === 2) open();
      await both;
    };
    // bob repacks too, through a database instance of his own.
    const bob = await t.signIn('bob');
    try {
      const bdb = await bob.open('git', DB_OPTS);
      const rs = await Promise.all([
        repack(adb, afs, 'twice', { min: 1, afterUpload: barrier }),
        repack(bdb, bob.fs, 'twice', { min: 1, afterUpload: barrier }),
      ]);
      expect(rs.filter((r) => r.ok)).toHaveLength(1);
      expect(rs.find((r) => !r.ok)).toEqual({ ok: false, why: 'the packs changed' });
    } finally {
      await bob.close();
    }
    const st = await readState(adb, 'twice');
    expect(st.packs).toHaveLength(1);
    await t.g('bob', ['clone', '-q', t.url('twice'), 'twice']);
    await t.g('bob', ['fsck', '--strict', '--no-dangling'], 'twice');
    expect(await t.g('bob', ['rev-parse', 'HEAD'], 'twice')).toBe(l.tip);
  });

  it('a pack pushed again after a repack adds no row; the refs move', async () => {
    const l = await seeded('again', 4);
    const first = (await readState(adb, 'again')).packs[1]!;
    await repack(adb, afs, 'again', { min: 1 });
    const rows = (await allPacks(adb, 'again')).length;
    // The same objects as the second push, as a new branch: the same pack.
    const second = sh(['rev-parse', 'HEAD~2'], l.dir).trim();
    const pack = l.cut(second, sh(['rev-parse', 'HEAD~3'], l.dir).trim());
    expect(pack?.hash).toBe(first.hash);
    const r = await push(adb, afs, {
      repo: 'again',
      format: 'sha1',
      updates: [{ ref: 'refs/heads/old', old: null, new: second }],
      device,
      pack: pack!,
    });
    expect(r.ok).toBe(true);
    expect((await allPacks(adb, 'again')).length).toBe(rows);
    expect((await readState(adb, 'again')).refs.get('refs/heads/old')).toBe(second);
    await t.g('bob', ['clone', '-q', t.url('again'), 'again']);
    await t.g('bob', ['fsck', '--strict', '--no-dangling'], 'again');
  });
});

describe('zen-git worker', () => {
  let worker: ReturnType<GitWorld['spawnZenGit']> | undefined;

  afterAll(async () => {
    if (worker) {
      worker.proc.kill('SIGTERM');
      await worker.exited;
      if (BENCH) {
        const rss = readFileSync(t.rssLog, 'utf8')
          .split('\n')
          .filter((x) => x.startsWith('zen-git.mjs worker'));
        console.log(`bench: the worker's peak RSS (KiB), at exit: ${rss.join(' | ')}`);
      }
    }
  });

  it('killed after its upload: nothing changes; the next worker repacks', async () => {
    await seeded('kill', REPACK_AT + 1);
    const before = await readState(adb, 'kill');
    expect(before.packs).toHaveLength(REPACK_AT + 1);
    const dead = t.spawnZenGit('alice', ['worker', t.fsUrl, '--grace', '3s'], {
      ZEN_GIT_TEST_DIE_AFTER_UPLOAD: '1',
    });
    expect(await dead.exited).toBe(128);
    const after = await readState(adb, 'kill');
    expect(after.packs.map((p) => p.hash)).toEqual(before.packs.map((p) => p.hash));
    expect((await allPacks(adb, 'kill')).length).toBe(REPACK_AT + 1);

    // The message, never acknowledged, goes to the next worker once the lease lapses.
    worker = t.spawnZenGit(
      'alice',
      ['worker', t.fsUrl, '--grace', '3s', '--publish', `pubrepo=${join(pub, 'git')}`],
      BENCH ? { ZEN_GIT_TRACE: '1' } : {},
    );
    await until(
      async () =>
        (await readState(adb, 'kill')).packs.length <= REPACK_AT &&
        worker!.lines.some((x) => /^kill: \d+ packs → 1/.test(x)),
      60_000,
      `the repack (${worker.lines.join(' | ')})`,
    );
  }, 120_000);

  it(`${PUSHES} pushes: the live packs settle; clones work; the gc removes the replaced packs`, async () => {
    const l = new Local('many');
    const m = freshMirror();
    const read: Uint8Array[] = [];
    const fs = counting(afs, read);
    // Bench: the repack group's committed cursor, polled.
    const cursors: { at: number; c: string }[] = [];
    let polling = !!BENCH;
    const poll = (async () => {
      if (!polling) return;
      const topic = adb.topic('git', 'repack');
      const c = await topic.group(topic.keysAt(topic.fs.keys.epoch).groupId('repack'), {
        mode: 'sequential',
      });
      while (polling) {
        try {
          cursors.push({
            at: Date.now(),
            c: Buffer.from((await c.cursor()).cursor).toString('hex'),
          });
        } catch (e) {
          cursors.push({ at: Date.now(), c: `error ${(e as Error).message}` });
        }
        await new Promise((r) => setTimeout(r, 2000));
      }
    })();
    const t0 = performance.now();
    let maxLive = 0;
    const readMs: number[] = [];
    for (let i = 0; i < PUSHES; i++) {
      await l.commitAndPush();
      if (i % 10 === 9) {
        const r0 = performance.now();
        const st = await readState(adb, 'many');
        readMs.push(performance.now() - r0);
        maxLive = Math.max(maxLive, st.packs.length);
      }
      if (i === 9) await syncRepo(m, { db: adb, fs }, 'many');
    }
    const pushS = (performance.now() - t0) / 1000;
    polling = false;
    await poll;
    if (BENCH) {
      let best = { n: 0, i: 0 };
      for (let i = 0, n = 1; i < cursors.length; i++, n++) {
        if (i > 0 && cursors[i]!.c !== cursors[i - 1]!.c) n = 1;
        if (n > best.n) best = { n, i };
      }
      const c = cursors[best.i];
      console.log(
        `bench: cursor polls ${cursors.length}; the longest unchanged ${best.n} polls (${best.n * 2} s), at ${c?.c} until ${c?.at}`,
      );
    }
    await until(
      async () => (await readState(adb, 'many')).packs.length <= REPACK_AT,
      120_000,
      `the live packs settle (${worker!.lines.slice(-5).join(' | ')})`,
    );
    const st = await readState(adb, 'many');
    // A clone made before the repacks fetches the rest.
    await syncRepo(m, { db: adb, fs }, 'many');
    expect(refsOf(m).get('refs/heads/main')).toBe(l.tip);
    sh(['--git-dir', m, 'fsck', '--strict', '--no-dangling'], t.dir);
    // A fresh clone through the helper.
    await t.g('bob', ['clone', '-q', '--bare', t.url('many'), 'many.git']);
    await t.g('bob', ['fsck', '--strict', '--no-dangling'], 'many.git');
    expect(await t.g('bob', ['rev-parse', 'main'], 'many.git')).toBe(l.tip);

    // After the grace (3 s), the gc: no replaced rows; only live packs in the tree.
    await until(
      async () => (await allPacks(adb, 'many')).every((p) => p.state === 'live'),
      60_000,
      'the gc',
    );
    const live = (await readState(adb, 'many')).packs;
    const tree = afs.tree(st.repo!.tree);
    const [objects] = (await tree.list(new Uint8Array(16))).filter(
      (n) => n.meta?.name === 'objects',
    );
    const [packDir] = (await tree.list(objects!.id)).filter((n) => n.meta?.name === 'pack');
    const names = (await tree.list(packDir!.id)).map((n) => n.meta?.name).sort();
    expect(names).toEqual(
      live.flatMap((p) => [`pack-${p.hash}.idx`, `pack-${p.hash}.pack`]).sort(),
    );

    const repacks = worker!.lines.filter((x) => x.startsWith('many: ') && x.includes('→'));
    expect(repacks.length).toBeGreaterThan(0);
    if (BENCH) {
      // The repack topic in offset order: where the gc events sit.
      const evs: { type: string; at: number; off: string }[] = [];
      try {
        for await (const e of adb.topic('git', 'repack').read()) {
          evs.push({
            type: decodeMsg(e.payload).type,
            at: Number(e.hlc >> 16n),
            off: Buffer.from(e.offset).toString('hex'),
          });
        }
      } catch (e) {
        console.log(`bench: reading the topic: ${(e as Error).message}`);
      }
      const back = evs.filter((x, i) => i > 0 && x.at + 2000 < evs[i - 1]!.at);
      console.log(
        [
          `bench: topic ${evs.length} events, ${evs.filter((x) => x.type === 'gc').length} gc; ${back.length} sent 2 s or more before the event ahead of them`,
          ...evs
            .flatMap((x, i) => (x.type === 'gc' ? evs.slice(Math.max(0, i - 2), i + 3) : []))
            .slice(0, 25)
            .map((x) => `  ${x.off} ${x.type} ${x.at}`),
        ].join('\n'),
      );
      // One `git repack -a -d` of the same history, for the bytes.
      const full = sh(['count-objects', '-v'], l.dir);
      sh(['repack', '-a', '-d', '-q'], l.dir);
      const one = Number(/size-pack: (\d+)/.exec(sh(['count-objects', '-v'], l.dir))![1]) * 1024;
      const bytes = live.reduce((a, p) => a + p.size, 0);
      const times = repacks.map((x) => Number(/, ([\d.]+) s;/.exec(x)?.[1] ?? 0));
      console.log(
        [
          `bench: ${PUSHES} pushes in ${pushS.toFixed(1)} s (${((pushS * 1000) / PUSHES).toFixed(0)} ms each)`,
          `${repacks.length} repacks, ${times.reduce((a, b) => a + b, 0).toFixed(1)} s in all, the longest ${Math.max(...times).toFixed(2)} s`,
          `live packs at most ${maxLive}, now ${live.length}; readState ${Math.min(...readMs).toFixed(0)}–${Math.max(...readMs).toFixed(0)} ms`,
          `live bytes ${bytes} against one git repack -a -d ${one} (${(bytes / one).toFixed(2)}×); before it: ${full.replace(/\n/g, ' ')}`,
          ...repacks.slice(-5),
          `other worker lines (${worker!.lines.length - repacks.length}):`,
          ...worker!.lines
            .filter((x) => !repacks.includes(x) && !x.includes('gc removed'))
            .slice(-15),
          (() => {
            const all = worker!.lines.filter((x) => x.startsWith('trace: '));
            const at = (x: string) => Number(x.split(' ')[1]);
            const tr = all.filter((x) => x.includes(' handled '));
            let gap = 0;
            let k = 0;
            for (let i = 1; i < tr.length; i++) {
              const d = at(tr[i]!) - at(tr[i - 1]!);
              if (d > gap) [gap, k] = [d, i];
            }
            const from = k ? at(tr[k - 1]!) : 0;
            const to = k ? at(tr[k]!) : 0;
            const inside = all.filter((x) => at(x) >= from && at(x) <= to);
            return [
              `trace: ${tr.length} handled, the largest gap ${gap} ms, with ${inside.length} lines:`,
              ...inside.slice(0, 60),
            ].join('\n');
          })(),
        ].join('\n'),
      );
    }
  }, 900_000);

  it('a public mirror: zen-git publish, a dumb-HTTP clone, and worker --publish after a push', async () => {
    const l = await seeded('pubrepo', 3);
    sh(['tag', '-a', '-m', 'v1', 'v1'], l.dir);
    const tagged = l.tip!;
    // The tag, as a push of known objects plus the tag object.
    const r = await push(adb, afs, {
      repo: 'pubrepo',
      format: 'sha1',
      updates: [{ ref: 'refs/tags/v1', old: null, new: sh(['rev-parse', 'v1'], l.dir).trim() }],
      device,
      ...(() => {
        const p = l.cut(sh(['rev-parse', 'v1'], l.dir).trim(), tagged);
        return p ? { pack: p } : {};
      })(),
    });
    expect(r.ok).toBe(true);
    // The worker's publish group may already have written a copy; publish now too.
    const out = await t.zenGit('bob', ['publish', t.url('pubrepo'), join(pub, 'git')]);
    expect(out.code, out.stderr).toBe(0);
    const http = `${t.w.url}/unencrypted/git/pubrepo.git`;
    await t.g('bob', ['clone', '-q', http, 'pub-clone']);
    expect(await t.g('bob', ['rev-parse', 'HEAD'], 'pub-clone')).toBe(tagged);
    expect(await t.g('bob', ['tag'], 'pub-clone')).toBe('v1');
    await t.g('bob', ['fsck', '--strict', '--no-dangling'], 'pub-clone');
    // The copy names no local path.
    expect(readFileSync(join(pub, 'git', 'pubrepo.git', 'config'), 'utf8')).not.toContain(t.dir);

    // A push: the worker publishes again, and a fetch from the copy gets it.
    const next = await l.commitAndPush();
    await until(
      async () => {
        const f = await t.run('bob', ['fetch', '-q', 'origin'], 'pub-clone');
        if (f.code !== 0) return false;
        return (await t.g('bob', ['rev-parse', 'origin/main'], 'pub-clone')) === next;
      },
      60_000,
      `the worker's publish (${worker?.lines.slice(-3).join(' | ')})`,
    );
  }, 120_000);
});

describe('setup (§10)', () => {
  it('grants the scheduler topic to writers, once', async () => {
    const ufs = await t.w.admin.fs(1).unlock({ passphrase: t.w.passphrase });
    try {
      const { setupGrants } = await import('../src/setup.js');
      const again = await setupGrants(
        t.w.admin,
        testUser(1).identity,
        1,
        ufs.topic('git').id,
        ufs.topic('zen', 'db', 'git', 'sched').id,
      );
      expect(again.added).toBe(0);
      const { doc } = await t.w.admin.acl.head();
      const sched = ufs.topic('zen', 'db', 'git', 'sched').id;
      const on = doc.grants.filter(
        (g) => g.topic && Buffer.from(g.topic).equals(Buffer.from(sched)),
      );
      // The admin, alice and bob write; ci only reads.
      expect(on).toHaveLength(3);
      const ciFp = Buffer.from(testUser(4).fp);
      expect(on.some((g) => ciFp.equals(Buffer.from(g.subject)))).toBe(false);
      for (const g of on) expect([...g.rights].sort()).toEqual(['consume', 'read']);
    } finally {
      ufs.close();
    }
  });
});
