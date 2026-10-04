# Milestone 4 plan: `@zen/client` (TypeScript + WASM, Node and browser)

## Context

The server is finished through M3.5 and the auth work: KV and commit (both modes), log, consume and leader, ephemeral and stream, the signed ACL, six sign-in methods, and the CRDT filesystem. **No client exists yet except the Rust test harness** (`crates/zen-server/tests/common/mod.rs`). zen-core already holds every client-side format (keys, tokens, seal kinds 1–6, keyslot types 1–5, identities, password key, OPAQUE, HLC, op chain) and is checked for wasm32. But it has no JS bindings.

Milestone 4 (DESIGN-3 §6) delivers `@zen/client`, the library that M5 (zen-db, the zen-fs replica, Loro) builds on. It covers:
* connection and sign-in
* unlocking keyslots
* KV and transactions
* log, consumer and leader
* stream and ephemeral
* keyslot and ACL admin
* filesystem operations

It runs on Node 22+ and in browsers. It also delivers **`zen-mount`**, a Node CLI that mounts a zen-serve fs as a local directory through FUSE (`@zen/fuse`, Step 3b).

**Decisions (user):**
* **Wire through zen-proto in WASM.** All CBOR encoding and decoding runs in Rust (zen-proto types), and the TS types are generated from them (tsify). One source of truth, and no canonical-CBOR rewrite for the ACL.
* **wasm-bindgen CLI, pinned to the lockfile's 0.2.129.** A new crate, `crates/zen-wasm`. An npm workspace under `packages/`.
* **vitest** on Node against a spawned embedded `zen-serve`, plus a **Playwright Chromium smoke test** using a virtual authenticator with PRF.
* **Passkeys through a pluggable authenticator.** The browser uses `navigator.credentials`. Node takes an injected `Authenticator`, and the test utilities ship a software one with ES256 and PRF.

