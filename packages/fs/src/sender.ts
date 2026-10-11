// The replica's sender (spec/fs.md §11), run by the owner: queued entries
// in order, packed into commits within the limits. Each batch's sealed
// content is uploaded first (idempotent), then the commit, which is stored
// before its first send and sent again, as is, until its outcome is known.
import { bytes, type PendingOp, type Prepared, sendCommit, type Tree, ZenError } from '@zen/client';
import type { ChunkPut, Limits, PagePut } from '@zen/wasm';
import {
  decodeJson,
  encodeEntry,
  encodeJson,
  INFLIGHT,
  type Inflight,
  type QOp,
  type QueueEntry,
} from './queue.js';
import type { Records } from './store.js';

const OP_OVERHEAD = 512;
const BACKOFF_MIN = 250;
const BACKOFF_MAX = 30_000;

/** What the sender needs of its replica. */
export interface SenderHost {
  readonly tree: Tree;
  readonly rec: Records;
  /** The queue, in order. */
  entries(): QueueEntry[];
  /** Whether to keep sending (owner, not closed). */
  running(): boolean;
  /** Dots of entries already sent, by entry key. */
  readonly learned: Map<string, Uint8Array>;
  /** Entries committed: `dots` by entry key (for its write, if any). */
  done(entries: QueueEntry[], dots: Map<string, Uint8Array>): Promise<void>;
  /** Entries refused for good. */
  failed(entries: QueueEntry[], e: unknown): Promise<void>;
  /** The server can (or can't) be reached. */
  reachable(ok: boolean, e?: unknown): void;
  /** A batch (commit `id`) is about to be sent: it may apply before its reply comes. */
  sending(entries: QueueEntry[], id: string): void;
}

const isNet = (e: unknown) =>
  e instanceof ZenError && (e.code === 'network' || e.code === 'commit_unknown');

/** An entry's op as sent: `after` resolved to the dots learned, timestamps kept. */
function wireOp(o: QOp, learned: Map<string, Uint8Array>): PendingOp {
  if (o.op === 'write') {
    const replaces = [...o.replaces];
    for (const a of o.after ?? []) {
      const d = learned.get(a);
      if (d && !replaces.some((x) => bytes.equal(x, d))) replaces.push(d);
    }
    return {
      op: 'write',
      node: o.node,
      replaces,
      chunks: o.chunks,
      ...(o.pages?.length ? { pages: o.pages } : {}),
      manifest: o.manifest,
    };
  }
  if (o.op === 'meta') {
    return {
      op: 'meta',
      node: o.node,
      meta: o.meta,
      ...(o.hlc !== undefined ? { hlc: o.hlc } : {}),
    };
  }
  return {
    op: 'move',
    node: o.node,
    parent: o.parent,
    ...(o.meta ? { meta: o.meta } : {}),
    ...(o.hlc !== undefined ? { hlc: o.hlc } : {}),
  };
}

function cost(o: QOp): number {
  let b = OP_OVERHEAD + 64;
  if (o.op === 'write') {
    b += o.manifest.length + 16 * (o.chunks.length + (o.pages?.length ?? 0));
    b += 12 * (o.replaces.length + (o.after?.length ?? 0));
  } else if (o.meta) b += o.meta.length;
  return b;
}

export class Sender {
  /** A batch is being sent. */
  sending = false;
  private loop: Promise<void> | undefined;
  private wakeUp: (() => void) | undefined;
  /** Commits (hex id) whose content was uploaded by this process. */
  private readonly uploaded = new Set<string>();
  private rebases = 0;
  private cap = Number.POSITIVE_INFINITY;

  constructor(private readonly host: SenderHost) {}

  /** Start the loop (the owner). */
  start(): void {
    if (this.loop || !this.host.running()) return;
    this.loop = this.run().finally(() => {
      this.loop = undefined;
    });
  }

  /** Something to send (or the server may be back). */
  wake(): void {
    if (this.wakeUp) this.wakeUp();
    else this.start();
  }

  /** Resolves once the loop has stopped (after `close`). */
  async stopped(): Promise<void> {
    this.wakeUp?.();
    await this.loop;
  }

  private sleep(ms?: number): Promise<void> {
    return new Promise((r) => {
      const t = ms === undefined ? undefined : setTimeout(done, ms);
      function done() {
        if (t) clearTimeout(t);
        r();
      }
      this.wakeUp = () => {
        this.wakeUp = undefined;
        done();
      };
    });
  }

  private async run(): Promise<void> {
    let backoff = BACKOFF_MIN;
    while (this.host.running()) {
      if (!this.host.entries().length) {
        await this.sleep();
        continue;
      }
      this.sending = true;
      try {
        await this.once();
        backoff = BACKOFF_MIN;
        this.host.reachable(true);
      } catch (e) {
        if (!isNet(e)) console.warn(`zen-fs: sending failed: ${(e as Error).message}`);
        this.host.reachable(false, e);
        this.sending = false;
        await this.sleep(backoff);
        backoff = Math.min(backoff * 2, BACKOFF_MAX);
      } finally {
        this.sending = false;
      }
    }
  }

  /** Entries from the front of the queue that fit one commit. */
  private pack(limits: Limits): QueueEntry[] {
    const out: QueueEntry[] = [];
    let ops = 0;
    let size = 0;
    const written = new Set<string>();
    for (const e of this.host.entries()) {
      if (out.length >= this.cap) break;
      const n = e.ops.length;
      const b = e.ops.reduce((s, o) => s + cost(o), 0);
      const w = e.ops.filter((o) => o.op === 'write').map((o) => bytes.hex(o.node));
      if (
        out.length &&
        (ops + n > limits.max_commit_ops ||
          size + b > limits.max_commit_bytes ||
          w.some((x) => written.has(x)))
      ) {
        break;
      }
      out.push(e);
      ops += n;
      size += b;
      for (const x of w) written.add(x);
    }
    return out;
  }

