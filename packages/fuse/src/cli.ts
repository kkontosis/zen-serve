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
  connect,
  type Session,
  type Unlock,
  type UnlockedFs,
  zw,
} from '@zen/client';
import { mount } from './mount.js';

const HELP = `usage: zen-mount <url> <mountpoint> [options]

Mount a tree of a zen-serve filesystem as a local directory (FUSE).

Sign-in (one of):
  --user NAME            password-derived key (password: $ZEN_PASSWORD or a prompt)
  --opaque NAME          OPAQUE password (password: $ZEN_PASSWORD or a prompt);
                         also unlocks the fs through the credential's keyslot
  --api-token-file FILE  an API token (or $ZEN_API_TOKEN)
  --device-key FILE      a device key file: JSON {identity_seed, device_seed, cert} (hex)

Unlocking the fs (default: what the sign-in gives, then the device key):
  --passphrase           ask for the fs passphrase (or $ZEN_PASSPHRASE)
  --recovery-key-file F  a 32-byte recovery key, hex

Mount:
  --fs ID                filesystem id (default 1)
  --tree HEX             tree id (16 bytes, hex)
  --new-tree             create a new tree and print its id
  --origin ORIGIN        the server origin as this client sees it (default: the URL's)
  --read-only            refuse every change
  --foreground           stay in the foreground (default: detach once mounted)
  --cache-dir DIR        reserved; chunks are cached in memory
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
      help: { type: 'boolean', short: 'h', default: false },
    },
  });
  if (values.help || positionals.length !== 2) {
    process.stderr.write(HELP);
    process.exit(values.help ? 0 : 2);
  }
  if (!values.tree === !values['new-tree'])
    throw new Error('give exactly one of --tree and --new-tree');
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

async function signIn(client: Client, a: Args): Promise<{ session: Session; device?: DeviceFile }> {
  if (a.user)
    return {
      session: await client.signInPassword(a.user, await secret('password', 'ZEN_PASSWORD')),
    };
  if (a.opaque)
    return {
      session: await client.signInOpaque(a.opaque, await secret('password', 'ZEN_PASSWORD')),
    };
  if (a.apiTokenFile || process.env.ZEN_API_TOKEN) {
    const t = a.apiTokenFile
      ? (await readFile(a.apiTokenFile, 'utf8')).trim()
      : process.env.ZEN_API_TOKEN!;
    return { session: client.withApiToken(t) };
  }
  if (a.deviceKey) {
    const d = JSON.parse(await readFile(a.deviceKey, 'utf8')) as DeviceFile;
    const identity = zw.SigningIdentity.fromSeed(bytes.fromHex(d.identity_seed));
    const device = zw.DeviceSecret.fromSeed(bytes.fromHex(d.device_seed));
    const session = await client.signInDevice({ identity, device, cert: bytes.fromHex(d.cert) });
    identity.free();
    device.free();
    return { session, device: d };
  }
  throw new Error('no sign-in method: give --user, --opaque, --api-token-file or --device-key');
}

async function unlock(session: Session, a: Args, device?: DeviceFile): Promise<UnlockedFs> {
  const fs = session.fs(a.fs);
  let how: Unlock | undefined;
  if (a.passphrase || process.env.ZEN_PASSPHRASE) {
    how = { passphrase: await secret('fs passphrase', 'ZEN_PASSPHRASE') };
  } else if (a.recoveryKeyFile) {
    how = { recoveryKey: bytes.fromHex((await readFile(a.recoveryKeyFile, 'utf8')).trim()) };
  } else if (device && !session.unlock.opaqueExportKey) {
    how = { device: zw.DeviceSecret.fromSeed(bytes.fromHex(device.device_seed)) };
  }
  return fs.unlock(how);
}

async function main(): Promise<void> {
  const a = parse(process.argv.slice(2));
  if (!a.foreground) return detach();
  const client = await connect(a.url, a.origin ? { origin: a.origin } : {});
  const { session, device } = await signIn(client, a);
  const ufs = await unlock(session, a, device);
  const treeId = a.tree ? bytes.fromHex(a.tree) : bytes.randomBytes(16);
  if (a.newTree) process.stdout.write(`tree ${bytes.hex(treeId)}\n`);
  const m = await mount(ufs.tree(treeId), a.mountpoint, { readOnly: a.readOnly });
  process.send?.('mounted');
  process.stderr.write(`mounted fs ${a.fs} tree ${bytes.hex(treeId)} on ${a.mountpoint}\n`);
  const stop = async () => {
    await m.unmount();
    ufs.close();
    process.exit(0);
  };
  process.once('SIGINT', stop);
  process.once('SIGTERM', stop);
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
