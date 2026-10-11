// The credential vault (spec/zendb.md §9.3): what signs an installation in
// and unlocks its filesystems, kept on a sealed local store so a worker or a
// restarted process needs no prompt.
//
//   store vault/<name>
//   device  = 1 ‖ lp(identity public) ‖ device seed(32) ‖ lp(device cert)
//   session = 1 ‖ lp(origin) ‖ lp(token) ‖ u64 expires_unix (0: unknown)
import type { SigningIdentity } from '@zen/wasm';
import { concat, fromUtf8, utf8 } from '../bytes.js';
import type { Client, DeviceCredentials } from '../client.js';
import { isCode, ZenError } from '../errors.js';
import type { Fs, UnlockedFs } from '../fs.js';
import type { Session } from '../session.js';
import { zw } from '../wasm.js';
import type { SealedStore } from './sealed.js';

const VERSION = 1;
const DEVICE = 1;
const FS = 2;
const SEED = 32;
/** A stored session this close to its expiry is not resumed. */
const MARGIN_SECS = 60;
const DEVICE_KEY = utf8('device');
const SESSION_KEY = utf8('session');

/** Options of `credentialVault`. */
export interface VaultOptions {
  /** Several identities can share one store under different names (default `default`). */
  name?: string;
  /** Accept a plaintext store (`sealedStore(…, {plaintext: true})`). Default: refused. */
  allowPlaintext?: boolean;
}

/** A device as the vault keeps it: the secret as its seed. */
export interface StoredDevice {
  /** The user's identity, or its public bytes (only those are kept). */
  identity: SigningIdentity | Uint8Array;
  /** The 32-byte seed of the device's `DeviceSecret`. */
  deviceSeed: Uint8Array;
  /** The device certificate the user issued. */
  cert: Uint8Array;
}

/** Options of `CredentialVault.signIn`. */
export interface VaultSignInOptions {
  /** Check a resumed session with one call (`/v1/grv`). Default true. */
  probe?: boolean;
}

/** Credentials kept on a sealed local store. */
export interface CredentialVault {
  readonly store: string;
  saveDevice(d: StoredDevice): Promise<void>;
  /** The stored device, with a fresh `DeviceSecret` (the caller frees it). */
  device(): Promise<DeviceCredentials | undefined>;
  /** Keep a session's token, bound to its client's origin. */
  saveSession(s: Session): Promise<void>;
  /**
   * A session: the stored one while it hasn't expired (and the server takes
   * it), else a device sign-in, which is stored. When the server can't be
   * reached, the stored session is returned, so local state works offline.
   */
  signIn(client: Client, opts?: VaultSignInOptions): Promise<Session>;
  /** Unlock `fs` with the stored device (offline too, from a kept header). */
  unlock(fs: Fs, opts?: { local?: SealedStore }): Promise<UnlockedFs>;
  /** Drop the stored session (after a logout). */
  forgetSession(): Promise<void>;
  /** Drop everything this vault holds. */
  forget(): Promise<void>;
}

function lp(b: Uint8Array): Uint8Array {
  const out = new Uint8Array(4 + b.length);
  new DataView(out.buffer).setUint32(0, b.length);
  out.set(b, 4);
  return out;
}

/** A reader over a record; throws on a short or malformed one. */
class Reader {
  private at = 0;
  private readonly view: DataView;
  constructor(private readonly b: Uint8Array) {
    this.view = new DataView(b.buffer, b.byteOffset, b.byteLength);
  }
  take(n: number): Uint8Array {
    if (this.at + n > this.b.length) throw new Error('short record');
    const out = this.b.slice(this.at, this.at + n);
    this.at += n;
    return out;
  }
  u8(): number {
    return this.take(1)[0]!;
  }
  u64(): bigint {
    const v = this.view.getBigUint64(this.at);
    this.take(8);
    return v;
  }
  lp(): Uint8Array {
    const n = this.view.getUint32(this.at);
    this.take(4);
    return this.take(n);
  }
  end(): void {
    if (this.at !== this.b.length) throw new Error('trailing bytes');
  }
}

