#!/usr/bin/env node
// zen-mount: mount a zen-serve filesystem tree as a local directory.
//
//   zen-mount <url> <mountpoint> [options]
//
// Secrets never come from the command line: they are read from a file, an
// environment variable, or a prompt.
import { spawn } from 'node:child_process';
import { readFile } from 'node:fs/promises';
import { createInterface } from 'node:readline';
import { parseArgs } from 'node:util';
import {
  bytes,
  type Client,
  type CredentialVault,
  connect,
  credentialVault,
  isCode,
  memoryLocalStore,
  type SealedStore,
  type Session,
  sealedStore,
  type Unlock,
  type UnlockedFs,
  zw,
} from '@zen/client';
import { nodeDiskLocalStore } from '@zen/client/node';
import { openReplica, type Replica } from '@zen/fs';
import { mount } from './mount.js';

const HELP = `usage: zen-mount <url> <mountpoint> [options]

Mount a tree of a zen-serve filesystem as a local directory (FUSE).

Sign-in (one of):
  --user NAME            password-derived key (password: $ZEN_PASSWORD or a prompt)
  --opaque NAME          OPAQUE password (password: $ZEN_PASSWORD or a prompt);
                         also unlocks the fs through the credential's keyslot
  --api-token-file FILE  an API token (or $ZEN_API_TOKEN)
  --device-key FILE      a device key file: JSON {identity_seed, device_seed, cert} (hex);
                         with --vault, it is stored there (the file can then go)
  --vault DIR            keep the device and the session here, sealed under a key
                         of this machine; later mounts need no secret

Unlocking the fs (default: what the sign-in gives, then the device key or the vault's):
  --passphrase           ask for the fs passphrase (or $ZEN_PASSPHRASE)
  --recovery-key-file F  a 32-byte recovery key, hex

Mount:
  --fs ID                filesystem id (default 1)
  --tree HEX             tree id (16 bytes, hex)
  --new-tree             create a new tree and print its id
  --origin ORIGIN        the server origin as this client sees it (default: the URL's)
  --read-only            refuse every change
  --foreground           stay in the foreground (default: detach once mounted)
  --cache-dir DIR        keep the replica here (sealed): the node table, content,
                         pins and writes not yet sent (default: in memory)
  --cache-size BYTES     the most content to keep (default 1 GiB; pins don't count)
  --pin PATH             keep PATH (a file, or a folder and all under it) offline;
                         repeatable, and remembered in --cache-dir
  --offline-ok           mount from --cache-dir when the server can't be reached
                         (needs --vault)
  -h, --help
`;

/** Parsed command line. */
export interface Args {
  url: string;
  mountpoint: string;
  fs: number;
  tree?: string;
  newTree: boolean;
  origin?: string;
  user?: string;
  opaque?: string;
  apiTokenFile?: string;
  deviceKey?: string;
  passphrase: boolean;
  recoveryKeyFile?: string;
  readOnly: boolean;
  foreground: boolean;
  cacheDir?: string;
  cacheSize: number;
  pins: string[];
  vault?: string;
  offlineOk: boolean;
}

export function parse(argv: string[]): Args {
  const { values, positionals } = parseArgs({
    args: argv,
    allowPositionals: true,
    options: {
      fs: { type: 'string', default: '1' },
      tree: { type: 'string' },
      'new-tree': { type: 'boolean', default: false },
      origin: { type: 'string' },
      user: { type: 'string' },
      opaque: { type: 'string' },
      'api-token-file': { type: 'string' },
      'device-key': { type: 'string' },
      passphrase: { type: 'boolean', default: false },
      'recovery-key-file': { type: 'string' },
      'read-only': { type: 'boolean', default: false },
      foreground: { type: 'boolean', default: false },
      'cache-dir': { type: 'string' },
      'cache-size': { type: 'string', default: String(2 ** 30) },
      pin: { type: 'string', multiple: true, default: [] },
      vault: { type: 'string' },
      'offline-ok': { type: 'boolean', default: false },
      help: { type: 'boolean', short: 'h', default: false },
    },
  });
  if (values.help || positionals.length !== 2) {
    process.stderr.write(HELP);
    process.exit(values.help ? 0 : 2);
  }
  if (!values.tree === !values['new-tree'])
    throw new Error('give exactly one of --tree and --new-tree');
  const cacheSize = Number(values['cache-size']);
  if (!Number.isSafeInteger(cacheSize) || cacheSize < 0)
    throw new Error('--cache-size takes a number of bytes');
  if (values['offline-ok'] && (!values['cache-dir'] || !values.vault))
    throw new Error('--offline-ok needs --cache-dir and --vault');
  return {
    url: positionals[0]!,
    mountpoint: positionals[1]!,
    fs: Number(values.fs),
    ...(values.tree ? { tree: values.tree } : {}),
    newTree: values['new-tree']!,
    ...(values.origin ? { origin: values.origin } : {}),
    ...(values.user ? { user: values.user } : {}),
    ...(values.opaque ? { opaque: values.opaque } : {}),
    ...(values['api-token-file'] ? { apiTokenFile: values['api-token-file'] } : {}),
    ...(values['device-key'] ? { deviceKey: values['device-key'] } : {}),
    passphrase: values.passphrase!,
    ...(values['recovery-key-file'] ? { recoveryKeyFile: values['recovery-key-file'] } : {}),
    readOnly: values['read-only']!,
    foreground: values.foreground!,
    ...(values['cache-dir'] ? { cacheDir: values['cache-dir'] } : {}),
    cacheSize,
    pins: values.pin as string[],
    ...(values.vault ? { vault: values.vault } : {}),
    offlineOk: values['offline-ok']!,
  };
}

