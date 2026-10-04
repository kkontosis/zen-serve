# HTTP / WebSocket API (v1)

The wire contract of zen-serve. It supersedes the draft in `docs/API.md` where the two differ. The storage layout behind it is in [keyspace.md](keyspace.md).

## 1. Conventions

* **Encoding.** Request and response bodies are **CBOR** (RFC 8949), `Content-Type: application/cbor`. Maps use text keys, with the field names below. Byte fields are CBOR byte strings. Integers are unsigned unless noted. `?` marks an optional field, which may be absent or `null`.
* **Methods.** Everything under `/v1` is `POST` with a CBOR body, except `GET /v1/info` and the WebSocket `GET /v1/stream`.
* **Authentication.** Requests carry `Authorization: Bearer <session token>`, where the token is base64url without padding (§3). Three requests don't need it: `/v1/info`, `/v1/auth/*` and `/v1/acl/put`.
* **Errors.** An error response is `{code: text, message: text}` with this status:

  | Status | `code` | Retry? |
  |---|---|---|
  | 400 | `bad_request` | no |
  | 401 | `unauthorized` (no or expired session) | after signing in again |
  | 403 | `forbidden` (ACL) | no |
  | 403 | `method_disabled` (the sign-in method is turned off on this server, auth.md §2) | no |
  | 404 | `not_found` (unknown fs, group, …) | no |
  | 409 | `conflict`, `too_old` (read version left the ~5 s window, or a transient storage error) | **yes**, the whole transaction |
  | 409 | `commit_unknown` (the storage could not tell whether the write applied) | only if idempotent: `/v1/commit` with the same `commit_id` is; otherwise re-read first |
  | 409 | `clock_skew` (an `hlc` is too far ahead, fs.md §3.4) | after fixing the clock |
  | 409 | `stale_op` (an `hlc` is past the horizon or needs too deep an undo, or the operation names a purged node, fs.md §3.4) | with a fresh `hlc` (rebase); not for a purged node |
  | 409 | `resync` (a change-feed cursor is older than the kept tombstones, fs.md §5) | with a full sync |
  | 409 | `version_mismatch` (ACL / header CAS), `group_exists`, `commit_id_reused` | no |
  | 412 | `cursor_moved`, `not_leader`, `claim_lost` | not for this event |
  | 413 | `too_large` | no |
  | 429 | `quota` (a quota or rate limit: groups per topic §8.1, ephemeral publishes §9.1) | later |

* **Ids.** `fs` is the `u32` fs_id. `topic` is a topic id (16·n bytes, 1 ≤ n ≤ 16). `key_token` is 16 bytes. `version` (a value version) is a 10-byte versionstamp. `offset` is a 12-byte event offset (keyspace.md §2). `read_version` is a `u64`.

## 2. `GET /v1/info`

No authentication. Returns:

```
{ server: text, api: 1,
  suites: [1],                       // supported suite ids (spec/suites.md)
  formats: [1],
  features: [text],                  // e.g. "kv", "log", "consume", "ephemeral", "static", "fs"
  cross_origin_isolation: bool,      // DESIGN-3 §4.2
  claimed: bool,                     // an ACL exists
  time_ms: u64,                      // server clock, unix ms (HLC observation, fs.md §2)
  limits: { max_key_bytes, max_value_bytes, max_envelope_bytes, max_commit_bytes,
            max_commit_ops, max_range_items, max_range_bytes,
            idempotency_ttl_secs, session_ttl_secs, claim_ttl_ms, ephemeral_ttl_secs,
            ephemeral_bytes_per_sec, ephemeral_burst_bytes,
            max_groups_per_topic,
            crdt_max_skew_ms, crdt_horizon_secs, crdt_max_redo, crdt_max_depth,
            chunk_grace_secs },
  auth?: { methods: [text], default?: text } }   // sign-in methods (auth.md §2)
```

