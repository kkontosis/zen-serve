import { defineConfig } from '@playwright/test';

// Browser smoke tests of @zen/client (packages/client/browser). Build first:
//   cargo build -p zen-server && scripts/build-wasm.sh && npm run build -w @zen/client
export default defineConfig({
  testDir: 'packages/client/browser',
  testMatch: '*.spec.ts',
  timeout: 120_000,
  use: { browserName: 'chromium', headless: true },
  reporter: 'list',
});
