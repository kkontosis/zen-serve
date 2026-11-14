// Playwright global setup: bundle the example apps (apps/dist) and the
// content origin's files (packages/sw/content) once per run.
import { execFileSync } from 'node:child_process';

export default function setup(): void {
  execFileSync(process.execPath, ['scripts/build-apps.mjs'], { stdio: 'inherit' });
  execFileSync(process.execPath, ['scripts/build-sw.mjs'], { stdio: 'inherit' });
}
