// Test SSH keys made with ssh-keygen, and OpenSSH `ssh` runs against them,
// for tests of relayed SSH services (packages/svc, apps/echo).
import { execFileSync, spawn } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

/** The host-key alias `known_hosts` names, whatever the port. */
const ALIAS = 'zen-test';

/** A host key and user keys in a temporary directory. */
export interface TestSshKeys {
  /** The directory holding the key files; `cleanup()` removes it. */
  dir: string;
  /** The host key: OpenSSH PEM, Ed25519, unencrypted. */
  hostKey: string;
  /** Its public line, `ssh-ed25519 AAAA…`. */
  hostLine: string;
  /** The user's private key file (Ed25519), for `ssh -i`. */
  userKeyFile: string;
  /** Its public line, for an authorized-keys list. */
  userLine: string;
  /** A second user key, authorized nowhere. */
  otherKeyFile: string;
  /**
   * `ssh` options to reach 127.0.0.1:`port` as `user`: no config, the
   * key `keyFile` (default the user's) only, the host key in a
   * `known_hosts` with strict checking, batch mode. The destination is
   * last; a command may follow.
   */
  sshArgs(port: number, opts?: { user?: string; keyFile?: string; options?: string[] }): string[];
  cleanup(): void;
}

function keygen(dir: string, name: string): void {
  try {
    execFileSync('ssh-keygen', ['-q', '-t', 'ed25519', '-N', '', '-C', 'test', '-f', name], {
      cwd: dir,
      stdio: 'pipe',
    });
  } catch (e) {
    if ((e as { code?: string }).code === 'ENOENT') {
      throw new Error('these tests need OpenSSH (ssh-keygen and ssh)');
    }
    throw e;
  }
}

/** Make a host key and two user keys. Needs `ssh-keygen`. */
export function testSshKeys(): TestSshKeys {
  const dir = mkdtempSync(join(tmpdir(), 'zen-ssh-'));
  for (const k of ['host', 'user', 'other']) keygen(dir, k);
  const read = (f: string) => readFileSync(join(dir, f), 'utf8');
  const line = (f: string) => read(f).trim().split(/\s+/).slice(0, 2).join(' ');
  const hostLine = line('host.pub');
  const knownHosts = join(dir, 'known_hosts');
  writeFileSync(knownHosts, `${ALIAS} ${hostLine}\n`);
  return {
    dir,
    hostKey: read('host'),
    hostLine,
    userKeyFile: join(dir, 'user'),
    userLine: line('user.pub'),
    otherKeyFile: join(dir, 'other'),
    sshArgs: (port, opts = {}) => [
      '-F',
      '/dev/null',
      '-p',
      String(port),
      '-i',
      opts.keyFile ?? join(dir, 'user'),
      ...[
        'IdentitiesOnly=yes',
        'IdentityAgent=none',
        'BatchMode=yes',
        'StrictHostKeyChecking=yes',
        `UserKnownHostsFile=${knownHosts}`,
        'GlobalKnownHostsFile=/dev/null',
        `HostKeyAlias=${ALIAS}`,
        'LogLevel=ERROR',
        'ConnectTimeout=30',
        ...(opts.options ?? []),
      ].flatMap((o) => ['-o', o]),
      '-T',
      `${opts.user ?? 'test'}@127.0.0.1`,
    ],
    cleanup: () => rmSync(dir, { recursive: true, force: true }),
  };
}

/** What an `ssh` run gave. */
export interface SshRun {
  code: number | null;
  stdout: Buffer;
  stderr: string;
}

/**
 * Run `ssh` with `args`, writing `stdin` (then closing it). Resolves when
 * it exits, or rejects after `timeoutMs` (default 60 s), killing it.
 */
export function runSsh(
  args: string[],
  stdin: Uint8Array | string = '',
  timeoutMs = 60_000,
): Promise<SshRun> {
  return new Promise((ok, fail) => {
    const p = spawn('ssh', args, { stdio: ['pipe', 'pipe', 'pipe'] });
    const out: Buffer[] = [];
    const err: Buffer[] = [];
    p.stdout.on('data', (d: Buffer) => out.push(d));
    p.stderr.on('data', (d: Buffer) => err.push(d));
    const t = setTimeout(() => {
      p.kill('SIGKILL');
      fail(new Error(`ssh timed out after ${timeoutMs} ms: ${Buffer.concat(err).toString()}`));
    }, timeoutMs);
    p.on('error', (e) => {
      clearTimeout(t);
      fail((e as { code?: string }).code === 'ENOENT' ? new Error('these tests need OpenSSH') : e);
    });
    p.on('close', (code) => {
      clearTimeout(t);
      ok({ code, stdout: Buffer.concat(out), stderr: Buffer.concat(err).toString() });
    });
    p.stdin.on('error', () => {});
    p.stdin.end(stdin);
  });
}
