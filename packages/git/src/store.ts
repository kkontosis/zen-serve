// The zen side of a repo (spec/git.md §3–§5): its state at one read version,
// a pack's download, and a push as one transaction.
import {
  createReadStream,
  createWriteStream,
  mkdtempSync,
  readFileSync,
  rmSync,
  statSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { Readable } from 'node:stream';
import { pipeline } from 'node:stream/promises';
import {
  bytes,
  isCode,
  type PendingOp,
  ROOT,
  Tree,
  type UnlockedFs,
  type Upload,
} from '@zen/client';
import type { Db, DbTransaction, Fields, Value } from '@zen/db';
import { git as run } from './gitcmd.js';

/** A `repos` row. */
export interface RepoRow extends Fields {
  name: string;
  format: 'sha1' | 'sha256';
  head: string;
  tree: Uint8Array;
  next_seq: number;
  created_ms: number;
  creator: Uint8Array;
}

/** A `refs` row. */
export interface RefRow extends Fields {
  repo: string;
  ref: string;
  oid: Uint8Array;
  device: Uint8Array;
  time_ms: number;
  seq: number;
}

/** A `packs` row. */
export interface PackRow extends Fields {
  repo: string;
  hash: string;
  seq: number;
  size: number;
  objects: number;
  nodes: Uint8Array[];
  /** `pending`: a repack's pack whose rows' commit hasn't landed (§9). */
  state: 'live' | 'replaced' | 'pending';
  replaced_ms?: number;
  by?: string;
  /** A repack's pack: the hashes it rolled up (§9). */
  of?: string[];
}

/** A repo as one read transaction saw it (§5.1). */
export interface RepoState {
  repo?: RepoRow;
  /** Ref name → oid in hex. */
  refs: Map<string, string>;
  /** The live packs, by `seq`. */
  packs: PackRow[];
}

/** Read a repo's row, refs and live packs in one transaction (§5.1). */
export function readState(db: Db, repo: string): Promise<RepoState> {
  return db.transaction(async (tx) => {
    const r = (await tx.table<RepoRow>('repos').get(repo)) ?? undefined;
    const refs = new Map<string, string>();
    const packs: PackRow[] = [];
    if (r) {
      for (const row of await tx.table<RefRow>('refs').query().where('repo', '=', repo).all()) {
        refs.set(row.ref, bytes.hex(row.oid));
      }
      packs.push(...(await livePacks(tx, repo)));
    }
    return { ...(r ? { repo: r } : {}), refs, packs };
  });
}

/** A repo's live packs in `seq` order: one range read of the index `live` (§3). */
function livePacks(tx: DbTransaction, repo: string, limit?: number): Promise<PackRow[]> {
  let q = tx
    .table<PackRow>('packs')
    .query()
    .where('repo', '=', repo)
    .where('state', '=', 'live')
    .orderBy('seq');
  if (limit !== undefined) q = q.limit(limit);
  return q.all();
}

/** A pack's file has gone (a repack's gc, after a stale listing): read the state again (§5). */
export class PackGone extends Error {
  constructor(readonly hash: string) {
    super(`pack ${hash} has gone (repacked)`);
    this.name = 'PackGone';
  }
}

/** Stream a pack's file (its first node) to `path`. A node gone throws `PackGone`. */
export async function downloadPack(
  fs: UnlockedFs,
  repo: RepoRow,
  pack: PackRow,
  path: string,
): Promise<void> {
  const tree = fs.tree(repo.tree);
  try {
    const src = Readable.fromWeb(tree.readStream(pack.nodes[0]!) as never);
    await pipeline(src, createWriteStream(path));
  } catch (e) {
    if (isCode(e, 'not_found') || isCode(e, 'stale_op')) throw new PackGone(pack.hash);
    throw e;
  }
  // A purged node reads as no version: an empty stream.
  const got = statSync(path).size;
  if (got === 0 && pack.size > 0) throw new PackGone(pack.hash);
  if (got !== pack.size) {
    throw new Error(`pack ${pack.hash}: ${got} bytes downloaded, its row says ${pack.size}`);
  }
}

/** One ref update: oids in hex, null for absent. */
export interface RefUpdate {
  ref: string;
  old: string | null;
  new: string | null;
}

/** A pack to push: its two files and git's hash. */
export interface PushPack {
  packPath: string;
  idxPath: string;
  hash: string;
  size: number;
  objects: number;
}

export interface PushArgs {
  repo: string;
  format: 'sha1' | 'sha256';
  updates: RefUpdate[];
  pack?: PushPack;
  device: Uint8Array;
  /** Test only: called after the upload, before the transaction. */
  afterUpload?: () => void | Promise<void>;
}

export type PushResult = { ok: true; seq: number } | { ok: false; why: string };

/** The `pushes` event's type. */
export const PUSH_EVENT = 'push';

/** The pushes topic of a repo (§4 step 3.5). */
export function pushesTopic(db: Db, repo: string) {
  return db.topic('git', repo, 'pushes');
}

/** The fs's repack topic (§9): `repack` and `gc` messages. */
export function repackTopic(db: Db) {
  return db.topic('git', 'repack');
}

/** A repack request's type (§4 step 3.5), and a gc's (§9). */
export const REPACK_EVENT = 'repack';
export const GC_EVENT = 'gc';

/** Live packs past which a repo is repacked (§9). */
export const REPACK_AT = 16;

/**
 * A push (§4 steps 2–4): the pack's files uploaded under an upload session,
 * then one transaction with the refs' compare-and-set, the files, the rows
 * and the event. A conflict re-runs it (zen-db's retry): it goes on while
 * the refs still have their old values, and is refused otherwise.
 */
export async function push(db: Db, fs: UnlockedFs, a: PushArgs): Promise<PushResult> {
  for (let attempt = 0; attempt < 3; attempt++) {
    const pre = await db.table<RepoRow>('repos').get(a.repo);
    const tree = fs.tree(pre?.tree ?? Tree.newId());
    let ops: PendingOp[] = [];
    let nodes: Uint8Array[] = [];
    const ups: Upload[] = [];
    try {
      if (a.pack) {
        ({ ops, nodes } = await packNodes(tree, !pre, a.pack.hash));
        const session = db.features.includes('upload_sessions');
        for (const [i, path] of [a.pack.packPath, a.pack.idxPath].entries()) {
          ups.push(
            await tree.upload(nodes[i]!, createReadStream(path), session ? { session: true } : {}),
          );
        }
        await a.afterUpload?.();
      }
      const r = await db.transaction(async (tx) => apply(tx, db, a, tree, pre, ops, nodes, ups));
      if (r !== 'again') return r;
    } finally {
      for (const u of ups) if (u.session) await tree.closeUpload(u.session).catch(() => {});
    }
  }
  return { ok: false, why: 'the repo changed under the push; try again' };
}

/** The nodes of a pack's two files, and the ops that create them (and the directories). */
async function packNodes(
  tree: Tree,
  fresh: boolean,
  hash: string,
): Promise<{ ops: PendingOp[]; nodes: Uint8Array[] }> {
  const b = tree.batch();
  let dir: Uint8Array;
  if (fresh) {
    dir = b.mkdir(b.mkdir(ROOT, 'objects'), 'pack');
  } else {
    const objects = await child(tree, ROOT, 'objects');
    const pack = objects && (await child(tree, objects, 'pack'));
    dir = pack ?? b.mkdir(objects ?? b.mkdir(ROOT, 'objects'), 'pack');
  }
  const nodes = [b.create(dir, `pack-${hash}.pack`), b.create(dir, `pack-${hash}.idx`)];
  return { ops: [...b.ops], nodes };
}

async function child(
  tree: Tree,
  parent: Uint8Array,
  name: string,
): Promise<Uint8Array | undefined> {
  for (const n of await tree.list(parent)) if (n.meta?.name === name) return n.id;
  return undefined;
}

const hexOrNull = (o: string | null): Uint8Array | null => (o === null ? null : bytes.fromHex(o));

async function apply(
  tx: DbTransaction,
  db: Db,
  a: PushArgs,
  tree: Tree,
  pre: RepoRow | undefined,
  ops: PendingOp[],
  nodes: Uint8Array[],
  ups: Upload[],
): Promise<PushResult | 'again'> {
  const repos = tx.table<RepoRow>('repos');
  const row = await repos.get(a.repo);
  // The files went up into another tree than the repo's (a first push raced): again.
  if (row ? !bytes.equal(row.tree, tree.id) : !!pre) return 'again';
  if (row && row.format !== a.format) {
    return { ok: false, why: `the repo is ${row.format}, the push ${a.format}` };
  }
  // 3.2: every ref's current oid must be the push's old value.
  const refs = tx.table<RefRow>('refs');
  for (const u of a.updates) {
    const cur = await refs.get([a.repo, u.ref]);
    if ((cur ? bytes.hex(cur.oid) : null) !== u.old) return { ok: false, why: 'fetch first' };
  }
  const now = Date.now();
  const seq = row?.next_seq ?? 1;
  // 3.3: the pack's files and row (none if the repo already holds that pack).
  if (a.pack) {
    const packs = tx.table<PackRow>('packs');
    // A known pack (live, or replaced: its objects are in a live one) is skipped.
    const have = await packs.get([a.repo, a.pack.hash]);
    if (have?.state === 'pending') {
      // A repack's files of the same objects, not yet live: they become this push's.
      await packs.put({ ...have, seq, state: 'live' });
    } else if (!have) {
      const live = row ? (await livePacks(tx, a.repo, REPACK_AT + 1)).length : 0;
      const files = tx.fs(tree);
      await files.ops(ops);
      for (const u of ups) await files.write(u, { replaces: [] });
      await packs.put({
        repo: a.repo,
        hash: a.pack.hash,
        seq,
        size: a.pack.size,
        objects: a.pack.objects,
        nodes,
        state: 'live',
      });
      // 3.5: past 16 live packs, a repack request.
      if (live + 1 > REPACK_AT) {
        tx.emit(repackTopic(db), REPACK_EVENT, new Map<Value, Value>([[1, a.repo]]));
      }
    }
  }
  const firstHead = a.updates.find((u) => u.new && u.ref.startsWith('refs/heads/'))?.ref;
  await repos.put({
    name: a.repo,
    format: a.format,
    head: row?.head ?? firstHead ?? 'refs/heads/main',
    tree: tree.id,
    next_seq: seq + 1,
    created_ms: row?.created_ms ?? now,
    creator: row?.creator ?? a.device,
  });
  // 3.4: the refs.
  for (const u of a.updates) {
    if (u.new === null) await refs.delete([a.repo, u.ref]);
    else {
      await refs.put({
        repo: a.repo,
        ref: u.ref,
        oid: bytes.fromHex(u.new),
        device: a.device,
        time_ms: now,
        seq,
      });
    }
  }
  // 3.5: the event.
  const body: Value = new Map<Value, Value>([
    [1, a.repo],
    [2, a.device],
    [
      3,
      a.updates.map(
        (u) =>
          new Map<Value, Value>([
            [1, u.ref],
            [2, hexOrNull(u.old)],
            [3, hexOrNull(u.new)],
          ]),
      ),
    ],
    [4, a.pack?.hash ?? null],
    [5, seq],
  ]);
  tx.emit(pushesTopic(db, a.repo), PUSH_EVENT, body);
  return { ok: true, seq };
}

/** Set HEAD (`repos.head`): `zen-git head`. */
export async function setHead(db: Db, repo: string, ref: string): Promise<boolean> {
  return db.transaction(async (tx) => {
    const t = tx.table<RepoRow>('repos');
    const row = await t.get(repo);
    if (!row) return false;
    await t.put({ ...row, head: ref });
    return true;
  });
}

// ------------------------------------------------------------ repack and gc

/**
 * The run a geometric roll-up takes (§9), as the index of its first pack in
 * `sizes` (the live packs' sizes in `seq` order): at least the newest two,
 * and the shortest run that leaves every older pack at least twice the bytes
 * of all the packs after it. Undefined with `min` packs or fewer.
 */
export function rollUp(sizes: number[], min = REPACK_AT): number | undefined {
  const n = sizes.length;
  if (n <= Math.max(1, min)) return undefined;
  let after = 0;
  const bad: boolean[] = new Array(n).fill(false);
  for (let j = n - 1; j >= 0; j--) {
    bad[j] = sizes[j]! < 2 * after;
    after += sizes[j]!;
  }
  let k = n - 2;
  for (let j = 0; j < n - 2; j++) {
    if (bad[j]) {
      k = j;
      break;
    }
  }
  return k;
}

export interface RepackOptions {
  /** The gc's grace (default 24 h). */
  graceMs?: number;
  /** Repack past this many live packs (default 16; `zen-git repack` uses 1). */
  min?: number;
  /** Where the temporary repository goes (default the OS's). */
  tmp?: string;
  /** Test only: called after the upload, before the transaction. */
  afterUpload?: () => void | Promise<void>;
}

export type RepackResult =
  | { ok: true; rolled: number; hash?: string; size?: number; left: number }
  | { ok: false; why: string };

export const GRACE_MS = 24 * 3_600_000;

/**
 * Repack a repo (§9): the geometric roll-up of its newest live packs, built
 * in a temporary repository from those packs alone, uploaded, and swapped in
 * by one transaction that schedules the gc of the packs it replaced.
 */
export async function repack(
  db: Db,
  fs: UnlockedFs,
  repo: string,
  o: RepackOptions = {},
): Promise<RepackResult> {
  const state = await readState(db, repo);
  const r = state.repo;
  if (!r) return { ok: false, why: `no such repo ${repo}` };
  const start = rollUp(
    state.packs.map((p) => p.size),
    o.min ?? REPACK_AT,
  );
  if (start === undefined) return { ok: true, rolled: 0, left: state.packs.length };
  const rolled = state.packs.slice(start);
  const tmp = mkdtempSync(join(o.tmp ?? tmpdir(), 'zen-git-repack-'));
  const tree = fs.tree(r.tree);
  const ups: Upload[] = [];
  try {
    // 1. The rolled-up packs, indexed without --strict (their bases may be older).
    const g = join(tmp, 'r.git');
    run(['init', '-q', '--bare', `--object-format=${r.format}`, g]);
    const dir = join(g, 'objects', 'pack');
    for (const p of rolled) {
      const path = join(dir, `pack-${p.hash}.pack`);
      await downloadPack(fs, r, p, path);
      const got = run(['--git-dir', g, 'index-pack', path]).trim().split(/\s+/).pop();
      if (got !== p.hash) throw new Error(`pack ${p.hash}: its contents are pack ${got}`);
    }
    // 2. Every object, packed again (never thin).
    const objects = run([
      '--git-dir',
      g,
      'cat-file',
      '--batch-all-objects',
      '--batch-check=%(objectname)',
    ]);
    const count = objects.split('\n').filter(Boolean).length;
    const out = join(tmp, 'new');
    const hash = run(['--git-dir', g, 'pack-objects', '-q', out], { input: objects }).trim();
    const packPath = `${out}-${hash}.pack`;
    const idxPath = `${out}-${hash}.idx`;
    // 3. Its object count is the union's.
    const n = run(['--git-dir', g, 'show-index'], { input: readFileSync(idxPath) })
      .split('\n')
      .filter(Boolean).length;
    if (n !== count) throw new Error(`the roll-up of ${repo} has ${n} objects, not ${count}`);
    const size = readFileSync(packPath).length;
    // 4. The upload.
    const { ops, nodes } = await packNodes(tree, false, hash);
    const session = db.features.includes('upload_sessions');
    for (const [i, path] of [packPath, idxPath].entries()) {
      ups.push(
        await tree.upload(nodes[i]!, createReadStream(path), session ? { session: true } : {}),
      );
    }
    await o.afterUpload?.();
    const grace = o.graceMs ?? GRACE_MS;
    const gcOf = (hashes: string[]) =>
      new Map<Value, Value>([
        [1, repo],
        [2, hashes],
      ]);
    // 5. The files' commit: the new pack's files and a `pending` row, whose
    // gc is scheduled now, so a job that stops before the rows' commit
    // leaves nothing for good. Tree operations serialize at the tree's
    // header (fs.md §7), so this commit is kept short and retried alone.
    const filed = await filesCommit(db, async (tx) => {
      const packs = tx.table<PackRow>('packs');
      const have = await packs.get([repo, hash]);
      if (have) return have.state !== 'pending';
      const files = tx.fs(tree);
      await files.ops(ops);
      for (const u of ups) await files.write(u, { replaces: [] });
      await packs.put({
        repo,
        hash,
        seq: rolled[rolled.length - 1]!.seq,
        size,
        objects: n,
        nodes,
        state: 'pending',
        of: rolled.map((x) => x.hash),
      });
      await tx.schedule(repackTopic(db), GC_EVENT, gcOf([hash]), { delayMs: grace });
      return true;
    });
    if (!filed) return { ok: false, why: 'the packs changed' };
    // 6. The rows' commit, with no tree operations: pushes don't conflict with it.
    return await db.transaction(async (tx): Promise<RepackResult> => {
      const packs = tx.table<PackRow>('packs');
      const rows: PackRow[] = [];
      for (const p of rolled) {
        const cur = await packs.get([repo, p.hash]);
        if (cur?.state !== 'live') return { ok: false, why: 'the packs changed' };
        rows.push(cur);
      }
      const now = Date.now();
      for (const row of rows) {
        await packs.put({ ...row, state: 'replaced', replaced_ms: now, by: hash });
      }
      const mine = await packs.get([repo, hash]);
      if (mine?.state === 'pending') await packs.put({ ...mine, state: 'live' });
      await tx.schedule(repackTopic(db), GC_EVENT, gcOf(rows.map((x) => x.hash)), {
        delayMs: grace,
      });
      return {
        ok: true,
        rolled: rows.length,
        hash,
        size,
        left: state.packs.length - rows.length + 1,
      };
    });
  } finally {
    for (const u of ups) if (u.session) await tree.closeUpload(u.session).catch(() => {});
    rmSync(tmp, { recursive: true, force: true });
  }
}

/**
 * Run a short commit with tree operations, retrying a conflict apart from
 * the slow work before it: the tree's header serializes every commit that
 * touches the tree, pushes included (fs.md §7).
 */
async function filesCommit<T>(db: Db, fn: (tx: DbTransaction) => Promise<T>): Promise<T> {
  for (let attempt = 0, wait = 20; ; attempt++, wait = Math.min(wait * 2, 1000)) {
    try {
      return await db.transaction(fn);
    } catch (e) {
      if (!isCode(e, 'conflict') || attempt >= 30) throw e;
      await new Promise((r) => setTimeout(r, wait * (0.5 + Math.random())));
    }
  }
}

/** A gc message's body (§9): the repo and the packs to collect. */
export function gcBody(body: Value): { repo: string; hashes: string[] } {
  const m = body as Map<Value, Value>;
  return { repo: m.get(1) as string, hashes: (m.get(2) as string[]) ?? [] };
}

/**
 * The gc (§9): the listed packs still `replaced` (or still `pending`: a
 * repack that stopped between its two commits) have their files moved to
 * `TRASH` and their rows deleted, in one transaction. Delivered again, it
 * finds nothing left to do.
 */
export async function collect(db: Db, fs: UnlockedFs, body: Value): Promise<number> {
  const { repo, hashes } = gcBody(body);
  const r = await db.table<RepoRow>('repos').get(repo);
  if (!r) return 0;
  const tree = fs.tree(r.tree);
  return db.transaction(async (tx) => {
    const packs = tx.table<PackRow>('packs');
    const b = tree.batch();
    let n = 0;
    for (const h of hashes) {
      const row = await packs.get([repo, h]);
      if (row?.state !== 'replaced' && row?.state !== 'pending') continue;
      for (const node of row.nodes) b.remove(node);
      await packs.delete([repo, h]);
      n++;
    }
    if (n) await tx.fs(tree).ops([...b.ops]);
    return n;
  });
}
