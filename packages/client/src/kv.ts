// KV and transactions (spec/api.md §5–6, DESIGN-3 §2.4).
import type {
  Append,
  ChunkPut,
  Commit,
  CommitResult,
  CrdtOp,
  Expect,
  ExpectRange,
  FsRange,
  Write,
} from '@zen/wasm';
import { compare, equal, hex, keyAfter, prefixEnd, randomBytes, utf8 } from './bytes.js';
import { ZenError } from './errors.js';
import type { UnlockedFs } from './fs.js';
import type { Session } from './session.js';
import { sleep } from './transport.js';
import { zw } from './wasm.js';

/** A KV path: elements are strings (UTF-8) or bytes (formats.md §3.1). */
export type Path = (string | Uint8Array)[];

/** One decrypted KV item. */
export interface KvEntry {
  /** The stored key: PRF tokens, 16 bytes per element. Names can't be read back. */
  key: Uint8Array;
  value: Uint8Array;
  /** The value's version (versionstamp). */
  version: Uint8Array;
}

/** Range options. */
export interface RangeOptions {
  limit?: number;
  reverse?: boolean;
}

const inRange = (k: Uint8Array, begin: Uint8Array, end: Uint8Array | undefined) =>
  compare(k, begin) >= 0 && (!end || compare(k, end) < 0);

const enc = (p: Path): Uint8Array[] => p.map((e) => (typeof e === 'string' ? utf8(e) : e));

/** Send a commit, replaying it with the same `commit_id` on a network error or `commit_unknown`. */
export async function sendCommit(session: Session, c: Commit, attempts = 4): Promise<CommitResult> {
  for (let i = 1; ; i++) {
    try {
      return await session.call('/v1/commit', zw.encodeCommit, zw.decodeCommitResult, c);
    } catch (e) {
      const replay = e instanceof ZenError && (e.code === 'network' || e.code === 'commit_unknown');
      if (!replay || i >= attempts) throw e;
      await sleep(50 * 2 ** i);
    }
  }
}

/** Plain KV reads and single writes. Multi-key atomic changes use `transaction`. */
export class Kv {
  constructor(private readonly fs: UnlockedFs) {}

  /** The stored key of a path. */
  key(path: Path): Uint8Array {
    return this.fs.keys.kvKey(enc(path));
  }

  /** Open a stored value (its key is the AAD context). */
  open(storedKey: Uint8Array, sealed: Uint8Array): Uint8Array {
    return this.fs.keysFor(sealed).openValue(storedKey, sealed);
  }

  /** Read one value; undefined if absent. */
  async get(path: Path): Promise<Uint8Array | undefined> {
    return (await this.getEntries([path]))[0]?.value;
  }

  /** Read several values at one snapshot (absent: undefined). */
  async getEntries(paths: Path[], readVersion?: bigint) {
    const keys = paths.map((p) => this.key(p));
    const r = await this.fs.session.call('/v1/kv/get', zw.encodeKvGet, zw.decodeKvItems, {
      fs: this.fs.id,
      keys,
      ...(readVersion !== undefined ? { read_version: readVersion } : {}),
    });
    return r.items.map((it) =>
      it.value && it.version
        ? { key: it.key, value: this.open(it.key, it.value), version: it.version }
        : undefined,
    );
  }

  /**
   * Every entry under a path prefix, in stored-key order (paging through
   * `more`). An empty prefix is the whole fs.
   */
  async *range(prefix: Path, opts: RangeOptions = {}): AsyncGenerator<KvEntry> {
    const begin = prefix.length ? this.key(prefix) : new Uint8Array(0);
    yield* this.rangeStored(begin, prefixEnd(begin), opts);
  }

