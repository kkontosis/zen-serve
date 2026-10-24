// Vitest global setup: bundle the example apps (apps/dist) and zen-connect
// once per run, before any test file starts. Test files don't build: a
// build empties apps/dist/<app> first, and files run in parallel, so one
// file's build would remove the workers another file is starting.
// ZEN_APPS_BUILT=1 skips the build, for several vitest runs at once on one
// checkout after one build (each run would otherwise rebuild the same
// directories under the others).
import { execFileSync } from 'node:child_process';

export default function setup(): void {
  if (process.env.ZEN_APPS_BUILT) return;
  for (const script of ['scripts/build-apps.mjs', 'scripts/build-zen-connect.mjs']) {
    execFileSync(process.execPath, [script], { stdio: 'inherit' });
  }
}
