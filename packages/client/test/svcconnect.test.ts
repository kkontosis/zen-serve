// Connecting to a `tcp-auth` service (api.md §14.9): `Session.svcConnect`
// against a handler driven through the svc requests.
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { utf8 } from '../src/bytes.js';
import {
  fullGrants,
  isCode,
  member,
  REVEALED,
  type Service,
  type Session,
  type SvcSocket,
  type UnlockedFs,
  zw,
} from '../src/index.js';
import { serviceToml, testUser } from '../src/testing/index.js';
import { type World, world } from './helpers.js';

let w: World;
let ufs: UnlockedFs;
let svc: Service;
let bob: Session;
const lease = new Uint8Array(16).fill(7);

beforeAll(async () => {
  w = await world();
  ufs = await w.session.fs(1).init((k) => [zw.createDeviceSlot(k, w.admin.device.devicePublic)]);
  svc = ufs.service('svc', 'echo');
  const plain = ufs.service('svc', 'plain');
  w.server = await w.server.restart({
    extra: serviceToml([
      { name: 'echo', fs: 1, id: svc.id, mode: 'tcp-auth' },
      { name: 'plain', fs: 1, id: plain.id, mode: 'tcp-auth', more: { max_conns: 1 } },
    ]),
  });
  // Bob may read and consume, not append.
  const b = testUser(2);
  await w.session.acl.update(w.admin.identity, (doc) => {
    doc.members.push(member(b.identity, [b.cert]));
    doc.grants.push(fullGrants(1, b.fp)[0]!, {
      fs: 1,
      topic: new Uint8Array(0),
      rights: ['read', 'consume'],
      subject: b.fp,
    });
  });
  bob = await w.client.signInDevice(b);
  await svc.lease(lease);
});
afterAll(() => w?.server.stop());

async function until(cond: () => boolean, what: string, ms = 10_000): Promise<void> {
  const end = Date.now() + ms;
  while (!cond()) {
    if (Date.now() > end) throw new Error(`timed out waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 10));
  }
}

const text = new TextDecoder();

describe('svcConnect', () => {
  it('a byte stream to the handler, fin both ways, then close 1000', async () => {
    let got = '';
    let fin = false;
    let closed: boolean | undefined;
    const s: SvcSocket = await w.session.svcConnect({
      fs: 1,
      svc: svc.id,
      onData: (d) => {
        got += text.decode(d);
      },
      onFin: () => {
        fin = true;
      },
      onClose: (reset) => {
        closed = reset;
      },
    });
    expect(s.conn.length).toBe(16);
    s.write(utf8('ping\n'));
    s.fin();
    const [c] = await svc.next(lease, { waitMs: 10_000 });
    expect(c!.conn).toEqual(s.conn);
    expect(c!.peer.user).toEqual(w.session.userFp);
    expect(c!.peer.device).toEqual(w.session.deviceFp);
    let r = await svc.read(c!.conn, c!.token, 0n, { waitMs: 5_000 });
    while ((r.flags & 1) === 0) r = await svc.read(c!.conn, c!.token, 0n, { waitMs: 5_000 });
    expect(text.decode(r.chunks[0]!.data)).toBe('ping\n');

    const k = svc.sealKeys();
    const { record, key } = k.sealRecord(c!.conn, 0n, 1 /* FIN */, utf8('pong\n'));
    await ufs.transaction(async (tx) => {
      tx.extra.svc = [
        {
          fs: 1,
          svc: svc.id,
          conn: c!.conn,
          token: c!.token,
          in_from: 0n,
          in_to: 5n,
          out_from: 0n,
          out: [record],
        },
      ];
    });
    expect(await svc.reveal([{ conn: c!.conn, off: 0n, key }])).toEqual([REVEALED]);
    await until(() => closed !== undefined, 'the close');
    expect(got).toBe('pong\n');
    expect(fin).toBe(true);
    expect(closed).toBe(false);
    expect(s.open).toBe(false);
  });

  it('a reset reaches the handler as conn_closed', async () => {
    const s = await w.session.svcConnect({ fs: 1, svc: svc.id });
    const [c] = await svc.next(lease, { waitMs: 10_000 });
    s.reset();
    let err: unknown;
    const end = Date.now() + 10_000;
    while (!err && Date.now() < end) {
      try {
        await svc.state(c!.conn, c!.token);
        await new Promise((r) => setTimeout(r, 50));
      } catch (e) {
        err = e;
      }
    }
    expect(isCode(err, 'conn_closed')).toBe(true);
  });

  it('refusals are ZenErrors', async () => {
    await expect(bob.svcConnect({ fs: 1, svc: svc.id })).rejects.toSatisfy(
      (e) => isCode(e, 'forbidden') && e.status === 403,
    );
    await expect(w.session.svcConnect({ fs: 1, svc: new Uint8Array(16) })).rejects.toSatisfy((e) =>
      isCode(e, 'not_found'),
    );
    // `plain` takes one connection at a time.
    const plain = ufs.service('svc', 'plain').id;
    const first = await w.session.svcConnect({ fs: 1, svc: plain });
    await expect(w.session.svcConnect({ fs: 1, svc: plain })).rejects.toSatisfy(
      (e) => isCode(e, 'quota') && e.status === 429,
    );
    first.reset();
    await bob.logout();
    await expect(bob.svcConnect({ fs: 1, svc: svc.id })).rejects.toSatisfy((e) =>
      isCode(e, 'unauthorized'),
    );
  });
});
