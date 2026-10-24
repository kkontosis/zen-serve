// Waking a handler without input (`c.wakeOn`, `ServeHandle.wake`,
// `c.again`), over raw and TLS, on the echo world. The handler here is
// the test's own: `WATCH k…` registers wake keys (none: clears them),
// `MORE n` writes n bytes across steps with `again()`, `BLOCK` holds the
// step until the test releases it, and `RACE k` wakes `k` from inside the
// step, before the step has registered it. Called with no input and keys
// registered, it writes `woke <n>`.
import { connect as tcp } from 'node:net';
import { connect } from 'node:tls';
import type { Db } from '@zen/db';
import { LineReader, type RawHandler, type ServeHandle, serve, tls } from '@zen/svc';
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { testCerts } from '../../../packages/client/src/testing/index.js';
import { sleep, startWorld, until, type World } from '../../shared/harness.js';
import { signInWorker, type WorkerContext } from '../../shared/node.js';
import { DB_OPTIONS, NS } from '../src/echo.js';
import { GRANTS } from './grants.js';

let w: World;
let alice: WorkerContext;
let db: Db;
let certs: ReturnType<typeof testCerts>;

beforeAll(async () => {
  w = await startWorld({
    grants: GRANTS,
    services: [
      { name: 'echo', path: ['svc', 'echo'] },
      { name: 'echo-tls', path: ['svc', 'echo-tls'] },
    ],
  });
  alice = await signInWorker({
    url: w.url,
    device: w.user('alice').deviceFile,
    passphrase: w.passphrase,
  });
  db = await alice.open(NS, { ...DB_OPTIONS, warn: () => {} });
  certs = testCerts();
});
afterAll(async () => {
  await alice?.close();
  await w?.stop();
});

const errors: unknown[] = [];
const onError = (e: unknown) => errors.push(e);
const handles: ServeHandle[] = [];
afterEach(async () => {
  for (const h of handles.splice(0)) await h.stop();
  expect(errors.splice(0)).toEqual([]);
});

/** What the handler did, for the checks. */
interface Probe {
  /** Handler calls. */
  calls: number;
  /** `room` before and after each write of `MORE`. */
  rooms: [number, number][];
  /** `BLOCK`: resolved once the step holds, and its release. */
  blocked?: Promise<void>;
  release?: () => void;
  /** `RACE k`: called inside the step. */
  race?: (key: string) => void;
}

interface AppState {
  /** Wakes answered. */
  n: number;
  keys: string[];
  /** Bytes `MORE` still has to write. */
  more: number;
}

const text = new TextDecoder();

function watcher(probe: Probe): RawHandler {
  return async (c) => {
    probe.calls++;
    const st: AppState = c.state ? JSON.parse(text.decode(c.state)) : { n: 0, keys: [], more: 0 };
    if (st.more > 0) {
      const take = Math.min(st.more, 10_000, c.room);
      const before = c.room;
      c.write('x'.repeat(take));
      probe.rooms.push([before, c.room]);
      st.more -= take;
      if (st.more > 0) c.again();
      else c.write('\ndone\n');
    } else if (c.input.length === 0 && st.keys.length) {
      st.n++;
      c.write(`woke ${st.n}\n`);
    }
    const r = new LineReader(c.input);
    for (let l = r.nextText(); l !== null; l = r.nextText()) {
      const [verb, ...args] = l.split(' ').filter(Boolean);
      if (verb === 'WATCH') {
        st.keys = args;
        c.write('ok\n');
      } else if (verb === 'MORE') {
        st.more = Number(args[0]);
        c.again();
      } else if (verb === 'BLOCK') {
        let held!: () => void;
        probe.blocked = new Promise<void>((r) => {
          held = r;
        });
        const go = new Promise<void>((r) => {
          probe.release = r;
        });
        held();
        await go;
        c.write('released\n');
      } else if (verb === 'RACE') {
        st.keys = args;
        probe.race?.(args[0]!);
        c.write('ok\n');
      }
    }
    c.ack(r.consumed);
    c.wakeOn(...st.keys);
    c.state = new TextEncoder().encode(JSON.stringify(st));
  };
}

/** A peer of the service: raw TCP, or TLS. */
interface Peer {
  write(s: string): void;
  got(): string;
  end(): void;
}

async function peer(kind: 'raw' | 'tls'): Promise<Peer> {
  const port = w.servicePort(kind === 'raw' ? 'echo' : 'echo-tls');
  const s =
    kind === 'raw'
      ? tcp(port, '127.0.0.1')
      : connect({ host: '127.0.0.1', port, ca: certs.ca, servername: 'localhost' });
  await new Promise<void>((ok, fail) =>
    s.once(kind === 'raw' ? 'connect' : 'secureConnect', ok).once('error', fail),
  );
  let buf = '';
  s.on('data', (d: Buffer) => {
    buf += d.toString();
  });
  s.on('error', () => {});
  return { write: (t) => s.write(t), got: () => buf, end: () => s.destroy() };
}

