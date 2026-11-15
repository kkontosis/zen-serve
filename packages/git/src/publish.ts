// The public mirror (spec/git.md §13): a plain directory for git's dumb HTTP
// protocol, served by the server's `/unencrypted`.
import { mkdirSync, mkdtempSync, renameSync, rmSync } from 'node:fs';
import { join } from 'node:path';
import { git } from './gitcmd.js';
import { lockMirror, mirrorPath, syncRepo } from './mirror.js';
import type { Opened } from './signin.js';
import { GitError, type RepoUrl } from './url.js';

/**
 * Write `<dir>/<repo>.git` from zen (§13): the mirror synced under its lock,
 * cloned into a temporary directory beside the copy, one pack, the dumb-HTTP
 * files, then swapped in by renames. Returns the copy's path.
 */
export async function publish(
  z: Pick<Opened, 'db' | 'fs'>,
  u: RepoUrl,
  dir: string,
  env: NodeJS.ProcessEnv = process.env,
): Promise<string> {
  const m = mirrorPath(u, env);
  mkdirSync(dir, { recursive: true });
  const dst = join(dir, `${u.repo}.git`);
  const tmp = mkdtempSync(join(dir, `.${u.repo}.git.tmp-`));
  try {
    const release = await lockMirror(m);
    try {
      const state = await syncRepo(m, z, u.repo);
      if (!state.repo) throw new GitError(`${u.url}: no such repo`);
      git(['clone', '-q', '--mirror', '--no-local', m, tmp]);
      // The copy names no local path, and runs no hooks.
      git(['--git-dir', tmp, 'remote', 'remove', 'origin']);
      git(['--git-dir', tmp, 'symbolic-ref', 'HEAD', state.repo.head]);
    } finally {
      release();
    }
    rmSync(join(tmp, 'hooks'), { recursive: true, force: true });
    // -n: the dumb-HTTP files are written next, once HEAD and the pack are final.
    git(['--git-dir', tmp, 'repack', '-a', '-d', '-q', '-n']);
    git(['--git-dir', tmp, 'update-server-info']);
    // Two renames; another publish of the same copy may be swapping too.
    const old = `${tmp}-old`;
    for (let i = 0; ; i++) {
      try {
        renameSync(dst, old);
      } catch (e) {
        if ((e as NodeJS.ErrnoException).code !== 'ENOENT') throw e;
      }
      try {
        renameSync(tmp, dst);
        break;
      } catch (e) {
        const code = (e as NodeJS.ErrnoException).code;
        if ((code !== 'ENOTEMPTY' && code !== 'EEXIST') || i >= 4) throw e;
      } finally {
        rmSync(old, { recursive: true, force: true });
      }
    }
    return dst;
  } finally {
    rmSync(tmp, { recursive: true, force: true });
  }
}
