# Serving zen-fs over HTTP through a service worker

A service worker on a **content origin** maps URLs to nodes of the members' filesystems and answers each request with a decrypted, streamed `Response`. A static website, a set of documents or a media library kept in an encrypted fs is then viewable in a browser, by the members who unlocked it, and offline from the worker's replica. The HTML is served unchanged.

The server sees nothing it wouldn't see from any replica (§9). The pages served run **no scripts** (§4), so content can't reach the keys the worker holds, even though it keeps the worker's origin. Sites that need JavaScript are out of scope (TECH_DEBT TD-SW-SCRIPTS).

Built in milestone 9 (`@zen/sw`, docs/MILESTONE-9.md). The isolation model rests on browser behaviour measured there (step 9.1): a document in an opaque origin (`sandbox` without `allow-same-origin`) isn't controlled by the worker in Chromium, so its subresources would skip the worker. Content therefore keeps the origin and loses scripts instead.

## 1. The content origin

* **One per deployment,** next to the app origin(s). It is configured on the server: `[content] origins`, the content origins, and `[content] dir`, the directory the `/_zen/` files are served from (api.md §10; deploying, docs/SW.md) and routed by the request's `Host`. It needs `public_origins` set: they are the app origins enrollment trusts (§8).
* **The server answers on it** only:
  * `/_zen/*`: fixed, trusted files. They are the worker (`/_zen/sw.js`, served with `Service-Worker-Allowed: /` and registered with scope `/`), the bootstrap page, the enrollment page, their scripts and wasm, and `/_zen/config.json` (§8).
  * `/v1/*`: the API, as on the app origin. The content origin is an accepted sign-in origin.
* **Every other path is the worker's.** The worker passes `/_zen/*` and `/v1/*` to the network and answers everything else itself. Before the worker is installed, or after the site data was cleared, those requests reach the server:
  * a navigation (`Sec-Fetch-Mode: navigate`) gets the bootstrap page, which registers the worker and then reloads the same URL
  * anything else gets 503 with `Zen-Reason: starting`, under §4's headers
  * nothing from `/unencrypted` is served on the content origin
* **The bootstrap and enrollment pages run scripts,** under a strict policy: `script-src 'self'`, Trusted Types, `frame-ancestors 'none'`. They reflect no input into the page. Content can navigate to them but can't run code in them.

## 2. Mounts

A mount names a directory of a tree:

```
{ name, fs: u32, tree: bytes(16), base: text, index: [text] = ["index.html"], listing: bool = false, root: bool = false }
```

* `name` is `[a-z0-9-]{1,63}`, unique on the content origin. `/<name>/<path>` maps to `<base>/<path>` in the tree. `base` is an absolute path in the tree (`/` for its root).
* At most one mount has `root`. It also answers `/` and the paths below it that don't start with another mount's name. Without a root mount, `/` is 404.
* Mounts are kept in the content origin's sealed store (`sw/mounts`, the device layer), written by enrollment (§8).

## 3. URL → node

* **Methods:** GET and HEAD. Anything else is 405 with `Allow: GET, HEAD`.
* **Segments:** the path is split on `/`, and each segment is percent-decoded and read as UTF-8. Refused with 400:
  * invalid percent-encoding or UTF-8
  * an empty segment other than a trailing one (`/a//b`)
  * a segment `.` or `..`
  * a decoded segment containing NUL or `/`
* **The query string** is ignored.
* **Mounts:** the first segment names a mount; the rest is the path below its `base`.
  * `/<name>` (no trailing `/`) is the mount's base directory, so it gets the 301 below, to `/<name>/`.
  * `/` goes to the root mount's base, or is 404 without a root mount.
  * A first segment that names no mount: the root mount gets the whole path, or it is 404 without one.
* **Lookup** is by display name, as the replica walks paths (fs.md §8 and §11), so a conflict copy is reachable under the name the replica shows for it.
* **Files** are served as below (§5–§6). A file's path with a trailing `/` names nothing: 404.
* **Directories:**
  * Without a trailing `/`: 301 to the same path with one, keeping the query, so relative links resolve.
  * With one: the first of the mount's `index` names that is a file is served.
  * Failing that: a generated listing if `listing` is set, otherwise 404.
* **A listing** is a short HTML page with no scripts and no ETag, under §4's headers:
  * the directories first, then the rest, each group sorted by name (UTF-16 code units)
  * each name HTML-escaped, its link percent-encoded segment by segment, with a trailing `/` for a directory
  * a `../` link first, except at the mount's base
  * no symlinks (they are 404)
* **Symlinks** are 404.

## 4. Headers