  /** Every entry of the stored-key range `[begin, end)`. */
  async *rangeStored(
    begin: Uint8Array,
    end: Uint8Array | undefined,
    opts: RangeOptions = {},
    readVersion?: bigint,
  ): AsyncGenerator<KvEntry> {
    let lo = begin;
    let hi = end;
    let left = opts.limit ?? Number.POSITIVE_INFINITY;
    while (left > 0) {
      const r = await this.fs.session.call('/v1/kv/range', zw.encodeKvRange, zw.decodeKvItems, {
        fs: this.fs.id,
        begin: lo,
        ...(hi ? { end: hi } : {}),
        ...(Number.isFinite(left) ? { limit: left } : {}),
        ...(opts.reverse ? { reverse: true } : {}),
        ...(readVersion !== undefined ? { read_version: readVersion } : {}),
      });
      for (const it of r.items) {
        yield { key: it.key, value: this.open(it.key, it.value!), version: it.version! };
        left--;
      }
      const last = r.items.at(-1);
      if (!r.more || !last) return;
      if (opts.reverse) hi = last.key;
      else lo = keyAfter(last.key);
    }
  }

  /** Write one value (a one-write transaction without reads). */
  async set(path: Path, value: Uint8Array): Promise<void> {
    await this.fs.transaction(async (tx) => tx.set(path, value));
  }

  /** Delete one value. */
  async delete(path: Path): Promise<void> {
    await this.fs.transaction(async (tx) => tx.delete(path));
  }
}

/** Transaction options. */
export interface TxnOptions {
  /**
   * `short` (default): reads at one read version, serializable through
   * read-conflict ranges, valid for about 5 s. `long`: no time limit; every
   * read becomes an `expect` (or a range hash) checked at commit.
   */
  mode?: 'short' | 'long';
  /** Attempts on `conflict` / `too_old` (default 8). */
  attempts?: number;
}

/** A transaction's view: reads, buffered writes and appends. */
export class Transaction {
  readonly writes = new Map<string, Write>();
  readonly clears: FsRange[] = [];
  readonly appends: Append[] = [];
  readonly extra: Partial<Commit> = {};
  /** The commit's result, once committed (for appended offsets and dots). */
  result: CommitResult | undefined;
  private rv0: bigint | undefined;
  private readonly conflicts: FsRange[] = [];
  private readonly expects = new Map<string, Expect>();
  private readonly expectRanges: ExpectRange[] = [];
  private readonly crdtOps: CrdtOp[] = [];
  private readonly chunks: ChunkPut[] = [];
  /** @internal */
  readonly commitHooks: ((r: CommitResult) => void)[] = [];
  /** @internal */
  readonly errorHooks: ((e: unknown, attempt: number) => Promise<boolean>)[] = [];

  constructor(
    readonly fs: UnlockedFs,
    readonly mode: 'short' | 'long',
  ) {}

  /**
   * The version every read of a short transaction is at, set by its first
   * read (undefined before it, and in long mode).
   */
  get readVersion(): bigint | undefined {
    return this.mode === 'short' ? this.rv0 : undefined;
  }

  private key(path: Path | Uint8Array): Uint8Array {
    return path instanceof Uint8Array ? path : this.fs.kv.key(path);
  }

  private rv(): { read_version?: bigint } {
    return this.mode === 'short' && this.rv0 !== undefined ? { read_version: this.rv0 } : {};
  }

  private noteRv(v: bigint): void {
    if (this.mode === 'short') this.rv0 ??= v;
  }

  /** This transaction's own value for a stored key: a write, a clear, or nothing. */
  private own(key: Uint8Array): { value: Uint8Array | undefined } | undefined {
    const w = this.writes.get(hex(key));
    if (w) return { value: w.value ? this.fs.kv.open(key, w.value) : undefined };
    if (this.clears.some((c) => inRange(key, c.begin, c.end))) return { value: undefined };
    return undefined;
  }

