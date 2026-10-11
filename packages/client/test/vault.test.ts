// The credential vault (zendb.md §9.3): sign in and unlock with no prompt.
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { IDBFactory, IDBKeyRange } from 'fake-indexeddb';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import {
  bytes,
  type Client,
  connect,
  credentialVault,
  type LocalStore,
  memoryLocalStore,
  type Session,
  type StoredDevice,
  sealedStore,
  ZenError,
  zw,
} from '../src/index.js';
import { indexedDbLocalStore } from '../src/local/idb.js';
import { nodeDiskLocalStore } from '../src/local/node.js';
import { type World, world } from './helpers.js';

let w: World;
let dir: string;
let stored: StoredDevice;
beforeAll(async () => {
  w = await world();
  dir = await mkdtemp(join(tmpdir(), 'zen-vault-'));
  const ufs = await w.session
    .fs(1)
    .init((k) => [zw.createDeviceSlot(k, w.admin.device.devicePublic)]);
  await ufs.kv.set(['probe'], bytes.utf8('hello'));
  ufs.close();
  // testUser(1): the device seed is 32 × (1 ^ 0x80).
  stored = {
    identity: w.admin.identity,
    deviceSeed: new Uint8Array(32).fill(0x81),
    cert: w.admin.cert,
  };
});
afterAll(async () => {
  w?.server.stop();
  if (dir) await rm(dir, { recursive: true, force: true });
});

/** A fresh client of the test server ("another process"). */
const fresh = () => connect(w.server.url, { origin: w.client.origin });

/** Record the paths a client calls; with `down`, every call fails with `network`. */
function watch(c: Client): { paths: string[]; down: (on: boolean) => void } {
  const orig = c.transport.call.bind(c.transport);
  const paths: string[] = [];
  let off = false;
  c.transport.call = ((path: string, ...rest: unknown[]) => {
    paths.push(path);
    if (off) return Promise.reject(new ZenError(0, 'network', 'offline (test)'));
    return (orig as (...a: unknown[]) => unknown)(path, ...rest);
  }) as typeof c.transport.call;
  return { paths, down: (on) => (off = on) };
}

let n = 0;
const backends: [string, () => LocalStore][] = [
  ['memory', () => memoryLocalStore()],
  ['node disk', () => nodeDiskLocalStore(join(dir, `d${n++}`))],
  [
    'IndexedDB',
    () => indexedDbLocalStore(`v${n++}`, { factory: new IDBFactory(), keyRange: IDBKeyRange }),
  ],
];

describe.each(backends)('%s backend', (_, make) => {
  it('keeps a device and a session, per name, and forgets them', async () => {
    const raw = make();
    const v = credentialVault(sealedStore(raw));
    const other = credentialVault(sealedStore(raw), { name: 'other' });
    expect(await v.device()).toBeUndefined();
    await v.saveDevice(stored);
    const d = (await v.device())!;
    expect(bytes.hex(d.device.fingerprint)).toBe(bytes.hex(w.admin.device.fingerprint));
    expect(bytes.hex(d.identity as Uint8Array)).toBe(bytes.hex(w.admin.identity.publicIdentity));
    expect(bytes.hex(d.cert)).toBe(bytes.hex(w.admin.cert));
    d.device.free();
    expect(await other.device()).toBeUndefined();

    // A new process on the same store signs in from the vault.
    const c = await fresh();
    const s = await v.signIn(c);
    const u = await v.unlock(s.fs(1));
    expect(bytes.fromUtf8((await u.kv.get(['probe']))!)).toBe('hello');
    u.close();

    await v.forget();
    expect(await v.device()).toBeUndefined();
    await expect(v.signIn(await fresh())).rejects.toMatchObject({ code: 'not_found' });
  });
});

