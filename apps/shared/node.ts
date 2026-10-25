// The Node side shared by the example apps: a worker (or a vitest scenario)
// signs in with a device key file, unlocks the fs and opens databases.
//
// The device file is zen-mount's `--device-key` format: hex `identity_seed`,
// `device_seed` and `cert`. With `vaultDir`, the device is kept in a
// credential vault there and the session is resumed across restarts.
import { readFile } from 'node:fs/promises';
import {
  bytes,
  type Client,
  connect,
  credentialVault,
  type DeviceCredentials,
  type Integrity,
  type IntegrityOptions,
  type Session,
  sealedStore,
  type UnlockedFs,
  zw,
} from '@zen/client';
import { nodeDiskLocalStore } from '@zen/client/node';
import { Db, type DbOptions } from '@zen/db';

/** A device key file (zen-mount `--device-key`). */
export interface DeviceFile {
  identity_seed: string;
  device_seed: string;
  cert: string;
}

/** Options of `signInWorker`. */
export interface WorkerSignIn {
  /** The server URL. */
  url: string;
  /** The origin signed into sessions (default: the URL's, with `localhost`). */
  origin?: string;
  /** The device key file, or its contents. */
  device: string | DeviceFile;
  /** The fs passphrase. */
  passphrase: string;
  /** The fs (default 1). */
  fs?: number;
  /** A directory for a credential vault (the session survives restarts). */
  vaultDir?: string;
  /**
   * Attach integrity to the fs (docs/CLIENT.md "Integrity"), signed by the
   * device of the file; `true` for the defaults. Pins are kept in the vault
   * directory when there is one.
   */
  integrity?: Omit<IntegrityOptions, 'device'> | true;
  /** A `fetch` for the client (tests that tamper with one worker's transport). */
  fetch?: typeof fetch;
}

/** A signed-in worker. */
export interface WorkerContext {
  client: Client;
  session: Session;
  fs: UnlockedFs;
  /** The fs's integrity, when attached (`integrity`). */
  integrity?: Integrity;
  /** Open a database (no Web Locks or tab channel in Node: each instance owns). */
  open(ns: string, opts?: DbOptions): Promise<Db>;
  /** Close every database opened here, then the fs. */
  close(): Promise<void>;
}

/** The origin a test server's clients sign (`http://localhost:port`). */
export function localOrigin(url: string): string {
  const u = new URL(url);
  return `${u.protocol}//localhost:${u.port}`;
}

async function deviceOf(d: string | DeviceFile): Promise<DeviceFile> {
  return typeof d === 'string' ? JSON.parse(await readFile(d, 'utf8')) : d;
}

/** Sign in with a device key, unlock the fs with the passphrase. */
export async function signInWorker(o: WorkerSignIn): Promise<WorkerContext> {
  const client = await connect(o.url, {
    origin: o.origin ?? localOrigin(o.url),
    ...(o.fetch ? { fetch: o.fetch } : {}),
  });
  const file = await deviceOf(o.device);
  const identity = zw.SigningIdentity.fromSeed(bytes.fromHex(file.identity_seed));
  const deviceSeed = bytes.fromHex(file.device_seed);
  const cert = bytes.fromHex(file.cert);
  let session: Session;
  const store = o.vaultDir ? sealedStore(nodeDiskLocalStore(o.vaultDir)) : undefined;
  if (store) {
    const vault = credentialVault(store);
    if (!(await vault.device())) await vault.saveDevice({ identity, deviceSeed, cert });
    session = await vault.signIn(client);
  } else {
    const device: DeviceCredentials = {
      identity,
      device: zw.DeviceSecret.fromSeed(deviceSeed),
      cert,
    };
    session = await client.signInDevice(device);
  }
  const locked = session.fs(o.fs ?? 1);
  let integrity: Integrity | undefined;
  if (o.integrity) {
    integrity = await locked.integrity({
      ...(o.integrity === true ? {} : o.integrity),
      ...(store && !(o.integrity !== true && o.integrity.local) ? { local: store } : {}),
      device: { device: zw.DeviceSecret.fromSeed(deviceSeed), cert },
    });
  }
  const fs = await locked.unlock({ passphrase: o.passphrase });
  const dbs: Db[] = [];
  return {
    client,
    session,
    fs,
    ...(integrity ? { integrity } : {}),
    async open(ns, opts = {}) {
      const db = await Db.open(fs, ns, { locks: null, channel: null, ...opts });
      dbs.push(db);
      return db;
    },
    async close() {
      await Promise.all(dbs.map((d) => d.close()));
      await integrity?.close().catch(() => {});
      fs.close();
    },
  };
}

/**
 * Sign in from the environment `harness.workerEnv` sets: `ZEN_URL`,
 * `ZEN_ORIGIN`, `ZEN_DEVICE_FILE`, `ZEN_PASSPHRASE`, and optionally
 * `ZEN_VAULT_DIR` and `ZEN_INTEGRITY=1` (attach integrity). For worker scripts.
 */
export function workerFromEnv(env: Record<string, string | undefined> = process.env) {
  const need = (k: string): string => {
    const v = env[k];
    if (!v) throw new Error(`$${k} is not set`);
    return v;
  };
  return signInWorker({
    url: need('ZEN_URL'),
    origin: need('ZEN_ORIGIN'),
    device: need('ZEN_DEVICE_FILE'),
    passphrase: need('ZEN_PASSPHRASE'),
    ...(env.ZEN_VAULT_DIR ? { vaultDir: env.ZEN_VAULT_DIR } : {}),
    ...(env.ZEN_INTEGRITY === '1' ? { integrity: true as const } : {}),
  });
}
