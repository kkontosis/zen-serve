import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { bytes, fullGrants, member, type UnlockedFs, zw } from '../src/index.js';
import { SoftAuthenticator, testUser } from '../src/testing/index.js';
import { type World, world } from './helpers.js';

const FLOOR = { m_cost_kib: 65536, t_cost: 1, p_cost: 1 };

let w: World;
let ufs: UnlockedFs;
let recovery: Uint8Array;
beforeAll(async () => {
  w = await world({ auth: { opaque: true } });
  ufs = await w.session.fs(1).init((k) => {
    const r = zw.createRecoverySlot(k);
    recovery = r.recoveryKey;
    return [r.slot, zw.createDeviceSlot(k, w.admin.device.devicePublic)];
  });
});
afterAll(() => w?.server.stop());

describe('keyslots', () => {
  it('a second init is refused', async () => {
    await expect(w.session.fs(1).init(() => [])).rejects.toMatchObject({
      code: 'version_mismatch',
    });
  });

  it('opens with each slot type', async () => {
    await ufs.kv.set(['probe'], bytes.utf8('hello'));
    const check = async (u: UnlockedFs) => {
      expect(bytes.fromUtf8((await u.kv.get(['probe']))!)).toBe('hello');
      u.close();
    };
    await check(await w.session.fs(1).unlock({ recoveryKey: recovery }));
    await check(await w.session.fs(1).unlock({ device: w.admin.device }));

    await ufs.addPassphrase('open sesame', FLOOR);
    await check(await w.session.fs(1).unlock({ passphrase: 'open sesame' }));
    await expect(w.session.fs(1).unlock({ passphrase: 'wrong' })).rejects.toMatchObject({
      code: 'decrypt',
    });

    // OPAQUE: the sign-in's export key opens the slot with no second prompt.
    const reg = await w.session.registerOpaque('alice', 'pw for opaque');
    await ufs.addOpaque(reg.id, reg.exportKey);
    const so = await w.client.signInOpaque('alice', 'pw for opaque');
    await check(await so.fs(1).unlock());

    // Passkey PRF: one touch signs in and returns the PRF output.
    const auth = new SoftAuthenticator();
    const pk = await w.session.registerPasskey(auth, { prf: true });
    await ufs.addPasskey(pk.id, pk.prf!);
    const salts = new Map([[bytes.b64url(pk.rawId), pk.prf!.salt]]);
    const sp = await w.client.signInPasskey(auth, { user: w.admin.fp, prfSalts: salts });
    expect(sp.unlock.prf?.output).toEqual(pk.prf!.output);
    await check(await sp.fs(1).unlock());
  });

  it('removes a slot', async () => {
    const id = await ufs.addPassphrase('temporary', FLOOR);
    expect(await ufs.removeSlot(id)).toBe(true);
    await expect(w.session.fs(1).unlock({ passphrase: 'temporary' })).rejects.toMatchObject({
      code: 'decrypt',
    });
  });

  it('a concurrent header change is merged, not lost', async () => {
    const other = await w.session.fs(1).unlock({ device: w.admin.device });
    await Promise.all([ufs.addPassphrase('one', FLOOR), other.addPassphrase('two', FLOOR)]);
    const h = (await w.session.fs(1).header())!.header;
    const passphrases = h.slots().filter((s) => zw.slotInfo(s).slotType === 1);
    expect(passphrases.length).toBeGreaterThanOrEqual(3);
    other.close();
  });

  it('rotation: old data stays readable, stale slots only read', async () => {
    const bob = testUser(2);
    await w.session.acl.update(w.admin.identity, (doc) => {
      doc.members.push(member(bob.identity, [bob.cert]));
      doc.grants.push(...fullGrants(1, bob.fp));
    });
    await ufs.addDevice(bob.device.devicePublic);
    // Revoke bob: rotate, keeping only the admin's device.
    const before = ufs.keys.epoch;
    const next = await ufs.rotate((k) => [zw.createDeviceSlot(k, w.admin.device.devicePublic)]);
    expect(next.keys.epoch).toBe(before + 1);
    await next.kv.set(['after'], bytes.utf8('new epoch'));
    const fresh = await w.session.fs(1).unlock({ device: w.admin.device });
    expect(bytes.fromUtf8((await fresh.kv.get(['probe']))!)).toBe('hello');
    expect(bytes.fromUtf8((await fresh.kv.get(['after']))!)).toBe('new epoch');
    expect(zw.peekSealed((await rawValue(fresh, ['after']))!).epoch).toBe(before + 1);
    // Bob's slot is gone.
    const sb = await w.client.signInDevice(bob);
    await expect(sb.fs(1).unlock({ device: bob.device })).rejects.toMatchObject({
      code: 'decrypt',
    });
    // The handle that rotated saw the new header: its keys are stale now, so
    // it still reads but refuses to seal new data.
    expect(ufs.writable).toBe(false);
    expect(bytes.fromUtf8((await ufs.kv.get(['probe']))!)).toBe('hello');
    await expect(ufs.kv.set(['x'], bytes.utf8('no'))).rejects.toMatchObject({ code: 'stale_slot' });
    fresh.close();
    next.close();
  });
});

async function rawValue(u: UnlockedFs, path: string[]): Promise<Uint8Array | undefined> {
  const r = await u.session.call('/v1/kv/get', zw.encodeKvGet, zw.decodeKvItems, {
    fs: u.id,
    keys: [u.kv.key(path)],
  });
  return r.items[0]?.value;
}