**Out of scope:**
* the zen-fs local replica and POSIX API, zen-db, Loro (all M5)
* Merkle integrity (M6)
* the service worker
* a human recovery-key encoding (that's for the UI spec)
* release notes (none until M6)
* FoundationDB: the client is tested on the embedded backend only (OPS.md §4.11)

**After compaction, re-read:**
* `spec/api.md` (whole), `spec/auth.md` §6–11, `spec/formats.md` §2–7, §9–11, `spec/fs.md`
* `crates/zen-proto/src/{lib,acl}.rs`
* `crates/zen-core/src/{keys,keyslot,token,seal,fs,sig,pwkey,opaque}.rs`
* `crates/zen-server/tests/common/mod.rs`: claim, sign-in and OPAQUE flows, and the WebSocket helpers, which are the reference client
* `crates/zen-server/tests/fs.rs`: the op flows
* `spec/OPS.md` §4

Branch `claude/brave-knuth-gk03mm` = `origin/main`. Open a new short PR at the end.

## Step 0: persist the plan
Write `docs/MILESTONE-4.md` (this plan), link it from DESIGN-3 §6 item 4 like 3.5, and commit.

## Step 1: spec gaps the client exposes (⚠️ SPEC CHANGES, `spec:` commits)

1. **fs header format** (new `formats.md` §12).

   The header is "opaque to the server", but no client format exists, so two clients couldn't interoperate. Proposal:

   ```
   u8 header_version = 1 ‖ u8 suite ‖ u32 fs ‖ u32 current_epoch
   ‖ u16 n_slots ‖ n × lp(keyslot)                 (formats.md §6)
   ‖ u16 n_chain ‖ n × lp(epoch-chain record)      (kind 3, formats.md §4)
   ```

   * Clients ignore slot types they don't know.
   * **Leakage and rollback** section:
     * The slot count and types are visible.
     * The server could drop slots or serve an older header. The result is denial of service, or an older epoch that the client detects through `current_epoch` versus newer sealed objects' `key_epoch`.
     * A signed header comes with M6.
   * zen-core: `FsHeader::{encode, decode, find_slot}`, plus vectors in `spec/test-vectors/header.json`.

2. **Who may write the fs header.** `put` needs admin, so a member can't add a slot for their own passkey. The proposal for M4 is to keep this rule and document it:
   * The client API has an admin path, and a member path that hands a prepared slot to an admin.
   * Relaxing the rule later becomes `TD-FS-HEADER-SELF-SLOT`.

3. **ACL vectors.** Add `spec/test-vectors/acl.json` (a canonical CBOR document, its hash and signature). Both sides use zen-proto, but the vectors pin the format for any third client.

4. **wasm32 coverage.** CI also runs `cargo check -p zen-core --features opaque --target wasm32-unknown-unknown`. auth.md claims OPAQUE builds for wasm, but nothing checks it today.

No server behaviour changes are planned. If the client finds a server bug, the fix gets its own commit and a test in zen-server.

## Step 2: `crates/zen-wasm` (Rust, `cdylib`, wasm32 only)

The binding layer is thin. All logic stays in zen-core and zen-proto.

* **Dependencies:**
  * `zen-core` with `opaque`
  * `zen-proto` with a new optional `ts` feature: `tsify-next` and `wasm-bindgen` derives on the request and response types. Native builds are unaffected; CI checks both.
  * `serde-wasm-bindgen`, `wasm-bindgen = "=0.2.129"`
* **Wire functions.** `encode_<Type>(js) -> Uint8Array` and `decode_<Type>(bytes) -> js` for every zen-proto request, response and `Frame`. A macro generates them from one list. Byte fields map to `Uint8Array` through serde_bytes.
* **Crypto handles.** Opaque JS classes own the secrets in WASM memory (`free()` zeroizes them):
  * `FsKeys`: generate, from_bundle, rotate, previous, and the per-epoch derived keys
  * `NameChain` and `TopicKeys`
  * `SigningIdentity` and `DeviceSecret`
  * `PasswordKey`
  * `OpaqueRegistration` and `OpaqueLogin`
  * `Clock` (HLC)
* **Free functions:**
  * seal and open for kinds 1, 2, 4, 5 and 6
  * the keyslot creators (types 1–5) and `open_slot`
  * the `FsHeader` codec
  * `session_message`, `normalize_login`, `passkey_credential_id`
  * `move_bytes`, `meta_bytes`, `write_bytes`, `chain_next`
  * `RangeHasher`
  * ACL build, sign and verify (`AclDoc`, then canonical CBOR, then `SIG_ACL`)
* **Randomness:** `OsRng` through getrandom `wasm_js`, already configured.
* **Errors** become a JS `ZenCryptoError` with a stable `code`.
* **Not a workspace default build:** it's a member, but `cargo test --all` on native only checks that it compiles (a `cfg` gate keeps it to stubs). A `cargo build -p zen-wasm --target wasm32-unknown-unknown --release` script produces the module.

## Step 3: packages (npm workspaces, root `package.json`, `package-lock.json` committed)

```
packages/
  zen-wasm/    @zen/wasm   generated by scripts/build-wasm.sh (wasm-bindgen --target web, both
                           .wasm and .d.ts); checked-in build script, generated files git-ignored
  client/      @zen/client TS sources, ESM, strict tsc; depends on @zen/wasm
```

* **Tooling:** `typescript`, `vitest`, `@biomejs/biome` (format and lint, the counterpart of fmt and clippy), `@playwright/test`. Nothing else in runtime dependencies.
* **Loading:** one `--target web` artifact. `init()` fetches the URL in a browser and reads the file in Node (`initSync`).
* **Transport:**
  * global `fetch` and `WebSocket` (Node 22 has both), injectable
  * Node mTLS through an `undici` dispatcher passed in by the caller, so the browser bundle doesn't pull in `undici`

### `@zen/client` API

| Module | Surface |
|---|---|
| `connect(url, opts)` | `Client` with `info()`, `challenge()`. Retries per api.md §1 (backoff on 429/503, `Retry-After`). Typed `ZenError {status, code}`. |
| `auth` | Session in hand (`token`, `expires`, `user_fp`, `device_fp`, `method`), `logout`, `credentials.list/remove`, and the six methods:<br>1. `signInDevice(identity, deviceSecret, cert)`<br>2. `signInPasskey(authenticator, {user?, unlock?})`, `registerPasskey`<br>3. `signInOpaque(name, password)`, `registerOpaque`<br>4. `withApiToken(token)`, `tokens.create`<br>5. `signInMtls()`, `registerMtls`<br>6. `signInPassword(name, password)`, `setPassword`<br>Origin: `opts.origin`, defaulting to `location.origin` or the URL's `scheme://host:port`. |
| `claim(claimToken, admin)` | Bootstraps a fresh server: builds and signs ACL v1, then `acl/put` with the claim. |
| `acl` | `get`, `verifyChain`, an editor (add/remove member, device or grant), `signAndPut` with CAS. |
| `fs(id)` | `header()`, then `unlock(Unlock)`, which picks the slot: passphrase, recovery, device, a passkey's PRF (matched by `device_fp` or `evalByCredential`) or an OPAQUE export key, all from the same sign-in. Returns `UnlockedFs`. Admin: `addSlot`, `removeSlot`, `rotate` (new epoch, chain record, re-wrap each openable slot), `initHeader`. |
| `UnlockedFs.kv` | `get/range` (decrypt, picking the epoch from the sealed header through `previous()`), names through `NameChain` paths. |
| `UnlockedFs.transaction(fn, {mode})` | Short mode: read version, record read conflicts, commit, retry on `conflict`/`too_old` with backoff (bounded). Long mode: `expect` and `expect_ranges` (RangeHasher). Idempotent `commit_id` replay on network errors. |
| `topic(path)` | `append` (sealed event, `key_token`), `read`, `consumer(group)` with an async iterator (lease, `next` long-poll, ack through commit `consume`, `nack`, DLQ ops), `leader(name)` with fencing-token renewal and an `onLost` callback. |
| `stream()` | WebSocket with the `auth` frame, `sub/unsub`, `epub/esub`. Reconnects and resubscribes from the last cursor. |
| `fs(id).tree(treeId)` | Covered in the next table. |

**Filesystem (`tree`), the operations layer M5 builds on:**

| Operation | What it does |
|---|---|
| `Clock` | Persisted through an injectable store; observes every server `hlc`. |
| `mkdir/create/move/rename/remove/setMeta` | `crdt_ops` with sealed meta. `remove` moves the node to TRASH. |
| `writeFile(node, bytes \| stream, {replaces})` | 64 KiB chunks with random ids. Reuses the ids of unchanged chunks when given the previous manifest. Uploads `chunks` over several commits within `max_commit_bytes`, then the `write` op. Returns the dot. |
| `readFile(node)` | `file/get`, then the versions. Several versions mean siblings, returned as a list so the caller shows conflict copies. Then `chunks/get` and open. |
| `children`, `get`, `list` | Decrypt meta. Paging. |
| `changes(cursor)` | Async iterator over the long-poll. 409 `resync` means a full resync. |
| `verifyChain()` | Recomputes the op chain from the ops this client knows, as in the `fs.rs` test. |
| Errors | `clock_skew` and `stale_op` are handled by a rebase (fresh HLC, then re-issue) per spec/fs.md. |

**Test utilities** (`@zen/client/testing`, not in the main entry):
* `SoftAuthenticator`: ES256 with "none" attestation, a sign counter, and PRF computed as HMAC over the salt with a per-credential secret
* `spawnServer()`

## Step 3b: `@zen/fuse`, mounting a zen-serve fs as a local directory (Node CLI)

A new package, `packages/fuse/`, built on `@cocalc/fuse-native`. `@fuse-bindings/fuse` doesn't exist on npm. This is a maintained fork of fuse-native (N-API prebuilds for Linux and macOS, libfuse 2 bundled, so no system headers are needed). It is the only part with a native dependency, so `@zen/client` stays pure.

**CLI:** `zen-mount <url> <mountpoint> --fs 1 [--tree <id>|--new-tree]` with these options:
* sign in with `--password-user`, `--api-token` or `--device-key <file>`
* unlock with `--passphrase` (prompted), the OPAQUE export key, or a device slot
* `--read-only`, `--foreground`
* `--cache-dir` (default `~/.cache/zen-mount/<fs>-<tree>`)

The mount unmounts cleanly on SIGINT and SIGTERM. Secrets are never taken on the command line: they come from a prompt, an environment variable or a file.

**Mapping onto the M4 client API (thin, no local replica: that's M5):**

| FUSE op | zen operation |
|---|---|
| `readdir`, `getattr`, `lookup` | `tree.children` and `tree.get`, with decrypted meta (name, mode, mtime, size from the manifest). An in-memory node cache, path → node id, is kept fresh by a background `changes()` iterator, so edits from other devices appear. |
| `mkdir`, `create`, `rename`, `unlink`, `rmdir`, `chmod`, `utimens` | `mkdir/create/move/rename/remove/setMeta` (remove moves to TRASH). `rmdir` of a non-empty directory gives `ENOTEMPTY`, checked client-side. |
| `open`, `read` | Fetch the current version's manifest and only the needed chunks. Chunk LRU cache in memory and in `--cache-dir`, sealed as on the server. |
| `write`, `truncate`, `release`/`flush`/`fsync` | Write-back buffer per open file. On `release`/`fsync`: chunk, reuse unchanged chunk ids, `writeFile(..., {replaces: [dot seen at open]})`. |
| Siblings (concurrent writes) | The newest version is the file. Each other sibling is listed as `name (conflict <device>-<n>).ext`, read-only. Deleting it resolves it (a `write` replacing that dot). |
| Duplicate names | Shown as `foo (2).txt` per DESIGN-4 §2.3, with stable ordering by node id. |
| `statfs` | Quota from `info` limits. |

Errors map to POSIX: `ENOENT`, `EEXIST`, `EACCES` (403), `EROFS`, `EIO` (others, logged). `stale_op` and `clock_skew` are handled inside the client by the rebase.

**Limits, stated in the README:**
* not POSIX-complete: no hard links, symlinks or xattrs in M4 (meta xattrs are reserved), no `mmap` coherence across devices
* last-close wins locally, siblings across devices
* largest file about 175 MiB (`TD-FS-LARGE-FILES`)

**Tests (`packages/fuse/test/`, vitest, Linux only, skipped when `/dev/fuse` or `fusermount` is missing):**
* mount against the spawned server, then plain `node:fs`: mkdir, write, read back, rename, `ls`, rm
* a file bigger than `max_commit_bytes`
* two mounts of the same tree: a write in one appears in the other through the change feed, and concurrent writes show a conflict copy
* unmount and remount: the data persists

CI: `apt-get install fuse` (provides `fusermount`), and the job loads the module when the runner allows it. If the runner can't mount, the tests are skipped with a logged reason; this machine has `/dev/fuse` but no `fusermount`, so the local check installs `fuse` first.

## Step 4: tests

**Harness:** vitest `globalSetup` builds nothing.
* CI and the scripts first build `zen-serve` (debug) and the wasm module.
* It then spawns `target/debug/zen-serve serve -c <tmp>/zen.toml` on a free port, with:
  * `[[fs]]` ids 1 and 2
  * `[auth] opaque = true, api_tokens = true`
  * Argon2 at the floor (64 MiB, t=1; OPS.md §4.7)
  * `cors_origins` and `public_origins` for the browser page
* It reads `<data_dir>/claim-token`, waits for `/v1/info`, and kills the explicit PID on teardown.
* Expensive fixtures (identities from seeds, the claimed server) are shared per file.

**Node tests (`packages/client/test/*.test.ts`), mirroring the server's tests:**
* **wire:** a round trip of every encoder through WASM; known CBOR bytes for a few types; the ACL vectors from `acl.json`, plus the new header vectors
* **auth:** each of the six methods signs in.
  * passkeys through `SoftAuthenticator`, including PRF unlock in one touch
  * mTLS through an undici dispatcher and the test PKI (a self-signed CA generated once in the test, with the server's native TLS)
  * wrong password and limiter behaviour, logout, credential list and remove
* **keyslots:** create and unlock types 1–5; header CAS conflict; rotation (old data still readable through `previous()`, new writes on the new epoch)
* **kv/txn:** CRUD, range, short-mode conflict and retry (two clients), long-mode `expect` mismatch, idempotent replay
* **log/consume/leader:**
  * append and read
  * consumer group ack, nack, then DLQ
  * two leaders racing: one wins, the fencing token rejects the loser, takeover on expiry
* **stream:** sub receives appends; ephemeral pub/sub between two clients; reconnect
* **fs:**
  * create, move, rename, delete; cycle no-op; concurrent rename and move
  * siblings and resolution
  * a multi-commit upload of a file larger than `max_commit_bytes`, with chunk reuse on rewrite
  * the change feed iterator wakes on a write; `stale_op` rebase; chain verify
* **errors:** permission denied for a member without rights; 429 handling

**Browser smoke (Playwright, `packages/client/browser/`):**
* A static page loads `@zen/client` and the wasm from a tiny Node static server; zen-serve has CORS for that origin.
* Chromium uses a CDP virtual authenticator (`ctap2`, `hasResidentKey`, `hasUserVerification`, `hasPrf`).
* Steps: register a passkey, sign in with one touch plus PRF unlock, KV put and get, write and read a file, WebSocket subscribe.
* Locally use the preinstalled Chromium (`PLAYWRIGHT_BROWSERS_PATH`); CI runs `npx playwright install --with-deps chromium`.

## Step 5: CI and docs

**New CI job, `client`** (ubuntu, in parallel with the others):
* rust-cache, the wasm32 target, and `wasm-bindgen-cli 0.2.129` (taiki-e/install-action, with a version check against `Cargo.lock`)
* `cargo build -p zen-server`, then `scripts/build-wasm.sh`
* `npm ci`, `biome check`, `tsc --noEmit`, `vitest run`, Playwright

The `rust` job adds the `opaque` wasm check.

**Docs:**
* `docs/CLIENT.md`: a usage guide (connect, sign in, unlock, KV, transactions, topics, fs) and the security notes (secrets in WASM memory, `free()`, Argon2 parameters for the browser vs native, origin)
* `spec/OPS.md`: client build pitfalls (a wasm-bindgen version mismatch, a stale wasm after Rust changes)
* `docs/STATS.md`: wasm size (raw and gzip), npm package size, Argon2 and ML-DSA timings in Node and in Chromium
* `README.md`: a pointer to the client
* `spec/TECH_DEBT.md`: new TDs only for things deferred (likely `TD-FS-HEADER-SELF-SLOT`, and `TD-CLIENT-WASM-OPT` if binaryen isn't wired)

## Commits
1. plan doc
2. `spec:` fs header format, ACL vectors, zen-core `FsHeader`, the opaque wasm check
3. zen-proto `ts` feature + `crates/zen-wasm` + `scripts/build-wasm.sh`
4. `packages/` scaffold, transport, info, auth (six methods) + harness + auth tests
5. keyslots, ACL admin, KV, transactions + tests
6. log, consumer, leader, stream + tests
7. filesystem + tests
8. `@zen/fuse` package, `zen-mount` CLI + mount tests
9. browser smoke, the CI job, docs and STATS (including `docs/CLIENT.md` §zen-mount)

Push after each green step. A new short PR at the end. Steps 4–7 can run as two agents on disjoint files once step 3 lands (OPS.md §4.9: one `target/`; I verify and push).

## Verification
* Rust:
  * `cargo fmt --check`
  * `cargo clippy --all-targets -- -D warnings` (default and `--no-default-features`)
  * `cargo test --all`
  * the wasm32 checks (default and `opaque`)
  * `gen_vectors` leaves `spec/test-vectors` unchanged
* Client: `scripts/build-wasm.sh && npm ci && npx biome check && npx tsc --noEmit && npx vitest run && npx playwright test`, against a debug `zen-serve`
* CI green on `rust`, `pure-rust`, `fdb` and `client`. The FDB suite is unaffected, since the server doesn't change.
* Watch disk (target/ is 11 GB now; the wasm release build adds about 1 GB): `CARGO_INCREMENTAL=0`.