Defaults: `max_key_bytes` 2,048; `max_value_bytes` and `max_envelope_bytes` 90,000; `max_commit_bytes` 8,000,000; `max_commit_ops` 10,000; `max_range_items` 10,000; `max_range_bytes` 8,000,000; `idempotency_ttl_secs` and `session_ttl_secs` 86,400; `claim_ttl_ms` 30,000; `ephemeral_ttl_secs` 60; `ephemeral_bytes_per_sec` 65,536 and `ephemeral_burst_bytes` 1,048,576 (§9.1; a server that doesn't send them has no ephemeral rate limit, which clients read as 0, "no limit"); `max_groups_per_topic` 64; `crdt_max_skew_ms` 60,000; `crdt_horizon_secs` 604,800; `crdt_max_redo` 1,000; `crdt_max_depth` 1,000; `chunk_grace_secs` 86,400.

* **Range reads** (`/v1/kv/range`, `/v1/log/read`, `tree/children`, `tree/changes`, `dlq/list`, stream subscriptions) return at most `max_range_items` items **and** stop once the items returned hold about `max_range_bytes` of keys and values; either cut sets `more`. A range the server must read whole (`expect_ranges`, `clear_ranges`, a file's versions) returns 413 `too_large` past either cap.

## 3. Sessions

The sign-in methods, their configuration, the credential store and the origin policy are specified in [auth.md](auth.md). Every method ends in a session token, except API tokens, which are bearer tokens themselves (auth.md §9).

### 3.1 `POST /v1/auth/challenge`

`{}` → `{challenge: bytes(32)}`. Challenges are single-use and expire after 60 s. Any node of a cluster accepts a challenge issued by another: a challenge is `nonce(12) ‖ u32 expires_unix ‖ MAC(16)` under a cluster-wide key, and its use is recorded when the session is created. Clients treat it as opaque.

### 3.2 `POST /v1/auth/session`

Device sign-in (method 1, auth.md §6).

```
{ challenge: bytes(32), origin: text,
  user: bytes,          // user's public identity (formats.md §7.2)
  cert: bytes,          // device certificate issued by `user` (formats.md §7.4)
  sig: bytes }          // device signature, purpose zen/v1/sig/session (formats.md §10)
→ Session

Session = { token: bytes(32), expires_unix: u64, user_fp: bytes(32),
            device_fp: bytes(32),   // the device, or for other methods the credential id (auth.md §3)
            method?: text }         // the sign-in method (auth.md §1)
```

403 `method_disabled` if device keys are off. Otherwise the server checks all of these, or returns 401:
* the challenge is live
* `origin` is one the server accepts (§3.3)
* `user` is a member of the current ACL
* `cert` verifies against `user`, and the certified device is listed under that member
* `sig` verifies with the certified device's signing key

A session is checked again on every request (auth.md §3): it stops working as soon as an ACL version removes its device, or its user for other methods, and while its method is turned off. Sessions are stored (hashed) in the keyspace, so every node of a cluster accepts them. Each node caches a session for up to 10 s.

### 3.4 `POST /v1/auth/logout`

`{}` → `{}`, authenticated. Ends the caller's session. The node that serves the request stops accepting it at once; other nodes stop within 10 s (their session cache).

### 3.3 Origin binding

`origin` is the server origin **as the client sees it**: `scheme://host[:port]`, with no trailing slash. Binding it stops a malicious server from relaying a challenge from the real one. The server accepts:
* each `public_origins` entry in its config, or
* if that list is empty, `http://<Host>` and `https://<Host>` from the request's `Host` header.

  The fallback trusts the `Host` header, which a relaying server chooses when it forwards the request, so it does **not** stop the relay attack above. It is for development; a production server sets `public_origins`, and warns at start-up when it is empty.

## 4. ACL and fs headers

### 4.1 `POST /v1/acl/put`

```
{ acl: bytes,            // signed ACL (formats.md §9)
  claim?: text }         // the claim token; required exactly for version 1
→ { version: u64 }
```

* The ACL is authenticated by its own signature, so this request needs no session.
* **Bootstrap.** While no ACL exists, the server keeps a random **claim token**. It prints the token at start-up and stores it in `<data_dir>/claim-token` (mode 0600). Version 1 is accepted only together with that token. Once version 1 commits, every node of the cluster forgets its token and deletes its `claim-token` file: the node that accepted it at once, every other node as soon as it sees the new ACL, and a node that was down when it next starts.
* Validation rules: formats.md §9.3.
* The version CAS failing returns 409 `version_mismatch`.

### 4.2 `POST /v1/acl/get`

`{from?: u64}` → `{head: u64, entries: [bytes]}`. Returns the signed ACLs from version `from` (default: `head`) up to `head`, in order, so clients can verify the chain (G1). Needs a session.

### 4.3 `POST /v1/fs/list`

`{}` → `{fs: [{id: u32, rights: [text]}]}`: the configured filesystems on which the caller has at least one right (fs or topic).

### 4.4 `POST /v1/fs/header/get` and `/v1/fs/header/put`

```
get: {fs}                                   → {header: bytes?, version: bytes(10)?}
put: {fs, header: bytes, expect: bytes(10)?} → {version: bytes(10)}
```

* `get` needs fs `read`, or admin.
* `put` needs admin. `expect` absent means "must not exist yet". A mismatch returns 409 `version_mismatch`.
* The header (volume header + keyslots) is opaque to the server.

## 5. KV

| Endpoint | Request | Response |
|---|---|---|
| `/v1/grv` | `{}` | `{read_version}` |
| `/v1/kv/get` | `{fs, keys: [bytes], read_version?}` | `{read_version, items: [{key, value: bytes?, version: bytes(10)?}]}` |
| `/v1/kv/range` | `{fs, begin: bytes, end: bytes?, limit?, reverse?, read_version?}` | `{read_version, items: [{key, value, version}], more: bool}` |

* Everything needs fs `read`.
* `keys` and range bounds are **stored keys** (formats.md §3.2). `value` is the sealed value, without the stored versionstamp; `version` is that versionstamp.
* `/v1/kv/get` returns one item per requested key, in request order, with `value` and `version` null for a missing key.
* `/v1/kv/range` reads `[begin, end)`, with `end` absent meaning the end of the fs.
  * `limit` defaults to and is capped by `max_range_items`; `max_range_bytes` caps the response as well (§2).
  * `more` means a limit cut the range short. To continue, set `begin` to the last key ‖ `0x00` (or `end` to the last key, when `reverse`).
* **Snapshots.** Reads at the same `read_version` see one consistent snapshot. If `read_version` is absent, the server takes a fresh one and returns it.
  * A `read_version` must come from `/v1/grv` or an earlier read response.
  * A read version older than about 5 s returns 409 `too_old`.

## 6. Commit: the only write path

```
POST /v1/commit
{ commit_id: bytes(16),
  read_version?: u64,                                       // short mode
  read_conflicts?: [{fs, begin: bytes, end: bytes?}],
  expect?: [{fs, key: bytes, version: bytes(10)?}],         // long mode; version null = key must be absent
  expect_ranges?: [{fs, begin: bytes, end: bytes?, hash: bytes(32)}],
  writes?: [{fs, key: bytes, value: bytes?}],               // value null = delete
  clear_ranges?: [{fs, begin: bytes, end: bytes?}],
  append?: [{fs, topic: bytes, key_token: bytes(16)?, envelope: bytes}],
  consume?: [{fs, group: bytes, partition?: u32, key_token: bytes(16)?,
              from: bytes(12), to: bytes(12), token: u64}],
  chunks?: [{fs, id: bytes(16), data: bytes}],              // filesystem chunks (fs.md §4.1)
  crdt_ops?: [CrdtOp] }                                     // filesystem operations (fs.md §3, §4)
→ { commit_version: u64, versionstamp: bytes(10), appended: [bytes(12)], dots: [bytes(12)] }

CrdtOp = {fs, tree: bytes(16), op: "move",  node: bytes(16), parent: bytes(16), hlc: u64, meta?: bytes}
       | {fs, tree: bytes(16), op: "meta",  node: bytes(16), hlc: u64, meta: bytes}
       | {fs, tree: bytes(16), op: "write", node: bytes(16), replaces?: [bytes(12)],
                                            chunks?: [bytes(16)], manifest: bytes}
```

The whole commit is **one storage transaction**: all of it applies, or none of it.

1. **Idempotency.** If `commit_id` already committed, the stored result is returned and nothing is applied again. The record is kept for `idempotency_ttl_secs` (default 24 h, G13). A `commit_id` reused by a different device returns 409 `commit_id_reused`.
2. **Permissions and limits.** Writes and clears need fs `write`. `expect` and `expect_ranges` need fs `read`. Appends need topic `append`, and consumes need topic `consume`. Then sizes and quotas are checked: at most `max_commit_ops` operations, and at most `max_commit_bytes` counting every payload plus 512 bytes per operation for its keys and index entries.
3. **Short mode.** With `read_version`, the transaction runs at that version, and every `read_conflicts` range conflicts with any write committed after it. That gives full serializability, including phantoms. Anything else the server reads (cursors, leases, idempotency) is checked the same way. A conflict returns 409 `conflict`.
4. **Long mode.**
   * Each `expect` key is re-read: its version must equal `version`, or the key must be absent when `version` is null.
   * Each `expect_ranges` range is re-read, and its hash must equal `hash`, where
     `hash = H("zen/v1/range-hash", concat over the range in key order of lp(stored_key) ‖ version(10))`, with `H(label, x) = BLAKE3.derive_key(label, x)`.
   * A range with more than `max_range_items` items, or more than `max_range_bytes` of keys and values, returns 413.
   * Any mismatch returns 409 `conflict`.
5. **Clears**, then **writes**. `clear_ranges` apply first (each range at most `max_range_items` keys and `max_range_bytes`, else 413), then `writes`, where the last write to a key wins.
6. **Consumes** (§8.3) are processed before appends, in order.
7. **Appends.** Each append gets offset `versionstamp ‖ u16(i)`, with `i` its index in `append`. Appends become visible only when the commit commits, so events published inside an aborted transaction never exist.
8. **Chunks** are stored (each needs fs `write`, at most `max_value_bytes`).
9. **CRDT operations** apply in list order (fs.md §3, §4); each needs fs `write`. `meta` is at most `max_value_bytes`; so is a `write`'s `manifest` plus 16 bytes per entry of `chunks` (the stored version holds both, keyspace.md §3.6), which bounds a file at about `max_value_bytes / 32` chunks. Each `write` gets the dot `versionstamp ‖ u16(i)`, where `i` is its index among the commit's writes, returned in `dots`. A replayed commit returns the same `dots`.

## 7. Log

### 7.1 `POST /v1/log/append`

`{commit_id, append: [...]}`: shorthand for a commit with only `append`. Same response as §6.

### 7.2 `POST /v1/log/read`

```
{fs, topic: bytes, after?: bytes(12), key_token?: bytes(16), limit?}
→ {events: [{offset: bytes(12), key_token: bytes(16)?, envelope: bytes}], more: bool}
```

* Returns events strictly after `after`, which defaults to the zero offset. With `key_token`, it returns only that key's events.
* Needs topic `read`.

## 8. Consumer groups

### 8.1 `POST /v1/consume/groups`

```
{ fs, group: bytes(1..64), topic: bytes,
  mode: "broadcast" | "sequential" | "partitioned" | "per_key" | "single_key",
  partitions?: u32,        // partitioned: 1..=256
  key_token?: bytes(16),   // single_key
  max_inflight?: u32,      // events handed out per `next`; default 1
  max_attempts?: u32,      // default 5; 0 = unlimited
  on_poison?: "dlq" | "block",   // default "dlq" (G7)
  start?: "earliest" | "latest" } // default "earliest"
→ {created: bool}
```

* A group is **immutable** (G8). Creating it again with identical parameters returns `created: false`. Different parameters return 409 `group_exists`.
* A topic holds at most `max_groups_per_topic` groups (every append pays for each one); the next creation returns 429 `quota`.
* Needs topic `consume`.
* `broadcast` stores only the definition. Its members read the log with their own cursors (§7.2, §9).
* **Partitions.** `partitioned` assigns an event to partition `u128_be(key_token) mod partitions`. Events with no key go to partition 0. Appends after the group's creation are indexed by partition (keyspace.md §3.3), so finding a partition's next event costs one read; events from before the creation are scanned once.
* A `per_key` group starting at `earliest` puts every key's first event on the ready list. A topic with more than 100,000 events returns 413; use `latest` instead.
* `per_key` and `single_key` groups only see events that have a `key_token`.

### 8.2 Leases (`sequential`, `partitioned`, `single_key`)

```
POST /v1/consume/lease    {fs, group, partition?: u32, token?: u64, ttl_ms?: u32}
→ {token: u64, expires_version: u64, cursor: bytes(12)}
POST /v1/consume/release  {fs, group, partition?, token}  → {}
```

* **Acquire or renew.** If the lease is empty, expired or released, the caller gets it with `token = previous token + 1`.
  * If the caller passes the current `token` and holds the lease, it's renewed with the same token.
  * Otherwise the response is 412 `not_leader`.
* `ttl_ms` defaults to 10,000. Expiry is measured in versions (DESIGN-3 §3.1). Expiry is never early. It can be late by a second or two: on an idle FoundationDB cluster the version clock advances in steps.

### 8.3 Delivery and the consume step

```
POST /v1/consume/next
{fs, group, partition?, token?, limit?, wait_ms?}
→ {events: [{offset, key_token?, envelope, from: bytes(12), token: u64, attempts: u32}]}
```

* **Lease modes.** `token` must be the current lease token, or the response is 412 `not_leader`. The response holds the next `min(limit, max_inflight)` events of the partition after its cursor. `from` is the cursor that the event's consume step must present.
  * For the first event, `from` is the cursor itself.
  * For each later prefetched event, `from` is the previous event's offset.
  * This is the **delivery gate**: an event is never handed out before the cursor reaches its predecessor, except as prefetch to the lease holder, and commits must still land in order.
* **`per_key`.** No lease is needed. The server takes up to `limit` keys from the front of the ready list that have no live claim, oldest pending event first. For each key it creates a claim with a fresh `token`, valid for `claim_ttl` (30 s by default), and returns the key's next event, with `from` = the key's last committed offset.
* **Long-polling.** With `wait_ms` (≤ 30,000) and nothing to deliver, the request waits for an append to the topic.
* **Consume step.** A consume entry in a commit, `{group, partition | key_token, from, to, token}`, checks four things and advances atomically with the rest of the commit:
  1. The token is current: the lease (412 `not_leader`) or the key's claim (412 `claim_lost`).
  2. The cursor equals `from` (412 `cursor_moved`).
  3. `to` is the next eligible event after `from` (412 `cursor_moved` otherwise).
  4. Then the cursor is set to `to` and the attempt counter is cleared.
  * For `per_key` it also moves the key's ready entry to the key's next event, if there is one, and releases the claim.
  * A commit with only `consume` acknowledges an event without writes.

### 8.4 Failure, poison events and the DLQ (G7)

```
POST /v1/consume/nack {fs, group, partition? | key_token?, offset: bytes(12), token} → {attempts: u32, dead_lettered: bool}
```

* **Nack** increments the event's attempt counter and, for `per_key`, releases the claim.
* When `attempts` reaches `max_attempts` and `on_poison` is `dlq`, then in the same transaction:
  * the event is copied to the group's dead-letter list
  * the cursor or key advances past it, exactly like a consume step
* With `on_poison = block`, the event is redelivered indefinitely.

```
POST /v1/consume/dlq/list  {fs, group, after?: bytes(12), limit?} → {items: [{id: bytes(12), offset, topic, key_token?, envelope}]}
POST /v1/consume/dlq/retry {fs, group, id, commit_id} → commit result (re-appends the envelope to its topic)
POST /v1/consume/dlq/drop  {fs, group, id} → {}
```

* `retry` re-appends the envelope unchanged, with a new offset. It still decrypts, because the AAD binds only the fs, topic and key token.

### 8.5 `POST /v1/consume/cursor`

`{fs, group, partition? | key_token?}` → `{cursor: bytes(12), low_watermark: bytes(12)?}`

`low_watermark` is the offset of the oldest pending event in a `per_key` group's ready list.

## 9. WebSocket `/v1/stream`

* Binary frames, each holding one CBOR map with an `op` field.
* The first frame must be `{op: "auth", token: bytes}`. Nothing else is accepted before it.

**Client → server:**

| `op` | Fields | Effect |
|---|---|---|
| `auth` | `token` | authenticate the stream |
| `sub` | `id: u32, fs, topic?, prefix?, after?: bytes(12)` | Subscribe to one topic, or to every topic under a topic-id prefix (an empty prefix means the whole fs). Needs topic `read`. History after `after` is streamed from storage, then live events follow **with no gap and no reordering** (G9). With no `after`, only new events are sent. |
| `unsub` | `id` | stop a subscription |
| `epub` | `fs, topic, data: bytes` | ephemeral publish: not in the log; kept for at most about a minute (§9.1). Needs topic `append`. Rate-limited per device (§9.1): over the limit, the reply is `err` with code `quota`. |
| `esub` | `id, fs, topic?, prefix?` | ephemeral subscribe: messages published after the `ok`. Needs topic `read`. |

**Server → client:**

| `op` | Fields |
|---|---|
| `ok` | `id?` |
| `ev` | `id, topic, offset, key_token?, envelope` |
| `eph` | `id, topic, data, sender: bytes(32)` (device fp) |
| `err` | `id?, code, message` |

* Consumer delivery is not pushed over the stream: `/v1/consume/next` with `wait_ms` long-polls instead (§8.3).
* Watches only wake a subscription. The data always comes from a range read after the subscription's cursor, so reconnecting with the last received `offset` loses nothing.
* Ephemeral data should be sealed by the client with the topic key plus a sequence number (G21). The server forwards it as opaque bytes.

### 9.1 Ephemeral messages across nodes

Ephemeral messages pass through a short-lived ring in storage (keyspace.md §3.5), so a subscriber on any node receives messages published on any other. Delivery is best effort: there is no history, and a subscriber that falls behind may miss messages. Entries are deleted after `limits.ephemeral_ttl_secs` (default 60 s), so they never reach the log, and appear in a backup only if it is taken within that window.

**Rate limit.** Ephemeral messages bypass the fs quotas, so each device's publishes are limited by a token bucket:
* `limits.ephemeral_bytes_per_sec` (default 65,536) sustained, with bursts of `limits.ephemeral_burst_bytes` (default 1,048,576). A message costs its `data` length plus 256 bytes. `ephemeral_bytes_per_sec = 0` turns the limit off.
* `ephemeral_burst_bytes` must be at least `max_envelope_bytes` + 256, so a message of any allowed size can be sent.
* Both values are advertised in `/v1/info` `limits` (§2), so a client can pace itself.
* A publish over the limit is refused with `quota` and not delivered. The client waits and retries, or drops the message.
* The buckets are kept in each node's memory, per device. A device that publishes through several nodes of a cluster gets the limit on each, so the cluster-wide limit is the per-node limit times the number of nodes. A node restart refills them.

## 10. Static files

* `GET /unencrypted/*`, the root aliases, and the SPA fallback for `GET` with `Accept: text/html` (DESIGN-3 §4.2).
* Responses carry a default `Content-Security-Policy` with `require-trusted-types-for 'script'` (G15), plus `X-Content-Type-Options: nosniff`.
* When `cross_origin_isolation = true`, **every** response, `/v1` included, also carries COOP `same-origin`, COEP `require-corp` and CORP `same-origin`.

## 11. `POST /v1/admin/status`

`{}` → storage health, admins only (operations.md §3.1):

```
{ backend: "embedded" | "fdb", available: bool, healthy: bool,
  redundancy?: text,           // FoundationDB: "single", "double", "triple"
  machines: u32, processes: u32, coordinators: u32, messages: [text] }
```

## 12. Filesystem (spec/fs.md)

All requests need fs `read`. Writes go through `/v1/commit` (§6). A **node state** is:

```
NodeState = { node: bytes(16),
              parent?: bytes(16),                 // absent: invisible (fs.md §1)
              move_hlc: u64, move_device: bytes(32),
              meta?: bytes, meta_hlc?: u64, meta_device?: bytes(32),
              versions: u32,                      // number of content versions (siblings when > 1)
              changed: bytes(12) }                // change offset (fs.md §5)
```

| Request | Response |
|---|---|
| `POST /v1/fs/tree/list {fs}` | `{trees: [{tree, ops: u64}]}` |
| `POST /v1/fs/tree/get {fs, tree, nodes: [bytes(16)], read_version?}` | `{read_version, nodes: [NodeState]}`. Unknown nodes are left out. |
| `POST /v1/fs/tree/children {fs, tree, parent, after?: bytes(16), limit?, read_version?}` | `{read_version, nodes: [NodeState], more}`, in node-id order after `after` |
| `POST /v1/fs/tree/changes {fs, tree, after?: bytes(12), limit?, wait_ms?}` | `{changes: [{offset: bytes(12), node, state?: NodeState}], cursor?: bytes(12), more}` |
| `POST /v1/fs/tree/chain {fs, tree}` | `{ops: u64, chain: bytes(32)}` (formats.md §11.5) |
| `POST /v1/fs/file/get {fs, tree, node, read_version?}` | `{read_version, versions: [{dot, device, chunks: [bytes(16)], manifest}]}` |
| `POST /v1/fs/chunks/get {fs, ids: [bytes(16)]}` | `{chunks: [{id, data?}]}`. `data` is absent for unknown ids. |

* **`changes`.**
  * It lists nodes changed after `after`, in change order; with no `after`, it lists every node.
  * A change without `state` is a purged node (tombstone).
  * `cursor` is the offset of the last change returned. Pass it as `after` next time; it is absent when nothing was returned and no `after` was given.
  * With `wait_ms` (at most 30,000), an empty result waits for the next change of the tree.
  * 409 `resync`: start again without `after`.
* **Limits.** `limit` defaults to 1,000 (`children`, `changes`) and is capped at `max_range_items`; `max_range_bytes` of node records also sets `more` (§2). `nodes` and `ids` take at most 1,000 and 64 entries. `file/get` returns 413 when a node's versions exceed either cap.
* **`read_version`** works as in KV reads (§5): several reads at one version see one snapshot.

