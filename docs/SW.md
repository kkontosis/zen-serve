# Viewing an encrypted site in a browser

A zen-serve deployment can serve a directory of an encrypted fs over plain HTTP, decrypted in the browser by a service worker. A static website, a set of documents or a media library kept in zen-fs is then viewable in a browser by the members who unlocked it, and offline from the worker's replica. This guide covers deploying it. spec/sw.md is the reference, and `@zen/sw` (packages/sw) is the code.

## 1. What it is

* **A content origin:** a second host name on the same server, for example `view.example.org` next to the app's `zen.example.org`. It serves nothing of the app. It serves only:
  * the worker and two small pages (bootstrap and enrollment) under `/_zen/`
  * the API under `/v1/`
* **The worker** answers every other path from a replica of the mounted trees. It signs in as a device of the member and decrypts in the browser, so the server sees only what any replica shows (sw.md §9).
* **Mounts** map a path prefix to a directory of a tree. For example, `/docs/` maps to `/site` of fs 1's tree `ab12…`.
* **No scripts run in what it serves.** Every response carries `Content-Security-Policy: sandbox allow-same-origin …; script-src 'none'` (sw.md §4). Content keeps the content origin, which holds the worker's device key, so it must not run code there. Sites that need JavaScript don't work.
  * Milestone 9's spike measured the alternatives: in Chromium, an opaque origin's pages skip the worker. The alternatives are kept in TECH_DEBT TD-SW-SCRIPTS.
* **One content origin per deployment.** Its sites are mounts.

## 2. DNS

Point the content host at the same address as the app host. One listener answers both, routed by `Host` (OPS.md §8).

```
zen.example.org.   IN A  203.0.113.10
view.example.org.  IN A  203.0.113.10     ; or a CNAME to zen.example.org
```

Pick a name of its own, not a path of the app origin: the separation is the origin.

## 3. TLS

