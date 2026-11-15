// Signing in without a prompt, and opening the `git` database (spec/git.md §1, §3).
import { homedir } from 'node:os';
import { join } from 'node:path';
import {
  type Client,
  type CredentialVault,
  connect,
  credentialVault,
  type Integrity,
  isCode,
  type SealedStore,
  type Session,
  sealedStore,
  type UnlockedFs,
} from '@zen/client';
import { nodeDiskLocalStore } from '@zen/client/node';
import { Db } from '@zen/db';
import { GitError } from './url.js';

/** The vault's directory: `$ZEN_GIT_VAULT`, else `$XDG_CONFIG_HOME/zen-git/vault`. */
export function vaultDir(env: NodeJS.ProcessEnv = process.env): string {
  if (env.ZEN_GIT_VAULT) return env.ZEN_GIT_VAULT;
  const config = env.XDG_CONFIG_HOME || join(homedir(), '.config');
  return join(config, 'zen-git', 'vault');
}

/** The vault and its store. */
export interface Vault {
  dir: string;
  store: SealedStore;
  vault: CredentialVault;
}

export function openVault(dir = vaultDir()): Vault {
  const store = sealedStore(nodeDiskLocalStore(dir));
  return { dir, store, vault: credentialVault(store) };
}

/** A signed-in, unlocked fs with its `git` database. */
export interface Opened {
  client: Client;
  session: Session;
  fs: UnlockedFs;
  integrity: Integrity;
  db: Db;
  /** This device's fingerprint (the `device` of rows and events). */
  device: Uint8Array;
  close(): Promise<void>;
}

/** The `git` database's tables (§3). */
export const SCHEMA = 1;
export const MIGRATIONS = {
  1: async (m: import('@zen/db').Migrator) => {
    await m.createTable('repos', { pk: ['name'], pad: true });
    await m.createTable('refs', { pk: ['repo', 'ref'], pad: true });
    await m.createIndex('refs', 'by_repo', { fields: [['repo', 'text']] });
    await m.createTable('packs', { pk: ['repo', 'hash'], pad: true });
    await m.createIndex('packs', 'by_repo', { fields: [['repo', 'text']] });
  },
};

/** Open the `git` database of an unlocked fs (§3): authenticated. */
export function openGitDb(fs: UnlockedFs): Promise<Db> {
  return Db.open(fs, 'git', {
    locks: null,
    channel: null,
    scheduler: false,
    integrity: 'authenticated',
    schema: SCHEMA,
    migrations: MIGRATIONS,
    warn: () => {},
  });
}

/**
 * Sign in from the vault as its device, attach integrity (pins in the vault,
 * checkpoints signed by its device), unlock the fs by its device slot, and
 * open the `git` database. Never prompts.
 */
export async function signIn(server: string, fsId: number, v = openVault()): Promise<Opened> {
  const d = await v.vault.device();
  if (!d) {
    throw new GitError(
      `no device in the vault ${v.dir}: run "zen-git login --server ${server} --device <file>" once`,
    );
  }
  const device = d.device.fingerprint;
  d.device.free();
  const client = await connect(server);
  let session: Session;
  try {
    session = await v.vault.signIn(client);
  } catch (e) {
    if (isCode(e, 'network')) throw new GitError(`${server} can't be reached: ${errText(e)}`);
    throw e;
  }
  const signer = await v.vault.signer();
  const locked = session.fs(fsId);
  const integrity = await locked.integrity({
    local: v.store,
    ...(signer ? { device: { device: signer.device, cert: signer.cert } } : {}),
  });
  let fs: UnlockedFs;
  try {
    fs = await v.vault.unlock(locked);
  } catch (e) {
    await integrity.close().catch(() => {});
    if (isCode(e, 'not_found') || isCode(e, 'forbidden')) {
      throw new GitError(
        `fs ${fsId}: this device can't unlock it (an admin adds its keyslot: fs.addDevice): ${errText(e)}`,
      );
    }
    throw e;
  }
  let db: Db;
  try {
    db = await openGitDb(fs);
  } catch (e) {
    await integrity.close().catch(() => {});
    fs.close();
    throw e;
  }
  return {
    client,
    session,
    fs,
    integrity,
    db,
    device,
    async close() {
      await db.close().catch(() => {});
      await integrity.close().catch(() => {});
      fs.close();
    },
  };
}

export function errText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}
