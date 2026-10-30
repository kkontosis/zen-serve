// Handler checkpoints (svc.md §12, formats.md §14 entry type 5): echo
// handlers with integrity name each connection's cursors in their device
// checkpoints; a successor syncs them before resuming a claim, and aborts a
// connection whose claim is older than what was checkpointed. With an
// authenticated handler database (zendb.md §14), a successor also catches
// that database rolled back.
import { readFile } from 'node:fs/promises';
import { type Socket, connect as tcp } from 'node:net';
import { bytes, type Integrity, IntegrityError, zw } from '@zen/client';
import type { CheckpointEntry, SvcClaim } from '@zen/wasm';
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { lie, truth } from '../../../packages/client/src/testing/index.js';
import { startWorld, until, type World } from '../../shared/harness.js';
import { type DeviceFile, signInWorker, type WorkerContext } from '../../shared/node.js';
import { DB_OPTIONS, NS, serveEcho } from '../src/echo.js';
import { GRANTS } from './grants.js';

let w: World;

beforeAll(async () => {
  w = await startWorld({
    grants: GRANTS,
    server: { adversary: true },
    services: [{ name: 'echo', path: ['svc', 'echo'] }],
  });
});
afterAll(async () => {
  await w?.stop();
});

const closers: (() => Promise<unknown>)[] = [];
afterEach(async () => {
  for (const c of closers.reverse()) await c().catch(() => {});
  closers.length = 0;
});

type SvcEntry = Extract<CheckpointEntry, { type: 'svc' }>;

/** A worker with integrity (a checkpoint after every step), its errors, and its db. */
async function worker(
  user: string,
  fetch?: typeof globalThis.fetch,
  ns = NS,
  authenticated = false,
) {
  const ctx: WorkerContext = await signInWorker({
    url: w.url,
    device: w.user(user).deviceFile,
    passphrase: w.passphrase,
    integrity: { sigEvery: 1 },
    ...(fetch ? { fetch } : {}),
  });
  closers.push(() => ctx.close());
  const db = await ctx.open(ns, {
    ...DB_OPTIONS,
    warn: () => {},
    ...(authenticated ? { integrity: 'authenticated' as const } : {}),
  });
  const errors: { e: unknown; conn?: Uint8Array }[] = [];
  const handle = () => {
    const h = serveEcho(db, { onError: (e, conn) => errors.push({ e, conn }) });
    closers.push(() => h.stop());
    return h;
  };
  return { ctx, integ: ctx.integrity as Integrity, db, errors, handle };
}

async function deviceFp(user: string): Promise<Uint8Array> {
  const f = JSON.parse(await readFile(w.user(user).deviceFile, 'utf8')) as DeviceFile;
  return zw.DeviceSecret.fromSeed(bytes.fromHex(f.device_seed)).fingerprint;
}

/** The connection entries of `user`'s newest checkpoint, as `integ` verified it. */
async function entries(integ: Integrity, user: string): Promise<SvcEntry[]> {
  const body = integ.deviceHead(await deviceFp(user))?.body;
  return (body?.objects ?? []).filter((o): o is SvcEntry => o.type === 'svc');
}

interface Peer {
  s: Socket;
  got(): string;
  closed: Promise<void>;
}

async function peer(): Promise<Peer> {
  const s = tcp(w.servicePort('echo'), '127.0.0.1');
  await new Promise<void>((ok, fail) => s.once('connect', ok).once('error', fail));
  let buf = '';
  s.on('data', (d) => {
    buf += d.toString();
  });
  s.on('error', () => {});
  return { s, got: () => buf, closed: new Promise<void>((ok) => s.once('close', () => ok())) };
}

async function say(p: Peer, line: string, reply: string): Promise<void> {
  const before = p.got();
  p.s.write(`${line}\n`);
  await until(() => p.got() === before + reply, 10_000, `the reply ${reply.trim()}`);
}

/** A `fetch` that records each `/v1/svc/next` answer with connections. */
function recording(): { fetch: typeof fetch; claims: SvcClaim[] } {
  const claims: SvcClaim[] = [];
  return {
    claims,
    fetch: async (input, init) => {
      const resp = await fetch(input, init);
      if (!String(input).endsWith('/v1/svc/next') || !resp.ok) return resp;
      const body = new Uint8Array(await resp.arrayBuffer());
      claims.push(...zw.decodeSvcClaims(body).conns);
      return new Response(body, { status: 200, headers: resp.headers });
    },
  };
}

