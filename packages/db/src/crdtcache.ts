// Read modes of CRDT tables (spec/zendb.md §19.4): a cache of merged
// states per `Db`, checked against the server with `/v1/crdt/versions`
// once an entry has expired.
import { bytes, ZenError, zw } from '@zen/client';
import type { ObjState } from '@zen/wasm';
import { crdtGet } from './crdt.js';
import type { Db } from './db.js';
import { DbError } from './errors.js';

/**
 * How a CRDT read finds its state (§19.4):
 * - `auto`: a cached state younger than `crdtCacheMs` as it is; an older
 *   one is checked by version, and fetched again only if it changed.
 *   Offline, whatever is cached.
 * - `cache`: whatever is cached, however old; the server only for misses.
 * - `server`: always from the server; offline it fails.
 */
export type CrdtRead = 'auto' | 'cache' | 'server';

/** Objects per `/v1/crdt/versions` (api.md §13.3). */
const VERSIONS_MAX = 1000;

interface Entry {
  /** The object's state; undefined when it has none. */
  state: ObjState | undefined;
  /** When the server last confirmed it (ms). */
  checked: number;
  /** A commit of this `Db` changed the object since: fetch it again when online. */
  stale: boolean;
}

export const isNetwork = (e: unknown) => e instanceof ZenError && e.code === 'network';

const newer = (a: Uint8Array, b: Uint8Array) => bytes.compare(a, b) >= 0;

/** The cache of CRDT object states of one `Db`, least recently used first. */
export class CrdtCache {
  private readonly m = new Map<string, Entry>();
  /** When each object was last marked stale (a counter), so a fetch in flight doesn't undo it. */
  private readonly marks = new Map<string, number>();
  private gen = 0;

  constructor(
    private readonly size: number,
    readonly freshMs: number,
  ) {}

  private touch(h: string): Entry | undefined {
    const e = this.m.get(h);
    if (e) {
      this.m.delete(h);
      this.m.set(h, e);
    }
    return e;
  }

  /** A cached entry. */
  get(h: string): Entry | undefined {
    return this.touch(h);
  }

  /** The generation to pass to `put` for a fetch starting now. */
  start(): number {
    return this.gen;
  }

  /**
   * Keep a state the server returned (undefined: the object has none). An
   * older version than the cached one is ignored; a state fetched before
   * the object was marked stale stays stale.
   */
  put(h: string, state: ObjState | undefined, gen: number): void {
    if (!this.size) return;
    const old = this.m.get(h);
    if (old?.state && state && !newer(state.version, old.state.version)) {
      old.checked = Date.now();
      return;
    }
    const stale = (this.marks.get(h) ?? -1) > gen;
    this.m.delete(h);
    this.m.set(h, { state, checked: Date.now(), stale });
    while (this.m.size > this.size) this.m.delete(this.m.keys().next().value!);
  }

  /** The server confirmed a cached state unchanged. */
  confirm(h: string): void {
    const e = this.m.get(h);
    if (e) e.checked = Date.now();
  }

  /** A commit changed these objects: the next read fetches them again. */
  markStale(objects: Iterable<Uint8Array>): void {
    this.gen++;
    for (const o of objects) {
      const h = bytes.hex(o);
      const e = this.m.get(h);
      if (e) e.stale = true;
      this.marks.set(h, this.gen);
    }
    if (this.marks.size > 4 * Math.max(this.size, 1024)) this.marks.clear();
  }

  /** The cached states of objects in `[begin, end)` (a table's rows). */
  statesIn(begin: Uint8Array, end: Uint8Array | undefined): ObjState[] {
    const out: ObjState[] = [];
    for (const e of this.m.values()) {
      const o = e.state?.object;
      if (!o || bytes.compare(o, begin) < 0 || (end && bytes.compare(o, end) >= 0)) continue;
      out.push(e.state!);
    }
    return out;
  }

