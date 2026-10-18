// Relayed services from the client (api.md §14): the requests, a step in a
// transaction, and pushed input over the stream (§14.10).
import { type Socket, connect as tcp } from 'node:net';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { utf8 } from '../src/bytes.js';
import {
  isCode,
  REVEALED,
  type Service,
  type Stream,
  type SvcPush,
  type UnlockedFs,
  type ZenError,
  zw,
} from '../src/index.js';
import { freePort, serviceToml } from '../src/testing/index.js';
import { type World, world } from './helpers.js';

let w: World;
let ufs: UnlockedFs;
let svc: Service;
let port: number;
let stream: Stream;

beforeAll(async () => {
  w = await world();
  ufs = await w.session.fs(1).init((k) => [zw.createDeviceSlot(k, w.admin.device.devicePublic)]);
  svc = ufs.service('svc', 'echo');
  // The service id is known only now: restart with the service configured.
  port = await freePort();
  w.server = await w.server.restart({
    extra: serviceToml([{ name: 'echo', fs: 1, id: svc.id, port }]),
  });
  stream = await w.session.stream();
});
afterAll(async () => {
  stream?.close();
  await w?.server.stop();
});

/** A TCP peer that collects what it receives. */
async function peer(): Promise<{ s: Socket; got: () => string; ended: Promise<void> }> {
  const s = tcp(port, '127.0.0.1');
  await new Promise<void>((ok, fail) => s.once('connect', ok).once('error', fail));
  let buf = '';
  s.on('data', (d) => {
    buf += d.toString();
  });
  const ended = new Promise<void>((ok) => s.once('end', ok));
  return { s, got: () => buf, ended };
}

async function until(cond: () => boolean, what: string, ms = 5000): Promise<void> {
  const end = Date.now() + ms;
  while (!cond()) {
    if (Date.now() > end) throw new Error(`timed out waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 10));
  }
}

describe('svc', () => {
  it('info', async () => {
    const i = await svc.info();
    expect(i.mode).toBe('tcp');
    expect(i.resume).toBe(true);
  });

  it('pushes ready and input, again after a reconnect, then a step and a reveal answer the peer', async () => {
    const lease = zw.randomBytes(16);
    await svc.lease(lease);
    const pushes: SvcPush[] = [];
    const sub = await stream.subscribeSvc(ufs, svc.id, lease, { onEvent: (p) => pushes.push(p) });
    const p = await peer();
    p.s.write('hi\n');
    await until(() => pushes.some((x) => x.type === 'ready'), 'svc_ready');
    const [c] = await svc.next(lease, { limit: 4 });
    expect(c).toBeDefined();
    const inputs = () =>
      pushes.filter((x): x is Extract<SvcPush, { type: 'input' }> => x.type === 'input');
    await until(() => inputs().length > 0, 'svc_in');
    expect(inputs()[0]!.off).toBe(0n);
    expect(new TextDecoder().decode(inputs()[0]!.data)).toBe('hi\n');

    // A reconnect resubscribes: the input is pushed again from in_acked.
    const before = inputs().length;
    stream.reconnect();
    await until(() => inputs().length > before, 'svc_in after the reconnect');
    expect(inputs().at(-1)!.off).toBe(0n);

    // A transaction holding only the step commits.
    const k = svc.sealKeys();
    const { record, key } = k.sealRecord(c!.conn, 0n, 0, utf8('HI\n'));
    await ufs.transaction(async (tx) => {
      tx.extra.svc = [
        {
          fs: 1,
          svc: svc.id,
          conn: c!.conn,
          token: c!.token,
          in_from: 0n,
          in_to: 3n,
          out_from: 0n,
          out: [record],
          state: k.sealState(c!.conn, 3n, 3n, utf8('s')),
          resumable: true,
        },
      ];
    });
    expect(await svc.reveal([{ conn: c!.conn, off: 0n, key }])).toEqual([REVEALED]);
    await until(() => p.got() === 'HI\n', 'the reply');
    const st = await svc.state(c!.conn, c!.token);
    expect(st.in_acked).toBe(3n);
    expect(new TextDecoder().decode(k.openState(c!.conn, 3n, 3n, st.state!))).toBe('s');

    // Releasing the lease ends the subscription with not_leader.
    let ended: ZenError | undefined;
    await sub.close();
    const sub2 = await stream.subscribeSvc(ufs, svc.id, lease, {
      onEvent: () => {},
      onError: (e) => {
        ended = e;
      },
    });
    await svc.releaseLease(lease);
    await until(() => ended !== undefined, 'the end of the subscription');
    expect(isCode(ended, 'not_leader')).toBe(true);
    await sub2.close();
    p.s.destroy();
  });

  it('a subscription without a live lease is refused', async () => {
    await expect(stream.subscribeSvc(ufs, svc.id, zw.randomBytes(16))).rejects.toSatisfy((e) =>
      isCode(e, 'not_leader'),
    );
  });
});