  private async loadInflight(): Promise<Inflight | undefined> {
    const raw = await this.host.rec.get('queue', INFLIGHT);
    if (!raw) return undefined;
    try {
      return decodeJson<Inflight>(raw);
    } catch {
      return undefined;
    }
  }

  private async dropInflight(): Promise<void> {
    this.host.rec.delete('queue', INFLIGHT);
    await this.host.rec.flush();
  }

  /** Send the queued content of `entries` (chunks, then the pages listing them). */
  private async upload(entries: QueueEntry[], limits: Limits): Promise<void> {
    const { tree, rec } = this.host;
    const fs = tree.fs.id;
    const puts: { kind: 'chunks' | 'pages'; id: Uint8Array }[] = [];
    for (const e of entries) for (const id of e.chunks) puts.push({ kind: 'chunks', id });
    for (const e of entries) for (const id of e.pages) puts.push({ kind: 'pages', id });
    let chunks: ChunkPut[] = [];
    let pages: PagePut[] = [];
    let size = 0;
    const send = async () => {
      if (!chunks.length && !pages.length) return;
      await sendCommit(tree.fs.session, {
        commit_id: bytes.randomBytes(16),
        ...(chunks.length ? { chunks } : {}),
        ...(pages.length ? { pages } : {}),
      });
      chunks = [];
      pages = [];
      size = 0;
    };
    let inPages = false;
    for (const p of puts) {
      const data = await rec.get(p.kind, p.id);
      if (!data) {
        throw new ZenError(0, 'not_found', `queued content ${bytes.hex(p.id)} is missing`);
      }
      // Pages go in commits after every chunk they may list.
      if (p.kind === 'pages' && !inPages) {
        inPages = true;
        await send();
      }
      const c = data.length + 16 + OP_OVERHEAD + (p.kind === 'pages' ? 48 * (data.length / 16) : 0);
      if (
        size + c > limits.max_commit_bytes ||
        chunks.length + pages.length + 1 > limits.max_commit_ops
      ) {
        await send();
      }
      size += c;
      if (p.kind === 'chunks') chunks.push({ fs, id: p.id, data });
      else pages.push({ fs, id: p.id, data });
    }
    await send();
  }

  /** Send one batch: the stored commit in flight, or a new one. */
  private async once(): Promise<void> {
    const { tree, rec, learned } = this.host;
    const limits = await tree.limits();
    const queue = this.host.entries();
    let inf = await this.loadInflight();
    let entries: QueueEntry[];
    let prepared: Prepared | undefined;
    if (inf) {
      const found = inf.keys.map((k) => queue.find((e) => e.key === k));
      if (found.some((e) => !e)) {
        await this.dropInflight();
        return;
      }
      entries = found as QueueEntry[];
    } else {
      entries = this.pack(limits);
      prepared = await tree.prepare(entries.flatMap((e) => e.ops.map((o) => wireOp(o, learned))));
      inf = {
        keys: entries.map((e) => e.key),
        commit: { commit_id: bytes.randomBytes(16), crdt_ops: prepared.ops },
      };
      rec.put('queue', INFLIGHT, encodeJson(inf));
      await rec.flush();
    }
    const id = bytes.hex(inf.commit.commit_id);
    this.host.sending(entries, id);
    let r: Awaited<ReturnType<typeof sendCommit>>;
    try {
      if (!this.uploaded.has(id)) {
        await this.upload(entries, limits);
        this.uploaded.add(id);
      }
      r = await sendCommit(tree.fs.session, inf.commit, 1);
    } catch (e) {
      if (isNet(e) || !(e instanceof ZenError)) throw e;
      this.uploaded.delete(id);
      if (e.code === 'stale_op' || e.code === 'clock_skew') {
        const ops = inf.commit.crdt_ops ?? [];
        const p: Prepared = prepared ?? {
          ops,
          hlcs: ops.flatMap((o) => (o.op === 'move' || o.op === 'meta' ? [o.hlc] : [])),
          pending: entries.flatMap((x) => x.ops.map((o) => wireOp(o, learned))),
        };
        if (await tree.rebase(p, e, ++this.rebases)) {
          // Ticked afresh at the next build.
          for (const x of entries) {
            for (const o of x.ops) if (o.op !== 'write') delete o.hlc;
            rec.put('queue', bytes.fromHex(x.key), encodeEntry(x));
          }
          await this.dropInflight();
          return;
        }
      }
      if (e.code === 'too_large' && entries.length > 1) {
        this.cap = Math.max(1, Math.floor(entries.length / 2));
        await this.dropInflight();
        return;
      }
      this.rebases = 0;
      this.cap = Number.POSITIVE_INFINITY;
      await this.host.failed(entries, e);
      return;
    }
    this.uploaded.delete(id);
    this.rebases = 0;
    this.cap = Number.POSITIVE_INFINITY;
    for (const o of inf.commit.crdt_ops ?? []) {
      if (o.op === 'move' || o.op === 'meta') tree.clock.accept(o.hlc);
    }
    const dots = new Map<string, Uint8Array>();
    let i = 0;
    for (const e of entries) {
      for (const o of e.ops) {
        if (o.op !== 'write') continue;
        const d = r.dots?.[i++];
        if (d) dots.set(e.key, d);
      }
    }
    await this.host.done(entries, dots);
  }
}
