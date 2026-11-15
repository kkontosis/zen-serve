// Running git (spec/git.md §6): without the repository variables git sets
// for a helper or a hook, which would override `--git-dir`.
import { execFileSync } from 'node:child_process';
import { GitError } from './url.js';

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
      maxBuffer: 1 << 30,
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
