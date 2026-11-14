// The worker's start (spec/sw.md §8): the wasm, the vault's device, each
// mounted fs unlocked and each mounted tree's replica, once per worker
// instance and shared by every request. A start that fails is tried again
// on the next request; `restart` drops one that succeeded.
import {
  bytes,
  type ChannelFactory,
  type Client,
  type ConnectOptions,
  connect,
  credentialVault,
  initWasm,
  isCode,
  type LocalStore,
  type LockManagerLike,
  type SealedStore,
  type Session,
  sealedStore,
  type UnlockedFs,
  ZenError,
} from '@zen/client';
import { openReplica, type Replica } from '@zen/fs';
import type { Site } from './handle.js';
import { type Mount, parseMounts } from './mounts.js';

/** Where the mounts are kept in the content origin's sealed store (sw.md §8). */
export const MOUNTS_STORE = 'sw';
export const MOUNTS_KEY = bytes.utf8('mounts');
/** How long a request waits for the start before 503 `starting` (sw.md §7). */
export const START_TIMEOUT_MS = 15_000;
/** The worker's replicas: a viewer mostly reads (sw.md §8, OPS.md §6). */
export const PENDING_BYTES = 8 << 20;

/** How long a new replica waits to win its owner election before it serves. */
const OWNER_WAIT_MS = 2_000;

/**
 * A replica just opened shows what its store kept: bring it up to date
 * before it serves, so a fresh worker doesn't answer 404 for what the
 * server has. Offline, it serves what it kept. If another instance owns
 * the replica (an old worker still finishing), that one's announcements
 * keep it current.
 */
async function caughtUp(r: Replica): Promise<Replica> {
  const end = Date.now() + OWNER_WAIT_MS;
  while (!r.isOwner && Date.now() < end) await new Promise((ok) => setTimeout(ok, 10));
  try {
    await r.sync();
  } catch (e) {
    if (!isCode(e, 'network') && !isCode(e, 'offline')) {
      await r.close().catch(() => {});
      throw e;
    }
  }
  return r;
}

/** Mounts as kept: JSON, `tree` in hex. */
export function encodeMounts(mounts: readonly Mount[]): Uint8Array {
  return bytes.utf8(JSON.stringify(mounts.map((m) => ({ ...m, tree: bytes.hex(m.tree) }))));
}

/** Mounts as kept, checked (`parseMounts`); throws on anything invalid. */
export function decodeMounts(b: Uint8Array): Mount[] {
  const v: unknown = JSON.parse(bytes.fromUtf8(b));
  if (!Array.isArray(v)) throw new TypeError('mounts: not a list');
  return parseMounts(
    v.map((m: unknown) =>
      m && typeof m === 'object' && typeof (m as { tree?: unknown }).tree === 'string'
        ? { ...m, tree: bytes.fromHex((m as { tree: string }).tree) }
        : m,
    ),
  );
}

/** Options of `startSite`. */
export interface StartOptions {
  /** The content origin: sign-ins are bound to it. */
  origin: string;
  /** The server's URL (default `origin`: the content origin serves `/v1`). */
  url?: string;
  /** The content origin's local store (the worker: `indexedDbLocalStore('zen-sw')`). */
  local: LocalStore;
  /**
   * Where the wasm is (default: next to the bundle, or the package in Node),
   * or a function giving it at each start (the worker: its cached copy).
   */
  wasm?: URL | string | (() => Promise<URL | string | Response>);
  /** Web Locks for the replicas' owner election (default `navigator.locks`; `null`: none). */
  locks?: LockManagerLike | null;
  /** The replicas' tab channel (default `BroadcastChannel`; `null`: none). */
  channel?: ChannelFactory | null;
  /** How long a request waits for the start (default `START_TIMEOUT_MS`). */
  timeoutMs?: number;
  /** Told of each error that becomes a 500 (`Site.onError`). */
  onError?(e: unknown, req: Request): void;
  /** Passed to `connect` (tests). */
  connect?: ConnectOptions;
}

/** The worker's site: `site()` for each request, `restart()` after an enrollment. */
export interface Viewer {
  /**
   * The site for one request. Never rejects: while the start fails, or
   * hasn't finished within the timeout, every path answers its error
   * (sw.md §7) and the next call tries again.
   */
  site(): Promise<Site>;
  /** Drop what was started, so the next request starts again (sw.md §1 `zen-restart`). */
  restart(): Promise<void>;
  /** The session, once started (tests). */
  session(): Session | undefined;
}

interface Started {
  mounts: Mount[];
  session: Session;
  fs: Map<number, UnlockedFs>;
  replicas: Map<string, Promise<Replica>>;
}

const locked = (why: string) => new ZenError(503, 'locked', why);

