// The replica's records on the sealed local store (spec/zendb.md §9.3,
// fs.md §11), in stores `zfs/<fs>/<hex tree>/<kind>`. Records are written
// behind (every 100 ms and at close); reads see pending writes first. The
// bytes waiting to be written are bounded: past the bound, `room()` makes
// the producers wait for the store (zen-mount's memory, milestone 8 step 6b).
import { bytes, type ChunkCache, type SealedStore } from '@zen/client';

export type Kind =
  | 'nodes'
  | 'sync'
  | 'versions'
  | 'chunks'
  | 'pages'
  | 'lru'
  | 'pins'
  | 'queue'
  | 'index';
export const KINDS: Kind[] = [
  'nodes',
  'sync',
  'versions',
  'chunks',
  'pages',
  'lru',
  'pins',
  'queue',
  'index',
];

const FLUSH_MS = 100;
/** Default bound on the bytes of records waiting to be written. */
export const DEFAULT_PENDING = 32 << 20;

type Pending = { kind: Kind; key: Uint8Array; value?: Uint8Array };
const sizeOf = (p: Pending | undefined) => p?.value?.length ?? 0;

/** The store names of a replica of tree `tree` of fs `fs`. */
export function replicaStores(fs: number, tree: Uint8Array): string[] {
  return KINDS.map((k) => `zfs/${fs}/${bytes.hex(tree)}/${k}`);
}

/** Write-behind access to one replica's stores. */
export class Records {
  private readonly pending = new Map<string, Pending>();
  /**
   * Records taken by a flush whose store write hasn't landed yet: still
   * read from here, so a record never reads as absent while it is written.
   */
  private readonly writing = new Map<string, Pending>();
  private timer: ReturnType<typeof setTimeout> | undefined;
  private flushing: Promise<void> = Promise.resolve();
  /** Bytes of values pending or in a batch not yet written. */
  private backlog = 0;
  private waiters: (() => void)[] = [];

  constructor(
    readonly sealed: SealedStore,
    readonly base: string,
    /** The most bytes waiting to be written before `room()` waits. */
    readonly maxPending: number = DEFAULT_PENDING,
  ) {}

  name(kind: Kind): string {
    return `${this.base}/${kind}`;
  }

  /** Bytes of records waiting to be written (pending, or being written). */
  get pendingBytes(): number {
    return this.backlog;
  }

  async get(kind: Kind, key: Uint8Array): Promise<Uint8Array | undefined> {
    const k = `${kind}:${bytes.hex(key)}`;
    const p = this.pending.get(k) ?? this.writing.get(k);
    if (p) return p.value;
    return this.sealed.get(this.name(kind), key);
  }

  put(kind: Kind, key: Uint8Array, value: Uint8Array): void {
    this.set({ kind, key, value });
  }

  delete(kind: Kind, key: Uint8Array): void {
    this.set({ kind, key });
  }

  private set(p: Pending): void {
    const k = `${p.kind}:${bytes.hex(p.key)}`;
    this.backlog += sizeOf(p) - sizeOf(this.pending.get(k));
    this.pending.set(k, p);
    this.schedule();
  }

  /**
   * Resolves once the bytes waiting to be written are within the bound:
   * at once below it, else when the store has caught up (a flush is
   * started now rather than at the next tick).
   */
  room(): Promise<void> {
    if (this.backlog <= this.maxPending) return Promise.resolve();
    if (this.pending.size) void this.flush().catch(() => {});
    return new Promise((r) => this.waiters.push(r));
  }

  private wake(): void {
    if (this.backlog > this.maxPending || !this.waiters.length) return;
    const ws = this.waiters;
    this.waiters = [];
    for (const w of ws) w();
  }

  /** Every record of a kind, pending writes included. */
  async list(kind: Kind): Promise<Map<string, Uint8Array>> {
    const out = new Map<string, Uint8Array>();
    for await (const [k, v] of this.sealed.list(this.name(kind))) out.set(bytes.hex(k), v);
    for (const p of [...this.writing.values(), ...this.pending.values()]) {
      if (p.kind !== kind) continue;
      if (p.value) out.set(bytes.hex(p.key), p.value);
      else out.delete(bytes.hex(p.key));
    }
    return out;
  }

  private schedule(): void {
    this.timer ??= setTimeout(() => {
      this.timer = undefined;
      void this.flush().catch(() => {});
    }, FLUSH_MS);
  }

  /** Write what is pending. A batch that fails doesn't stop the later ones. */
  flush(): Promise<void> {
    if (this.timer) {
      clearTimeout(this.timer);
      this.timer = undefined;
    }
    const batch = [...this.pending.entries()];
    for (const [k, p] of batch) this.writing.set(k, p);
    this.pending.clear();
    // Written (or given up): read from the store, unless a later flush took the key since.
    const landed = (k: string, p: Pending) => {
      if (this.writing.get(k) === p) this.writing.delete(k);
      this.backlog -= sizeOf(p);
    };
    this.flushing = this.flushing
      .catch(() => {})
      .then(async () => {
        let i = 0;
        try {
          for (; i < batch.length; i++) {
            const [k, p] = batch[i]!;
            const store = this.name(p.kind);
            if (p.value) await this.sealed.put(store, p.key, p.value);
            else await this.sealed.delete(store, [p.key]);
            landed(k, p);
            this.wake();
          }
        } finally {
          // A failed write: what is left of the batch isn't waiting any more.
          for (; i < batch.length; i++) landed(...batch[i]!);
          this.wake();
        }
      });
    return this.flushing;
  }

