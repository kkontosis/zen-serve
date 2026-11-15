// The mirror (spec/git.md §5, §6): a bare repository synced from zen and
// served by git's own upload-pack and receive-pack.
import { execFileSync, spawn } from 'node:child_process';
import {
  appendFileSync,
  existsSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  renameSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';
import { bytes, type UnlockedFs } from '@zen/client';
import { downloadPack, type RepoState } from './store.js';
import { GitError, type RepoUrl } from './url.js';

/** Git's variables for a helper or a hook; removed for git run on the mirror (§6). */
const GIT_VARS =
  /^GIT_(DIR|WORK_TREE|INDEX_FILE|OBJECT_DIRECTORY|ALTERNATE_OBJECT_DIRECTORIES|PREFIX|COMMON_DIR|NAMESPACE|QUARANTINE_PATH)$/;

/** `env` without git's repository variables. */
export function cleanEnv(env: NodeJS.ProcessEnv = process.env): NodeJS.ProcessEnv {
  return Object.fromEntries(Object.entries(env).filter(([k]) => !GIT_VARS.test(k)));
}

/** Run git with `env` (default: the clean one); returns stdout. */
export function git(
  args: string[],
  opts: { input?: string | Buffer; env?: NodeJS.ProcessEnv; cwd?: string } = {},
): string {
  try {
    return execFileSync('git', args, {
      encoding: 'utf8',
      maxBuffer: 1 << 28,
      env: opts.env ?? cleanEnv(),
      stdio: ['pipe', 'pipe', 'pipe'],
      ...(opts.input !== undefined ? { input: opts.input } : {}),
      ...(opts.cwd ? { cwd: opts.cwd } : {}),
    });
  } catch (e) {
    const err = (e as { stderr?: string }).stderr?.trim();
    throw new GitError(`git ${args.slice(0, 3).join(' ')}: ${err || (e as Error).message}`);
  }
}

/** The mirror's path: `$XDG_CACHE_HOME/zen-git/<host>/<fs>/<repo>.git` (§6). */
export function mirrorPath(u: RepoUrl, env: NodeJS.ProcessEnv = process.env): string {
  const cache = env.XDG_CACHE_HOME || join(homedir(), '.cache');
  return join(cache, 'zen-git', encodeURIComponent(u.host), String(u.fs), `${u.repo}.git`);
}

/** The hook script: it runs `zen-git hook` only under the helper (§7). */
const HOOK = `#!/bin/sh
# zen-git: a push is one zen transaction (spec/git.md §4). Only through git-remote-zen.
if [ -z "$ZEN_GIT_HOOK" ] || [ -z "$ZEN_GIT_URL" ]; then
  echo "zen: push to the zen:// URL, not into this mirror" >&2
  exit 1
fi
exec "$ZEN_GIT_NODE" "$ZEN_GIT_HOOK" hook
`;

/** Files the mirror keeps beside git's. */
const IMPORTED = 'zen-imported';
const TREE = 'zen-tree';
const PUSHED = 'zen-pushed';

/** Packs past which the mirror repacks itself (§6). */
export const LOCAL_REPACK = 16;

/** Create the mirror (or recreate it, if its format or zen's tree differs). */
export function initMirror(m: string, format: string, tree?: Uint8Array): void {
  if (existsSync(m)) {
    const fmt = git(['--git-dir', m, 'rev-parse', '--show-object-format']).trim();
    const t = readText(join(m, TREE));
    if (fmt === format && (!tree || !t || t === bytes.hex(tree))) {
      if (tree && !t) writeFileSync(join(m, TREE), bytes.hex(tree));
      return;
    }
    rmSync(m, { recursive: true, force: true });
  }
  mkdirSync(join(m, '..'), { recursive: true });
  git(['init', '-q', '--bare', `--object-format=${format}`, m]);
  for (const [k, v] of [
    ['uploadpack.allowFilter', 'true'],
    ['uploadpack.allowAnySHA1InWant', 'true'],
    ['receive.unpackLimit', '1'],
    ['receive.autogc', 'false'],
    ['gc.auto', '0'],
    ['core.logAllRefUpdates', 'false'],
  ] as const) {
    git(['--git-dir', m, 'config', k, v]);
  }
  writeFileSync(join(m, 'hooks', 'pre-receive'), HOOK, { mode: 0o755 });
  if (tree) writeFileSync(join(m, TREE), bytes.hex(tree));
}

function readText(p: string): string | undefined {
  try {
    return readFileSync(p, 'utf8').trim();
  } catch {
    return undefined;
  }
}

/** The pack hashes the mirror has imported (or pushed). */
export function imported(m: string): Set<string> {
  return new Set((readText(join(m, IMPORTED)) ?? '').split('\n').filter(Boolean));
}

function addImported(m: string, hash: string): void {
  appendFileSync(join(m, IMPORTED), `${hash}\n`);
}

/**
 * Sync the mirror to `state` (§5 steps 2–3): the missing packs in `seq`
 * order, each checked by `index-pack --strict`; then the refs and HEAD;
 * then a local repack past `LOCAL_REPACK` packs.
 */
export async function syncMirror(m: string, fs: UnlockedFs, state: RepoState): Promise<void> {
  const repo = state.repo;
  if (!repo) return;
  const have = imported(m);
  const tmp = join(m, 'zen-tmp');
  mkdirSync(tmp, { recursive: true });
  for (const p of state.packs) {
    if (have.has(p.hash)) continue;
    const pack = join(tmp, `${p.hash}.pack`);
    const idx = join(tmp, `${p.hash}.idx`);
    try {
      await downloadPack(fs, repo, p, pack);
      let got: string;
      try {
        got = git(['--git-dir', m, 'index-pack', '--strict', '-o', idx, pack])
          .trim()
          .split(/\s+/)
          .pop()!;
      } catch (e) {
        throw new GitError(`pack ${p.hash} (seq ${p.seq}) doesn't import: ${(e as Error).message}`);
      }
      if (got !== p.hash) throw new GitError(`pack ${p.hash}: its contents are pack ${got}`);
      const dst = join(m, 'objects', 'pack', `pack-${p.hash}`);
      renameSync(pack, `${dst}.pack`);
      renameSync(idx, `${dst}.idx`);
      addImported(m, p.hash);
    } finally {
      rmSync(pack, { force: true });
      rmSync(idx, { force: true });
    }
  }
  setRefs(m, state);
  const packs = readdirSync(join(m, 'objects', 'pack')).filter((f) => f.endsWith('.pack'));
  if (packs.length > LOCAL_REPACK) git(['--git-dir', m, 'repack', '-a', '-d', '-q']);
}

/** The mirror's refs set to zen's, and HEAD (§5 step 3). */
function setRefs(m: string, state: RepoState): void {
  const have = refsOf(m);
  let cmds = '';
  for (const r of have.keys()) if (!state.refs.has(r)) cmds += `delete ${r}\n`;
  for (const [r, o] of state.refs) if (have.get(r) !== o) cmds += `update ${r} ${o}\n`;
  if (cmds) git(['--git-dir', m, 'update-ref', '--stdin'], { input: cmds });
  git(['--git-dir', m, 'symbolic-ref', 'HEAD', state.repo!.head]);
}

/** The mirror's refs: name → oid. */
export function refsOf(m: string, env?: NodeJS.ProcessEnv): Map<string, string> {
  const out = git(['--git-dir', m, 'for-each-ref', '--format=%(objectname) %(refname)'], {
    ...(env ? { env } : {}),
  });
  const refs = new Map<string, string>();
  for (const line of out.split('\n')) {
    const at = line.indexOf(' ');
    if (at > 0) refs.set(line.slice(at + 1), line.slice(0, at));
  }
  return refs;
}

/** The hook records the pack it pushed: its hash and the tips it brings. */
export function recordPushed(m: string, hash: string, tips: string[]): void {
  writeFileSync(join(m, PUSHED), [hash, ...tips].join('\n'));
}

/**
 * After `receive-pack`: a pack the hook pushed counts as imported when the
 * mirror has its tips (the quarantine was moved in), else it is left to the
 * next sync to download.
 */
export function settlePushed(m: string): void {
  const p = join(m, PUSHED);
  const lines = (readText(p) ?? '').split('\n').filter(Boolean);
  rmSync(p, { force: true });
  const [hash, ...tips] = lines;
  if (!hash) return;
  for (const t of tips) {
    try {
      git(['--git-dir', m, 'cat-file', '-e', t]);
    } catch {
      return;
    }
  }
  addImported(m, hash);
}

// ------------------------------------------------------------------ the lock

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

function alive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (e) {
    return (e as NodeJS.ErrnoException).code === 'EPERM';
  }
}

