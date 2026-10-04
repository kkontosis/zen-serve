import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { fullGrants, isCode, member, ZenError } from '../src/index.js';
import { SoftAuthenticator, testUser } from '../src/testing/index.js';
import { type World, world } from './helpers.js';

let w: World;
beforeAll(async () => {
  w = await world({ auth: { opaque: true, api_tokens: true } });
});
afterAll(() => w?.server.stop());

describe('sign-in methods', () => {
  it('reports the server', async () => {
    const info = await w.client.info();
    expect(info.claimed).toBe(true);
    expect(info.auth?.methods).toContain('device_key');
    expect(typeof info.time_ms).toBe('bigint');
  });

  it('1: device keys', async () => {
    expect(w.session.info?.method).toBe('device_key');
    expect(w.session.userFp).toEqual(w.admin.fp);
    expect((await w.session.fsList()).map((f) => f.id)).toEqual([1, 2]);
  });

  it('a device outside the ACL is refused', async () => {
    const stranger = testUser(9);
    await expect(w.client.signInDevice(stranger)).rejects.toMatchObject({ status: 401 });
  });

  it('6: password-derived key', async () => {
    await w.session.setPassword('alice', 'correct horse');
    const s = await w.client.signInPassword('alice', 'correct horse');
    expect(s.info?.method).toBe('password_key');
    expect(s.userFp).toEqual(w.admin.fp);
    await expect(w.client.signInPassword('alice', 'wrong')).rejects.toMatchObject({ status: 401 });
  });

  it('3: OPAQUE, with the export key kept for unlocking', async () => {
    const reg = await w.session.registerOpaque('alice-o', 'battery staple');
    const s = await w.client.signInOpaque('alice-o', 'battery staple');
    expect(s.info?.method).toBe('opaque');
    expect(s.deviceFp).toEqual(reg.id);
    expect(s.unlock.opaqueExportKey).toEqual(reg.exportKey);
    await expect(w.client.signInOpaque('alice-o', 'nope')).rejects.toMatchObject({ status: 401 });
  });

  it('2: passkeys, with PRF', async () => {
    const auth = new SoftAuthenticator();
    const reg = await w.session.registerPasskey(auth, { label: 'soft', prf: true });
    expect(reg.prf?.output).toHaveLength(32);
    const s = await w.client.signInPasskey(auth);
    expect(s.info?.method).toBe('passkey');
    expect(s.deviceFp).toEqual(reg.id);
    // With the user named, the allow list is the user's passkeys.
    const s2 = await w.client.signInPasskey(auth, { user: w.admin.fp });
    expect(s2.userFp).toEqual(w.admin.fp);
  });

  it('4: API tokens', async () => {
    const t = await w.session.createApiToken({ user: w.admin.fp, label: 'ci' });
    expect(t.token.startsWith('zen_at_')).toBe(true);
    const s = w.client.withApiToken(t.token);
    expect((await s.fsList()).length).toBe(2);
    await w.session.removeCredential(t.id);
    await expect(s.fsList()).rejects.toMatchObject({ status: 401 });
  });

  it('lists credentials and logs out', async () => {
    const creds = await w.session.credentials();
    const methods = creds.map((c) => c.method).sort();
    expect(methods).toEqual(['opaque', 'passkey', 'password_key']);
    const s = await w.client.signInDevice(w.admin);
    await s.logout();
    await expect(s.fsList()).rejects.toSatisfy((e) => isCode(e, 'unauthorized'));
  });

  it('extends the ACL chain and verifies it', async () => {
    const bob = testUser(2);
    const v = await w.session.acl.update(w.admin.identity, (doc) => {
      doc.members.push(member(bob.identity, [bob.cert]));
      doc.grants.push(...fullGrants(1, bob.fp));
    });
    expect(v.version).toBe(2n);
    const sb = await w.client.signInDevice(bob);
    expect((await sb.fsList()).map((f) => f.id)).toEqual([1]);
    // A non-admin can't sign the next version.
    await expect(
      sb.acl.update(bob.identity, (doc) => {
        doc.admins.push(bob.fp);
      }),
    ).rejects.toBeInstanceOf(ZenError);
  });
});
