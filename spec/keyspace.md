# Server keyspace (storage layout)

The layout zen-serve uses in its ordered key-value store. Both backends use exactly these bytes: the embedded backend (`zen-store`, milestone 2) and FoundationDB (milestone 3). So a dump from one can be loaded into the other.

## 1. Encoding

* **Tuple encoding.** Keys are packed with the [FoundationDB tuple layer](https://github.com/apple/foundationdb/blob/main/design/tuple.md) encoding. Only this subset is used:

  | Type | Code | Encoding |
  |---|---|---|
  | byte string | `0x01` | bytes, every `0x00` escaped as `0x00 0xFF`, then a `0x00` terminator |
  | text string | `0x02` | UTF-8, escaped and terminated like a byte string |
  | integer | `0x0c`–`0x1c` | `0x14` for zero; `0x14 + n` then n big-endian bytes for positive values; `0x14 − n` then the ones' complement for negative values (n ≤ 8) |
  | versionstamp | `0x33` | 12 bytes: the 10-byte commit versionstamp, then a 2-byte big-endian user version |

  The encoding preserves order: tuples sort the same way as their packed bytes.
* **`pack(a, b, …)`** is the packed tuple. **`pack(…) ‖ raw`** appends raw bytes after a packed prefix. Raw suffixes are used for client-chosen keys, so range bounds map one-to-one.
* **Prefix ranges.** `range(p) = [p, strinc(p))`, where `strinc` drops trailing `0xFF` bytes and increments the last byte.

## 2. Versions, versionstamps, offsets

* **Version** (`u64`). A commit version from a clock running at about 1,000,000 versions per second. Every commit gets one, and versions strictly increase. The embedded backend computes `max(last + 1, unix_micros)`. FoundationDB's versions also advance at about 1,000,000 per second, but from the cluster's creation rather than the unix epoch; `zen-serve migrate` advances a new cluster past the embedded versions (operations.md). Leases and claims expire at a version (DESIGN-3 §3.1).
* **Versionstamp** (10 bytes). `u64(version) ‖ u16(batch_order)`. The embedded backend always uses batch order 0.
* **Offset** (12 bytes). An event's position: `versionstamp ‖ u16(index)`, where `index` is the append's position in the commit's `append` list. Offsets are unique, totally ordered across an fs and strictly increasing in commit order.
  * The zero offset (12 × `0x00`) means "before the first event".
* **Value version.** Every KV value and fs header is stored as `versionstamp(10) ‖ payload`. The versionstamp is written by the backend at commit time (FDB: `SET_VERSIONSTAMPED_VALUE`). It is the `version` clients `expect` (API §5).

## 3. Layout

`fs` is the `fs_id` integer. `topic` is a topic id (16·n bytes), `key` an event key token (16 bytes), `group` a consumer-group name (1–64 bytes, client-chosen and preferably a token), `vs` a 12-byte versionstamp element and `part` a partition integer.

### 3.1 KV and fs headers

| Key | Value |
|---|---|
| `pack("kv", fs) ‖ stored_key` | `versionstamp ‖ sealed value` (formats.md §4) |
| `pack("hdr", fs)` | `versionstamp ‖ header blob` (volume header + keyslots, opaque to the server) |
| `pack("q", fs, "bytes")` | `i64` little-endian, atomic add: bytes used by KV values and events |
| `pack("q", fs, "keys")` | `i64` little-endian, atomic add: number of KV keys |

Quota counters are read with snapshot reads, so concurrent commits don't conflict on them. That makes them approximate under heavy concurrency, which is fine for quotas.

### 3.2 Event log

| Key | Value |
|---|---|
| `pack("log", fs, topic, vs)` | event entry (below) |
| `pack("lk", fs, topic, key, vs)` | empty: per-key index (DESIGN-4 §1.1) |
| `pack("lh", fs, topic)` | `versionstamp` of the last append; watched to wake topic subscribers |
| `pack("gl", fs, vs)` | `topic`: fs-wide index in offset order, used by prefix subscriptions |
| `pack("gh", fs)` | `versionstamp` of the last append in the fs; watched to wake prefix subscribers |

**Event entry:** `u8 flags ‖ [key_token(16) if flags & 1] ‖ envelope`. The envelope is the sealed event (formats.md §4, kind 2), stored verbatim.

### 3.3 Consumer groups

| Key | Value |
|---|---|
| `pack("cg", fs, group)` | `{def, start, indexed_from?}` CBOR: the normalized definition (api.md §8.1), the start offset (the cursor of any partition or key that has not committed yet), and for `partitioned` the newest offset at creation: events after it are in the group's partition index, older ones are scanned |
| `pack("ct", fs, topic, group)` | `u8 mode ‖ [u32 partitions]`: index of the groups on a topic, read on append; the partition count is present for `partitioned` groups |
| `pack("lp", fs, group, part, vs)` | empty: a `partitioned` group's index of its topic's events by partition, written on append for every event after `indexed_from` |
| `pack("cc", fs, group, part)` | `offset`: committed cursor (`sequential` uses part 0; `single_key` uses part 0) |
| `pack("cl", fs, group, part)` | lease: `holder_fp(32) ‖ u64 token ‖ u64 expires_version` |
| `pack("kc", fs, group, key)` | `offset ‖ u64 last_claim_token`: last committed offset for one key (`per_key`) |
| `pack("kr", fs, group, vs, key)` | empty: the ready list, oldest pending event first (`per_key`) |
| `pack("kp", fs, group, key)` | `offset` of the key's entry in the ready list |
| `pack("km", fs, group, key)` | claim: `holder_fp(32) ‖ u64 token ‖ u64 expires_version ‖ offset` |
| `pack("ca", fs, group, sub)` | attempts: `offset ‖ u32 count`, where `sub` is the partition integer or the key token bytes |
| `pack("dlq", fs, group, vs)` | dead-letter entry: `offset(12) ‖ lp(topic) ‖ event entry` |

Mode bytes: 1 `broadcast`, 2 `sequential`, 3 `partitioned`, 4 `per_key`, 5 `single_key`.

### 3.4 Commits, ACL, server metadata

| Key | Value |
|---|---|
| `pack("cid", commit_id)` | idempotency record: `versionstamp ‖ u16 appended_count ‖ device_fp(32) ‖ u16 write_count` (§3.6) |
| `pack("cix", vs, commit_id)` | empty: expiry index for idempotency records, oldest first |
| `pack("acl", version)` | signed ACL (formats.md §9), every version kept (the membership log) |
| `pack("acl_head")` | `u64 version` of the current ACL |
| `pack("meta", name)` | metadata: `"version"` (embedded backend's version clock), `"challenge_key"` (32 random bytes, the cluster-wide challenge MAC key, api.md §3.1) |

Metadata belongs to one store (or one cluster): `export` skips every `meta` key, `import` ignores them in a file, and they don't count as data when `import` checks that the target is empty (operations.md §6). Each target keeps or creates its own.

### 3.5 Sessions, challenges, ephemeral ring

Every node of a cluster shares these, so a request can go to any node.

| Key | Value |
|---|---|
| `pack("sess", H(token))` | `user_fp(32) ‖ cred(32) ‖ u64 expires_unix ‖ u8 method` |
| `pack("chal", challenge)` | `u64 expires_unix`: a consumed challenge, kept until it would have expired |
| `pack("eph", fs, vs)` | ephemeral message: `u16 len ‖ topic ‖ sender_fp(32) ‖ data` |
| `pack("eh", fs)` | `versionstamp` of the last ephemeral message; watched by each node's tailer |

* `H(token)` is `BLAKE3.derive_key("zen-serve 2025 session token", token)`, so a dump or backup holds no usable bearer tokens.
* `cred` is the device fingerprint for a device session and the credential id for the other sign-in methods; `method` is the method id (auth.md §1, §3). A record written before sign-in methods existed is 72 bytes, without `method`, and is a device session.
* The sweeper deletes expired sessions and consumed challenges, and ephemeral entries older than `limits.ephemeral_ttl_secs` (api.md §9.1).

Leases are never deleted: the stored token is what keeps fencing tokens increasing.

### 3.6 Filesystem trees (spec/fs.md)

`tree`, `node`, `parent` and `chunk` are 16-byte ids, stored as byte-string elements. `hlc` is an integer element. `dev` is the 32-byte device fingerprint. `cvs` is a 12-byte versionstamp element.

| Key | Value |
|---|---|
| `pack("tr", fs, tree)` | tree header: `u64 ops ‖ chain(32) ‖ resync_before(12)` |
| `pack("th", fs, tree)` | `versionstamp` of the tree's last change; watched by `changes` long-polls |
| `pack("tn", fs, tree, node)` | node record (below) |
| `pack("tc", fs, tree, parent, node)` | empty: children index of the node's current parent |
| `pack("tm", fs, tree, hlc, dev)` | move log: `node ‖ parent ‖ u8 has_old ‖ [old_parent ‖ u64 old_hlc ‖ old_dev(32)]`: the parent and move timestamp the move replaced, restored on undo |
| `pack("tv", fs, tree, cvs, node)` | change index: empty, or `0x01` for a purged node's tombstone |
| `pack("tx", fs, tree, cvs, node)` | empty: tombstones only, so the sweeper can drop old ones without scanning the change index |
| `pack("ts", fs, tree)` | empty: the sweep index, a tree that may have work for the sweeper (fs.md §6). Every commit with a `move` on the tree sets it (a blind write); the sweeper clears it once the tree has no move log, no `TRASH` children and no tombstones |
| `pack("tsi", fs)` | empty: the sweep index of the fs is complete. Absent on data from before the index: the sweeper then lists every tree with a header (`tr`) in `ts` once, and sets it |
| `pack("tq", fs, tree)` | `node(16)`: the trash-purge cursor, the `TRASH` child the sweeper's next purge round starts at (fs.md §6); absent means the first. Cleared with the tree's `ts` entry |
| `pack("tp", fs, tree, node)` | `cvs(12)` of the node's tombstone: a purged node by id, so operations naming it are refused (fs.md §3.4); dropped with the tombstone |
| `pack("tf", fs, tree, node, dot)` | content version: `dev(32) ‖ u32 n ‖ n × chunk ‖ manifest` |
| `pack("ck", fs, chunk)` | sealed chunk |
| `pack("cr", fs, chunk)` | `i64` little-endian, atomic add: number of versions referencing the chunk |
| `pack("cz", fs, cvs, chunk)` | empty: chunk GC candidate, from upload or the release of a reference |
| `pack("cp", fs, chunk)` | `cvs(12)` of the chunk's newest GC candidate: only that candidate can delete the chunk, so each upload or release restarts the grace period |

**Node record:**

```
changed(12) ‖ u8 flags ‖ parent(16) ‖ u64 move_hlc ‖ move_dev(32)
            ‖ u64 meta_hlc ‖ meta_dev(32) ‖ u32 versions ‖ meta
```

* `changed` is written as a versionstamp at commit time, the same offset as the node's `tv` entry. The `u16` index numbers the changed nodes of the commit.
* `flags`: bit 0 = has a parent, bit 1 = has meta. When a bit is clear, the matching fields are zero.
* `meta` runs to the end of the record.

**Notes:**
* The move log is in timestamp order. Undo and redo read the range after a move's key.
* A node's old `tv` entry is cleared when it changes again, so the change index holds one entry per live node, plus tombstones.
* `resync_before` in the tree header is the newest tombstone the sweeper has dropped. A `changes` cursor before it gets 409 `resync`.
* The sweeper visits only the trees in `ts`. A move is the only operation that adds a move-log entry or a `TRASH` child, and tombstones only come from purging a tree that is listed, so a tree outside the index has nothing to sweep (chunk GC is per fs, through `cz`).
* The idempotency record (§3.4) is `versionstamp ‖ u16 appended_count ‖ device_fp(32) ‖ u16 write_count`. Records written before milestone 3.5 have no `write_count`, which then reads as 0.

### 3.7 Sign-in: credentials and origins (auth.md)

| Key | Value |
|---|---|
| `pack("origins")` | CBOR `[text]`: the pinned sign-in origins (auth.md §5.2); absent when nothing is pinned |

These keys are data, not server metadata: `export` copies them and `import` restores them, so a restored or migrated cluster keeps its pin.
