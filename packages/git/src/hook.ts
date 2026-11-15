// The mirror's pre-receive hook (spec/git.md §4, §7): the push's pack cut
// inside the quarantine, then one zen transaction. Exit 0 lets git update
// the mirror's refs; anything else refuses the whole push.
import { spawn } from 'node:child_process';
import {
  closeSync,
  createWriteStream,
  mkdirSync,
  openSync,
  readFileSync,
  readSync,
  rmSync,
  statSync,
} from 'node:fs';
import { join } from 'node:path';
import { isCode } from '@zen/client';
import { git, recordPushed, refsOf } from './mirror.js';
import { errText, signIn } from './signin.js';
import { type PushPack, push, type RefUpdate } from './store.js';
import { GitError, parseRepoUrl, type RepoUrl } from './url.js';

const ZERO = /^0+$/;

/** The `old new ref` lines git gives the hook on stdin. */
export function parseUpdates(input: string): RefUpdate[] {
  const out: RefUpdate[] = [];
  for (const line of input.split('\n')) {
    if (!line.trim()) continue;
    const [old, nw, ref] = line.split(' ');
    if (!old || !nw || !ref) throw new GitError(`a hook line git didn't write: ${line}`);
    out.push({ ref, old: ZERO.test(old) ? null : old, new: ZERO.test(nw) ? null : nw });
  }
  return out;
}

/** The object count from a v2 `.idx` (the last fanout entry). */
export function idxObjects(idx: string): number {
  const fd = openSync(idx, 'r');
  try {
    const b = Buffer.alloc(4);
    readSync(fd, b, 0, 4, 8 + 255 * 4);
    return b.readUInt32BE(0);
  } finally {
    closeSync(fd);
  }
}

/**
 * Cut the push's pack (§4 step 1): the new tips, excluding every ref the
 * mirror has (zen's as of the sync), never thin; checked by `index-pack
 * --strict`, which writes the `.idx`. Undefined when it has no objects.
 * Runs with the hook's environment, so the quarantine is visible.
 */
export async function cutPack(
  m: string,
  tips: string[],
  dir: string,
): Promise<PushPack | undefined> {
  if (!tips.length) return undefined;
  const env = process.env;
  const exclude = [...new Set(refsOf(m, env).values())].map((o) => `^${o}`);
  mkdirSync(dir, { recursive: true });
  const packPath = join(dir, `push-${process.pid}.pack`);
  const idxPath = join(dir, `push-${process.pid}.idx`);
  await new Promise<void>((ok, fail) => {
    const c = spawn('git', ['pack-objects', '--revs', '--stdout', '-q'], {
      env,
      stdio: ['pipe', 'pipe', 'inherit'],
    });
    const out = createWriteStream(packPath);
    c.stdout!.pipe(out);
    c.once('error', fail);
    let code: number | null = null;
    let closed = false;
    const done = () => {
      if (code === null || !closed) return;
      code === 0 ? ok() : fail(new GitError(`git pack-objects exited ${code}`));
    };
    out.once('close', () => {
      closed = true;
      done();
    });
    c.once('exit', (c2) => {
      code = c2 ?? 1;
      done();
    });
    c.stdin!.end(`${[...tips, ...exclude].join('\n')}\n`);
  });
  const hash = git(['index-pack', '--strict', '-o', idxPath, packPath], { env })
    .trim()
    .split(/\s+/)
    .pop()!;
  const objects = idxObjects(idxPath);
  if (!objects) {
    rmSync(packPath, { force: true });
    rmSync(idxPath, { force: true });
    return undefined;
  }
  return { packPath, idxPath, hash, size: statSync(packPath).size, objects };
}

/** `zen-git hook`: run by the mirror's pre-receive script. */
export async function hook(): Promise<number> {
  const url = parseRepoUrl(process.env.ZEN_GIT_URL ?? '');
  const m = process.env.ZEN_GIT_MIRROR;
  if (!m) throw new GitError('ZEN_GIT_MIRROR is not set: the hook runs under git-remote-zen');
  const updates = parseUpdates(readFileSync(0, 'utf8'));
  if (!updates.length) return 0;
  const format = git(['rev-parse', '--show-object-format'], { env: process.env }).trim();
  const tips = [...new Set(updates.flatMap((u) => (u.new ? [u.new] : [])))];
  const pack = await cutPack(m, tips, join(m, 'zen-tmp'));
  try {
    return await pushIt(url, m, format, updates, tips, pack);
  } catch (e) {
    // fs write is the push's right (§10); the topic's append, the event's.
    if (isCode(e, 'forbidden')) {
      process.stderr.write(`zen: push refused: ${errText(e)} (spec/git.md §10)\n`);
      return 1;
    }
    throw e;
  } finally {
    if (pack) {
      rmSync(pack.packPath, { force: true });
      rmSync(pack.idxPath, { force: true });
    }
  }
}

async function pushIt(
  url: RepoUrl,
  m: string,
  format: string,
  updates: RefUpdate[],
  tips: string[],
  pack: PushPack | undefined,
): Promise<number> {
  const z = await signIn(url.server, url.fs);
  try {
    const r = await push(z.db, z.fs, {
      repo: url.repo,
      format: format as 'sha1' | 'sha256',
      updates,
      device: z.device,
      ...(pack ? { pack } : {}),
      ...(process.env.ZEN_GIT_TEST_DIE_AFTER_UPLOAD
        ? {
            afterUpload: () => {
              process.kill(process.pid, 'SIGKILL');
            },
          }
        : {}),
    });
    if (!r.ok) {
      process.stderr.write(`zen: push refused: ${r.why}\n`);
      return 1;
    }
    if (pack) recordPushed(m, pack.hash, tips);
    return 0;
  } finally {
    await z.close();
  }
}