describe('handler checkpoints', () => {
  it('honest: the cursors ride in checkpoints; stop checkpoints; a successor resumes', async () => {
    const a = await worker('alice');
    const h1 = a.handle();
    const p = await peer();
    await say(p, 'a', '1: a\n');
    await say(p, 'b', '2: b\n');
    await h1.stop();
    // stop() checkpointed the connection's newest cursors before releasing.
    const mine = await entries(a.integ, 'alice');
    expect(mine).toHaveLength(1);
    expect(mine[0]).toMatchObject({ inAcked: 4n, outHead: 10n });
    // A successor on another device (fresh pins) syncs, checks, and continues.
    const b = await worker('bob');
    b.handle();
    await say(p, 'c', '3: c\n');
    const seen = await entries(b.integ, 'alice');
    expect(seen[0]).toMatchObject({ inAcked: 4n, outHead: 10n });
    const key = `5${bytes.hex(seen[0]!.svc)}.${bytes.hex(seen[0]!.conn)}`;
    expect(b.integ.floor(key)).toMatchObject({ inAcked: 6n, outHead: 15n });
    expect([...a.integ.alarms, ...b.integ.alarms]).toEqual([]);
    expect([...a.errors, ...b.errors]).toEqual([]);
    p.s.end();
  });

  it('a rolled-back claim: the successor reports it and aborts the connection', async () => {
    const rec = recording();
    const a = await worker('alice', rec.fetch);
    let h = a.handle();
    const p = await peer();
    await say(p, 'a', '1: a\n');
    await say(p, 'b', '2: b\n');
    await h.stop();
    // Claimed again in the middle: the snapshot the server will replay.
    h = a.handle();
    await say(p, 'c', '3: c\n');
    await say(p, 'd', '4: d\n');
    await h.stop();
    const old = rec.claims.find((c) => c.in_acked === 4n);
    expect(old).toBeDefined();

    // The successor's transport replays that claim, with the real token.
    const tamper: typeof fetch = async (input, init) => {
      const resp = await fetch(input, init);
      if (!String(input).endsWith('/v1/svc/next') || !resp.ok) return resp;
      const r = zw.decodeSvcClaims(new Uint8Array(await resp.arrayBuffer()));
      r.conns = r.conns.map((c) =>
        bytes.equal(c.conn, old!.conn) ? { ...old!, token: c.token } : c,
      );
      return new Response(zw.encodeSvcClaims(r) as Uint8Array<ArrayBuffer>, {
        status: 200,
        headers: { 'content-type': resp.headers.get('content-type') ?? 'application/cbor' },
      });
    };
    const b = await worker('bob', tamper);
    b.handle();
    await until(() => b.errors.length > 0, 10_000, 'the rollback report');
    const err = b.errors[0]!.e as IntegrityError;
    expect(err).toBeInstanceOf(IntegrityError);
    expect(err.alarm).toMatchObject({ kind: 'rollback', object: 'svc' });
    expect(bytes.equal(err.alarm.id!.slice(-old!.conn.length), old!.conn)).toBe(true);
    await p.closed; // aborted
    expect(p.got()).toBe('1: a\n2: b\n3: c\n4: d\n');
  });

  it('a rolled-back handler database: the successor catches it and gives the connection up', async () => {
    const a = await worker('alice', undefined, 'echo-auth-db', true);
    expect(a.db.integrity).toBe('authenticated');
    // The successor's device opens the database now, and handles later.
    const b = await worker('bob', undefined, 'echo-auth-db', true);
    const h = a.handle();
    const p = await peer();
    await say(p, 'a', '1: a\n');
    // The server freezes the database as it is (its NsRoot and state tree); writes still apply.
    const [begin, end] = a.db.keys.range('n', new Uint8Array(16));
    const keys = [a.db.keys.nsRoot()];
    for await (const e of a.db.fs.kv.rangeStored(begin, end)) keys.push(e.key);
    await lie(
      w.admin,
      keys.map((key) => ({ kind: 'kv' as const, fs: 1, key })),
    );
    try {
      await say(p, 'b', '2: b\n');
      await h.stop(); // checkpoints the NsRoot after `b`
      expect(a.errors).toEqual([]);
      // The successor syncs before it resumes; the database it reads is behind.
      b.handle();
      p.s.write('c\n');
      await until(() => b.errors.length > 0, 10_000, 'the rollback report');
      const err = b.errors[0]!.e as IntegrityError;
      expect(err).toBeInstanceOf(IntegrityError);
      expect(err.alarm).toMatchObject({ kind: 'rollback', object: 'db' });
      expect(err.alarm.detail).toContain('went back');
      await p.closed; // given up
      expect(p.got()).toBe('1: a\n2: b\n');
    } finally {
      await truth(w.admin);
      p.s.end();
    }
  });

  it('a withheld checkpoint: the successor sees the older one (the window); the handler alarms itself', async () => {
    const a = await worker('alice');
    const h = a.handle();
    const p = await peer();
    await say(p, 'a', '1: a\n');
    // Alice's body is cumulative: earlier tests' connections ride in it too.
    let conn: Uint8Array | undefined;
    await until(
      async () => {
        conn = (await entries(a.integ, 'alice')).find((x) => x.inAcked === 2n)?.conn;
        return conn !== undefined;
      },
      10_000,
      'a checkpoint',
    );
    // The server freezes alice's checkpoints here; later ones are accepted but hidden.
    await lie(w.admin, [{ kind: 'checkpoints', fs: 1, device: await deviceFp('alice') }]);
    try {
      await say(p, 'b', '2: b\n');
      await h.stop();
      const b = await worker('bob');
      b.handle();
      await say(p, 'c', '3: c\n');
      // The successor saw only the older checkpoint: the claim passes (the window).
      const seen = (await entries(b.integ, 'alice')).find((x) => bytes.equal(x.conn, conn!));
      expect(seen).toMatchObject({ inAcked: 2n, outHead: 5n });
      expect(b.integ.alarms).toEqual([]);
      expect(b.errors).toEqual([]);
      // Alice's own sync finds its head below its pin.
      await expect(a.integ.sync()).rejects.toBeInstanceOf(IntegrityError);
      expect(a.integ.alarms[0]).toMatchObject({ kind: 'rollback', object: 'checkpoint' });
      expect(a.integ.trusted).toBe(false);
    } finally {
      await truth(w.admin);
      p.s.end();
    }
  });
});
