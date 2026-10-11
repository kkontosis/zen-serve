// Unlocking offline from the header kept on the local store (zendb.md §9.3).
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import {
  bytes,
  memoryLocalStore,
  type Session,
  sealedStore,
  type UnlockedFs,
  ZenError,
  zw,
} from '../src/index.js';
import { type World, world } from './helpers.js';

let w: World;
let ufs: UnlockedFs;
beforeAll(async () => {
  w = await world();
  ufs = await w.session.fs(1).init((k) => [zw.createDeviceSlot(k, w.admin.device.devicePublic)]);
  await ufs.kv.set(['probe'], bytes.utf8('hello'));
});
afterAll(() => w?.server.stop());

/** Take a session offline: every call fails with `network`; returns the paths tried. */
function offline(s: Session): { calls: string[]; restore: () => void } {
  const orig = s.call.bind(s);
  const calls: string[] = [];
  s.call = (async (path: string) => {
    calls.push(path);
    throw new ZenError(0, 'network', 'offline (test)');
  }) as Session['call'];
  return { calls, restore: () => (s.call = orig) };
}

describe('unlock offline (§9.3)', () => {
  it('unlocks from the kept header, with no call but the failed one', async () => {
    const local = sealedStore(memoryLocalStore());
    const how = { device: w.admin.device };
    const online = await w.session.fs(1).unlock(how, { local });
    online.close();
    const net = offline(w.session);
    try {
      const u = await w.session.fs(1).unlock(how, { local });
      expect(u.keys.fsId).toBe(1);
      expect(bytes.hex(u.keys.db('x').prefix)).toBe(bytes.hex(ufs.keys.db('x').prefix));
      expect(net.calls).toEqual(['/v1/fs/header/get']);
      u.close();
    } finally {
      net.restore();
    }
  });

  it('nothing kept: the network error stands', async () => {
    const local = sealedStore(memoryLocalStore());
    const net = offline(w.session);
    try {
      await expect(
        w.session.fs(1).unlock({ device: w.admin.device }, { local }),
      ).rejects.toMatchObject({ code: 'network' });
    } finally {
      net.restore();
    }
  });

  it('refuses a store with the fs layer', async () => {
    const local = sealedStore(memoryLocalStore(), { fs: ufs });
    await expect(w.session.fs(1).unlock({ device: w.admin.device }, { local })).rejects.toThrow(
      /fs layer/,
    );
  });

  it('forget removes the kept header', async () => {
    const local = sealedStore(memoryLocalStore());
    (await w.session.fs(1).unlock({ device: w.admin.device }, { local })).close();
    await w.session.fs(1).forget(local);
    const net = offline(w.session);
    try {
      await expect(
        w.session.fs(1).unlock({ device: w.admin.device }, { local }),
      ).rejects.toMatchObject({ code: 'network' });
    } finally {
      net.restore();
    }
  });

  it('a header changed by editHeader is kept as changed', async () => {
    const local = sealedStore(memoryLocalStore());
    const u = await w.session.fs(1).unlock({ device: w.admin.device }, { local });
    await u.addPassphrase('kept', { m_cost_kib: 65536, t_cost: 1, p_cost: 1 });
    const net = offline(w.session);
    try {
      (await w.session.fs(1).unlock({ passphrase: 'kept' }, { local })).close();
    } finally {
      net.restore();
      u.close();
    }
  });
});
