// Vitest global setup: bundle the example apps (apps/dist) and zen-connect
// once per run, before any test file starts. Test files don't build: a
// build empties apps/dist/<app> first, and files run in parallel, so one
// file's build would remove the workers another file is starting.
import { execFileSync } from 'node:child_process';

export default function setup(): void {
  for (const script of ['scripts/build-apps.mjs', 'scripts/build-zen-connect.mjs']) {
    execFileSync(process.execPath, [script], { stdio: 'inherit' });
  }
}