describe('signing in (§9.3)', () => {
  it('resumes a stored session with one call, and signs in by device when there is none', async () => {
    const v = credentialVault(sealedStore(memoryLocalStore()));
    await v.saveDevice(stored);
    const c1 = await fresh();
    const t1 = watch(c1);
    const s1 = await v.signIn(c1);
    expect(t1.paths).toEqual(['/v1/auth/challenge', '/v1/auth/session']);
    expect(s1.info?.expires_unix).toBeGreaterThan(0n);

    const c2 = await fresh();
    const t2 = watch(c2);
    const s2 = await v.signIn(c2);
    expect(s2.token).toBe(s1.token);
    expect(t2.paths).toEqual(['/v1/grv']);
    // Without the probe: no call at all.
    const c3 = await fresh();
    const t3 = watch(c3);
    expect((await v.signIn(c3, { probe: false })).token).toBe(s1.token);
    expect(t3.paths).toEqual([]);
  });

  it('offline: the stored session, and unlocking from the kept header', async () => {
    const raw = memoryLocalStore();
    const v = credentialVault(sealedStore(raw));
    const local = sealedStore(raw);
    await v.saveDevice(stored);
    const s1 = await v.signIn(await fresh());
    (await v.unlock(s1.fs(1), { local })).close();

    const c = await fresh();
    const t = watch(c);
    t.down(true);
    const s = await v.signIn(c);
    expect(s.token).toBe(s1.token);
    const u = await v.unlock(s.fs(1), { local });
    expect(u.keys.fsId).toBe(1);
    u.close();
    expect(t.paths).toEqual(['/v1/grv', '/v1/fs/header/get']);
  });

  it('offline with an expired session: the stored one; with none, the network error', async () => {
    const v = credentialVault(sealedStore(memoryLocalStore()));
    await v.saveDevice(stored);
    const c = await fresh();
    const t = watch(c);
    t.down(true);
    await expect(v.signIn(c)).rejects.toMatchObject({ code: 'network' });
    await v.saveSession(fakeSession(c, 'old-token', 1n));
    expect((await v.signIn(c)).token).toBe('old-token');
  });

  it('an expiring session falls back to the device', async () => {
    const v = credentialVault(sealedStore(memoryLocalStore()));
    await v.saveDevice(stored);
    const c = await fresh();
    const soon = BigInt(Math.floor(Date.now() / 1000) + 30);
    await v.saveSession(fakeSession(c, 'expiring', soon));
    const t = watch(c);
    const s = await v.signIn(c);
    expect(s.token).not.toBe('expiring');
    expect(t.paths).toEqual(['/v1/auth/challenge', '/v1/auth/session']);
    // The new session was stored.
    expect((await v.signIn(await fresh(), { probe: false })).token).toBe(s.token);
  });

  it('a session the server refuses (after logout) falls back to the device', async () => {
    const v = credentialVault(sealedStore(memoryLocalStore()));
    await v.saveDevice(stored);
    const s1 = await v.signIn(await fresh());
    await s1.logout();
    const c = await fresh();
    const t = watch(c);
    const s2 = await v.signIn(c);
    expect(s2.token).not.toBe(s1.token);
    expect(t.paths).toEqual(['/v1/grv', '/v1/auth/challenge', '/v1/auth/session']);
    await s2.readVersion();
  });

  it('a refused session and no device: not_found, and the session is dropped', async () => {
    const v = credentialVault(sealedStore(memoryLocalStore()));
    const c = await fresh();
    await v.saveSession(fakeSession(c, 'unknown', 0n));
    await expect(v.signIn(c)).rejects.toMatchObject({ code: 'not_found' });
    const t = watch(c);
    await expect(v.signIn(c)).rejects.toMatchObject({ code: 'not_found' });
    expect(t.paths).toEqual([]);
  });

  it('never sends a token to another origin', async () => {
    const v = credentialVault(sealedStore(memoryLocalStore()));
    await v.saveDevice(stored);
    const elsewhere = await connect(w.server.url, { origin: 'https://elsewhere.example' });
    await v.saveSession(fakeSession(elsewhere, 'theirs', 0n));
    const c = await fresh();
    const t = watch(c);
    const s = await v.signIn(c);
    expect(s.token).not.toBe('theirs');
    expect(t.paths).toEqual(['/v1/auth/challenge', '/v1/auth/session']);
  });
});

describe('refusals and tampering', () => {
  it('refuses the fs layer, and plaintext unless allowed', async () => {
    const ufs = await w.session.fs(1).unlock({ device: w.admin.device });
    expect(() => credentialVault(sealedStore(memoryLocalStore(), { fs: ufs }))).toThrow(TypeError);
    ufs.close();
    const clear = sealedStore(memoryLocalStore(), { device: false, plaintext: true });
    expect(() => credentialVault(clear)).toThrow(TypeError);
    const v = credentialVault(clear, { allowPlaintext: true });
    await v.saveDevice(stored);
    expect(await v.device()).toBeDefined();
    expect(() => credentialVault(sealedStore(memoryLocalStore()), { name: '' })).not.toThrow();
  });

  it('a seed of the wrong length is refused', async () => {
    const v = credentialVault(sealedStore(memoryLocalStore()));
    await expect(v.saveDevice({ ...stored, deviceSeed: new Uint8Array(31) })).rejects.toThrow(
      TypeError,
    );
  });

  it('a tampered record reads as absent', async () => {
    const raw = memoryLocalStore();
    const v = credentialVault(sealedStore(raw, { warn: () => {} }));
    await v.saveDevice(stored);
    const key = bytes.utf8('device');
    const rec = (await raw.get(v.store, key))!;
    rec[rec.length - 1]! ^= 1;
    await raw.put(v.store, key, rec);
    expect(await v.device()).toBeUndefined();
    await expect(v.signIn(await fresh())).rejects.toMatchObject({ code: 'not_found' });
  });

  it('a record that opens but does not parse reads as absent', async () => {
    const sealed = sealedStore(memoryLocalStore());
    const v = credentialVault(sealed);
    await sealed.put(v.store, bytes.utf8('device'), new Uint8Array([1, 0, 0]));
    expect(await v.device()).toBeUndefined();
    await sealed.put(v.store, bytes.utf8('device'), new Uint8Array([2]));
    expect(await v.device()).toBeUndefined();
  });
});

/** A session object as `saveSession` sees it (token, origin, expiry). */
function fakeSession(client: Client, token: string, expires: bigint): Session {
  return { client, token, info: { expires_unix: expires } } as unknown as Session;
}
