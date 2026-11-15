#!/usr/bin/env node
// zen-git (spec/git.md §1, §10): login, setup, head, and the mirror's hook.
//
//   zen-git login --server URL --device FILE   put a device in the vault, once
//   zen-git setup zen://<host>/<fs> --device FILE
//                                              an admin's topic grants (§10)
//   zen-git head zen://<host>/<fs>/<repo> <branch>
//                                              set HEAD
//   zen-git hook                               (the mirror's pre-receive hook)
//
// The vault is $ZEN_GIT_VAULT, else $XDG_CONFIG_HOME/zen-git/vault.
// Secrets never come from the command line: a device key file's path does.
import { readFile } from 'node:fs/promises';
import { parseArgs } from 'node:util';
import { bytes, connect, zw } from '@zen/client';
import { hook } from './hook.js';
import { main } from './main.js';
import { setupGrants } from './setup.js';
import { openVault, signIn } from './signin.js';
import { setHead } from './store.js';
import { GitError, parseFsUrl, parseRepoUrl } from './url.js';

const HELP = `usage:
  zen-git login --server URL --device FILE
  zen-git setup zen://<host>/<fs> --device FILE   (an admin; ZEN_PASSPHRASE unlocks
                                                   the fs if this device has no keyslot)
  zen-git head zen://<host>/<fs>/<repo> <branch>
the vault: $ZEN_GIT_VAULT, else $XDG_CONFIG_HOME/zen-git/vault`;

interface DeviceFile {
  identity_seed: string;
  device_seed: string;
  cert: string;
}

async function deviceFile(path: string | undefined): Promise<DeviceFile> {
  if (!path) throw new GitError(`--device FILE is needed\n${HELP}`);
  return JSON.parse(await readFile(path, 'utf8')) as DeviceFile;
}

async function login(args: string[]): Promise<number> {
  const { values } = parseArgs({
    args,
    options: { server: { type: 'string' }, device: { type: 'string' } },
  });
  if (!values.server) throw new GitError(`--server URL is needed\n${HELP}`);
  const d = await deviceFile(values.device);
  const v = openVault();
  const identity = zw.SigningIdentity.fromSeed(bytes.fromHex(d.identity_seed));
  await v.vault.saveDevice({
    identity,
    deviceSeed: bytes.fromHex(d.device_seed),
    cert: bytes.fromHex(d.cert),
  });
  identity.free();
  const session = await v.vault.signIn(await connect(values.server));
  const fp = session.deviceFp;
  process.stdout.write(`signed in; the device is in ${v.dir}${fp ? ` (${bytes.hex(fp)})` : ''}\n`);
  return 0;
}

async function setup(args: string[]): Promise<number> {
  const { values, positionals } = parseArgs({
    args,
    allowPositionals: true,
    options: { device: { type: 'string' } },
  });
  const u = parseFsUrl(positionals[0] ?? '');
  const d = await deviceFile(values.device);
  const identity = zw.SigningIdentity.fromSeed(bytes.fromHex(d.identity_seed));
  const device = zw.DeviceSecret.fromSeed(bytes.fromHex(d.device_seed));
  try {
    const client = await connect(u.server);
    const session = await client.signInDevice({ identity, device, cert: bytes.fromHex(d.cert) });
    const locked = session.fs(u.fs);
    const pass = process.env.ZEN_PASSPHRASE;
    const fs = await locked.unlock(pass ? { passphrase: pass } : { device });
    try {
      const r = await setupGrants(session, identity, u.fs, fs.topic('git').id);
      process.stdout.write(`fs ${u.fs}: ${r.added} grant(s) added on the topic prefix ("git")\n`);
    } finally {
      fs.close();
    }
  } finally {
    identity.free();
    device.free();
  }
  return 0;
}

async function head(args: string[]): Promise<number> {
  const [url, branch] = args;
  if (!url || !branch) throw new GitError(HELP);
  const u = parseRepoUrl(url);
  const ref = branch.startsWith('refs/') ? branch : `refs/heads/${branch}`;
  const z = await signIn(u.server, u.fs);
  try {
    if (!(await setHead(z.db, u.repo, ref))) throw new GitError(`${u.url}: no such repo`);
  } finally {
    await z.close();
  }
  process.stdout.write(`HEAD is ${ref}\n`);
  return 0;
}

main(async (argv) => {
  const [cmd, ...rest] = argv;
  if (cmd === 'login') return login(rest);
  if (cmd === 'setup') return setup(rest);
  if (cmd === 'head') return head(rest);
  if (cmd === 'hook') return hook();
  process.stderr.write(`${HELP}\n`);
  return cmd === '--help' || cmd === '-h' ? 0 : 2;
});
