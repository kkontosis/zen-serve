# @zen/client

The TypeScript client of zen-serve, for Node 22+ and browsers (`packages/client`). Every format and algorithm runs in the WASM build of zen-core (`crates/zen-wasm`, `@zen/wasm`), and so does the CBOR wire encoding, through zen-proto: the client and the server share one definition of every byte. The server only ever sees ciphertext and opaque ids.

`zen-mount`, a FUSE mount of a zen-serve filesystem, is in [§9](#9-zen-mount).

## 1. Building

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version <the wasm-bindgen version in Cargo.lock> --locked
scripts/build-wasm.sh          # crates/zen-wasm → packages/zen-wasm/pkg
npm ci
npm run build -w @zen/client   # packages/client/dist
```

Tests run against a real server, the debug build on the embedded backend:

```sh
cargo build -p zen-server
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
* `fs.close()` zeroizes the keys. Keys and secrets live in WASM memory behind handles; free handles you created yourself (`.free()`).

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
* Both retry the whole function on `conflict` / `too_old` (8 attempts, jittered backoff), so keep side effects out of it. Commits carry a random `commit_id` and are replayed with it after a network error or `commit_unknown`, which the server makes idempotent.
* `tx.clearPrefix(path)` deletes a subtree; reads in the transaction see its own writes and clears.

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
* Ephemeral messages are sealed like events, under a reserved key token, and carry the sender's HLC; receivers drop replays.

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
* **The clock.** Every tree of a session shares one hybrid logical clock, corrected once from the server's time. On `stale_op` or `clock_skew` the client rebases: it re-issues the operations with fresh timestamps. A clock store (`TreeOptions.clock`) keeps the clock across restarts.
* **Files** are 64 KiB chunks with random ids, uploaded over as many commits as `max_commit_bytes` needs, then one `write` with the sealed manifest. Given the previous version's chunk index (`previous`), unchanged chunks are reused. Concurrent writes that replaced the same version become **siblings**: `versions(node)` returns them all, `readFile(node, {version})` reads one, `resolve(node, keep)` keeps one.
* **Reads** fetch only the chunks a range needs (`read(node, version, offset, length)`).
* **Duplicate names** are possible, since the server can't see names: `displayNames(children)` gives `foo.txt`, `foo (2).txt`, … in node-id order.
* **The change feed** yields batches of changed nodes; a batch with `resync: true` means the cursor is too old and the caller must re-read the tree.
* `verifyChain(ops)` recomputes the tree's operation chain from the operations this client recorded (`TreeOptions.onOp`) and compares it with the server's.

## 9. zen-mount

`zen-mount` (packages/fuse) mounts one tree of an fs as a local directory, through FUSE.

```sh
sudo apt-get install fuse3 libfuse-dev pkg-config     # libfuse 2 headers: the module compiles at npm ci
npm run build -w @zen/fuse
ZEN_PASSWORD=… ZEN_PASSPHRASE=… node packages/fuse/dist/cli.js https://zen.example.org ~/zen \
  --user alice --passphrase --tree <hex>              # or --new-tree; --foreground; --read-only
```

* Sign-in by password key (`--user`), OPAQUE (`--opaque`, which also unlocks through its keyslot), an API token or a device key file; unlocking by passphrase, recovery key, device key or the sign-in. Secrets come from the environment, files or a prompt, never the command line.
* Without `--foreground` it detaches once mounted. SIGINT or SIGTERM flushes open files and unmounts.
* **How it maps:** directories are listed from the server and cached until the tree's change feed reports a change, so other devices' edits appear within a second or so. Reads fetch chunk ranges. A file opened for writing is buffered in memory and written as one new version on `close` or `fsync`, replacing the version it opened.
* **Conflicts:** two devices writing the same file concurrently leave siblings. The newest is shown as the file and the others as read-only `name (conflict <device>-<n>).ext`; deleting the copy resolves the conflict in favour of the file.
* **Limits** (`TD-FUSE-REPLICA`): no local replica (no offline use), no hard links, symlinks or xattrs, no `mmap` coherence across devices, and files up to about 175 MiB (`TD-FS-LARGE-FILES`).

## 10. Security notes

* **Secrets stay in WASM memory** behind handles (`FsKeys`, `SigningIdentity`, `DeviceSecret`, `PasswordKey`, OPAQUE state); `free()` and `UnlockedFs.close()` zeroize them. Values passed in from JS (passwords, recovery keys) are copied; JS strings can't be wiped.
* **Argon2id parameters.** The floor is 64 MiB, t=1. Browsers should register with 256 MiB, t=3 (the default of `addPassphrase`); 1 GiB takes ~8 s in WASM (docs/STATS.md §5.2). The parameters the server returns are bounded on use (formats.md §6), so a hostile server can't make a client hang.
* **Origins.** Every sign-in signs the origin the client believes it talks to. A browser page served by the server uses its own origin; elsewhere, pass the public origin (auth.md §5).
* **What the server can still do** to a client: refuse service, drop or roll back headers (formats.md §12), withhold events or tree changes. The ACL chain is pinned by the client; signed headers and the authenticated tier come in milestone 6.
