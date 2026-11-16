#!/usr/bin/env node
// zen-git (spec/git.md §1, §9, §10, §13): login, setup, head, worker, repack,
// publish, and the mirror's hook.
//
//   zen-git login --server URL --device FILE   put a device in the vault, once
//   zen-git setup zen://<host>/<fs> --device FILE
//                                              an admin's topic grants (§10)
//   zen-git head zen://<host>/<fs>/<repo> <branch>
//                                              set HEAD
//   zen-git worker zen://<host>/<fs> [--grace 24h] [--publish <repo>=<dir>]...
//                                              the fs's repack job, gc and publishing
//   zen-git repack zen://<host>/<fs>/<repo>    one roll-up now
//   zen-git publish zen://<host>/<fs>/<repo> <dir>
//                                              write <dir>/<repo>.git for dumb HTTP
//   zen-git hook                               (the mirror's pre-receive hook)
//
// The vault is $ZEN_GIT_VAULT, else $XDG_CONFIG_HOME/zen-git/vault.
// Secrets never come from the command line: a device key file's path does.
import { readFile } from 'node:fs/promises';
import { parseArgs } from 'node:util';
import { bytes, connect, zw } from '@zen/client';
import { hook } from './hook.js';
import { main } from './main.js';
import { publish } from './publish.js';
import { setupGrants } from './setup.js';
import { openVault, signIn } from './signin.js';
import { repack, setHead } from './store.js';
import { GitError, parseFsUrl, parseRepoUrl } from './url.js';
import { runWorker } from './worker.js';

const HELP = `usage:
  zen-git login --server URL --device FILE
  zen-git setup zen://<host>/<fs> --device FILE   (an admin; ZEN_PASSPHRASE unlocks
                                                   the fs if this device has no keyslot)
  zen-git head zen://<host>/<fs>/<repo> <branch>
  zen-git worker zen://<host>/<fs> [--grace 24h] [--publish <repo>=<dir>]...
  zen-git repack zen://<host>/<fs>/<repo>
  zen-git publish zen://<host>/<fs>/<repo> <dir>
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
      const r = await setupGrants(
        session,
        identity,
        u.fs,
        fs.topic('git').id,
        fs.topic('zen', 'db', 'git', 'sched').id,
      );
      process.stdout.write(`fs ${u.fs}: ${r.added} grant(s) added\n`);
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

/** A duration: `30s`, `10m`, `24h`, `2d`, or milliseconds. */
function parseDuration(s: string): number {
  const m = /^(\d+)(ms|s|m|h|d)?$/.exec(s);
  if (!m) throw new GitError(`${s}: a duration is like 30s, 10m, 24h or 2d`);
  const unit = { ms: 1, s: 1e3, m: 6e4, h: 3.6e6, d: 8.64e7 }[m[2] ?? 'ms']!;
  return Number(m[1]) * unit;
}

async function worker(args: string[]): Promise<number> {
  const { values, positionals } = parseArgs({
    args,
    allowPositionals: true,
    options: { grace: { type: 'string' }, publish: { type: 'string', multiple: true } },
  });
  const u = parseFsUrl(positionals[0] ?? '');
  const pub = new Map<string, string>();
  for (const p of values.publish ?? []) {
    const at = p.indexOf('=');
    if (at <= 0) throw new GitError(`--publish ${p}: expected <repo>=<dir>`);
    pub.set(p.slice(0, at), p.slice(at + 1));
  }
  const ctl = new AbortController();
  for (const sig of ['SIGINT', 'SIGTERM'] as const) process.once(sig, () => ctl.abort());
  const die = process.env.ZEN_GIT_TEST_DIE_AFTER_UPLOAD;
  await runWorker({
    ...u,
    graceMs: values.grace ? parseDuration(values.grace) : undefined,
    publish: pub,
    signal: ctl.signal,
    ...(die ? { afterUpload: () => void process.kill(process.pid, 'SIGKILL') } : {}),
  });
  return 0;
}

async function repackCmd(args: string[]): Promise<number> {
  const u = parseRepoUrl(args[0] ?? '');
  const z = await signIn(u.server, u.fs);
  try {
    const r = await repack(z.db, z.fs, u.repo, { min: 1 });
    if (!r.ok) throw new GitError(`${u.url}: repack refused: ${r.why}`);
    process.stdout.write(
      r.rolled
        ? `${u.repo}: ${r.rolled} packs → 1 (${r.size} bytes); ${r.left} live\n`
        : `${u.repo}: nothing to repack (${r.left} live)\n`,
    );
  } finally {
    await z.close();
  }
  return 0;
}

async function publishCmd(args: string[]): Promise<number> {
  const [url, dir] = args;
  if (!url || !dir) throw new GitError(HELP);
  const u = parseRepoUrl(url);
  const z = await signIn(u.server, u.fs);
  try {
    process.stdout.write(`published to ${await publish(z, u, dir)}\n`);
  } finally {
    await z.close();
  }
  return 0;
}

main(async (argv) => {
  const [cmd, ...rest] = argv;
  if (cmd === 'login') return login(rest);
  if (cmd === 'setup') return setup(rest);
  if (cmd === 'head') return head(rest);
  if (cmd === 'worker') return worker(rest);
  if (cmd === 'repack') return repackCmd(rest);
  if (cmd === 'publish') return publishCmd(rest);
  if (cmd === 'hook') return hook();
  process.stderr.write(`${HELP}\n`);
  return cmd === '--help' || cmd === '-h' ? 0 : 2;
});
