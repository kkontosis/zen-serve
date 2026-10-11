// Sagas against a spawned zen-serve (zendb.md §12.5): an orchestrator, three
// participants with compensations, failures, timeouts and stuck sagas.
import { bytes, type Topic, type UnlockedFs, zw } from '@zen/client';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { type World, world } from '../../client/test/helpers.js';
import { Db, DbError, decodeMsg, ReplyError, type Saga, type Value } from '../src/index.js';

let w: World;
let fs: UnlockedFs;
let n = 0;
const ns = () => `sg${++n}`;
const quiet = { warn: () => {} };
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

beforeAll(async () => {
  w = await world({ limits: { claim_ttl_ms: 1000 } });
  fs = await w.session.fs(1).init((k) => [zw.createDeviceSlot(k, w.admin.device.devicePublic)]);
});
afterAll(() => w?.server.stop());

async function until(cond: () => boolean | Promise<boolean>, ms = 15_000): Promise<void> {
  const end = Date.now() + ms;
  while (!(await cond())) {
    if (Date.now() > end) throw new Error('timed out waiting');
    await sleep(25);
  }
}

async function events(t: Topic) {
  const out = [];
  for await (const e of t.read()) out.push(decodeMsg(e.payload));
  return out;
}

/**
 * Saga data. `fail`: the step whose participant replies with an error;
 * `slow`: the step whose participant replies after 800 ms.
 */
type Order = Record<string, Value>;

/**
 * An orchestrator `o` and participants on `p` (one database): reserve
 * (release), charge (refund), ship (no compensation). Every participant
 * call is counted by `"<type>:<saga hex>"`.
 */
async function setup(opts: { shipTimeoutMs?: number; refundFails?: boolean } = {}) {
  const name = ns();
  const open = (extra = {}) =>
    Db.open(fs, name, {
      ...quiet,
      locks: null,
      schedulerOptions: { pollMs: 50, ttlMs: 1000 },
      ...extra,
    });
  const o = await open();
  const p = await open({ scheduler: false });
  const t = (s: string) => o.topic('svc', name, s);
  const calls = new Map<string, number>();
  const count = (type: string, saga: Uint8Array | undefined) => {
    const k = `${type}:${bytes.hex(saga!)}`;
    calls.set(k, (calls.get(k) ?? 0) + 1);
  };
  const reply: Record<string, Value> = {
    reserve: { reserved: true },
    charge: { charged: 10 },
    ship: { shipped: true },
  };
  for (const s of ['reserve', 'charge', 'ship', 'release', 'refund']) {
    p.serve<Order>(
      {
        group: 'svc',
        topic: t(s),
        mode: 'per_key',
        maxInflight: 4,
        retry: 'delay',
        backoff: () => 50,
        maxAttempts: 2,
      },
      async (m) => {
        count(m.type, m.saga);
        if (s === 'refund' && opts.refundFails) throw new Error('the bank is down');
        if (m.body.fail === s) throw new ReplyError('declined', { step: s });
        if (m.body.slow === s) await sleep(800);
        return reply[s] ?? null;
      },
    );
  }
  const saga = o.saga<Order>('order', [
    { name: 'reserve', command: t('reserve'), compensate: t('release') },
    { name: 'charge', command: t('charge'), compensate: t('refund') },
    { name: 'ship', command: t('ship'), timeoutMs: opts.shipTimeoutMs ?? 10_000 },
  ]);
  const state = async (id: Uint8Array) => (await saga.get(id))?.status;
  const ran = (type: string, id: Uint8Array) => calls.get(`order.${type}:${bytes.hex(id)}`) ?? 0;
  const close = () => {
    o.close();
    p.close();
  };
  return { o, p, t, saga: saga as Saga<Order>, state, ran, close };
}