  /** Read a value (sees this transaction's own writes). */
  async get(path: Path): Promise<Uint8Array | undefined> {
    const key = this.key(path);
    const own = this.writes.get(hex(key));
    if (own) return own.value ? this.fs.kv.open(key, own.value) : undefined;
    const r = await this.fs.session.call('/v1/kv/get', zw.encodeKvGet, zw.decodeKvItems, {
      fs: this.fs.id,
      keys: [key],
      ...this.rv(),
    });
    this.noteRv(r.read_version);
    const it = r.items[0]!;
    if (this.mode === 'short') {
      this.conflicts.push({ fs: this.fs.id, begin: key, end: keyAfter(key) });
    } else if (!this.expects.has(hex(key))) {
      this.expects.set(hex(key), {
        fs: this.fs.id,
        key,
        ...(it.version ? { version: it.version } : {}),
      });
    }
    return it.value ? this.fs.kv.open(key, it.value) : undefined;
  }

  /**
   * Read every entry under a prefix. In long mode the whole range is
   * hashed into an `expect_ranges` check, so it must fit one response.
   */
  async range(prefix: Path): Promise<KvEntry[]> {
    const begin = prefix.length ? this.key(prefix) : new Uint8Array(0);
    const end = prefixEnd(begin);
    const out: KvEntry[] = [];
    const hasher = this.mode === 'long' ? new zw.RangeHasher() : undefined;
    let lo = begin;
    for (;;) {
      const r = await this.fs.session.call('/v1/kv/range', zw.encodeKvRange, zw.decodeKvItems, {
        fs: this.fs.id,
        begin: lo,
        ...(end ? { end } : {}),
        ...this.rv(),
      });
      this.noteRv(r.read_version);
      for (const it of r.items) {
        hasher?.update(it.key, it.version!);
        out.push({ key: it.key, value: this.fs.kv.open(it.key, it.value!), version: it.version! });
      }
      const last = r.items.at(-1);
      if (!r.more || !last) break;
      if (hasher) throw new ZenError(413, 'too_large', 'a long-mode range must fit one read');
      lo = keyAfter(last.key);
    }
    const range: FsRange = { fs: this.fs.id, begin, ...(end ? { end } : {}) };
    if (hasher) {
      this.expectRanges.push({ ...range, hash: hasher.finalize() });
      hasher.free();
    } else {
      this.conflicts.push(range);
    }
    // Overlay this transaction's own writes and clears.
    const inRange = (k: Uint8Array) => compare(k, begin) >= 0 && (!end || compare(k, end) < 0);
    const merged = new Map(out.map((e) => [hex(e.key), e]));
    for (const c of this.clears) {
      for (const [h, e] of merged) {
        if (compare(e.key, c.begin) >= 0 && (!c.end || compare(e.key, c.end) < 0)) merged.delete(h);
      }
    }
    for (const [h, w] of this.writes) {
      if (!inRange(w.key)) continue;
      if (w.value) {
        merged.set(h, {
          key: w.key,
          value: this.fs.kv.open(w.key, w.value),
          version: new Uint8Array(10),
        });
      } else merged.delete(h);
    }
    return [...merged.values()].sort((a, b) => compare(a.key, b.key));
  }

  /**
   * Read values (by path or stored key) at the transaction's read version
   * without recording them: no read conflict, no `expect`. For data whose
   * consistency is checked another way (content-addressed nodes, records
   * the caller `expectKey`s). Sees this transaction's own writes.
   */
  async snapshotGet(keys: (Path | Uint8Array)[]): Promise<(KvEntry | undefined)[]> {
    const stored = keys.map((k) => this.key(k));
    const out: (KvEntry | undefined)[] = new Array(stored.length);
    const ask: number[] = [];
    stored.forEach((key, i) => {
      const own = this.own(key);
      if (!own) ask.push(i);
      else if (own.value) out[i] = { key, value: own.value, version: new Uint8Array(10) };
    });
    if (!ask.length) return out;
    const r = await this.fs.session.call('/v1/kv/get', zw.encodeKvGet, zw.decodeKvItems, {
      fs: this.fs.id,
      keys: ask.map((i) => stored[i]!),
      ...this.rv(),
    });
    this.noteRv(r.read_version);
    r.items.forEach((it, j) => {
      if (it.value && it.version) {
        out[ask[j]!] = {
          key: it.key,
          value: this.fs.kv.open(it.key, it.value),
          version: it.version,
        };
      }
    });
    return out;
  }

