// git-remote-zen (spec/git.md §7): a remote helper with `connect`. Each
// connect syncs the mirror and hands git to git's own service on it.
import { readSync, writeSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import {
  cleanEnv,
  git,
  lockMirror,
  mirrorPath,
  runService,
  settlePushed,
  syncRepo,
} from './mirror.js';
import { signIn } from './signin.js';
import { GitError, parseRepoUrl, type RepoUrl } from './url.js';

/** Read one line from fd 0, a byte at a time: nothing past it is consumed (§7). */
function readLine(): string | null {
  const b = Buffer.alloc(1);
  const parts: number[] = [];
  for (;;) {
    let n: number;
    try {
      n = readSync(0, b, 0, 1, null);
    } catch (e) {
      if ((e as NodeJS.ErrnoException).code === 'EAGAIN') continue;
      throw e;
    }
    if (n === 0) return parts.length ? Buffer.from(parts).toString('utf8') : null;
    if (b[0] === 10) return Buffer.from(parts).toString('utf8');
    parts.push(b[0]!);
  }
}

const out = (s: string) => writeSync(1, s);

/** The object format of the repo git runs the helper for (its `GIT_DIR`), sha1 outside one. */
function localFormat(): 'sha1' | 'sha256' {
  if (!process.env.GIT_DIR) return 'sha1';
  try {
    const f = git(['rev-parse', '--show-object-format'], { env: process.env }).trim();
    return f === 'sha256' ? 'sha256' : 'sha1';
  } catch {
    return 'sha1';
  }
}

/** Whether git is configured for protocol v0 or v1 (`protocol.version`). */
function oldProtocol(): boolean {
  try {
    const v = git(['config', '--get', 'protocol.version'], { env: process.env }).trim();
    return v === '0' || v === '1';
  } catch {
    return false;
  }
}

/** The `zen-git` bundle beside this one: the hook runs it. */
function zenGitPath(): string {
  return fileURLToPath(new URL('./zen-git.mjs', import.meta.url));
}

async function connect(u: RepoUrl, service: 'upload-pack' | 'receive-pack'): Promise<number> {
  const m = mirrorPath(u);
  const release = await lockMirror(m);
  let held = true;
  const unlock = () => {
    if (held) release();
    held = false;
  };
  try {
    const z = await signIn(u.server, u.fs);
    try {
      const state = await syncRepo(m, z, u.repo, localFormat());
      if (!state.repo && service === 'upload-pack') {
        throw new GitError(`${u.url}: no such repo (a first push creates it)`);
      }
    } finally {
      await z.close();
    }
    // upload-pack reads without the lock (§6).
    if (service === 'upload-pack') unlock();
    out('\n');
    const env: NodeJS.ProcessEnv = {
      ...cleanEnv(),
      ZEN_GIT_URL: u.url,
      ZEN_GIT_MIRROR: m,
      ZEN_GIT_NODE: process.execPath,
      ZEN_GIT_HOOK: zenGitPath(),
      ...(service === 'upload-pack' && !oldProtocol() ? { GIT_PROTOCOL: 'version=2' } : {}),
    };
    const code = await runService(service, m, env);
    if (service === 'receive-pack') settlePushed(m);
    return code;
  } finally {
    unlock();
  }
}

/** The helper's command loop. Returns its exit code. */
export async function helper(argv: string[]): Promise<number> {
  const [, url] = argv;
  if (!url) throw new GitError('usage: git-remote-zen <remote> <url> (git runs it)');
  const u = parseRepoUrl(url);
  for (;;) {
    const line = readLine();
    if (line === null || line === '') return 0;
    if (line === 'capabilities') {
      out('connect\noption\n\n');
    } else if (line.startsWith('option ')) {
      const name = line.split(' ')[1];
      out(name === 'verbosity' || name === 'progress' ? 'ok\n' : 'unsupported\n');
    } else if (line.startsWith('connect ')) {
      const svc = line.slice('connect '.length);
      if (svc !== 'git-upload-pack' && svc !== 'git-receive-pack') {
        throw new GitError(`unsupported service ${svc}`);
      }
      return connect(u, svc === 'git-upload-pack' ? 'upload-pack' : 'receive-pack');
    } else {
      throw new GitError(`unknown command from git: ${line}`);
    }
  }
}
