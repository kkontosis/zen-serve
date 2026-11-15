// The zen side of a repo (spec/git.md §3–§5): its state at one read version,
// a pack's download, and a push as one transaction.
import { createReadStream, createWriteStream } from 'node:fs';
import { Readable } from 'node:stream';
import { pipeline } from 'node:stream/promises';
import { bytes, type PendingOp, ROOT, Tree, type UnlockedFs, type Upload } from '@zen/client';
import type { Db, DbTransaction, Fields, Value } from '@zen/db';

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
  state: 'live' | 'replaced';
  replaced_ms?: number;
  by?: string;
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
      for (const p of await tx.table<PackRow>('packs').query().where('repo', '=', repo).all()) {
        if (p.state === 'live') packs.push(p);
      }
      packs.sort((a, b) => a.seq - b.seq);
    }
    return { ...(r ? { repo: r } : {}), refs, packs };
  });
}

/** Stream a pack's file (its first node) to `path`. */
export async function downloadPack(
  fs: UnlockedFs,
  repo: RepoRow,
  pack: PackRow,
  path: string,
): Promise<void> {
  const tree = fs.tree(repo.tree);
  const src = Readable.fromWeb(tree.readStream(pack.nodes[0]!) as never);
  await pipeline(src, createWriteStream(path));
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
    const have = await packs.get([a.repo, a.pack.hash]);
    if (have?.state !== 'live') {
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