  clear(): void {
    this.m.clear();
    this.marks.clear();
  }
}

/** `/v1/crdt/versions`, in batches: each object's version, empty for none. */
export async function crdtVersions(
  db: Db,
  objects: Uint8Array[],
  readVersion?: bigint,
): Promise<Uint8Array[]> {
  const out: Uint8Array[] = [];
  for (let i = 0; i < objects.length; i += VERSIONS_MAX) {
    const r = await db.fs.session.call(
      '/v1/crdt/versions',
      zw.encodeCrdtVersionsReq,
      zw.decodeCrdtVersions,
      {
        fs: db.fs.id,
        objects: objects.slice(i, i + VERSIONS_MAX),
        ...(readVersion !== undefined ? { read_version: readVersion } : {}),
      },
    );
    out.push(...r.versions);
  }
  return out;
}

/** Fetch states with `crdt/get` and keep them (absent ones too). */
async function fetchStates(
  db: Db,
  objects: Uint8Array[],
  out: Map<string, ObjState | undefined>,
  readVersion?: bigint,
): Promise<void> {
  if (!objects.length) return;
  const cache = db.crdtCache;
  const gen = cache.start();
  const got = new Map(
    (await crdtGet(db, objects, readVersion)).map((s) => [bytes.hex(s.object), s]),
  );
  for (const o of objects) {
    const h = bytes.hex(o);
    const s = got.get(h);
    cache.put(h, s, gen);
    out.set(h, s);
  }
}

/** Keep the states a `crdt/range` page returned. */
export function cacheStates(db: Db, states: ObjState[], gen: number): void {
  for (const s of states) db.crdtCache.put(bytes.hex(s.object), s, gen);
}

/**
 * The states of objects (§19.4), by object (hex): undefined for an object
 * without state. A miss while offline is `offline`, unless `known` says the
 * caller can do without it (then undefined).
 */
export async function crdtStates(
  db: Db,
  objects: Uint8Array[],
  mode: CrdtRead,
  readVersion?: bigint,
  known: (h: string) => boolean = () => false,
): Promise<Map<string, ObjState | undefined>> {
  const cache = db.crdtCache;
  const out = new Map<string, ObjState | undefined>();
  if (mode === 'server') {
    await fetchStates(db, objects, out, readVersion);
    return out;
  }
  // `server` aside, a fetch that fails offline falls back to the cache below.
  const now = Date.now();
  const check: Uint8Array[] = [];
  const miss: Uint8Array[] = [];
  const cachedOf = new Map<string, Entry>();
  for (const o of objects) {
    const h = bytes.hex(o);
    if (out.has(h) || cachedOf.has(h)) continue;
    const e = cache.get(h);
    if (!e) miss.push(o);
    else {
      cachedOf.set(h, e);
      if (e.stale) miss.push(o);
      else if (mode === 'cache' || now - e.checked < cache.freshMs) out.set(h, e.state);
      else check.push(o);
    }
  }
  try {
    if (check.length) {
      const gen = cache.start();
      const vs = await crdtVersions(db, check, readVersion);
      check.forEach((o, i) => {
        const h = bytes.hex(o);
        const e = cachedOf.get(h)!;
        const v = vs[i]!;
        const have = e.state?.version ?? new Uint8Array();
        if (bytes.equal(v, have) && gen === cache.start()) {
          cache.confirm(h);
          out.set(h, e.state);
        } else miss.push(o);
      });
    }
    await fetchStates(db, miss, out, readVersion);
  } catch (e) {
    if (!isNetwork(e)) throw e;
    // Offline: what is cached, however old (§19.4).
    for (const o of [...check, ...miss]) {
      const h = bytes.hex(o);
      if (out.has(h)) continue;
      const c = cachedOf.get(h);
      if (!c && !known(h)) {
        throw new DbError('offline', 'a CRDT row that is not cached, while offline');
      }
      out.set(h, c?.state);
    }
  }
  return out;
}
