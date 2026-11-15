// The bins' entry: exit codes, and errors as `zen: <message>`.
import { initWasm } from '@zen/client';
import { GitError } from './url.js';

/** Run `fn(argv)`; a `GitError` prints its message, anything else its stack too. */
export function main(fn: (argv: string[]) => Promise<number>): void {
  initWasm()
    .then(() => fn(process.argv.slice(2)))
    .then(
      (code) => process.exit(code),
      (e: unknown) => {
        const msg =
          e instanceof GitError || !process.env.ZEN_GIT_DEBUG
            ? (e as Error).message
            : ((e as Error).stack ?? String(e));
        process.stderr.write(`zen: ${msg}\n`);
        process.exit(e instanceof GitError ? 128 : 1);
      },
    );
}
