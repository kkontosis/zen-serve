import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { utf8 } from '../src/bytes.js';
import {
  type EphemeralMessage,
  fullGrants,
  isCode,
  member,
  type Session,
  Stream,
  type StreamEvent,
  type UnlockedFs,
  type ZenError,
  zw,
} from '../src/index.js';
import { testUser } from '../src/testing/index.js';
import { type World, world } from './helpers.js';

let w: World;
let ufs: UnlockedFs;
let bob: Session;
let bobFs: UnlockedFs;

const text = (b: Uint8Array) => new TextDecoder().decode(b);

/** The next `n` items of an iterator, failing after `ms`. */
async function take<T>(it: AsyncIterator<T>, n: number, ms = 5000): Promise<T[]> {
  const out: T[] = [];
  const deadline = Date.now() + ms;
  while (out.length < n) {
    const left = deadline - Date.now();
    const r = await Promise.race([
      it.next(),
      new Promise<never>((_, fail) =>
        setTimeout(() => fail(new Error(`timed out after ${out.length}/${n}`)), left),
      ),
    ]);
    if (r.done) throw new Error('ended early');
    out.push(r.value);
  }
  return out;
}

const payloads = (evs: StreamEvent[]) => evs.map((e) => (e.opened ? text(e.payload) : '?'));

beforeAll(async () => {
  w = await world();
  let recoveryKey!: Uint8Array;
  ufs = await w.session.fs(1).init((k) => {
    const r = zw.createRecoverySlot(k);
    recoveryKey = r.recoveryKey;
    return [r.slot];
  });
  const b = testUser(2);
  await w.session.acl.update(w.admin.identity, (doc) => {
    doc.members.push(member(b.identity, [b.cert]));
    doc.grants.push(...fullGrants(1, b.fp));
  });
  bob = await w.client.signInDevice(b);
  bobFs = await bob.fs(1).unlock({ recoveryKey });
});
afterAll(() => w?.server.stop());

describe('stream', () => {
  it('a subscription receives appends in order', async () => {
    const t = ufs.topic('live');
    await t.append(utf8('before'));
    const s = await w.session.stream();
    const sub = await s.subscribe(t);
    const it = sub[Symbol.asyncIterator]();
    for (const p of ['a', 'b', 'c']) await t.append(utf8(p), { key: p });
    const evs = await take(it, 3);
    expect(payloads(evs)).toEqual(['a', 'b', 'c']);
    const e = evs[0]!;
    expect(e.opened && e.sender).toEqual(w.session.deviceFp);
    expect(e.keyToken).toEqual(t.keyToken('a'));
    await sub.close();
    expect((await it.next()).done).toBe(true);
    s.close();
  });

  it('replays history after an offset, then goes live', async () => {
    const t = ufs.topic('history');
    const first = await t.append(utf8('h1'));
    await t.append(utf8('h2'));
    const s = await Stream.open(w.session);
    const events: StreamEvent[] = [];
    await s.subscribe(t, { after: first, onEvent: (e) => events.push(e) });
    await t.append(utf8('h3'));
    const deadline = Date.now() + 5000;
    while (events.length < 2 && Date.now() < deadline) await new Promise((r) => setTimeout(r, 20));
    expect(payloads(events)).toEqual(['h2', 'h3']);
    s.close();
  });

  it('resumes from the last offset after a reconnect, losing nothing', async () => {
    const t = ufs.topic('resume');
    const s = await Stream.open(w.session, { backoffMs: [200, 1000] });
    const sub = await s.subscribe(t);
    const it = sub[Symbol.asyncIterator]();
    await t.append(utf8('0'));
    await t.append(utf8('1'));
    expect(payloads(await take(it, 2))).toEqual(['0', '1']);

    // Drop the socket and append while it is down.
    s.reconnect();
    expect(s.connected).toBe(false);
    for (let i = 2; i < 6; i++) await t.append(utf8(String(i)));
    const after = await take(it, 4);
    expect(payloads(after)).toEqual(['2', '3', '4', '5']);
    expect(s.connected).toBe(true);

    // And once more, appending before and after the reconnection.
    await t.append(utf8('6'));
    s.reconnect();
    await t.append(utf8('7'));
    const more = await take(it, 2);
    expect(payloads(more)).toEqual(['6', '7']);
    // Nothing was repeated.
    await t.append(utf8('8'));
    expect(payloads(await take(it, 1))).toEqual(['8']);
    s.close();
  });

  it('subscribes to a prefix, opening the topics it knows', async () => {
    const parent = ufs.topic('app');
    const known = parent.child('known');
    const unknown = parent.child('unknown');
    const s = await w.session.stream();
    const sub = await s.subscribe({ fs: ufs, prefix: parent, topics: [known] });
    const it = sub[Symbol.asyncIterator]();
    await known.append(utf8('k'));
    await unknown.append(utf8('u'));
    await parent.append(utf8('p'));
    const [k, u, p] = await take(it, 3);
    expect(k!.opened && text(k!.payload)).toBe('k');
    expect(k!.topic).toEqual(known.id);
    expect(u!.opened).toBe(false);
    expect(u!.topic).toEqual(unknown.id);
    expect(p!.opened && text(p!.payload)).toBe('p');
    s.close();
  });

  it('refuses a subscription without rights', async () => {
    const fs2 = await w.session.fs(2).init((k) => [zw.createRecoverySlot(k).slot]);
    const s = await bob.stream();
    await expect(s.subscribe(fs2.topic('secret'))).rejects.toSatisfy((e) => isCode(e, 'forbidden'));
    // The stream stays usable.
    const t = bobFs.topic('fine');
    const sub = await s.subscribe(t);
    await t.append(utf8('ok'));
    expect(payloads(await take(sub[Symbol.asyncIterator](), 1))).toEqual(['ok']);
    s.close();
  });

  it('ephemeral pub/sub between two sessions', async () => {
    const errors: ZenError[] = [];
    const sa = await Stream.open(w.session, { onError: (e) => errors.push(e) });
    const sb = await bob.stream();
    const t = ufs.topic('presence');
    const sub = await sb.subscribeEphemeral(bobFs.topic('presence'));
    const it = sub[Symbol.asyncIterator]();
    await sa.publish(t, utf8('hello'));
    await sa.publish(t, utf8('again'));
    const msgs: EphemeralMessage[] = await take(it, 2);
    expect(msgs.map((m) => text(m.data))).toEqual(['hello', 'again']);
    expect(msgs[0]!.sender).toEqual(w.session.deviceFp);
    expect(msgs[0]!.via).toEqual(w.session.deviceFp);
    expect(msgs[1]!.hlc > msgs[0]!.hlc).toBe(true);
    // Not in the log.
    const log = [];
    for await (const e of t.read()) log.push(e);
    expect(log).toEqual([]);
    expect(errors).toEqual([]);
    sa.close();
    sb.close();
  });

  it('a closed stream ends its subscriptions', async () => {
    const s = await w.session.stream();
    const sub = await s.subscribe(ufs.topic('closing'));
    const it = sub[Symbol.asyncIterator]();
    const pending = it.next();
    s.close();
    expect((await pending).done).toBe(true);
    await expect(s.subscribe(ufs.topic('closing'))).rejects.toSatisfy((e) => isCode(e, 'closed'));
  });
});
