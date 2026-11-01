# @zen/client

The TypeScript client of zen-serve, for Node 22+ and browsers (`packages/client`). Every format and algorithm runs in the WASM build of zen-core (`crates/zen-wasm`, `@zen/wasm`), and so does the CBOR wire encoding, through zen-proto: the client and the server share one definition of every byte. The server only ever sees ciphertext and opaque ids.

`zen-mount`, a FUSE mount of a zen-serve filesystem, is in [§9](#9-zen-mount). The packages on top of the client have their own docs: `@zen/db` (tables, indexes, transactions, the broker, CRDT tables) in [DB.md](DB.md), `@zen/loro` (Loro documents on a topic, with snapshots and offline edits) in [DB.md §20](DB.md#20-loro-documents), `@zen/fs` (the local replica) in [§8](#8-the-filesystem) below, and `@zen/svc` (handlers of relayed TCP services) in [§11](#11-relayed-services-zensvc).

## 1. Building

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version <the wasm-bindgen version in Cargo.lock> --locked
scripts/build-wasm.sh          # crates/zen-wasm → packages/zen-wasm/pkg,
                               # crates/zen-svc-wasm → packages/svc-wasm/pkg
npm ci
npm run build -w @zen/client   # packages/client/dist
```

Tests run against a real server, the debug build on the embedded backend, with the test adversary (`test-utils`) that the integrity tests make lie:

```sh
cargo build -p zen-server --features test-utils
npx vitest run                 # Node: packages/*/test
npx playwright test            # Chromium: packages/client/browser
```

## 2. Connecting and signing in

```ts
import { connect } from '@zen/client';

const client = await connect('https://zen.example.org');   // loads the WASM core
const session = await client.signInPassword('alice', 'correct horse');
```

`connect` takes the server URL and, optionally, `origin`: the server origin as this client sees it, which every sign-in is bound to (spec/auth.md §5). It defaults to `location.origin` in a page the server serves, and to the URL's origin otherwise. Behind a proxy that renames the host, pass the public origin.

| Method (auth.md) | Sign in | Register (needs a session) |
|---|---|---|
| 1 device key | `client.signInDevice({identity, device, cert})` | an admin adds the device certificate to the ACL |
| 2 passkey | `client.signInPasskey(authenticator, {user?, prfSalts?})` | `session.registerPasskey(authenticator, {prf: true})` |
| 3 OPAQUE | `client.signInOpaque(name, password)` | `session.registerOpaque(name, password)` |
| 4 API token | `client.withApiToken('zen_at_…')` | `session.createApiToken({user})` (admins) |
| 5 TLS client certificate | `client.signInMtls()` | `session.registerMtls()` |
| 6 password-derived key (default) | `client.signInPassword(name, password)` | `session.setPassword(name, password)` |

* **Passkeys** go through an `Authenticator`: `browserAuthenticator()` uses `navigator.credentials`; in Node, pass your own (`@zen/client/testing` has a software one for tests).
* **mTLS in Node** needs an undici `Agent` with the certificate: `connect(url, {fetchInit: {dispatcher}})`. A browser presents its own certificate.
* A wrong password with OPAQUE is detected by the client itself (the server's response doesn't open); it still surfaces as the same 401 as every other credential failure.
* Errors are `ZenError {status, code, retryAfterMs?}` with the codes of spec/api.md §1. Requests refused with 503, or 429 with `Retry-After`, are retried automatically (3 attempts).

**Claiming a new server** (api.md §4.1): `claim(client, claimToken, adminIdentity, {members, grants})` signs ACL version 1 and pins the client's origin with it.

## 3. The ACL

`session.acl.head()` fetches the signed chain and verifies it (formats.md §9.3): each version names the previous one's hash and is signed by one of its admins. The client pins the newest version it verified and refuses a chain that doesn't extend it. `session.acl.update(adminIdentity, (doc) => { … })` signs and sends the next version, retrying on a concurrent change.

## 4. Unlocking a filesystem

Data keys live in the fs header's keyslots (formats.md §6, §12). The header is opaque to the server.

```ts
const fs = await session.fs(1).unlock({ passphrase: 'open sesame' });
// or {recoveryKey}, {device}, {prfOutput, credentialId}, {opaqueExportKey, credentialId}
```

* After an **OPAQUE** sign-in, or a **passkey** sign-in that asked for the PRF output (`prfSalts`, from `prfSlotSalts(header)`), `unlock()` with no argument opens the matching slot: one prompt signs in and unlocks.
* **Admins** create the header (`fs(1).init(keys => [slots…])`) and change it: `addPassphrase`, `addRecovery` (returns the key, to show once), `addDevice`, `addPasskey`, `addOpaque`, `removeSlot`. Changes are compare-and-set on the header version, retried on a concurrent change. Members can't write the header yet (`TD-FS-HEADER-SELF-SLOT`): they prepare a slot and an admin adds it.
* **Rotation** (`fs.rotate(keys => [new slots])`) starts a new epoch, removes every old slot and adds the new ones. Data sealed under older epochs stays readable through the epoch chain. A handle unlocked by a slot of an older epoch reads, but refuses to seal (`stale_slot`).
* `fs.close()` zeroizes the keys; using them afterwards throws `closed`. Keys and secrets live in WASM memory behind handles; free handles you created yourself (`.free()`).
* **Offline.** `unlock(how, {local})` keeps the header on a local store (§10) and unlocks from it when the server can't be reached. The store can't have the fs layer (it needs the keys being unlocked): an app that wants the fs layer for its data uses two `sealedStore`s over one backend. `fs(1).forget(local)` deletes the kept header at sign-out. Offline, sign in by resuming the session the app kept (`client.resume(token)`); a resumed session holds no unlock material, so pass it to `unlock` explicitly.

## 5. KV and transactions

Paths are lists of elements (strings or bytes). The server sees only PRF tokens of them (16 bytes per element), so **names can't be read back from a range**: store the name in the value if you need it.

```ts
await fs.kv.set(['users', 'alice'], bytes);
const v = await fs.kv.get(['users', 'alice']);
for await (const e of fs.kv.range(['users'])) { … }

await fs.transaction(async (tx) => {
  const n = Number(decode(await tx.get(['counter'])));
  tx.set(['counter'], encode(n + 1));
});
```

* **Short mode** (default): reads at one read version, registered as read-conflict ranges, so the transaction is serializable, phantoms included. It must finish within the ~5 s MVCC window.
* **Long mode** (`{mode: 'long'}`): no time limit. Every read becomes an `expect` (a value version, or absence) and every range a hash, checked at commit (DESIGN-3 §2.4). A long-mode range must fit one response.
* Both retry the whole function on `conflict` / `too_old` (8 attempts, jittered backoff), whether the commit or a read inside the function returned it, so keep side effects out of it. A `ZenError` the function throws itself with one of those codes re-runs it the same way. Commits carry a random `commit_id` and are replayed with it after a network error or `commit_unknown`, which the server makes idempotent.
* `tx.clearPrefix(path)` deletes a subtree, `tx.clearRange(begin, end)` a stored-key range; reads in the transaction see its own writes and clears.
* Reads and writes also take stored keys (`fs.kv.key(path)`, or a key a library computed) in place of paths. `tx.getAll(keys)` reads several in one request, each recorded like `get`. `tx.rangeStored(begin, end, {limit})` reads a stored-key range; with a limit, only the part it read is recorded, so a paged scan conflicts with changes in its pages and not beyond.
* **Snapshot reads** (`tx.snapshotGet(paths)`, `tx.snapshotRange(prefix)`) read at the transaction's read version (`tx.readVersion`, set by its first read) but record nothing: no read conflict, no `expect`. Use them for data whose consistency is checked another way, such as content-addressed nodes.
* `tx.expectKey(path, version)` makes the commit fail with `conflict` unless the key is still at `version` (still absent for `undefined`). It puts a record read earlier, such as one from a cache, into the read set, in either mode.
* Other parts add to the commit with `tx.addCrdtOps(ops)` and `tx.addChunks(chunks)`. `tx.onCommit(fn)` runs once the attempt commits; `tx.onError(fn)` runs when it fails, and if a hook returns true the transaction runs again (a tree rebase does this). `tx.result` carries `dots`, `offsets` and `set_dots`.
* `tx.getEntries(keys)` is `getAll` with each entry's key and version (undefined for the transaction's own writes), for caching. `tx.owns(key)` tells whether the transaction wrote, deleted or cleared a key.
* `{retry: (e) => boolean}` chooses which errors re-run the function (default: `conflict` and `too_old`). zen-db's `auto` mode retries only `conflict` in its short phase.
* `commitTransaction(tx)` commits a `new Transaction(fs, mode)` the caller built itself, and runs its `onCommit` hooks. It returns undefined when there is nothing to send. On a failure the caller decides; there is no retry. zen-db's `db.begin()` uses it.
* A transaction's `commitId` is fixed when it is made: calling `commitTransaction(tx)` again after a lost response sends the same `commit_id`, so the commit applies at most once (api.md §6). `{attempts}` bounds its own replays on a network error (default 4). zen-db's CRDT queue relies on it.

## 6. Topics, consumers and leaders

```ts
const topic = fs.topic('orders', 'eu');                  // a path; the server sees a PRF id
const offset = await topic.append(bytes, { key: 'order-42' });
for await (const ev of topic.read({ key: 'order-42' })) { … }   // decrypted, any epoch
await fs.transaction(async (tx) => { tx.set(['o', '42'], v); topic.appendIn(tx, event); });
```

* Events are sealed with the topic's key (formats.md §5): the sender (device fingerprint), an HLC, an optional causation, and the payload. An event with a key carries a 16-byte key token the server can index without learning the key.
* **Consumer groups** (`topic.group(name, {mode, …})`): `sequential`, `partitioned` and `single_key` hand out events under a lease; `per_key` under per-key claims; `broadcast` is a definition only. `consumer.deliveries({waitMs})` long-polls, taking and renewing the lease. `consumer.ack(d, tx)` puts the consume step into a transaction, so the event is acknowledged **only if the transaction's writes commit**: exactly-once processing. `nack` counts attempts, and after `maxAttempts` the event goes to the dead-letter list (`consumer.dlq.list/retry/drop`).
* **Group names are per fs**, not per topic: a name used on another topic returns `group_exists`.
* **Leaders** (`topic.leader(name, {ttlMs, onLost})`) build on a `sequential` group's lease: `campaign()` until elected, automatic renewal, `token` as the fencing token. Do the leader's writes in the transaction that acks the event it handles (`leader.ack(d, tx)`): a deposed leader's commit then fails as a whole with 412 `not_leader`. Writes that carry no consume step are not fenced.

## 7. The stream

```ts
const stream = await session.stream();
const sub = await stream.subscribe(topic, { after: lastOffset });
for await (const ev of sub) { … }                        // history after `after`, then live
await stream.publish(topic, data);                        // ephemeral: not in the log
```

* Subscriptions survive dropped connections: the stream reconnects with backoff and resumes each subscription after the last offset it delivered, so nothing is lost or repeated. A subscription without `after` starts at the read version when it was made, for the same reason.
* A prefix subscription (`{fs, prefix, topics}`) opens the events of the topics it is given; others arrive unopened (`opened: false`), since topic ids can't be turned back into paths.
* Ephemeral messages are sealed like events, under a reserved key token, and carry the sender's HLC and the publishing stream's id (12 random bytes, in the causation field); receivers drop replays per sender and stream (api.md §9.2), so two sessions of one device don't drop each other's messages.

## 8. The filesystem

```ts
const tree = fs.tree(Tree.newId());                       // a tree exists once it has operations
const dir = await tree.mkdir(ROOT, 'docs');
const file = await tree.create(dir, 'notes.txt');
await tree.writeFile(file, bytes);                        // replaces the current versions
const data = await tree.readFile(file);
for await (const batch of tree.changes(cursor, { waitMs: 25_000 })) { … }
```

* **Operations** (`mkdir`, `create`, `move`, `rename`, `remove` to the trash, `setMeta`, or several in one `batch()`) are CRDT operations merged by the server (spec/fs.md). Names and metadata are sealed; the server sees ids and the tree's shape.
* **The clock.** A session has one hybrid logical clock (`session.clock`), corrected once from the server's time. Event timestamps, tree operations and CRDT rows all come from it, so they are ordered with each other. Another device's event timestamp more than a minute ahead of the corrected clock isn't observed, because the server never checks those. On `stale_op` or `clock_skew` the client rebases: it re-issues the operations with fresh timestamps. To keep the clock across restarts, give it a `ClockStore` (`session.useClock(new TreeClock({store}))`, before anything takes a timestamp). The store also keeps the installation's 16-byte **counter actor** (`clock.actor`, for CRDT counters), and it must persist the actor together with the timestamp. Copying a store to another installation is a bug.
* **In a transaction.** `tree.writeIn(tx, ops)` (or `batch.commitIn(tx)`) adds operations to a transaction's commit, so they commit atomically with its KV writes. A rebase then re-runs the transaction. `tree.upload(node, data)` uploads a file's chunks without writing it, and `tree.writeIn(tx, [await tree.writeOp(upload)])` writes it in a transaction. Chunks that are never written are collected after `chunk_grace_secs`.
* **Files** are chunks with random ids, uploaded over as many commits as `max_commit_bytes` needs (up to 3 in flight, `parallel`), then one `write` with the sealed manifest. The fs policy's `chunking` says how content is cut:
  * **`cdc` (the default):** by content (formats.md §11.7), 8–88 KiB, 32 KiB on average at the defaults, with a chunker keyed so the server can't predict the cuts. An insertion changes only the chunks around it. These are version-3 manifests.
  * **`fixed`:** 64 KiB chunks (smaller on a server whose `max_value_bytes` is under 64 KiB), in version-2 manifests.
  * **Which applies:** a write follows the form of the version it builds on (`previous` or `base`), so their chunks are reused. `chunking` or `chunkSize` in the options override it. A server without `fs_cdc`, or with a `max_value_bytes` too small for the chunker's defaults, gets fixed chunks; so does a known size past what a CDC version holds (below).
  * Given the previous version's chunk index (`previous`), chunks whose bytes are unchanged (by SHA-256) are reused.
* **Concurrent writes** that replaced the same version become **siblings**: `versions(node)` returns them all, `readFile(node, {version})` reads one, `resolve(node, keep)` keeps one.
* **Large files.** A file of up to a few thousand chunks lists them in the manifest; a longer one is **paged**: its chunks go in pages, and the manifest lists the pages with their digests (formats.md §11.3, §11.6).
  * **Fixed chunks:** pages of up to 5,624 ids, up to about 640 GiB at the defaults.
  * **CDC:** pages of up to 4,499 ids and lengths, which need not be full, up to about 170 GiB when they are (at least half that after many edits; a write that would overflow re-cuts its pages full). A larger file is written with `chunking: 'fixed'`: the writer picks it itself when it knows the size (a `Uint8Array`, or `size`), and otherwise fails with 413 `too_large` saying so.
  * `FileVersion` has `size`, `chunkCount`, `chunking`, and `chunks` (inline) or `pages`; `manifest` is version 1, 2 or 3.
  * Paged files need a server with the `fs_pages` feature (and `fs_cdc` for CDC); without it, a file past the inline limit gets 413.
* **Sources and streams.**
  * **Writing:** `writeFile` and `upload` take bytes, an iterable or async iterable of byte arrays, or a `ReadableStream`. They hold the commits in flight plus about 52 bytes per chunk for the index (`WrittenFile.index`).
  * **Known items:** a source may also hold `KnownChunk`s and `KnownPage`s, content of the base version taken over without its plaintext. Where one doesn't fall on a cut of the new content, the writer fetches its plaintext from the base and cuts it again, until the cuts line up.
  * **Reading:** `readStream(node, {version, offset, length, readAhead})` fetches windows of about 4 MiB, 3 ahead by default. `readFile` holds the whole file.
  * **Indexes:** `chunkIndex(node, version)` hashes through the stream when it isn't given the bytes; with `{hashes: false}` it reads only the pages.
* **Edits and appends.** `edit(node, version, [{offset, remove, insert}, …])` writes a version changed in place: untouched pages are taken over unread, and only the touched pages and the chunks around each edit are fetched and uploaded, so the cost doesn't grow with the file. `append(node, bytes)` is an edit at the end: one page and one chunk are read. `editUpload` is the same without the write. A file of fixed chunks keeps them: there an insertion or deletion of other than whole chunks shifts every later chunk, and the rest of the file is read and uploaded again.
* **Resumable uploads.** `upload(node, data, {onProgress})` reports an `UploadState` after each commit, in commit order. The state is the pages done plus at most one page of chunks, so it stays small. Keep it, sealed: it holds the file key. `onProgress`'s second argument lists the chunk and page ids that commit put. `upload(node, rest, {resume: state})` continues with the rest of the source, from `state.size`, sending nothing again. Without a session it works while the uploaded chunks are within `chunk_grace_secs` (24 h by default).
* **Upload sessions** (api.md §12.1). With `{session: true}`, `upload` and `writeFile` run under a session that holds what they upload until it is closed or expires (`sessionTtlSecs`, default the server's `upload_ttl_secs` of 7 days; renewed as the upload goes). That makes a resume work after the grace period, and the state carries the session. `writeFile` closes it after the write. `upload` leaves it open (`Upload.session`): close it with `tree.closeUpload(session)` once written. `tree.uploads()` lists the caller's open sessions on the fs (at most 64), so a client that lost its state can close them. A resume after the session expired fails with 412 `upload_gone`.
* **File keys** (fs.md §4.2) are a policy of the fs, kept in its header: `fs.policy` (`{fileKeys, rekeyAfterRotate, chunking}`) and, for admins, `fs.setPolicy({fileKeys: true})` (or `{chunking: 'fixed'}`). Off (the default), chunks are sealed under the fs keys and reused across rotations. On, each file gets a key of its own, which milestone 11 will use to share one file; after a rotation, `rekeyAfterRotate` says when a file gets a fresh key (`rewrite`: on a write without `previous`; `eager`: on its next write, re-uploading every chunk; `keep`: never). Readers need no setting: each manifest says how its chunks are keyed.
* **Reads** fetch only the chunks a range needs (`read(node, version, offset, length)`), and for a paged file only the pages they are in, found by the pages' byte counts. Pages are checked against their digests (and, under CDC, their counts and lengths) and kept in memory (256 per tree); `TreeOptions.pageCache` keeps them elsewhere, like `chunkCache` for sealed chunks.
* **Duplicate names** are possible, since the server can't see names: `displayNames(children)` gives `foo.txt`, `foo (2).txt`, … in node-id order.
* **The change feed** yields batches of changed nodes; a batch with `resync: true` means the cursor is too old and the caller must re-read the tree.
* `verifyChain(ops)` recomputes the tree's operation chain from the operations this client recorded (`TreeOptions.onOp`) and compares it with the server's.

### The local replica (`@zen/fs`)

`@zen/fs` keeps a **replica** of a tree on a sealed local store (spec/fs.md §11): every node, followed through the change feed, and file content as it is read, so listings and kept files work offline. It has no DOM or Node imports, so it runs in workers too.

```ts
import { openReplica } from '@zen/fs';

const r = await openReplica(ufs, treeId, { local, cacheBytes: 512 << 20 });
await r.sync();                                   // optional: catch up now (it follows the feed anyway)
await r.readdir('/docs', { withTypes: true });    // [{name, type}], display names and conflict copies
await r.stat('/docs/a.txt');                      // {type, mode, size, mtimeMs, node, versions, conflict}
await r.readFile('/docs/a.txt');
await r.read('/big.bin', offset, length);         // fetches only the chunks (and pages) it needs
const s = await r.createReadStream('/big.bin', { start, end });
await r.readlink('/link'); await r.getxattr('/x', 'user.k'); await r.listxattr('/x');
await r.pin('/docs');                             // keep a folder's content offline, now and later
r.onChange((ids) => …);                           // nodes that changed
await r.close();
```

* **What is kept:** the whole node table, as the server sends it (meta still sealed); versions, chunks and pages when read, sealed as on the server, least recently used evicted past `cacheBytes` (default 1 GiB). **Pinned** files and folders keep their current content and are refreshed as they change.
* **Paths** use display names (`foo (2).txt` for a duplicate) and show a file's other sibling versions as read-only conflict copies, `notes (conflict ab12cd34-2).md`.
* **Offline**, everything kept reads as before; content that isn't kept fails with `offline`, and `status().online` is false until the feed answers again.
* **Several tabs** on one store (IndexedDB): the owner (a Web Lock) follows the feed and the others reload what it announces on the tab channel. In Node a disk store holds one replica (a lock file; one left by a dead process is taken over). `Replica.wipe(local, fs, tree)` deletes a replica.

**Writes** work offline: each call is applied to what the replica shows at once, queued in the store, and sent later by the owner (spec/fs.md §11.1).

```ts
await r.mkdir('/docs');
await r.writeFile('/docs/a.txt', bytes, { contentType: 'text/plain' });  // only changed chunks are sealed
await r.symlink('docs/a.txt', '/link');
await r.rename('/docs/a.txt', '/b.txt');          // replaces an existing file, or an empty directory
await r.unlink('/b.txt'); await r.rmdir('/docs');
await r.truncate('/big.bin', 1 << 20);
await r.chmod(p, 0o600); await r.utimes(p, ms); await r.setxattr(p, 'user.k', v); await r.removexattr(p, 'user.k');
await r.writeFile('/x', data, { wait: true });    // resolve once the server has it, not once queued

const h = await r.open('/big.bin', 'r+');          // 'r', 'r+', 'w' (truncates), 'a'; {create, exclusive, mode}
await h.write(patch, offset); await h.read(n, offset);
await h.close();                                  // flush: one write, sealing only the chunks written
await r.flush();                                  // resolves once every queued write is sent; offline: rejects
r.onError((e, entries) => …);                     // writes the server refused (they are dropped)
```

* **A call returns once it is queued** (in the store, so it survives a restart), offline too; `{wait: true}` waits for the server. `status()` gives `queued` and `sending`.
* **Changed chunks only.** A handle keeps the chunks it writes in memory; on flush the others are taken over from the version it opened by id (`KnownChunk`), with no plaintext read, and only pages whose ids changed are new. `writeFile` reuses unchanged chunks by hash. Both need the version's chunk index, built once with a streamed read and kept; offline it can be built from pinned content, else the whole file is sealed.
* **Conflicts.** A write replaces the version shown (and this replica's earlier writes of the file), so a write made elsewhere meanwhile stays as a conflict copy, on every replica. Deleting the copy resolves it, keeping the version shown.
* **Writing offline over a file whose versions aren't kept fails with `offline`**: read, stat or pin it first. Versions this replica wrote are kept.
* **Chunks:** the replica writes fixed-size chunks for now (milestone 8 step 4 makes it cut by content). It reads files of either kind; the first flush of an edit to a file of variable chunks rewrites the file whole.
* **Sending.** Entries go in order, as few commits as the limits allow; a batch's commit is stored first and sent again with its `commit_id` after a crash. A refused entry (rights, a node purged meanwhile) is dropped and reported to `onError`; `stale_op` and `clock_skew` are rebased.
* **Tabs.** Any tab writes; the owner sends, and the others hear when their writes are sent or refused.
* The `zen.` xattr names are reserved (`zen.content_type` holds the media type, `stat().contentType`).
* `Tree.seal(node, data, {sink})` (sealed chunks and pages go to the sink, nothing is sent) and `Tree.writeOpOf(upload, replaces)` are the client pieces under this; `PendingOp` move and meta ops may carry the `hlc` taken when queued (a rebase ticks a fresh one), and a `FileSource` may hold known chunks of the previous version.

## 9. zen-mount

`zen-mount` (packages/fuse) mounts one tree of an fs as a local directory, through FUSE, on a local replica (`@zen/fs`, §8).

```sh
sudo apt-get install fuse3 libfuse-dev pkg-config     # libfuse 2 headers: the module compiles at npm ci
npm run build -w @zen/client -w @zen/fs -w @zen/fuse
# The first time: store the device key in a vault, keep the replica on disk, pin a folder.
node packages/fuse/dist/cli.js https://zen.example.org ~/zen --tree <hex> \
  --device-key device.json --vault ~/.zen/vault --cache-dir ~/.zen/cache --pin /docs
# Later (device.json can go), also when the server is down:
node packages/fuse/dist/cli.js https://zen.example.org ~/zen --tree <hex> \
  --vault ~/.zen/vault --cache-dir ~/.zen/cache --offline-ok
```

* **Sign-in** by password key (`--user`), OPAQUE (`--opaque`, which also unlocks through its keyslot), an API token, a device key file, or the vault. Unlocking by passphrase, recovery key, device key (the file's or the vault's) or the sign-in. Secrets come from the environment, files, a prompt or the vault, never the command line.
* **`--vault DIR`:** a credential vault (§10) sealed under a key of this machine. A `--device-key` given with it is stored there; every session is stored after sign-in; with neither, the vault signs in (the stored session, else the stored device) and unlocks.
* **`--cache-dir DIR`:** the replica (node table, content, pins, writes not yet sent) and the kept fs header, in a sealed disk store. One mount per directory (a lock file). Without it, everything is in memory. `--cache-size` bounds the content kept (default 1 GiB; pins don't count), and `--pin PATH` (repeatable, remembered) keeps a file or a folder offline.
* **`--offline-ok`** (needs `--cache-dir` and `--vault`): when the server can't be reached at start, mount from the cache. Without it the CLI exits with status 1. Offline, the node table lists, kept content reads, and a file whose content isn't kept lists with size 0 and fails to read (`EIO`).
* Without `--foreground` it detaches once mounted. SIGINT or SIGTERM unmounts (open files queue their writes) and waits up to 10 s for the queue to be sent. With `--cache-dir`, what is left is sent at the next mount; in memory it would be lost, so it waits for the server (a second signal stops at once).
* **How it maps:** listings, `stat`, symlinks and xattrs come from the replica's node table, kept current through the change feed; the kernel caches them for `attrTimeout` (1 s). Reads go through the replica's content cache. A file open for writing keeps only the chunks written; `flush`, `fsync` and `close` queue one write with only those chunks sealed. **`fsync` makes a write durable on this device** (with `--cache-dir`), not on the server: it is sent as soon as the server can be reached.
* **Symlinks** are `symlink` nodes. **xattrs:** the kernel's `user.<x>` is the stored `<x>` (`user.zen.content_type` is the media type); other namespaces give `ENOTSUP`.
* **Conflicts:** two devices writing the same file concurrently leave siblings. The newest is shown as the file and the others as read-only `name (conflict <device>-<n>).ext`; deleting the copy resolves the conflict in favour of the file.
* **Limits:** no hard links (`EPERM`), no `mmap` coherence across devices, inode numbers from the kernel (fuse-native has no `use_ino`), and no cache invalidation pushed to the kernel (changes from elsewhere show within `attrTimeout`).

## 10. The local store

What an app keeps on the device (zen-db's queue of CRDT writes; caches, the replica) goes into a **local store**, sealed (spec/zendb.md §9.3).

```ts
import { indexedDbLocalStore, memoryLocalStore, sealedStore } from '@zen/client';
import { nodeDiskLocalStore } from '@zen/client/node';     // Node only: imports node:fs

const local = sealedStore(indexedDbLocalStore('my-app'));   // device layer (the default)
const both = sealedStore(nodeDiskLocalStore('/var/lib/my-app'), { fs });  // device + fs layers
const clear = sealedStore(memoryLocalStore(), { device: false, plaintext: true });
await local.put('notes', key, value);
await local.get('notes', key);                              // undefined if absent or unreadable
for await (const [k, v] of local.list('notes')) { … }
```

* **Backends** (`LocalStore`): `memoryLocalStore()` (per process), `indexedDbLocalStore(name)` (shared by an origin's tabs; one IndexedDB database, no version changes when stores are added), `nodeDiskLocalStore(dir)` (a file per record, written to a temporary file, synced, then renamed, so a crash leaves the old record or the new one). `@zen/client/node` keeps the Node one out of browser bundles.
* **Layers.** The **device layer** (on by default) is AES-256-GCM under a non-extractable key the backend makes once and keeps: a `CryptoKey` in IndexedDB, `dir/device.key` (mode 0600) in Node, a fresh one per process in memory. `device: myKey` uses the app's key; `device: false` turns the layer off. The **fs layer** (`fs: unlockedFs`, off by default) seals each record under the fs keys too (a kind-8 sealed object), so it opens only for someone who can unlock the fs; with both, the device layer wraps it.
* **Plaintext** is a choice the app states: neither layer requires `plaintext: true`. The records then sit on disk in the clear with no tamper check. It fits a disk the OS already encrypts, data that isn't sensitive, or a trusted host.
* **What doesn't open reads as absent**, and is deleted with a warning: a tampered record, one copied to another key, store or fs, one sealed with other layers (a store's layers never mix), or every record once the device key is gone.
* A `SealedStore` binds each record to its store name and key, so records can't be swapped between them.

### Credentials (the vault)

A worker, a helper or a restarted process can sign in and unlock with no prompt from a **credential vault** on a sealed store (spec/zendb.md §9.3):

```ts
import { credentialVault, indexedDbLocalStore, sealedStore } from '@zen/client';

const local = sealedStore(indexedDbLocalStore('my-app'));
const vault = credentialVault(local);                     // store vault/default
await vault.saveDevice({ identity, deviceSeed, cert });   // once, after enrolling the device
const session = await vault.signIn(client);               // a stored session, or a device sign-in
const fs = await vault.unlock(session.fs(1), { local });  // the device keyslot; offline from the kept header
await session.logout(); await vault.forget();             // sign-out
```

* **What it holds:** the device (its 32-byte seed, its cert and the identity's public bytes; the `DeviceSecret` is rebuilt from the seed) and the last session's token with its origin and expiry. `{name}` keeps several identities in one store.
* **`signIn(client)`** resumes the stored session while it hasn't expired (60 s margin), checked with one `/v1/grv` call (`{probe: false}` skips it). A session the server refuses (401) falls back to a device sign-in, and the new session is stored. When the server can't be reached, the stored session is returned: the kept header and local caches work, and calls fail with `network` until it is back. A token is only ever sent to the origin it was issued for.
* **The store** needs the device layer (G15: the seed and token sit at rest under a non-extractable key), or `plaintext: true` and `allowPlaintext`. It can't have the fs layer, which needs the keys it unlocks. `Db.wipe` leaves the vault; `forget()` (or `forgetSession()`) clears it.
* **A checkpoint signer** (§12): `saveSigner` keeps an enrolled device key in the same format, and `signer()` returns it, or the sign-in device when none was enrolled.

## 11. Relayed services (`@zen/svc`)

[SVC.md](SVC.md) is the guide to running and writing relayed services (configuration, keys, takeover, what the server sees, costs); this section is the API.

A relayed service (spec/svc.md) is a TCP port of zen-serve whose connections are served by handlers on member clients: the server stores each connection's bytes and never reads them. The service id is the topic id of a path, conventionally `svc/<name>`, and the server's `[[service]]` table names it; serving needs topic `consume` on it.

```ts
import { serve } from '@zen/svc';

const h = serve(db, {
  service: ['svc', 'echo'],
  handler: async (c, tx) => {                       // one step: in one transaction with tx
    const nl = c.input.lastIndexOf(10) + 1;          // input from in_acked, as bytes
    if (nl) {
      c.write(c.input.subarray(0, nl));              // output, released after the commit
      await tx.table('lines').insert({ conn: bytes.hex(c.conn), at: Date.now() });
      c.ack(nl);                                     // consume whole lines only
    }
    if (c.eof) c.end();                              // FIN; also close(), abort()
  },
});
await h.stop();                                      // the server requeues its connections
```

* **One step, one commit.** The handler runs in a `db.transaction` whose commit also carries the step: the ack, the output and the connection's state. A conflict re-runs the handler. So a reply is sent if and only if the handler's writes committed, and a peer never sees output of a step that didn't.
* **State.** `c.state` (bytes, or null) is sealed with the step (kind 9, bound to the connection's cursors) and comes back in the next step, also on another handler. Ack only whole protocol units: what isn't acked comes back as input.
* **Takeover.** `serve` holds a handler lease (`leaseTtlMs`, default 5 s, renewed every second). When the handler dies, its connections move to another handler once the lease lapses, and that one continues from the last committed step: the output of a committed step is revealed by whichever handler holds the connection (its keys are escrowed under the service key), and the input not acked is pushed again. On a `resume: false` service, output is plain and a connection whose handler dies is closed.
* **Where it runs.** Like consumers, `serve` runs while the database is the owner, so of a browser's tabs one serves. Keys come from the unlocked fs (`fs.service(...path).keysAt(epoch)`).
* **Lines.** `LineReader` reads the whole lines of a step's input, ended by CRLF or a bare LF, and says how much to ack; a partial line waits for the rest:
  ```ts
  const r = new LineReader(c.input, { max: 1000 });   // the longest line, with its terminator
  for (let l = r.nextText(); l !== null; l = r.nextText()) { … }   // next() gives bytes
  c.ack(r.consumed);                                   // r.rest(): what follows; r.tooLong
  ```
* **Waking a handler without input.** A handler runs when input arrives, but some protocols must speak when something else changed: IMAP's IDLE tells a client of new mail. A step registers keys with `c.wakeOn(...keys)` (they replace the keys of earlier steps; `c.wakeOn()` clears them), and `h.wake(key)` gives each connection held under those keys a step whose handler is called with no new input, under `tls()` too:
  ```ts
  const h = serve(db, { service: ['svc', 'imaps'], layer, handler: async (c, tx) => {
    // … read commands; while idling: tell the client what changed, then
    c.wakeOn(user);
  } });
  await db.on(receivedTopic(db), (m) => h.wake(m.body.user));
  ```
  A wake that comes while a step runs is not lost: if the keys that step leaves include it, the connection steps again. Keys live only in the process that holds the connection: a successor calls the handler once when it claims a connection, with no input, so the handler registers them again (and tells its client what changed during the takeover). An idle connection costs no steps.
* **Output larger than a step.** A step writes at most `c.room` bytes (`out_window`, less what the layer adds). A handler with more to send writes part of it, keeps its place in `c.state` and calls `c.again()`: the next step follows at once, without new input. IMAP sends a 10 MB message this way, 256 KiB a step, and a successor continues the literal where the last committed step left it.

### TLS: `tls()`

A layer sits between the connection's bytes and the handler and gives the handler its view of each step: `raw()` (the default) hands the bytes through; `tls(…)` terminates TLS 1.3 and 1.2 in the handler (svc.md §7.1), so the server relays ciphertext it can't read; `ssh(…)` (below) serves SSH. Under `tls()` the handler is the same `RawHandler`: it sees plaintext, and `c.tls` tells it the connection's TLS parameters.

```ts
import { serve, tls } from '@zen/svc';

const cert = await db.table('certs').get('imap');   // kept wherever the app keeps it
const h = serve(db, {
  service: ['svc', 'imaps'],
  layer: tls({ cert: cert.chain, key: cert.key, alpn: ['imap'] }),
  handler: async (c, tx) => {
    // c.input: plaintext; c.tls: {version, suite, sni, alpn, truncated}
    // c.write() is encrypted; c.end() and c.close() send close_notify first
  },
});
```

* **Certificates.** `cert` is the PEM chain, leaf first; `key` its PEM key, PKCS#8 or SEC1: ECDSA P-256 or P-384, or Ed25519 (RSA server keys aren't supported, TD-TLS-RSA-SERVER-KEY). One certificate per service, no SNI-based choice and no client certificates. The echo app keeps its certificate in a table, sealed like every row; an admin helper and ACME are later work (TD-SVC-ACME). A bad certificate or key is reported through `onError` when `serve` starts, and nothing is claimed.
* **Versions.** `versions: ['1.3']` turns 1.2 off. 1.2 offers only the ECDHE-ECDSA AEAD suites and requires the extended master secret; there are no tickets, no resumption, no early data and no renegotiation.
* **The module.** The engine is a second WASM module, `@zen/svc-wasm` (about 1.2 MB, 0.5 MB gzipped, with the SSH engine), loaded the first time a service uses `tls()` or `ssh()`. Node reads it from `node_modules`; a browser fetches `zen_svc_wasm_bg.wasm` next to the glue (`scripts/build-apps.mjs` copies it next to an app's bundle), or from `tls({wasm})`.
* **When the handler runs.** On the step that finishes the handshake (so a server-first protocol can greet), and then on new plaintext or an EOF it hasn't seen. `c.eof` is set on close_notify, and on an EOF without it, which `c.tls.truncated` tells apart. `c.end()` sends close_notify, then FIN; `c.close()` close_notify, then closes; `c.abort()` resets without close_notify.
* **What a takeover keeps.** Once the handshake is done, the engine state (keys, sequence numbers, the plaintext not acked) is sealed with every step, and a successor restores it and continues the live connection; the step that finishes the handshake also makes the connection resumable. A handshake can't be serialized: if its handler dies, or a handshake step's commit outcome is unknown, the connection is aborted (TD-SVC-TLS-HANDSHAKE-RESUME), and the client connects again.

### STARTTLS: `starttls()`

Protocols such as SMTP (RFC 3207), IMAP and POP3 begin in plain and switch to TLS on a command. `starttls(…)` takes the same options as `tls(…)`; it begins as `raw()`, and the handler switches with `c.startTls()`:

```ts
import { LineReader, serve, starttls } from '@zen/svc';

serve(db, {
  service: ['svc', 'smtp'],
  layer: starttls({ cert: cert.chain, key: cert.key }),
  handler: (c, tx) => {
    const r = new LineReader(c.input);
    for (let l = r.nextText(); l !== null; l = r.nextText()) {
      if (l.toUpperCase() === 'STARTTLS' && !c.tls) {
        c.write('220 2.0.0 Ready to start TLS\r\n');   // sent in plain
        c.startTls();                                    // the input after this line is TLS
        break;
      }
      …
    }
    c.ack(r.consumed);
  },
});
```

* **The switch** happens after the step's writes, which go out in plain. The input after what the step acks is handed to the TLS engine, so plaintext a client sent after the command (pipelined, or injected by an attacker in transit: CVE-2011-0411) is never read as a command: the handshake fails on it and the connection closes. Stop reading at the command and ack only up to it.
* **After the handshake** the handler runs again with `c.tls` set and empty input (SMTP waits for a new `EHLO`; a protocol whose server speaks first can greet), then as under `tls()`.
* **Sealing.** The step that switches also stops the inbox's sealing (`unseal`, below): what follows is TLS ciphertext.
* **Takeover.** The plain phase is resumable from the first step, as `raw()`; so is a connection that has switched but not begun its handshake. A handshake in progress isn't: if its handler dies, the connection is closed. The TLS phase is resumable again, as under `tls()`.
* `c.startTls()` throws under the other layers, after the switch, and with `end()`, `close()` or `abort()` in the same step.

### SSH: `ssh()`

`ssh(…)` serves SSH in the handler (svc.md §7.2): the key exchange (`mlkem768x25519-sha256` or `curve25519-sha256`), the Ed25519 host key, `publickey` logins and `session` channels with `exec`, `shell` and `subsystem`. An SSH connection isn't one byte stream, so the handler gets an `SshStep` instead of a `RawStep`: the login question to answer, the step's events, and the channels.

```ts
import { serve, ssh } from '@zen/svc';

const host = await db.table('certs').get('ssh');    // the host key's PEM, kept by the app
const h = serve(db, {
  service: ['svc', 'git'],
  layer: ssh({ hostKey: host.key }),
  handler: async (c, tx) => {
    if (c.question) {                                // may this key log in as this user?
      const k = await tx.table('ssh_keys').get([c.question.user, c.question.key.fingerprint]);
      c.answer(k !== undefined);
    }
    for (const e of c.events) if (e.type === 'exec') { /* e.ch, e.command: keep it in c.state */ }
    for (const ch of c.channels) {
      const n = ch.write(ch.data);                   // a cat: write what fits, ack that much
      ch.ack(n);
      if (ch.eof && ch.data.length === 0) { ch.exit(0); ch.close(); }
    }
  },
});
```

* **Logins.** The handler decides: `c.question` is `{user, key: {algorithm, fingerprint, blob}, signed}`, asked once without a signature (a yes lets the client sign) and once with one (a yes logs it in). Answer in the step that asks; a question left unanswered is refused. User keys are Ed25519, or RSA (2,048 to 4,096 bits) with SHA-2 signatures. `c.user` is set once logged in. The echo app keeps `authorized_keys` in a table, `ssh_keys`, keyed by user and fingerprint.
* **The host key** is an unencrypted OpenSSH Ed25519 PEM (`ssh-keygen -t ed25519 -N ''`), kept wherever the app keeps it; the echo app keeps it in `certs`. A bad key is reported through `onError` when `serve` starts.
* **Events and channels.** `c.events` holds this step's `open`, `exec` (with `command`), `shell`, `subsystem` (with `name`), `data`, `eof` and `close`. A command comes only once, in its event, so keep it in `c.state` with whatever else a channel needs. `ch.data` is what the client sent and the handler hasn't acked; `ch.ack(n)` consumes it, which opens the client's window. `ch.write()` and `ch.stderr()` return how much they took: what the client's window takes, what fits the engine's 16 KiB queue, and what fits the step's output. Keep the rest (a `cat` simply doesn't ack it). `ch.exit(code)`, `ch.end()` and `ch.close()` follow what is queued. `c.disconnect()` ends the connection.
* **When the handler runs.** On every step, since a window adjust carries no event but lets a writer go on. A step whose writes were cut short by `out_window`, or that acked data while the engine held input back, runs again at once. Once the engine has ended (the client disconnected, or a protocol error, sent as DISCONNECT and reported through `onError`), the connection is closed without the handler.
* **The window.** Each channel is offered `window` bytes, by default (the service's `window` − 64 KiB) / 10, so that 10 channels' data in flight and a key exchange fit the relay's input window (svc.md §7.2). SSH services should set `window = 1048576` in their `[[service]]` table.
* **What a takeover keeps:** everything, from the first step. The engine state (the version lines, both KEXINITs during a key exchange, keys and sequence numbers, the login in progress, the channels with their windows and buffers) is sealed with every step, so a successor continues the session mid-transfer, mid-rekey or mid-key-exchange; a question pending at a takeover is asked again.
* **Not supported** (TD-SVC-SSH-SCOPE): passwords and keyboard-interactive logins, a PTY, port and agent forwarding, ECDSA and security-key user keys.

### Members as clients: `tcp-auth` and `zen-connect`

A `tcp` service is a public port: anyone on the internet can connect, and the handler authenticates them itself (an SSH key, a TLS client, a password in the protocol). A `tcp-auth` service has no port. Members connect through zen-serve's own listener, as a WebSocket on `/v1/svc/connect` with their zen token (api.md §14.9), and the server admits them only with topic `append` on the service id. Everything after that is the same as for `tcp`: the queue, the handlers, the steps, the takeover. The handler sees the connector in the claim's `peer`, `{user, device}`, instead of an address.

```ts
const s = await session.svcConnect({
  fs: 1,
  svc: bytes.fromHex('8899aabbccddeeff0011223344556677'),  // the [[service]] id
  onData: (d) => out.write(d),
  onFin: () => out.end(),                // the service half-closed
  onClose: (reset) => {},                // false after a normal end (close 1000)
});
s.write(bytes.utf8('hello\n'));          // in frames of at most 64 KiB
if (s.bufferedAmount > 1 << 20) { /* pause the source */ }
s.fin();                                 // half-close; s.reset() resets
```

It works in browsers too (it authenticates with a first frame, not a header). A refusal rejects with a `ZenError`: `unauthorized`, `forbidden` (no `append`), `not_found` (no such service, or not `tcp-auth`), or `quota` (`max_conns`, or too many connections from the address).

**`zen-connect`** (`@zen/svc`'s bin; `node scripts/build-zen-connect.mjs` builds `packages/svc/dist/zen-connect.mjs`) makes a `tcp-auth` service a local stream for ordinary clients. Without `--listen` it joins stdin and stdout to one connection, which is what ssh's `ProxyCommand` wants; with `--listen HOST:PORT`, each local connection accepted gets its own.

```
# ~/.ssh/config: git and ssh to the service, signed in with a device key.
Host git.zen
  HostKeyAlias git.zen
  ProxyCommand zen-connect --url https://zen.example.org --fs 1 --svc 8899aabbccddeeff0011223344556677 --device-key ~/.config/zen/device.json

# Then:  git clone ssh://git@git.zen/repo.git
# Or a local port for any TCP client:
#   zen-connect --url https://zen.example.org --svc 8899… --api-token-file ~/.config/zen/token --listen 127.0.0.1:2222
```

* **Sign-in:** `--api-token-file FILE` (or `$ZEN_API_TOKEN`), `--device-key FILE`, or `--user NAME` with the password in `$ZEN_PASSWORD`. Nothing is asked on the terminal: under `ProxyCommand`, stdin is the connection. With `--listen`, a session about to expire is signed in again before the next connection.
* **Ends.** The local side's EOF is a `fin`; the service's `fin` ends the local output; a reset on either side resets the other. ssh hangs up its ProxyCommand as it exits: zen-connect still forwards what ssh wrote, then waits up to 5 s for the service to close.
* **Rights are checked once**, when the connection opens, as a TCP peer is admitted once. Removing a member's `append` doesn't end their open connections; the handler knows who is connected and can end them itself.
* **Backpressure.** Writes pause the local source while more than 1 MiB waits in the WebSocket. What the service sends can't be paused (the standard WebSocket has no way to): a local reader slower than the service makes zen-connect buffer it. SSH's own windows bound that; other protocols may not (TD-SVC-CONNECT-BACKPRESSURE).
* **The address bucket.** `per_ip_rate` and `per_ip_burst` apply to the WebSocket client's address. Behind a reverse proxy every connector has the proxy's address: set `per_ip_rate = 0` there.

### The sealed inbox

Over TLS or SSH, the input the server stores is ciphertext. A plaintext protocol (SMTP before STARTTLS, a line protocol) would leave the peer's bytes at rest in the keyspace and its backups. A service with an `inbox` seals them (svc.md §9): the owning node encapsulates to the inbox's X-Wing key once per connection and stores each input chunk sealed under the derived key. Only handlers holding the inbox's seed can read them.

```ts
// The admin, once: a seed, kept in a sealed row; its public key goes in the config.
const seed = zw.randomBytes(32);
await db.table('certs').put({ name: 'inbox', key: bytes.hex(seed) });
console.log(`inbox = "${bytes.hex(zw.inboxPublic(seed))}"`);  // [[service]] … inbox = "…"

// Each handler:
serve(db, {
  service: ['svc', 'smtp'],
  inbox: seed,                      // read back from the row
  handler: (c, tx) => {
    // … a protocol that turns to ciphertext without starttls():
    c.unseal();                     // what follows needs no sealing
  },
});
```

* **The handler sees plaintext as usual.** The runtime opens each chunk at its offset, so a chunk moved, dropped or altered fails, and the connection is released with `close`. A successor handler decapsulates again from the claim's ciphertext.
* **Without the seed** a handler can't serve the service: it reports an `inbox` error through `onError` and releases each connection with `close`.
* **`c.unseal()`** is sticky and sent once, with the step. Under `starttls()`, `c.startTls()` does it. The server stores plain chunks from the next batch it stores after the step commits; chunks sealed before that still open.
* **What it protects:** input at rest (the keyspace, backups, exports). The server process still sees the bytes in transit, as it reads them from the socket.
* **Changing the key** needs the old seed until the connections opened under it end: there is no rotation helper yet (TD-SVC-INBOX-ROTATION).

## 12. Integrity

What this catches and what it doesn't, how to turn it on everywhere, and what to do about an alarm: [INTEGRITY.md](INTEGRITY.md). This section is the TypeScript API.

The server can't read or forge data, but it can serve an **older** valid answer, **withhold** the newest events, or show two groups of clients two histories (a **fork**). Integrity detects these up to fork consistency (spec/formats.md §14, DESIGN.md §2.6). It is opt-in per `Fs` object:

```ts
const fs = session.fs(1);
const integrity = await fs.integrity({
  device: { device, cert },   // the signer; omit to verify only
  local,                      // where pins are kept (store integrity/<fs>); default memory
  sigEvery: 64, delayMs: 5000,
  onAlarm: (a) => report(a),
});
const ufs = await fs.unlock();          // attach before unlocking, so the header is checked too
await integrity.sync();                 // verify every device's newest checkpoint
const sub = await integrity.watch(await session.stream());   // or sync on every pushed head
```

* **Checkpoints.** A device with a signer writes a hash-chained, signed, sealed **checkpoint** (§14.1) after `sigEvery` changes, `delayMs` after the last one, on `checkpoint()` and on `close()`. Each names the newest state of every object the device wrote or read through (topic heads and its event chains, trees, relayed connections, authenticated zen-db namespaces, volumes (zen-blk)), the ACL and signed header it verified, and every other device's head it verified (`seen`). Processes of one device share one sequence: a put that loses the race merges the winner's entries and retries.
* **Pins.** `sync()` verifies each device's head: its signature (the cert from the ACL, or the device registry checked against the ACL's member), its chain from the pinned one, its `seen` links, and that no object went back. It raises the **pins**: per device `(seq, hash, body)`, and per object the newest state any device vouched for. Pins persist on `local`, with the ACL version, so a new process continues where the last stopped.
* **What is checked against the pins:** a topic read without `key` or `limit` that reaches the end must reach the pinned head and each pinned chain position; `topic.verifyHead()` checks the pinned head is still served; the ACL can't go below its pin; the header can't go below its pinned version (or change at it); this device's head can't go below what it wrote.
* **Event chains** (§5.1). With integrity attached, appends write version-2 bodies: each writer's events on a topic form chains `(chain id, dseq, dprev)`, advanced only when a commit is known to have applied. Unfiltered readers (`read` without `key`, subscriptions) report a skipped `dseq` as a **gap** and another body at a seen position as a **rewrite**; filtered readers (keys, consumer groups) check against the pins only. `sender` is the signer's device.
* **Trees** (fs.md §9). A replica (`@zen/fs`) with integrity attached keeps the tree's state hash next to its node table and checks it wherever its table is exactly the server's at a caught-up `changes` answer: `integrity.observe([2, tree, ops, chain, state])`. An `ops` below what a verified checkpoint (or this device) named is a **rollback**; the same `ops` with another chain or state is a **fork** (another history, or a wrong merge). The check that matters is against floors known *before* the answer was asked for, so honest writes racing a checkpoint never alarm. Trash purges extend the op chain (formats.md §11.5), so replicas before and after one agree. After an alarm the replica stops following the feed (`sync()` rethrows it, `status().alarm` has it); local writes still queue and send, since the server can't forge them. A replica's own writes count toward the cadence; what it only verified rides in the next checkpoint.
* **Relayed connections** (svc.md §12). A handler (`@zen/svc`) notes `[5, svc, conn, in_acked, out_head]` after each committed step, writes a checkpoint before it hands a live connection back or releases its lease, and syncs the fs's checkpoints before it resumes claimed connections. A claim below the newest verified entry is a rollback: the connection is aborted. Docs/SVC.md §6 has the window.
* **Authenticated zen-db namespaces** (zendb.md §14, docs/DB.md §7.1). Every read of a database opened `integrity: 'authenticated'` is checked against its namespace root (the NsRoot) read in the same transaction, and the NsRoot against the floor of `[3, ns, seq, NH]`: a lower `seq` is a rollback; the same `seq` with another hash, or a successor whose `prev` isn't the floor's hash, is a fork. Writers note the new root after each commit (counted); readers note what they read. The client hook underneath is `Transaction.onRead`: it sees every server answer before a read returns it.
* **Alarms.** `{kind: rollback | fork | gap | rewrite | invalid, object, id?, detail}`: recorded in `integrity.alarms`, passed to `onAlarm`, and thrown from the call that found it as an `IntegrityError` (code `integrity`). After one, `integrity.trusted` is false and no more checkpoints are written: the device won't sign a view it knows is false. What to do next is the app's policy: stop writing, alert the user, compare pins.
* **Comparing out of band.** `exportPins()` gives a few hundred bytes (the ACL and header pins and each device's head) to pass over another channel (a QR code, a chat); `comparePins(bytes)` on the other client returns the alarms. Two clients the server keeps on separate histories find a fork this way, or as soon as either sees a checkpoint of the other side.
* **Signed headers** (§12.1). `fs.init(slots, {signed: true})` or `ufs.signHeader()` signs the header with the admin's identity (`session.useIdentity(identity)`); every later edit (slots, policy, rotation) signs its successor and fails with `signer_needed` without the identity. Readers verify the signature against an admin of the verified ACL, and with integrity attached pin `(version, HH)`.
* **Enrollment.** A session without a device key (passkey, OPAQUE, API token, mTLS) enrolls one: `session.enrollDevice(identity, {vault})` makes a device key, certifies it with the user identity and registers it (api.md §15.2); `session.approveDevice(devicePublic, identity)` certifies a key made elsewhere (a phone showing its key as a code to a laptop that holds the identity). An enrolled device signs checkpoints; signing in by device key still needs its cert in the ACL.
* **Not covered:** a server that keeps two groups apart forever (fork consistency; an independent witness is TD-WITNESS), and rollbacks within the cadence window of a device that hasn't checkpointed yet (`sigEvery: 1` closes it).
* **Native programs** use the same checkpoints and pins through the Rust client (§13).

## 13. The Rust client

[`crates/zen-client`](../crates/zen-client) is a native client for programs that aren't in a browser or Node: the block device (zen-blk, zen-ublk) and the `zen-client` CLI. It covers what they need, with the same rules as `@zen/client`, so a TS device and a Rust device verify each other's checkpoints:

* **Sign-in** with a device key (method 1), or a session or API token (`Client::session_from_token`). A device-key session signs in again on its own after a 401 (expired or revoked), and resends: commits carry their `commit_id` and checkpoint puts are idempotent, so a resend applies at most once. A long-running daemon outlives `session_ttl_secs`.
* **The ACL** walked from the pinned version (`session.acl().head()`), as in §3.
* **The fs header**, signed or not (§12, "Signed headers"), `init` with device keyslots, `add_device_slot`, `edit_header`, `sign_header`, `rotate`, and `unlock_device` with the device's own keyslot. Older epochs open through the header's chain.
* **KV** reads and transactions (short and long mode, `expect_key`, own writes overlaid, retries on `conflict`/`too_old`), commits replayed with one `commit_id`, and **fences**: `tx.fence_lease(&lease)` makes a commit fail with `claim_lost` once another device holds the lease (api.md §16). `Lease::acquire`, `renew`, `release`.
* **Integrity** (§12): `fs.integrity(IntegrityOptions {signer, pins, sig_every, delay, on_alarm})`, then `note`, `observe`/`observe_at`, `checkpoint`, `sync`, `export_pins`, `compare_pins`, `close`. Bodies of every entry type are verified and carried forward; Rust writers note volumes and namespaces.
* **TLS**: rustls on the pure-Rust provider (zen-tls-rustcrypto, with the hybrid X25519MLKEM768 key exchange), TLS 1.3 only, verifying with the platform's roots or only a given CA (`TlsOptions {ca}`). The crate builds without a C compiler.
* **Not in it:** topics, streams, trees, relayed services, zen-db, and sign-in by passkey, OPAQUE or client certificate. Those stay in the TS packages.

**Files.** The **device key file** is zen-mount's (§9): JSON `{identity_seed, device_seed, cert}` in hex, mode 0600. The **pins file** holds what this device verified for one fs: the pinned ACL version, the header pin and each device's head (formats.md §14.4), as CBOR, written atomically with mode 0600. Keep it with the device key: without it, the next run starts over and trusts whatever the server serves at its first sync. A pins file that doesn't decode, or is another fs's, is refused rather than ignored.

**The CLI.**

```
zen-client --url URL --device-key FILE --fs N [--pins FILE] [--origin O] [--ca FILE] COMMAND
  sync                     verify every device's newest checkpoint
  checkpoint               sync, then write a checkpoint of this device (prints its seq)
  pins export [--out F]    exported pins (formats.md §14.5): raw to F, or hex on stdout
  pins compare F|-         compare another client's pins (raw or hex)
  kv get|set|list PATH     values by path: /-separated UTF-8 elements
  header                   {signed, epoch, slots}
```

Every command signs in, attaches integrity with the pins file (default: the key file's path + `.pins-<fs>`), unlocks the fs with the device's keyslot and syncs first, so `kv get` is checked against the floors too. An alarm prints as JSON on stderr (`{kind, object, id?, detail}`) and exits **3**; other errors exit 1.

**Comparing pins between a phone and a server.** The phone (TS) shows `integrity.exportPins()` as a QR code or sends it over chat; on the server, `zen-client … pins compare -` reads it. Exit 0: both views are one history so far. The other way, `zen-client … pins export` prints hex, which `integrity.comparePins(fromHex(…))` checks.

**Sharing it.** The connect options and their defaults are `zen_client::cli::ConnectArgs` (a clap `Args`), with `connect` doing the sign-in, attach, unlock and sync above. `zen-ublk` ([BLK.md](BLK.md)) takes the same options and shares the pins file, so the two tools are two processes of one device: their checkpoint puts merge (formats.md §14.3).

## 14. Security notes

* **Secrets stay in WASM memory** behind handles (`FsKeys`, `SigningIdentity`, `DeviceSecret`, `PasswordKey`, OPAQUE state); `free()` and `UnlockedFs.close()` zeroize them. Values passed in from JS (passwords, recovery keys) are copied; JS strings can't be wiped.
* **Argon2id parameters.** The floor is 64 MiB, t=1. Browsers should register with 256 MiB, t=3 (the default of `addPassphrase`); 1 GiB takes ~8 s in WASM (docs/STATS.md §5.2). The parameters the server returns are bounded on use (formats.md §6), so a hostile server can't make a client hang.
* **Origins.** Every sign-in signs the origin the client believes it talks to. A browser page served by the server uses its own origin; elsewhere, pass the public origin (auth.md §5).
* **What the server can still do** to a client: refuse service, withhold or roll back what it serves. With integrity (§12) and signed headers, a rollback, a withheld tail or a fork is detected once a checkpoint covers it, or when pins are compared; without them, only the ACL chain is pinned. Trees, relayed connections and authenticated zen-db databases are covered the same way (§12).
