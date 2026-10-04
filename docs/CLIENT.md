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