  /**
   * Every entry under a path prefix (an empty prefix: the whole fs), read
   * at the transaction's read version without recording it, as
   * `snapshotGet`. Sees this transaction's own writes and clears.
   */
  snapshotRange(prefix: Path, opts: RangeOptions = {}): AsyncGenerator<KvEntry> {
    const begin = prefix.length ? this.key(prefix) : new Uint8Array(0);
    return this.snapshotRangeStored(begin, prefixEnd(begin), opts);
  }

  /** `snapshotRange` over the stored-key range `[begin, end)`. */
  async *snapshotRangeStored(
    begin: Uint8Array,
    end: Uint8Array | undefined,
    opts: RangeOptions = {},
  ): AsyncGenerator<KvEntry> {
    const rev = !!opts.reverse;
    const before = (a: Uint8Array, b: Uint8Array) => (rev ? compare(a, b) > 0 : compare(a, b) < 0);
    // This transaction's writes in the range, in output order.
    const mine = [...this.writes.values()]
      .filter((w) => inRange(w.key, begin, end))
      .sort((a, b) => (rev ? compare(b.key, a.key) : compare(a.key, b.key)));
    let m = 0;
    let left = opts.limit ?? Number.POSITIVE_INFINITY;
    const ownEntry = (w: Write): KvEntry | undefined =>
      w.value
        ? { key: w.key, value: this.fs.kv.open(w.key, w.value), version: new Uint8Array(10) }
        : undefined;
    let lo = begin;
    let hi = end;
    for (;;) {
      if (left <= 0) return;
      const r = await this.fs.session.call('/v1/kv/range', zw.encodeKvRange, zw.decodeKvItems, {
        fs: this.fs.id,
        begin: lo,
        ...(hi ? { end: hi } : {}),
        ...(Number.isFinite(left) ? { limit: left } : {}),
        ...(rev ? { reverse: true } : {}),
        ...this.rv(),
      });
      this.noteRv(r.read_version);
      for (const it of r.items) {
        // Own writes that come first.
        while (m < mine.length && before(mine[m]!.key, it.key)) {
          const e = ownEntry(mine[m++]!);
          if (e) {
            yield e;
            if (--left <= 0) return;
          }
        }
        if (m < mine.length && equal(mine[m]!.key, it.key)) {
          const e = ownEntry(mine[m++]!);
          if (e) {
            yield e;
            if (--left <= 0) return;
          }
          continue;
        }
        if (this.clears.some((c) => inRange(it.key, c.begin, c.end))) continue;
        yield { key: it.key, value: this.fs.kv.open(it.key, it.value!), version: it.version! };
        if (--left <= 0) return;
      }
      const last = r.items.at(-1);
      if (!r.more || !last) break;
      if (rev) hi = last.key;
      else lo = keyAfter(last.key);
    }
    for (; m < mine.length && left > 0; m++) {
      const e = ownEntry(mine[m]!);
      if (e) {
        yield e;
        left--;
      }
    }
  }

  /**
   * Make the commit depend on a key's version (by path or stored key): it
   * fails with `conflict` unless the key is still at `version`, or still
   * absent for `undefined`. Puts a record read earlier (a cache) into the
   * read set, in either mode.
   */
  expectKey(key: Path | Uint8Array, version: Uint8Array | undefined): void {
    const k = this.key(key);
    this.expects.set(hex(k), { fs: this.fs.id, key: k, ...(version ? { version } : {}) });
  }

  /** Add CRDT operations to the commit (`Tree.writeIn`, CRDT rows). */
  addCrdtOps(ops: CrdtOp[]): void {
    this.crdtOps.push(...ops);
  }