/** Read a line from the terminal without echoing it. */
async function secret(prompt: string, env: string): Promise<string> {
  const v = process.env[env];
  if (v !== undefined) return v;
  if (!process.stdin.isTTY) throw new Error(`no terminal to ask for ${prompt}: set $${env}`);
  const rl = createInterface({ input: process.stdin, output: process.stderr, terminal: true });
  const out = rl as unknown as { _writeToOutput: (s: string) => void; output: NodeJS.WriteStream };
  process.stderr.write(`${prompt}: `);
  out._writeToOutput = () => {};
  const answer = await new Promise<string>((r) => rl.question('', r));
  rl.close();
  process.stderr.write('\n');
  return answer;
}

interface DeviceFile {
  identity_seed: string;
  device_seed: string;
  cert: string;
}

async function signIn(
  client: Client,
  a: Args,
  vault?: CredentialVault,
): Promise<{ session: Session; device?: DeviceFile }> {
  let out: { session: Session; device?: DeviceFile };
  if (a.user) {
    out = {
      session: await client.signInPassword(a.user, await secret('password', 'ZEN_PASSWORD')),
    };
  } else if (a.opaque) {
    out = {
      session: await client.signInOpaque(a.opaque, await secret('password', 'ZEN_PASSWORD')),
    };
  } else if (a.apiTokenFile || process.env.ZEN_API_TOKEN) {
    const t = a.apiTokenFile
      ? (await readFile(a.apiTokenFile, 'utf8')).trim()
      : process.env.ZEN_API_TOKEN!;
    out = { session: client.withApiToken(t) };
  } else if (a.deviceKey) {
    const d = JSON.parse(await readFile(a.deviceKey, 'utf8')) as DeviceFile;
    const identity = zw.SigningIdentity.fromSeed(bytes.fromHex(d.identity_seed));
    if (vault) {
      // Kept in the vault: later mounts sign in from it, and the file can go.
      await vault.saveDevice({
        identity,
        deviceSeed: bytes.fromHex(d.device_seed),
        cert: bytes.fromHex(d.cert),
      });
      identity.free();
      return { session: await vault.signIn(client) };
    }
    const device = zw.DeviceSecret.fromSeed(bytes.fromHex(d.device_seed));
    const session = await client.signInDevice({ identity, device, cert: bytes.fromHex(d.cert) });
    identity.free();
    device.free();
    return { session, device: d };
  } else if (vault) {
    return { session: await vault.signIn(client) };
  } else {
    throw new Error(
      'no sign-in method: give --user, --opaque, --api-token-file, --device-key or --vault',
    );
  }
  await vault?.saveSession(out.session);
  return out;
}

async function unlock(
  session: Session,
  a: Args,
  local: SealedStore | undefined,
  device?: DeviceFile,
  vault?: CredentialVault,
): Promise<UnlockedFs> {
  const fs = session.fs(a.fs);
  const opts = local ? { local } : {};
  let how: Unlock | undefined;
  if (a.passphrase || process.env.ZEN_PASSPHRASE) {
    how = { passphrase: await secret('fs passphrase', 'ZEN_PASSPHRASE') };
  } else if (a.recoveryKeyFile) {
    how = { recoveryKey: bytes.fromHex((await readFile(a.recoveryKeyFile, 'utf8')).trim()) };
  } else if (device && !session.unlock.opaqueExportKey) {
    how = { device: zw.DeviceSecret.fromSeed(bytes.fromHex(device.device_seed)) };
  } else if (vault && !session.unlock.opaqueExportKey && (await hasDevice(vault))) {
    return vault.unlock(fs, opts);
  }
  return fs.unlock(how, opts);
}

