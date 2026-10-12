# Example apps

Five small web apps on `@zen/db`, `@zen/fs` and `@zen/loro`, specified in [`docs/EXAMPLES.md`](../docs/EXAMPLES.md) and built in milestone 5 step 10, plus `hello`, the smallest app, which is the template of the others and the smoke test of the app build.

## Layout of an app

```
apps/<name>/
  src/       the app's logic: functions over a Db (and a Tree or Replica), no DOM
  ui/        main.ts (the page, plain DOM), index.html, style.css
  test/      vitest scenarios, in Node (*.test.ts) and the app's grants (grants.ts)
  e2e/       Playwright specs (*.spec.ts), in Chromium
  workers/   Node workers (*.ts), if the app has any
```

* **Logic lives in `src/`**, so the vitest scenarios drive it directly. The UI calls the same functions and stays thin.
* **`ui/main.ts`** calls `boot` (`apps/shared/boot.ts`): a sign-in form (login name, password, fs passphrase), then the database opened on an IndexedDB local store sealed by a device key, and a status bar. It ends with `ready(ctx, {...})`, which sets `window.app` for the tests.
* **Grants.** Each app's `test/grants.ts` lists what its members get (EXAMPLES.md "Grants"). The tests run with those grants only, never full rights.
* **Imports.** App code imports `@zen/client`, `@zen/db`, `@zen/fs` and `@zen/loro`; the build and vitest map them to the package sources. `apps/shared` is imported by relative path. Playwright specs import only `apps/shared/harness.ts` (and the app's `test/grants.ts`), and act through the page.

## Shared code (`apps/shared`)

| File | For |
|---|---|
| `boot.ts` | the browser: `boot`, `ready`, `h` (elements), `render`, `message` |
| `node.ts` | workers and scenarios: `signInWorker` (a device key file, optionally a vault), `workerFromEnv` |
| `harness.ts` | tests: `startWorld` (a server serving `apps/dist`, members with the app's grants), `openApp` (a page signed in), `spawnWorker` / `workerEnv`, `until`, `net` (an offline switch on a Node session) |
| `fractional.ts` | position keys between two others (`keyBetween`, `keysBetween`) |

## Build, serve, test

```sh
cargo build -p zen-server && scripts/build-wasm.sh   # once
npm run build:apps                 # apps/dist/<name>/ (bundles, WASM), workers/*.mjs
node scripts/build-apps.mjs kanban --watch
npx vitest run apps/<name>         # scenarios (the worker bundles must be built)
npx playwright test --project apps # browser specs (builds the apps first)
```

To try an app by hand, run zen-serve with `unencrypted_dir = "<repo>/apps/dist"` and open `http://localhost:<port>/unencrypted/<name>/index.html`. A member needs a password (`session.setPassword`) and the fs a passphrase slot; `startWorld` in `harness.ts` shows the set-up.

## Rules for building an app

* Write only under `apps/<name>/`. A change needed in a package, in `apps/shared` or in the wiring is asked for, not made.
* No new dependencies.
* Before calling it done: `npx biome check apps/<name>`, `npx tsc -p apps/tsconfig.json`, `npx vitest run apps/<name>` three times, `npx playwright test apps/<name>`. Report every flaky run.
* Kill only the processes a test started (`WorkerProcess.kill`), never by name or pattern.