A service worker needs a secure context. Browsers allow it only over HTTPS, except on `localhost` (and Chromium's `*.localhost`, which the tests use).

* **Native TLS** (operations.md §8.1) loads one certificate. Its names must cover both hosts: a certificate with both names in its subject alternative names, or a wildcard.

  ```toml
  [tls]
  cert = "/etc/zen/zen-and-view.pem"   # SAN: zen.example.org, view.example.org
  key = "/etc/zen/zen-and-view.key"
  ```

* **Or a reverse proxy** (operations.md §8.3) that terminates TLS for both names. It must:
  * pass `Host` through, or every request for the content host reaches the app's routes (OPS.md §8)
  * keep the headers zen-serve sends on `/_zen/`: the CSP with Trusted Types, and `Service-Worker-Allowed: /` on `sw.js`
  * forward the WebSocket upgrade for `/v1/stream`, as for the app origin
* **Renewal:** zen-serve has no ACME yet, and it reads the certificate once at start-up, so a renewed certificate needs a restart (TECH_DEBT TD-TLS-ACME). A proxy that does ACME avoids the restart.

## 4. The config

```toml
public_origins = ["https://zen.example.org"]     # the app origins: enrollment trusts these

[content]
origins = ["https://view.example.org"]           # routed by Host
dir = "/srv/zen/sw"                              # the built content dir (§5)
```

The server refuses to start when:
* `[content]` is set and `public_origins` is empty
* an origin in `[content] origins` is also in `public_origins`
* an origin isn't of the form `scheme://host[:port]`
* the origin list is empty, or `dir` is missing

`/v1/info` lists `content_origins`, and the content origin is an accepted sign-in origin. Nothing from `/unencrypted` is served there.

## 5. Building and installing the content dir

```sh
scripts/build-wasm.sh
node scripts/build-sw.mjs --out /srv/zen/sw
```

* **What it writes:** `sw.js` (a module worker), `boot.html` and `boot.js`, `enroll.html` and `enroll.js`, `pages.css` and `zen_wasm_bg.wasm`. The server serves them at `/_zen/`.
* **If `boot.html` is missing,** every navigation on the content origin gets the sandboxed 503 `starting`, and the worker is never registered.
* **An update:** bump `BUILD` in `packages/sw/web/sw.ts`, rebuild, and replace the directory's files.
  * Browsers pick up the new worker at their next update check, which is at the latest a navigation a day after the last check. It takes over at once.
  * Its activation drops the old build's cache.

## 6. Enrolling a viewer

Each browser that views the site enrolls its content origin once, from the app:

```ts
import { enrollViewer } from '@zen/sw';

await enrollViewer({
  session,                 // the member, signed in
  identity,                // the member's SigningIdentity
  admin: identity,         // an admin's, to add the device to the ACL
  fs: [ufs],               // the unlocked fs the mounts name
  mounts: [{ name: 'docs', fs: 1, tree, base: '/site' }],
  contentOrigin: 'https://view.example.org',
});
```

* **The flow:**
  * The app opens `/_zen/enroll.html` on the content origin as a popup.
  * The page shows the app's origin, the mounts and the new device's fingerprint, and asks the user to confirm.
  * Only then does the app certify the device, add a device keyslot to each fs, and (with `admin`) add the device to the ACL.
  * The page keeps the device and the mounts in the content origin's vault and starts the worker (sw.md §8).
* **The member's signing identity is needed.** A browser app signed in with a password holds none, so the example apps have no "enable viewing" button yet (TECH_DEBT TD-SW-IDENTITY).
* **Without an admin,** the device is certified but not in the ACL. Its sign-in is refused, and the worker answers 503 `locked` until an admin adds the fingerprint the page showed (TD-SW-SESSION-HANDOVER would remove this step).
* **Re-enrolling** replaces the device and the mounts.
* **Revoking:** an admin removes the device's certificate from the ACL, and a member removes its keyslots. Clearing the content origin's site data removes it from that browser.

## 7. What is served

The mount `docs` with `base: '/site'` maps `https://view.example.org/docs/a/b.html` to `/site/a/b.html` in the tree:
* **Directories:** a directory gets a 301 to the path with a trailing `/`. Then it gets its index file (`index.html` by default), or a listing if the mount has `listing: true`, or 404. A mount with `root: true` also answers `/`.
* **Media types:** a file's `zen.content_type` xattr, or its extension (sw.md §5).
* **Single ranges:** 206 for one range, 416 past the end, and `If-Range` against a strong ETag. Media seeks this way.
* **Offline:** what the replica keeps is served with the server unreachable. Anything else gets 503 `offline`.
* **Errors** are short pages with a `Zen-Reason` header (sw.md §7):
  * `locked`: not enrolled, or the device can't sign in
  * `offline`
  * `starting`: the worker didn't start within 15 s
  * `integrity`: a chunk didn't verify (502)
* **Every response,** errors included, carries the sandbox policy, `nosniff`, `no-referrer`, `Cross-Origin-Resource-Policy: same-origin` and `Cache-Control: no-store`. Pages can't load anything from another origin, and forms don't submit.

## 8. Costs

docs/STATS.md §7.6 has the measurements: throughput through the worker against `Tree.readStream`, the time to answer from a running and a stopped worker, seeks, and the worker's memory. In short:
* **A stopped worker answers in about 0.1 s:** the wasm comes from Cache Storage, the vault resumes the session, the fs unlocks, and the replica loads from IndexedDB. A running one answers a small file in about 3 ms.
* **Seeks:** a 64 KiB range takes about 7 ms, and an `<audio>` seek 50–70 ms.
* **A large file streams at 42–49 MiB/s,** a third of `readStream`'s rate in Node. The cost is in the browser, not yet found (TECH_DEBT TD-SW-THROUGHPUT).
* **Memory while streaming: about 30 MiB live.** That is the worker's replica's fetched content waiting to be written (`pendingBytes`, 8 MiB) and the read's windows (up to 4 MiB each, `readAhead` 3), besides the wasm's memory.

## 9. Limits

* **Tested in Chromium only** (Playwright). Firefox and WebKit are untested, both for the isolation (TD-SW-SCRIPTS item 4) and for the worker.
* **No scripts,** so no single-page apps.
* **One content origin per deployment.** Per-site origins need scoped devices (TD-SW-SCRIPTS item 1).
* **The worker only reads.** Writes go through the app.

## 10. Troubleshooting

* **Every page is the bootstrap page, reloading:** the worker isn't registering. Look in the browser console for:
  * a TLS error: not a secure context
  * a blocked `Service-Worker-Allowed`: a proxy dropped it
  * a 404 for `/_zen/sw.js`: a wrong `[content] dir`
* **503 `locked`:** the browser isn't enrolled, the device isn't in the ACL yet, or its keyslot was removed.
* **503 `starting`:** the worker took more than 15 s to start, usually because the server was slow to answer its sign-in. It tries again on the next request.
* **The app's routes on the content host:** the proxy didn't pass `Host` through.
* More pitfalls: OPS.md §8.
