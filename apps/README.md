# Example apps

Five small web apps on `@zen/db`, `@zen/fs` and `@zen/loro`, specified in [`docs/EXAMPLES.md`](../docs/EXAMPLES.md) and built in milestone 5 step 10, plus `hello`, the smallest app, which is the template of the others and the smoke test of the app build.

`echo` (milestone 5.5 step 5) is a relayed TCP service on `@zen/svc` (spec/svc.md): each line a peer sends on the service's port is answered with `<n>: <line>` and stored in `lines`, in one transaction. Its handler runs as a Node worker (`workers/handler.ts`) or in the page (the Serve button). The tests kill handler workers mid-stream and check that every line is answered and stored exactly once, in order. Since step 8 the same handler also serves `echo-tls` behind `tls()` (TLS 1.3 and 1.2, the certificate in the `certs` table; the page's Serve TLS button, or `ECHO_LAYER=tls` for the worker), and a `mirror` handler serves `mirror-tls`: a worker killed mid-stream leaves the TLS connection going, one killed mid-handshake leaves a connection the server aborts, and 20 MB come back intact through two kills. Since step 10 it serves `echo-ssh` behind `ssh()` (`ECHO_LAYER=ssh`, or the page's Serve SSH button): users log in with a key of the `ssh_keys` table (the host key is in `certs`), and a channel runs `cat`, `lines` (the line echo, with its rows), `exit <n>`, or `git-upload-pack`, a stub repository's ref advertisement that `git ls-remote ssh://…` lists (real git comes with milestone 10). With OpenSSH as the client, 50 MB come back intact through `cat` across two worker kills with rekeys every 4 MB, and a worker killed between the client's KEXINIT and its key-exchange init leaves the key exchange to its successor. Since step 11 both also run as `tcp-auth` services, `echo-auth` and `echo-ssh-auth` (`ECHO_LAYER=auth` and `ssh-auth`), which members reach through zen-serve with `zen-connect`: the tests run OpenSSH and `git ls-remote` with zen-connect as their `ProxyCommand` (a member without `append` is refused), and Node peers through `zen-connect --listen`, with a handler worker killed mid-stream. Since step 12, `echo-inbox` (`ECHO_LAYER=inbox`) has a sealed inbox: the server stores its input sealed to a key whose seed is in `certs`, until a `STARTTLS` line makes the handler call `unseal()`. The tests check that lines after it arrive plain, that a worker killed mid-stream across the switch leaves every line answered once, and that a handler without the seed closes the connection.

`mail` (milestone 5.5 step 13a) is an inbound mail server: SMTP (RFC 5321, with STARTTLS, SIZE, PIPELINING and 8BITMIME) on `svc/smtp`, served by `@zen/svc` with `starttls()` and a sealed inbox, the certificate and the inbox seed in its `certs` table. Recipients are the addresses of `mailboxes`. DATA is staged as `parts` rows as it arrives, each step's bytes committed with its ack; at its end one commit stores a `messages` row for each recipient (with a `Received:` header), deletes the draft, emits a `message` event on `mail/received` and carries the `250`, so a message is stored exactly once whatever dies when. Its handler runs as a Node worker (`workers/smtp.ts`), which also sweeps drafts left by connections that died mid-DATA. The tests send mail with Python's `smtplib`, plain and over STARTTLS with TLS 1.2 and 1.3, check refusals, pipelining and that plaintext injected after `STARTTLS` is never read as a command, and SIGKILL workers between the end of DATA and the reply and in the middle of a 3 MB message.

Since step 13b `mail` reads its mail back. IMAP (a subset of RFC 3501, with IDLE and LITERAL+) runs on `svc/imaps` over implicit TLS with the same certificate (`workers/imap.ts`); the service needs `idle_secs = 1860`, since an idling client may be quiet for 29 minutes. Clients log in with their zen name or one of their addresses and an **app password**, which the page makes (20 random bytes as four base32 groups; only its SHA-256 is stored, in `app_passwords`) and revokes; a revoked password ends its sessions at their next step. INBOX is the only mailbox: SELECT, FETCH (bodies, sections, partials; no ENVELOPE or BODYSTRUCTURE), STORE, SEARCH, EXPUNGE and IDLE. An idling session is woken (`c.wakeOn(user)`) by `wakeOnMail` when mail arrives or the mailbox changes elsewhere, a STORE in another session or the page, announced on `mail/changed`. The page shows the inbox live, opens a message (which marks it read), flags, marks unread and deletes. The tests use Python's `imaplib` and a Node TLS client: an idling client hears of new mail, flags and expunges from elsewhere; a worker SIGKILLed in the middle of a 10 MB FETCH leaves its successor to finish the literal, with `\Seen` set once; one killed while a client idles leaves its successor to tell the client of the next message. The Playwright spec runs both services as workers and checks that what the page does reaches an idling IMAP client.

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
| `harness.ts` | tests: `startWorld` (a server serving `apps/dist`, members with the app's grants, relayed `services` on free ports), `openApp` (a page signed in), `spawnWorker` / `workerEnv`, `until`, `net` (an offline switch on a Node session) |
| `fractional.ts` | position keys between two others (`keyBetween`, `keysBetween`) |

## Build, serve, test

```sh
cargo build -p zen-server && scripts/build-wasm.sh   # once
npm run build:apps                 # apps/dist/<name>/ (bundles, WASM), workers/*.mjs
node scripts/build-apps.mjs kanban --watch
npx vitest run apps/<name>         # scenarios (builds the apps and zen-connect first; ZEN_APPS_BUILT=1 skips that)
npx playwright test --project=apps # browser specs (builds the apps first)
```

To try an app by hand, run zen-serve with `unencrypted_dir = "<repo>/apps/dist"` and open `http://localhost:<port>/unencrypted/<name>/index.html`. A member needs a password (`session.setPassword`) and the fs a passphrase slot; `startWorld` in `harness.ts` shows the set-up.

## Rules for building an app

* Write only under `apps/<name>/`. A change needed in a package, in `apps/shared` or in the wiring is asked for, not made.
* No new dependencies.
* Before calling it done: `npx biome check apps/<name>`, `npx tsc -p apps/tsconfig.json`, `npx vitest run apps/<name>` three times, `npx playwright test --project=apps apps/<name>`. Report every flaky run.
* Kill only the processes a test started (`WorkerProcess.kill`), never by name or pattern.