/** A site whose every path fails with `err`: a root mount whose replica rejects. */
function failing(
  err: unknown,
  mounts: readonly Mount[] = [],
  onError?: (e: unknown, req: Request) => void,
): Site {
  const all: Mount = {
    name: 'zen',
    fs: 0,
    tree: new Uint8Array(16),
    base: '/',
    index: [],
    listing: false,
    root: true,
  };
  // Keep the named mounts (so `/name` routes as usual), and send the rest to `all`.
  return {
    mounts: [...mounts.filter((m) => !m.root), all],
    replica: () => Promise.reject(err),
    ...(onError ? { onError } : {}),
  };
}

/** The viewer of the content origin (sw.md §8). */
export function startSite(o: StartOptions): Viewer {
  const timeoutMs = o.timeoutMs ?? START_TIMEOUT_MS;
  let sealed: SealedStore | undefined;
  let started: Promise<Started> | undefined;
  let current: Started | undefined;
  /** The mounts last read, for the error site's routing. */
  let known: Mount[] = [];

  async function start(): Promise<Started> {
    await initWasm(typeof o.wasm === 'function' ? await o.wasm() : o.wasm);
    sealed ??= sealedStore(o.local);
    const kept = await sealed.get(MOUNTS_STORE, MOUNTS_KEY);
    if (!kept) throw locked('not enrolled: no mounts');
    const mounts = decodeMounts(kept);
    known = mounts;
    const vault = credentialVault(sealed);
    const client: Client = await connect(o.url ?? o.origin, { ...o.connect, origin: o.origin });
    let session: Session;
    try {
      session = await vault.signIn(client);
    } catch (e) {
      // No device, or its sign-in refused (not in the ACL yet, or removed).
      if (isCode(e, 'not_found') || isCode(e, 'unauthorized') || isCode(e, 'method_disabled')) {
        throw locked(`the device can't sign in: ${(e as Error).message}`);
      }
      throw e;
    }
    const fs = new Map<number, UnlockedFs>();
    try {
      for (const id of new Set(mounts.map((m) => m.fs))) {
        try {
          fs.set(id, await vault.unlock(session.fs(id), { local: sealed }));
        } catch (e) {
          if (isCode(e, 'decrypt')) throw locked(`no keyslot of fs ${id} opens with the device`);
          throw e;
        }
      }
    } catch (e) {
      for (const u of fs.values()) u.close();
      throw e;
    }
    return { mounts, session, fs, replicas: new Map() };
  }

  function begin(): Promise<Started> {
    if (!started) {
      const p = start();
      started = p;
      p.then(
        (s) => {
          if (started === p) current = s;
          // Restarted while it started: drop it.
          else for (const u of s.fs.values()) u.close();
        },
        () => {
          if (started === p) started = undefined;
        },
      );
    }
    return started;
  }

  function replicaOf(s: Started, m: Mount): Promise<Replica> {
    const key = `${m.fs}/${bytes.hex(m.tree)}`;
    let r = s.replicas.get(key);
    if (!r) {
      r = openReplica(s.fs.get(m.fs)!, m.tree, {
        local: sealed!,
        pendingBytes: PENDING_BYTES,
        ...(o.locks !== undefined ? { locks: o.locks } : {}),
        ...(o.channel !== undefined ? { channel: o.channel } : {}),
      }).then(caughtUp);
      s.replicas.set(key, r);
      r.catch(() => {
        if (s.replicas.get(key) === r) s.replicas.delete(key);
      });
    }
    return r;
  }

  /** `p`, or 503 `starting` once the timeout passes. */
  async function bounded<T>(p: Promise<T>): Promise<T> {
    let timer: ReturnType<typeof setTimeout> | undefined;
    const late = new Promise<never>((_, fail) => {
      timer = setTimeout(() => fail(new ZenError(503, 'starting', 'still starting')), timeoutMs);
    });
    try {
      return await Promise.race([p, late]);
    } finally {
      clearTimeout(timer);
    }
  }

  return {
    async site() {
      const p = begin();
      let timer: ReturnType<typeof setTimeout> | undefined;
      const late = new Promise<'late'>((ok) => {
        timer = setTimeout(() => ok('late'), timeoutMs);
      });
      try {
        const s = await Promise.race([p, late]);
        if (s === 'late')
          return failing(new ZenError(503, 'starting', 'still starting'), known, o.onError);
        return {
          mounts: s.mounts,
          replica: (m) => bounded(replicaOf(s, m)),
          ...(o.onError ? { onError: o.onError } : {}),
        };
      } catch (e) {
        return failing(e, known, o.onError);
      } finally {
        clearTimeout(timer);
      }
    },

    async restart() {
      const s = current;
      started = undefined;
      current = undefined;
      known = [];
      if (!s) return;
      for (const r of s.replicas.values()) {
        await r.then((x) => x.close()).catch(() => {});
      }
      for (const u of s.fs.values()) u.close();
    },

    session: () => current?.session,
  };
}
