import { defineConfig } from '@playwright/test';

// Browser tests. Build first:
//   cargo build -p zen-server && scripts/build-wasm.sh && npm run build -w @zen/client
// * smoke: @zen/client in Chromium (packages/client/browser)
// * apps: the example apps (apps/*/e2e), bundled by the global setup
export default defineConfig({
  timeout: 120_000,
  use: { browserName: 'chromium', headless: true },
  reporter: 'list',
  projects: [
    { name: 'smoke', testDir: 'packages/client/browser', testMatch: '*.spec.ts' },
    { name: 'apps', testDir: 'apps', testMatch: '*/e2e/*.spec.ts' },
  ],
  globalSetup: './scripts/playwright-setup.ts',
});