/**
 * Hold the mirror exclusively (§6): a directory `<mirror>.lock` holding the
 * holder's pid, taken over when that pid is gone. Returns the release.
 */
export async function lockMirror(m: string, timeoutMs = 600_000): Promise<() => void> {
  const dir = `${m}.lock`;
  mkdirSync(join(m, '..'), { recursive: true });
  const end = Date.now() + timeoutMs;
  let since = 0;
  for (;;) {
    try {
      mkdirSync(dir);
      writeFileSync(join(dir, 'pid'), String(process.pid));
      let held = true;
      const release = () => {
        if (!held) return;
        held = false;
        process.off('exit', release);
        rmSync(dir, { recursive: true, force: true });
      };
      process.on('exit', release);
      return release;
    } catch (e) {
      if ((e as NodeJS.ErrnoException).code !== 'EEXIST') throw e;
    }
    const pid = Number(readText(join(dir, 'pid')));
    if (pid > 0) {
      since = 0;
      if (!alive(pid)) {
        rmSync(dir, { recursive: true, force: true });
        continue;
      }
    } else {
      // Made, its pid not written yet; a holder that died there leaves it so.
      since ||= Date.now();
      if (Date.now() - since > 10_000) {
        rmSync(dir, { recursive: true, force: true });
        continue;
      }
    }
    if (Date.now() > end) throw new GitError(`the mirror ${m} stays locked by pid ${pid || '?'}`);
    await sleep(50);
  }
}

/** Run a git service on the mirror with stdio inherited; resolves with its exit code. */
export function runService(
  service: 'upload-pack' | 'receive-pack',
  m: string,
  env: NodeJS.ProcessEnv,
): Promise<number> {
  return new Promise((ok, fail) => {
    const c = spawn('git', [service, m], { stdio: 'inherit', env });
    c.once('error', fail);
    c.once('exit', (code, sig) => ok(code ?? (sig ? 128 : 1)));
  });
}