describe('sagas (§12.5)', () => {
  it('the happy path: every step once, the replies merged into data', async () => {
    const { saga, state, ran, close } = await setup();
    const id = await saga.start({ order: 'o1' });
    await until(async () => (await state(id)) === 'done');
    const s = (await saga.get(id))!;
    expect(s.data).toEqual({ order: 'o1', reserved: true, charged: 10, shipped: true });
    expect(s.done).toEqual([0, 1, 2]);
    expect(s.corr).toBeUndefined();
    expect(s.timeout).toBeUndefined();
    for (const x of ['reserve', 'charge', 'ship']) expect(ran(x, id)).toBe(1);
    expect(ran('reserve.compensate', id)).toBe(0);
    close();
  });

  it('an error reply: the completed steps are compensated, then failed', async () => {
    const { saga, state, ran, close } = await setup();
    const id = await saga.start({ fail: 'charge' });
    await until(async () => (await state(id)) === 'failed');
    const s = (await saga.get(id))!;
    expect(s.data.error).toEqual({ message: 'declined', data: { step: 'charge' } });
    expect(ran('reserve.compensate', id)).toBe(1);
    expect(ran('charge.compensate', id)).toBe(0);
    expect(ran('ship', id)).toBe(0);
    // No timeout is left behind.
    expect((await saga.get(id))!.timeout).toBeUndefined();
    close();
  });

  it('a timeout compensates; the late reply is stale', async () => {
    const { o, saga, state, ran, close } = await setup({ shipTimeoutMs: 300 });
    const id = await saga.start({ slow: 'ship' });
    await until(async () => (await state(id)) === 'failed');
    const s = (await saga.get(id))!;
    expect(s.data.error).toEqual({ message: 'timeout', step: 'ship' });
    expect(ran('charge.compensate', id)).toBe(1);
    expect(ran('reserve.compensate', id)).toBe(1);
    // The late reply arrives and is acknowledged without a transition.
    await until(async () => (await events(saga.topic)).some((m) => m.type === 'order.ship.reply'));
    await sleep(500);
    expect(await saga.get(id)).toEqual(s);
    expect(ran('charge.compensate', id)).toBe(1);
    expect(await o.table('$sched').query().page()).toMatchObject({ rows: [] });
    close();
  });

  it('start in a transaction that throws: no row and no command', async () => {
    const { o, t, saga, close } = await setup();
    let id: Uint8Array | undefined;
    await expect(
      o.transaction(async (tx) => {
        id = await saga.start({}, tx);
        throw new Error('no');
      }),
    ).rejects.toThrow('no');
    await sleep(200);
    expect(await saga.get(id!)).toBeUndefined();
    expect(await events(t('reserve'))).toEqual([]);
    close();
  });

  it('a compensation in the DLQ leaves the saga stuck', async () => {
    const { t, saga, state, ran, close } = await setup({ refundFails: true });
    const id = await saga.start({ fail: 'ship' });
    await until(async () => (await state(id)) === 'stuck');
    expect(ran('charge.compensate', id)).toBe(2);
    // It stopped there: the older step was not compensated.
    expect(ran('reserve.compensate', id)).toBe(0);
    expect((await events(t('refund').child('zen.dlq'))).length).toBe(1);
    close();
  });

  it('instances run in parallel', async () => {
    const { saga, state, close } = await setup();
    const ids = await Promise.all([saga.start({ k: 1 }), saga.start({ k: 2 })]);
    await until(async () => (await Promise.all(ids.map(state))).every((s) => s === 'done'));
    expect((await saga.get(ids[1]!))!.data.k).toBe(2);
    close();
  });

  it('refuses a saga without steps or with duplicate names', async () => {
    const db = await Db.open(fs, ns(), { ...quiet, locks: null, scheduler: false });
    const t = db.topic('x');
    expect(() => db.saga('s', [])).toThrow(DbError);
    expect(() =>
      db.saga('s', [
        { name: 'a', command: t },
        { name: 'a', command: t },
      ]),
    ).toThrow(DbError);
    db.close();
  });
});