Every response the worker makes carries these, whatever its status (200, 206, 301, 304, 4xx, 5xx):

```
Content-Security-Policy: sandbox allow-same-origin allow-popups allow-popups-to-escape-sandbox allow-downloads;
  default-src 'self' data: blob:; script-src 'none'; object-src 'none'; style-src 'self' 'unsafe-inline';
  base-uri 'none'; form-action 'none'; frame-ancestors 'none'
X-Content-Type-Options: nosniff
Referrer-Policy: no-referrer
Cross-Origin-Resource-Policy: same-origin
Cache-Control: no-store
```

* **No script runs** in a page served this way: inline scripts, event handlers, `javascript:` URLs, and scripts in SVG documents are all refused, and with `script-src 'none'` scripts aren't even fetched (measured in step 9.1). So content can't read the origin's IndexedDB, where the worker's vault and replica live, though it keeps the origin. Keeping the origin is what keeps the page controlled by the worker.
* **Allowed:** styles (inline too), images, media, fonts and frames from the content origin; links opening new tabs, which escape the sandbox but land on responses that carry it again; downloads.
* **Refused:** forms, plugins, and any load from another origin. The last also keeps a page from telling a third party what is being read.
* **The text above is the reference.** The server's constant (`statics.rs` `SANDBOX_CSP`, sent on the content origin's 503 `starting`, §1) and `@zen/sw`'s are both tested against it, its whitespace folded to single spaces, so the two can't drift apart.
* **The policy is fixed.** A configurable policy would be one setting away from letting scripts run next to the vault (TD-SW-SCRIPTS).
* `Cache-Control: no-store` keeps plaintext out of the HTTP cache.

## 5. Media types

`Content-Type` is, in order:
1. The node's `zen.content_type` xattr (formats.md §11.2), when it is a media type as RFC 9110 §8.3.1 defines it (`type "/" subtype *( OWS ";" OWS parameter )`).
2. Otherwise by the name's extension, case-insensitive:

   | Extension | Type |
   |---|---|
   | `html`, `htm` | `text/html; charset=utf-8` |
   | `css` | `text/css; charset=utf-8` |
   | `js`, `mjs` | `text/javascript; charset=utf-8` |
   | `json` | `application/json` |
   | `txt` | `text/plain; charset=utf-8` |
   | `md` | `text/markdown; charset=utf-8` |
   | `xml` | `application/xml` |
   | `csv` | `text/csv; charset=utf-8` |
   | `svg` | `image/svg+xml` |
   | `png`, `gif`, `webp`, `avif`, `bmp` | `image/<ext>` |
   | `jpg`, `jpeg` | `image/jpeg` |
   | `ico` | `image/x-icon` |
   | `mp4`, `m4v` | `video/mp4` |
   | `webm` | `video/webm` |
   | `ogv` | `video/ogg` |
   | `mp3` | `audio/mpeg` |
   | `m4a` | `audio/mp4` |
   | `ogg`, `oga`, `opus` | `audio/ogg` |
   | `wav` | `audio/wav` |
   | `flac` | `audio/flac` |
   | `pdf` | `application/pdf` |
   | `woff`, `woff2`, `ttf`, `otf` | `font/<ext>` |
   | `wasm` | `application/wasm` |
   | `zip` | `application/zip` |
3. Otherwise `application/octet-stream`.

Serving `text/javascript` is harmless here: §4 refuses to run it.

## 6. Validators and ranges

* **The version served** is the node's current version, the newest by dot when there are siblings (fs.md §4).
* **`ETag`:** strong, `"<hex node>.<hex dot>"` (node id and version dot, lowercase hex). A version's bytes never change, so the validator is strong.
  * Only a version the server holds has one. A queued local version (fs.md §11), which a worker that doesn't write never shows, gets no `ETag`, so `If-None-Match` and `If-Range` never match it.
  * An empty file with no version at all (created, never written) has no `ETag` either.
* **The body** is that version's bytes, read by the version the `ETag` names, not by path again, so a version that replaces it during the request doesn't mix into the response.
* **`If-None-Match`** listing that ETag, or `*`, gives 304 with the ETag and §4's headers.
* **`Accept-Ranges: bytes`** on every file response.
* **`Range`** (RFC 9110 §14):
  * One range, `bytes=a-b`, `bytes=a-` or `bytes=-n`, satisfiable (`a < size`, or `n > 0` for a non-empty file): 206 with `Content-Range: bytes first-last/size` and that slice. `b` past the end is clamped. `-n` larger than the file means the whole file.
  * Unsatisfiable: 416 with `Content-Range: bytes */size`.
  * Malformed syntax, another unit, or several ranges: 200 with the whole file. RFC 9110 lets a server ignore `Range`.
  * With `If-Range`, the range applies only if it equals the ETag; otherwise 200 with the whole file. A date there never matches.