function start(kind: 'raw' | 'tls', probe: Probe): ServeHandle {
  const h =
    kind === 'raw'
      ? serve(db, { service: ['svc', 'echo'], handler: watcher(probe), onError })
      : serve(db, {
          service: ['svc', 'echo-tls'],
          layer: tls({ cert: certs.chain, key: certs.key }),
          handler: watcher(probe),
          onError,
        });
  handles.push(h);
  return h;
}

const fresh = (): Probe => ({ calls: 0, rooms: [] });

describe.each(['raw', 'tls'] as const)('waking (%s)', (kind) => {
  it('a wake reaches the connections watching its key, and no others', async () => {
    const probe = fresh();
    const h = start(kind, probe);
    const a = await peer(kind);
    const b = await peer(kind);
    a.write('WATCH k1 k2\n');
    b.write('WATCH k3\n');
    await until(() => a.got() === 'ok\n' && b.got() === 'ok\n', 10_000, 'both watching');
    h.wake('k2');
    await until(() => a.got() === 'ok\nwoke 1\n', 10_000, 'a woken');
    h.wake('k1');
    await until(() => a.got() === 'ok\nwoke 1\nwoke 2\n', 10_000, 'a woken again');
    h.wake('nobody');
    await sleep(300);
    expect(b.got()).toBe('ok\n');
    h.wake('k3');
    await until(() => b.got() === 'ok\nwoke 1\n', 10_000, 'b woken');
    expect(a.got()).toBe('ok\nwoke 1\nwoke 2\n');
    a.end();
    b.end();
  });

  it('wakeOn() clears the keys', async () => {
    const probe = fresh();
    const h = start(kind, probe);
    const a = await peer(kind);
    a.write('WATCH k\n');
    await until(() => a.got() === 'ok\n', 10_000, 'watching');
    a.write('WATCH\n');
    await until(() => a.got() === 'ok\nok\n', 10_000, 'cleared');
    const calls = probe.calls;
    h.wake('k');
    await sleep(300);
    expect(a.got()).toBe('ok\nok\n');
    expect(probe.calls).toBe(calls);
    a.end();
  });

  it('a wake while the handler runs leads to one more step', async () => {
    const probe = fresh();
    const h = start(kind, probe);
    const a = await peer(kind);
    a.write('WATCH k\n');
    await until(() => a.got() === 'ok\n', 10_000, 'watching');
    a.write('BLOCK\n');
    await until(() => probe.blocked !== undefined, 10_000, 'the step to block');
    await probe.blocked;
    h.wake('k');
    probe.release!();
    await until(() => a.got() === 'ok\nreleased\nwoke 1\n', 10_000, 'the wake after the step');
    await sleep(300);
    expect(a.got()).toBe('ok\nreleased\nwoke 1\n');
    a.end();
  });

  it('a wake before the step registers its key still reaches it', async () => {
    const probe = fresh();
    const h = start(kind, probe);
    // Inside the step: the change it would announce came after its read.
    probe.race = (k) => h.wake(k);
    const a = await peer(kind);
    a.write('RACE r\n');
    await until(() => a.got() === 'ok\nwoke 1\n', 10_000, 'the raced wake');
    a.end();
  });

  it('again() steps without input until the handler stops, and room shrinks as it writes', async () => {
    const probe = fresh();
    start(kind, probe);
    const a = await peer(kind);
    a.write('MORE 50000\n');
    const want = `${'x'.repeat(50_000)}\ndone\n`;
    await until(() => a.got() === want, 10_000, 'all of MORE');
    expect(probe.rooms.length).toBe(5);
    for (const [before, after] of probe.rooms) expect(before - after).toBe(10_000);
    a.end();
  });

  it('a successor calls the handler once on its claim, and a later wake reaches it', async () => {
    const one = fresh();
    const h1 = start(kind, one);
    const a = await peer(kind);
    a.write('WATCH k\n');
    await until(() => a.got() === 'ok\n', 10_000, 'watching');
    await h1.stop();
    const two = fresh();
    const h2 = start(kind, two);
    // Its claim calls the handler with no input: it registers its keys.
    await until(() => a.got() === 'ok\nwoke 1\n', 15_000, 'the call on the claim');
    // Once: another call without input would write another `woke`.
    await sleep(300);
    expect(a.got()).toBe('ok\nwoke 1\n');
    h2.wake('k');
    await until(() => a.got() === 'ok\nwoke 1\nwoke 2\n', 10_000, 'a wake after the takeover');
    a.end();
  });

  it('idle connections cost no steps', async () => {
    const probe = fresh();
    const h = start(kind, probe);
    const conns = await Promise.all([1, 2, 3].map(() => peer(kind)));
    for (const c of conns) c.write('WATCH k\n');
    await until(() => conns.every((c) => c.got() === 'ok\n'), 10_000, 'all watching');
    await sleep(100);
    const calls = probe.calls;
    await sleep(1500);
    h.wake('other');
    await sleep(300);
    expect(probe.calls).toBe(calls);
    for (const c of conns) c.end();
  });
});