  /** Add chunk uploads to the commit. */
  addChunks(chunks: ChunkPut[]): void {
    this.chunks.push(...chunks);
  }

  /** Run `fn` with the result once this attempt commits. */
  onCommit(fn: (r: CommitResult) => void): void {
    this.commitHooks.push(fn);
  }

  /**
   * Run `fn` when this attempt's commit fails. If any hook returns true
   * (it fixed what failed, as a tree rebase does), the transaction runs
   * again, within its attempts.
   */
  onError(fn: (e: unknown, attempt: number) => Promise<boolean>): void {
    this.errorHooks.push(fn);
  }

  /** Write a value. */
  set(path: Path, value: Uint8Array): void {
    const key = this.key(path);
    this.writes.set(hex(key), {
      fs: this.fs.id,
      key,
      value: this.fs.sealKeys().sealValue(key, value),
    });
  }

  /** Delete a value. */
  delete(path: Path): void {
    const key = this.key(path);
    this.writes.set(hex(key), { fs: this.fs.id, key });
  }

  /** Delete everything under a path prefix (applied before the writes). */
  clearPrefix(prefix: Path): void {
    const begin = this.key(prefix);
    const end = prefixEnd(begin);
    for (const [h, w] of this.writes) {
      if (compare(w.key, begin) >= 0 && (!end || compare(w.key, end) < 0)) this.writes.delete(h);
    }
    this.clears.push({ fs: this.fs.id, begin, ...(end ? { end } : {}) });
  }

  /** The commit body for this attempt. */
  build(): Commit {
    const c: Commit = { commit_id: randomBytes(16), ...this.extra };
    if (this.crdtOps.length) c.crdt_ops = [...(c.crdt_ops ?? []), ...this.crdtOps];
    if (this.chunks.length) c.chunks = [...(c.chunks ?? []), ...this.chunks];
    if (this.mode === 'short' && this.rv0 !== undefined) {
      c.read_version = this.rv0;
      if (this.conflicts.length) c.read_conflicts = this.conflicts;
    }
    if (this.expects.size) c.expect = [...this.expects.values()];
    if (this.expectRanges.length) c.expect_ranges = this.expectRanges;
    if (this.writes.size) c.writes = [...this.writes.values()];
    if (this.clears.length) c.clear_ranges = this.clears;
    if (this.appends.length) c.append = this.appends;
    return c;
  }

  /** Whether the commit would carry nothing. */
  get empty(): boolean {
    const x = this.extra;
    return (
      !this.writes.size &&
      !this.clears.length &&
      !this.appends.length &&
      !this.crdtOps.length &&
      !this.chunks.length &&
      !x.consume?.length &&
      !x.chunks?.length &&
      !x.crdt_ops?.length
    );
  }
}

/**
 * Run `fn` in a transaction and commit it, re-running it on `conflict` or
 * `too_old`, or when an `onError` hook asks for it. A transaction that only
 * reads commits nothing.
 */
export async function transaction<T>(
  fs: UnlockedFs,
  fn: (tx: Transaction) => Promise<T>,
  opts: TxnOptions = {},
): Promise<T> {
  const attempts = opts.attempts ?? 8;
  for (let i = 1; ; i++) {
    const tx = new Transaction(fs, opts.mode ?? 'short');
    const out = await fn(tx);
    if (tx.empty) return out;
    try {
      tx.result = await sendCommit(fs.session, tx.build());
    } catch (e) {
      if (i >= attempts) throw e;
      const retryable = e instanceof ZenError && e.retryable;
      let again = retryable;
      for (const h of tx.errorHooks) if (await h(e, i)) again = true;
      if (!again) throw e;
      if (retryable) await sleep(Math.min(10 * 2 ** i, 1000) * (0.5 + Math.random()));
      continue;
    }
    for (const h of tx.commitHooks) h(tx.result);
    return out;
  }
}
