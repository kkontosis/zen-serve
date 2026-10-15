// Playwright global setup: bundle the example apps (apps/dist) once per run.
import { execFileSync } from 'node:child_process';

export default function setup(): void {
  execFileSync(process.execPath, ['scripts/build-apps.mjs'], { stdio: 'inherit' });
}