async function hasDevice(vault: CredentialVault): Promise<boolean> {
  const d = await vault.device();
  d?.device.free();
  return !!d;
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** Wait up to `ms` for the replica's queued writes to reach the server. */
async function drain(replica: Replica, ms: number): Promise<boolean> {
  const end = Date.now() + ms;
  for (;;) {
    const left = end - Date.now();
    if (left <= 0) return false;
    const done = await Promise.race([
      replica.flush().then(
        () => true,
        () => false,
      ),
      sleep(left).then(() => false),
    ]);
    if (done) return true;
    await sleep(Math.min(500, Math.max(0, end - Date.now())));
  }
}

async function main(): Promise<void> {
  const a = parse(process.argv.slice(2));
  if (!a.foreground) return detach();
  const client = await connect(a.url, a.origin ? { origin: a.origin } : {});
  let online = true;
  try {
    await client.info();
  } catch (e) {
    if (!isCode(e, 'network') || !a.offlineOk) throw e;
    online = false;
    process.stderr.write("zen-mount: the server can't be reached; mounting from the cache\n");
  }
  const vault = a.vault ? credentialVault(sealedStore(nodeDiskLocalStore(a.vault))) : undefined;
  const local = a.cacheDir ? sealedStore(nodeDiskLocalStore(a.cacheDir)) : undefined;
  const { session, device } = await signIn(client, a, vault);
  const ufs = await unlock(session, a, local, device, vault);
  const treeId = a.tree ? bytes.fromHex(a.tree) : bytes.randomBytes(16);
  if (a.newTree) process.stdout.write(`tree ${bytes.hex(treeId)}\n`);
  const replica = await openReplica(ufs, treeId, {
    local: local ?? sealedStore(memoryLocalStore()),
    cacheBytes: a.cacheSize,
    locks: null,
    channel: null,
  });
  // Mount a complete table: catch up first (offline, the kept one).
  if (online) await replica.sync();
  for (const p of a.pins) {
    await replica.pin(p).catch((e: unknown) => {
      process.stderr.write(`zen-mount: --pin ${p}: ${(e as Error).message}\n`);
    });
  }
  const m = await mount(replica, a.mountpoint, { readOnly: a.readOnly });
  process.send?.('mounted');
  process.stderr.write(`mounted fs ${a.fs} tree ${bytes.hex(treeId)} on ${a.mountpoint}\n`);
  let stopping = false;
  const stop = async () => {
    if (stopping) process.exit(1); // a second signal: stop at once
    stopping = true;
    await m.unmount();
    if (!(await drain(replica, 10_000))) {
      const { queued } = await replica.status();
      if (local) {
        process.stderr.write(
          `zen-mount: ${queued} write(s) not sent yet; kept in ${a.cacheDir} for the next mount\n`,
        );
      } else {
        // In memory they would be lost: wait for the server.
        while (!(await drain(replica, 10_000))) {
          const s = await replica.status();
          process.stderr.write(
            `zen-mount: waiting to send ${s.queued} write(s) (in memory only; signal again to drop them)\n`,
          );
        }
      }
    }
    await replica.close();
    ufs.close();
    process.exit(0);
  };
  process.on('SIGINT', stop);
  process.on('SIGTERM', stop);
}

/** Re-run in the foreground as a detached child, and exit once it has mounted. */
async function detach(): Promise<void> {
  const child = spawn(
    process.execPath,
    [...process.execArgv, process.argv[1]!, ...process.argv.slice(2), '--foreground'],
    {
      detached: true,
      stdio: ['inherit', 'inherit', 'inherit', 'ipc'],
    },
  );
  await new Promise<void>((ok, fail) => {
    child.once('message', () => ok());
    child.once('exit', (code) => fail(new Error(`zen-mount exited with ${code}`)));
  });
  child.disconnect();
  child.unref();
}

main().catch((e: unknown) => {
  process.stderr.write(`zen-mount: ${e instanceof Error ? e.message : String(e)}\n`);
  process.exit(1);
});