/** Parse a record, or absent when it doesn't parse (§9.3). */
function parse<T>(b: Uint8Array | undefined, f: (r: Reader) => T): T | undefined {
  if (!b) return undefined;
  try {
    const r = new Reader(b);
    if (r.u8() !== VERSION) return undefined;
    const v = f(r);
    r.end();
    return v;
  } catch {
    return undefined;
  }
}

interface StoredSession {
  origin: string;
  token: string;
  expires: bigint;
}

/** A vault on `sealed` (§9.3). */
export function credentialVault(sealed: SealedStore, opts: VaultOptions = {}): CredentialVault {
  if (sealed.layers & FS) {
    throw new TypeError('the vault cannot have the fs layer: it holds what unlocks the fs');
  }
  if (!(sealed.layers & DEVICE) && !opts.allowPlaintext) {
    throw new TypeError('a vault without the device layer needs allowPlaintext: true');
  }
  const store = `vault/${opts.name ?? 'default'}`;

  async function stored(): Promise<StoredSession | undefined> {
    return parse(await sealed.get(store, SESSION_KEY), (r) => ({
      origin: fromUtf8(r.lp()),
      token: fromUtf8(r.lp()),
      expires: r.u64(),
    }));
  }

  const vault: CredentialVault = {
    store,

    async saveDevice(d) {
      if (d.deviceSeed.length !== SEED) throw new TypeError('a device seed is 32 bytes');
      const identity = d.identity instanceof Uint8Array ? d.identity : d.identity.publicIdentity;
      await sealed.put(
        store,
        DEVICE_KEY,
        concat(new Uint8Array([VERSION]), lp(identity), d.deviceSeed, lp(d.cert)),
      );
    },

    async device() {
      const d = parse(await sealed.get(store, DEVICE_KEY), (r) => ({
        identity: r.lp(),
        seed: r.take(SEED),
        cert: r.lp(),
      }));
      if (!d) return undefined;
      return { identity: d.identity, device: zw.DeviceSecret.fromSeed(d.seed), cert: d.cert };
    },

    async saveSession(s) {
      const exp = new Uint8Array(8);
      new DataView(exp.buffer).setBigUint64(0, s.info?.expires_unix ?? 0n);
      await sealed.put(
        store,
        SESSION_KEY,
        concat(new Uint8Array([VERSION]), lp(utf8(s.client.origin)), lp(utf8(s.token)), exp),
      );
    },

    async signIn(client, o = {}) {
      const s = await stored();
      const mine = s && s.origin === client.origin ? s : undefined;
      const now = BigInt(Math.floor(Date.now() / 1000));
      const fresh = mine && (mine.expires === 0n || mine.expires > now + BigInt(MARGIN_SECS));
      if (mine && fresh) {
        const resumed = client.resume(mine.token);
        if (o.probe === false) return resumed;
        try {
          await resumed.readVersion();
          return resumed;
        } catch (e) {
          if (isCode(e, 'network')) return resumed;
          if (!isCode(e, 'unauthorized')) throw e;
        }
      }
      const d = await vault.device();
      if (!d) {
        if (mine) await vault.forgetSession();
        throw new ZenError(0, 'not_found', 'nothing in the vault signs in');
      }
      try {
        const session = await client.signInDevice(d);
        await vault.saveSession(session);
        return session;
      } catch (e) {
        // Offline: the stored session still opens local state.
        if (mine && isCode(e, 'network')) return client.resume(mine.token);
        throw e;
      } finally {
        d.device.free();
      }
    },

    async unlock(fs, o = {}) {
      const d = await vault.device();
      if (!d) throw new ZenError(0, 'not_found', 'no device in the vault');
      try {
        return await fs.unlock({ device: d.device }, o);
      } finally {
        d.device.free();
      }
    },

    async forgetSession() {
      await sealed.delete(store, [SESSION_KEY]);
    },

    async forget() {
      await sealed.clear(store);
    },
  };
  return vault;
}
