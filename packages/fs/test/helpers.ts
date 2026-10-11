// Test helpers of @zen/fs: installations with their own sessions, a switch
// to take one offline, and in-memory Web Locks and BroadcastChannels.
import {
  type BroadcastChannelLike,
  connect,
  type LockManagerLike,
  type Session,
  type UnlockedFs,
  ZenError,
} from '@zen/client';
import type { World } from '../../client/test/helpers.js';

/** A session's calls, recorded, and a switch that makes them fail with `network`. */
export interface Net {
  calls: { path: string; req: unknown }[];
  down: boolean;
  count(path: string): number;
  reset(): void;
}

/** Wrap a session's calls. */
export function watch(s: Session): Net {
  const orig = s.call.bind(s);
  const net: Net = {
    calls: [],
    down: false,
    count: (p) => net.calls.filter((c) => c.path === p).length,
    reset: () => {
      net.calls.length = 0;
    },
  };
  s.call = ((path: string, enc: never, dec: never, req: unknown) => {
    net.calls.push({ path, req });
    if (net.down) return Promise.reject(new ZenError(0, 'network', 'offline (test)'));
    return orig(path, enc, dec, req as never);
  }) as Session['call'];
  return net;
}

/** Another installation of the admin: its own client, session and unlocked fs 1. */
export async function installation(w: World): Promise<{ ufs: UnlockedFs; net: Net }> {
  const client = await connect(w.server.url, { origin: w.client.origin });
  const session = await client.signInDevice(w.admin);
  const net = watch(session);
  const ufs = await session.fs(1).unlock({ device: w.admin.device });
  return { ufs, net };
}

/** Retry `f` until it stops throwing (or `ms` passes). */
export async function until<T>(f: () => Promise<T> | T, ms = 10_000): Promise<T> {
  const end = Date.now() + ms;
  for (;;) {
    try {
      return await f();
    } catch (e) {
      if (Date.now() > end) throw e;
      await new Promise((r) => setTimeout(r, 50));
    }
  }
}

/** Web Locks in memory: one exclusive queue per name. */
export class FakeLocks implements LockManagerLike {
  private readonly tails = new Map<string, Promise<void>>();

  async request(
    name: string,
    opts: { mode: 'exclusive'; signal?: AbortSignal },
    cb: (lock: unknown) => Promise<unknown>,
  ): Promise<unknown> {
    const prev = this.tails.get(name) ?? Promise.resolve();
    let done!: () => void;
    const mine = new Promise<void>((r) => {
      done = r;
    });
    this.tails.set(
      name,
      prev.then(() => mine),
    );
    try {
      await new Promise<void>((res, rej) => {
        prev.then(res);
        opts.signal?.addEventListener('abort', () => rej(new Error('AbortError')));
      });
      return await cb({ name });
    } finally {
      done();
    }
  }
}

/** In-memory BroadcastChannels: async, structured-cloned, never to the sender. */
export class FakeBus {
  private readonly chans = new Map<string, Set<BroadcastChannelLike>>();

  open = (name: string): BroadcastChannelLike => {
    const set = this.chans.get(name) ?? new Set();
    this.chans.set(name, set);
    const ch: BroadcastChannelLike = {
      onmessage: null,
      postMessage(m: unknown) {
        const data = structuredClone(m);
        for (const c of set) {
          if (c === ch) continue;
          setTimeout(() => set.has(c) && c.onmessage?.({ data }), 0);
        }
      },
      close() {
        set.delete(ch);
      },
    };
    set.add(ch);
    return ch;
  };
}