* **HEAD** gets the same status and headers as GET, with no body.
* **Bodies are streamed:** the worker reads the version window by window (`readStream`), so a response holds no more than the read-ahead in memory, whatever the file's size.

## 7. Errors

Each error is a short static HTML page under §4's headers.

| Status | When | `Zen-Reason` |
|---|---|---|
| 400 | §3's refusals | |
| 404 | no such node, a symlink, a directory without an index or listing, or no mount | |
| 405 | a method other than GET or HEAD | |
| 502 | a chunk or page of the version fails to verify (formats.md §11) | `integrity` |
| 503 | no device yet, or unlocking failed; the page links to `/_zen/enroll.html` | `locked` |
| 503 | the content isn't kept and the server can't be reached | `offline` |
| 503 | the worker's replica is still loading, or the request reached the server before the worker (§1) | `starting` |

An error after the response has started (a chunk that fails to verify, or the network lost mid-file) errors the body stream, so the browser sees a truncated body, never wrong bytes.

**The replica's errors** map to responses so:

| Error (`@zen/fs`, `@zen/client`) | Response |
|---|---|
| `not_found`, `not_dir` | 404 |
| `offline`, `network` | 503 `offline` |
| `integrity` (`IntegrityError`), `format`, a chunk or page that fails to open (`ZenCryptoError`) | 502 `integrity` |
| `closed` (the replica is restarting) | 503 `starting` |
| the worker's start-up failing with `locked` or `starting` | 503 with that reason |
| anything else | 500, with no reason |

An unexpected error is 500, never 404, so it can't pass for a missing file.

## 8. Credentials and enrollment

The worker signs in as a **device of the member**, registered in the signed ACL (auth.md §6), and unlocks each mounted fs with a device keyslot (formats.md §6). Both are set up once, from the app origin, which holds the member's keys.

**Enrollment:**
1. The app opens `https://<content origin>/_zen/enroll.html` as a popup.
2. The enrollment page makes a device key. It keeps the seed in the content origin's vault (`vault/default`, a sealed store with the device layer: CLIENT.md §10). It then posts `{type: "zen-enroll-hello", devicePublic}` to `window.opener`, with `targetOrigin` set to each app origin it trusts.
3. **The app origins trusted** are listed in `/_zen/config.json` (`{app_origins: [text]}`), which the server writes from `public_origins`, minus the content origins.
4. **Each side checks `event.origin`.** The enrollment page accepts messages only from a trusted app origin. The app accepts them only from its configured content origin.
5. **The app's session,** signed in and unlocked:
   * certifies the key (`approveDevice`, api.md §15.2)
   * adds a device keyslot on each fs it mounts (`fs.addDevice`)
   * if its user is an admin, adds the certificate to the member's ACL entry, an admin-signed ACL change (formats.md §9)
6. The app answers `{type: "zen-enroll-done", cert, identityPublic, mounts}`.
7. The enrollment page shows the app origin and the mounts and asks the user to confirm. Then it stores the device (`saveDevice`) and the mounts, registers the worker, and closes.
8. **A member who isn't an admin:** the page shows the device's fingerprint for an admin to add. Until then the worker can't sign in, and serves only what its replica keeps: nothing, on a fresh origin. A handed-over session would remove this step (TD-SW-SESSION-HANDOVER).

**On every start** (the browser stops idle workers and starts them on the next request): the worker initialises the wasm, then `vault.signIn` (the device), `vault.unlock(fs, {local})` for each mounted fs, and `openReplica` for each mounted tree. Requests wait for that start. Offline, `signIn` and `unlock` succeed from what the vault and the kept header hold (CLIENT.md §10), and the replica serves what it keeps.

**Revoking:** an admin removes the device's certificate from the ACL, and a member removes its keyslots. Clearing the content origin's site data removes the device and the replica from that browser.

**What the device can do:** it has its member's rights (rights belong to members, not devices). Nothing on the content origin can use it but the worker, since no other script runs there (§4).

## 9. Leakage

* **The server sees** what a replica of each mounted tree shows (fs.md §10): which tree, which pages and chunks are fetched, and when. It also learns that this device signs in from the content origin.
* **The page's own requests** never reach the server: the worker answers them all, from the replica. So which file is viewed, and which part, shows only as chunk fetches for what isn't kept yet.
* A pinned mount (the replica's `pin`, CLIENT.md §8) fetches everything ahead, which hides what is read at the cost of storage.