  /** Delete every record of every kind (`Replica.wipe`). */
  async clear(): Promise<void> {
    for (const p of this.pending.values()) this.backlog -= sizeOf(p);
    this.pending.clear();
    this.wake();
    await this.flushing;
    for (const k of KINDS) await this.sealed.clear(this.name(k));
  }
}

/** A use record: `u64 bytes ‖ u64 last use (ms)`. */
export function encodeUse(size: number, used: number): Uint8Array {
  const out = new Uint8Array(16);
  const v = new DataView(out.buffer);
  v.setBigUint64(0, BigInt(size));
  v.setBigUint64(8, BigInt(used));
  return out;
}

export function decodeUse(b: Uint8Array): { size: number; used: number } | undefined {
  if (b.length !== 16) return undefined;
  const v = new DataView(b.buffer, b.byteOffset, 16);
  return { size: Number(v.getBigUint64(0)), used: Number(v.getBigUint64(8)) };
}

/**
 * Kept chunks and pages, as the tree's caches: sealed bytes as on the
 * server, each with a use record for eviction (least recently used first).
 * What the tree fetches is only a cache: while the records waiting to be
 * written are past their bound, it isn't kept (unless `keeping`), so reads
 * neither wait for the store nor fill memory. Sealed content goes through
 * `keep`, which always stores it.
 */
export class Content {
  /** Bytes added since the last eviction pass (a hint to run one). */
  added = 0;
  /** While above 0, fetched content is kept past the bound too (pinned content). */
  keeping = 0;

  constructor(
    private readonly rec: Records,
    private readonly onAdd: () => void,
  ) {}

  private cache(kind: 'chunks' | 'pages', tag: number): ChunkCache {
    return {
      get: async (id) => {
        const b = await this.rec.get(kind, id);
        if (b) this.use(tag, id, b.length);
        return b;
      },
      put: (id, data) => {
        if (!this.keeping && this.rec.pendingBytes > this.rec.maxPending) return;
        this.store(kind, tag, id, data);
      },
    };
  }

  private store(kind: 'chunks' | 'pages', tag: number, id: Uint8Array, data: Uint8Array): void {
    this.rec.put(kind, id, data);
    this.use(tag, id, data.length);
    this.added += data.length;
    this.onAdd();
  }

  /** Store sealed content, whatever is waiting (the caller waits with `Records.room`). */
  keep(kind: 'chunks' | 'pages', id: Uint8Array, data: Uint8Array): void {
    this.store(kind, kind === 'chunks' ? 0x63 : 0x70, id, data);
  }

  readonly chunks = this.cache('chunks', 0x63);
  readonly pages = this.cache('pages', 0x70);

  private use(tag: number, id: Uint8Array, size: number): void {
    this.rec.put('lru', bytes.concat(new Uint8Array([tag]), id), encodeUse(size, Date.now()));
  }

  /**
   * Evict least recently used content until the total is at most `limit`,
   * never what `keep()` holds (hex `c`/`p` ‖ id). `keep` is asked once the
   * content is listed, with nothing awaited from there to the deletes, so
   * what was sealed or queued meanwhile is in it. Returns the total left.
   */
  async evict(limit: number, keep: () => Set<string>): Promise<number> {
    this.added = 0;
    const all = await this.rec.list('lru');
    const items: { key: string; size: number; used: number }[] = [];
    let total = 0;
    for (const [key, v] of all) {
      const u = decodeUse(v);
      if (!u) {
        this.rec.delete('lru', bytes.fromHex(key));
        continue;
      }
      total += u.size;
      items.push({ key, ...u });
    }
    if (total <= limit) return total;
    items.sort((a, b) => a.used - b.used);
    const kept = keep();
    for (const it of items) {
      if (total <= limit) break;
      if (kept.has(it.key)) continue;
      const key = bytes.fromHex(it.key);
      this.rec.delete(key[0] === 0x63 ? 'chunks' : 'pages', key.slice(1));
      this.rec.delete('lru', key);
      total -= it.size;
    }
    return total;
  }

  /** The total of kept content, in bytes. */
  async total(): Promise<number> {
    let t = 0;
    for (const v of (await this.rec.list('lru')).values()) t += decodeUse(v)?.size ?? 0;
    return t;
  }
}

/** The key a use record of chunk or page `id` has (hex). */
export const useKey = (kind: 'chunk' | 'page', id: Uint8Array): string =>
  bytes.hex(bytes.concat(new Uint8Array([kind === 'chunk' ? 0x63 : 0x70]), id));
